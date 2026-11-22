//! TCP port forwarding over iroh (plan §2, option A).
//!
//! The client listens on a loopback port. Each TCP connection made to it opens one QUIC
//! bi-stream to the host on [`ALPN`]. The stream begins with a header naming the
//! [`Service`]; the host asks its [`Gatekeeper`] whether that peer may use it, connects to
//! the service's local address (`127.0.0.1:3389` for Remote Desktop), answers with a status
//! byte, and then both sides copy bytes until either closes.
//!
//! The host never opens a port to the network. The service is reached only from the host's
//! own loopback, only by peers the gatekeeper lets through, and only over iroh's encrypted,
//! key-authenticated connection.

use std::{
    collections::HashMap,
    fmt,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::Mutex,
    task::JoinHandle,
};
use tracing::{debug, warn};

pub const ALPN: &[u8] = b"deskbeam/tunnel/1";

/// The first bytes of every tunnel stream.
const MAGIC: &[u8; 4] = b"DBT1";

/// How long the host waits for its local service to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What a tunnel leads to on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Service {
    RemoteDesktop = 1,
}

impl Service {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::RemoteDesktop),
            _ => None,
        }
    }
}

/// The host's answer to a stream's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Open = 0,
    /// The host's owner has not allowed this device.
    Denied = 1,
    /// Nothing listens on the host for this service (e.g. Remote Desktop is off).
    Unavailable = 2,
    /// The host does not offer this service.
    UnknownService = 3,
}

impl Status {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Open),
            1 => Some(Self::Denied),
            2 => Some(Self::Unavailable),
            3 => Some(Self::UnknownService),
            _ => None,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Open => "open",
            Self::Denied => "the other device did not allow this one",
            Self::Unavailable => "nothing is listening on the other device (is Remote Desktop on?)",
            Self::UnknownService => "the other device does not offer this",
        })
    }
}

/// Decides, on the host, whether a peer may use a service.
pub trait Gatekeeper: Send + Sync + fmt::Debug + 'static {
    /// May take a while: it can wait for the host's owner to answer.
    fn allow(&self, peer: EndpointId, service: Service) -> impl Future<Output = bool> + Send;
}

/// The host side: an iroh protocol handler for [`ALPN`].
#[derive(Debug)]
pub struct TunnelServer<G> {
    gate: Arc<G>,
    targets: Arc<HashMap<Service, SocketAddr>>,
}

impl<G: Gatekeeper> TunnelServer<G> {
    /// `targets` names, per service, the local address the host connects to.
    pub fn new(gate: G, targets: HashMap<Service, SocketAddr>) -> Self {
        Self {
            gate: Arc::new(gate),
            targets: Arc::new(targets),
        }
    }
}

impl<G: Gatekeeper> ProtocolHandler for TunnelServer<G> {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id();
        while let Ok((send, recv)) = connection.accept_bi().await {
            let (gate, targets) = (self.gate.clone(), self.targets.clone());
            tokio::spawn(async move {
                if let Err(e) = serve_stream(gate.as_ref(), &targets, peer, send, recv).await {
                    debug!(%peer, "tunnel stream ended: {e:#}");
                }
            });
        }
        Ok(())
    }
}

async fn serve_stream<G: Gatekeeper>(
    gate: &G,
    targets: &HashMap<Service, SocketAddr>,
    peer: EndpointId,
    mut send: SendStream,
    mut recv: RecvStream,
) -> anyhow::Result<()> {
    let mut header = [0u8; 5];
    recv.read_exact(&mut header).await?;
    anyhow::ensure!(&header[..4] == MAGIC, "not a tunnel stream");

    let answer = |status: Status| [status as u8];
    let Some(service) = Service::from_byte(header[4]) else {
        send.write_all(&answer(Status::UnknownService)).await?;
        send.finish()?;
        return Ok(());
    };
    let Some(target) = targets.get(&service).copied() else {
        send.write_all(&answer(Status::UnknownService)).await?;
        send.finish()?;
        return Ok(());
    };
    if !gate.allow(peer, service).await {
        debug!(%peer, ?service, "tunnel denied");
        send.write_all(&answer(Status::Denied)).await?;
        send.finish()?;
        return Ok(());
    }
    let tcp = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target)).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => {
            debug!(%target, "service not reachable: {e}");
            send.write_all(&answer(Status::Unavailable)).await?;
            send.finish()?;
            return Ok(());
        }
        Err(_) => {
            send.write_all(&answer(Status::Unavailable)).await?;
            send.finish()?;
            return Ok(());
        }
    };
    let _ = tcp.set_nodelay(true);
    send.write_all(&answer(Status::Open)).await?;
    debug!(%peer, ?service, %target, "tunnel open");
    pipe(tcp, send, recv).await
}

/// Copies both ways until both directions are done.
async fn pipe(tcp: TcpStream, mut send: SendStream, mut recv: RecvStream) -> anyhow::Result<()> {
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let to_quic = async {
        let copied = tokio::io::copy(&mut tcp_read, &mut send).await;
        let _ = send.finish();
        copied
    };
    let to_tcp = async {
        let copied = tokio::io::copy(&mut recv, &mut tcp_write).await;
        let _ = tcp_write.shutdown().await;
        copied
    };
    let (up, down) = tokio::join!(to_quic, to_tcp);
    debug!(
        sent = up.as_ref().ok(),
        received = down.as_ref().ok(),
        "tunnel stream closed"
    );
    Ok(())
}

/// Opens the client's connection to the host. The engine implements it with the node's
/// endpoint, which can be rebuilt between dials.
pub trait Dialer: Send + Sync + 'static {
    fn dial(&self) -> impl Future<Output = anyhow::Result<Connection>> + Send;
}

/// What happened to one forwarded TCP connection, for the client to report.
#[derive(Debug, Clone)]
pub enum Outcome {
    Opened,
    Refused(Status),
    Failed(String),
}

/// The client side: a loopback listener whose connections go to one host's service.
/// Dropping it stops listening; streams already open run until they close.
pub struct Forward {
    local: SocketAddr,
    active: Arc<AtomicUsize>,
    listener: JoinHandle<()>,
}

impl Forward {
    /// Listens on `bind` (use a loopback address) and forwards to `service` on the host
    /// `dialer` reaches. `report` hears how each connection went.
    pub async fn start<D: Dialer>(
        bind: SocketAddr,
        service: Service,
        dialer: D,
        report: impl Fn(Outcome) + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(bind).await?;
        let local = listener.local_addr()?;
        let active = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(Client {
            dialer,
            connection: Mutex::new(None),
            report: Box::new(report),
        });
        let counter = active.clone();
        let task = tokio::spawn(async move {
            loop {
                let (tcp, from) = match listener.accept().await {
                    Ok(accepted) => accepted,
                    Err(e) => {
                        warn!("tunnel listener: {e}");
                        continue;
                    }
                };
                debug!(%from, "forwarding a connection");
                let (client, counter) = (shared.clone(), counter.clone());
                tokio::spawn(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let outcome = client.forward(tcp, service).await;
                    counter.fetch_sub(1, Ordering::SeqCst);
                    if let Err(e) = outcome {
                        (client.report)(Outcome::Failed(format!("{e:#}")));
                    }
                });
            }
        });
        Ok(Self {
            local,
            active,
            listener: task,
        })
    }

    /// Where the RDP client connects, e.g. `127.0.0.1:13390`.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Forwarded connections open now.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

impl fmt::Debug for Forward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Forward")
            .field("local", &self.local)
            .field("active", &self.active())
            .finish()
    }
}

struct Client<D> {
    dialer: D,
    /// One QUIC connection carries all of this forward's streams; it is redialed when closed.
    connection: Mutex<Option<Connection>>,
    report: Box<dyn Fn(Outcome) + Send + Sync>,
}

impl<D: Dialer> Client<D> {
    async fn connection(&self) -> anyhow::Result<Connection> {
        let mut held = self.connection.lock().await;
        if let Some(conn) = held.as_ref()
            && conn.close_reason().is_none()
        {
            return Ok(conn.clone());
        }
        let conn = self.dialer.dial().await?;
        *held = Some(conn.clone());
        Ok(conn)
    }

    async fn forward(&self, tcp: TcpStream, service: Service) -> anyhow::Result<()> {
        let _ = tcp.set_nodelay(true);
        let conn = self.connection().await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let mut header = [0u8; 5];
        header[..4].copy_from_slice(MAGIC);
        header[4] = service as u8;
        send.write_all(&header).await?;
        let mut status = [0u8; 1];
        recv.read_exact(&mut status).await?;
        match Status::from_byte(status[0]) {
            Some(Status::Open) => {
                (self.report)(Outcome::Opened);
                pipe(tcp, send, recv).await
            }
            Some(refused) => {
                (self.report)(Outcome::Refused(refused));
                Ok(())
            }
            None => anyhow::bail!("the host answered {}", status[0]),
        }
    }
}
