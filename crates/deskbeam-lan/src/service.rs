//! The LAN between paired devices: one link per peer, packets routed by address.
//!
//! A link is an iroh connection on [`ALPN`]. It opens with a handshake on a bi-stream:
//! the dialing side sends [`MAGIC`] and its address; the other side checks the peer may
//! join and the address is free, and answers with a status byte and its own address.
//! After that, packets travel as QUIC datagrams, or as frames on the same stream when
//! they are larger than a datagram on that path.
//!
//! A peer's packets reach this device's adapter only when they come from the peer's own
//! address and go to this device's address, so a peer can't pose as another device or
//! reach anything but this device.
//!
//! The link and its reconnecting follow iroh-lan's `connection.rs` and `direct_connect.rs`
//! (MIT, see `LICENSE-MIT-iroh-lan`), ported to iroh 1.x. Peers come from pairing rather
//! than from a network name and password.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    net::Ipv4Addr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};
use tracing::{debug, info};

use crate::{
    addr,
    device::{Device, Tasks},
    packet::{self, read_frame, write_frame},
};

pub const ALPN: &[u8] = b"deskbeam/lan/1";

/// The first bytes of a link's handshake.
const MAGIC: &[u8; 4] = b"DBL1";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the dialer looks for peers to connect.
const DIAL_EVERY: Duration = Duration::from_secs(2);
/// How long to wait before dialing a peer again after a failure or refusal.
const RETRY_AFTER: Duration = Duration::from_secs(10);
/// Packets larger than a datagram, queued for the link's stream.
const STREAM_QUEUE: usize = 256;

/// Why a peer's device turned a link down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Refusal {
    /// That device hasn't allowed this one to join its LAN.
    Denied = 1,
    /// LAN mode is off on that device.
    Off = 2,
    /// The two devices' addresses clash with each other or another device's.
    Conflict = 3,
}

impl Refusal {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Denied),
            2 => Some(Self::Off),
            3 => Some(Self::Conflict),
            _ => None,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Denied => "it has not allowed LAN mode with this device",
            Self::Off => "LAN mode is off there",
            Self::Conflict => "its LAN address clashes with another device's",
        })
    }
}

/// Where the link to one wanted peer stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerState {
    Connecting,
    Connected { address: Ipv4Addr },
    Refused(Refusal),
    Failed(String),
}

/// LAN mode while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanStatus {
    pub address: Ipv4Addr,
    /// The adapter's name as the OS shows it.
    pub adapter: String,
    /// The peers this device wants links with, and how each stands.
    pub peers: HashMap<EndpointId, PeerState>,
}

/// What the LAN asks of the app: who may join, and how to reach them.
pub trait Peers: Send + Sync + 'static {
    /// May `peer` be in this device's LAN? Asked for every link, either way round.
    fn allow(&self, peer: EndpointId) -> impl Future<Output = bool> + Send;
    /// Opens a connection to `peer` on [`ALPN`].
    fn dial(&self, peer: EndpointId) -> impl Future<Output = anyhow::Result<Connection>> + Send;
    /// Whether `peer` looks reachable now. Peers that don't aren't dialed; they can still
    /// dial in.
    fn online(&self, _peer: EndpointId) -> bool {
        true
    }
}

/// LAN mode: an iroh protocol handler for [`ALPN`], registered for the node's lifetime,
/// that takes part in the LAN between [`LanService::start`] and [`LanService::stop`].
pub struct LanService<P> {
    inner: Arc<Inner<P>>,
}

impl<P> Clone for LanService<P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<P> fmt::Debug for LanService<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LanService")
            .field(
                "running",
                &self.inner.running.read().expect("lock").is_some(),
            )
            .finish()
    }
}

struct Inner<P> {
    peers: P,
    running: RwLock<Option<Arc<Running>>>,
}

/// One session of LAN mode, from start to stop.
struct Running {
    me: EndpointId,
    address: Ipv4Addr,
    adapter: String,
    to_os: mpsc::Sender<Bytes>,
    links: Mutex<HashMap<EndpointId, Link>>,
    wanted: Mutex<HashSet<EndpointId>>,
    states: Mutex<HashMap<EndpointId, PeerState>>,
    /// Peers being dialed now, and when each may be dialed again.
    dialing: Mutex<HashSet<EndpointId>>,
    retry_at: Mutex<HashMap<EndpointId, Instant>>,
    next_link: AtomicU64,
    wake: tokio::sync::Notify,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

struct Link {
    id: u64,
    address: Ipv4Addr,
    conn: Connection,
    dialed_by_me: bool,
    /// Packets too large for a datagram go here, to the stream writer.
    large: mpsc::Sender<Bytes>,
    _tasks: Tasks,
}

impl<P: Peers> LanService<P> {
    pub fn new(peers: P) -> Self {
        Self {
            inner: Arc::new(Inner {
                peers,
                running: RwLock::new(None),
            }),
        }
    }

    /// Joins the LAN as `me` at `address`, with `device` as the adapter.
    pub fn start(&self, me: EndpointId, address: Ipv4Addr, device: Device) -> anyhow::Result<()> {
        let mut slot = self.inner.running.write().expect("lock");
        anyhow::ensure!(slot.is_none(), "LAN mode is already on");
        let adapter = device.name().to_string();
        let running = Arc::new(Running {
            me,
            address,
            adapter,
            to_os: device.to_os.clone(),
            links: Mutex::new(HashMap::new()),
            wanted: Mutex::new(HashSet::new()),
            states: Mutex::new(HashMap::new()),
            dialing: Mutex::new(HashSet::new()),
            retry_at: Mutex::new(HashMap::new()),
            next_link: AtomicU64::new(1),
            wake: tokio::sync::Notify::new(),
            tasks: Mutex::new(Vec::new()),
        });
        let outbound = tokio::spawn(route_outbound(running.clone(), device));
        let dialer = tokio::spawn(dial_loop(self.inner.clone(), Arc::downgrade(&running)));
        running
            .tasks
            .lock()
            .expect("lock")
            .extend([outbound, dialer]);
        *slot = Some(running);
        info!(%address, "LAN mode on");
        Ok(())
    }

    /// Leaves the LAN: closes every link and removes the adapter.
    pub fn stop(&self) {
        let Some(running) = self.inner.running.write().expect("lock").take() else {
            return;
        };
        for task in running.tasks.lock().expect("lock").drain(..) {
            task.abort();
        }
        for (_, link) in running.links.lock().expect("lock").drain() {
            link.conn.close(0u32.into(), b"LAN mode off");
        }
        info!("LAN mode off");
    }

    pub fn is_running(&self) -> bool {
        self.inner.running.read().expect("lock").is_some()
    }

    /// The peers in this device's LAN: it keeps links with them, and turns anyone else
    /// away. Links to others are closed.
    pub fn set_wanted(&self, peers: impl IntoIterator<Item = EndpointId>) {
        let Some(running) = self.running() else {
            return;
        };
        let wanted: HashSet<EndpointId> = peers.into_iter().collect();
        {
            let mut links = running.links.lock().expect("lock");
            links.retain(|peer, link| {
                let keep = wanted.contains(peer);
                if !keep {
                    debug!(%peer, "LAN: no longer wanted, closing the link");
                    link.conn.close(0u32.into(), b"not wanted");
                }
                keep
            });
        }
        running
            .states
            .lock()
            .expect("lock")
            .retain(|peer, _| wanted.contains(peer));
        let changed = {
            let mut current = running.wanted.lock().expect("lock");
            let changed = *current != wanted;
            *current = wanted;
            changed
        };
        if changed {
            running.wake.notify_one();
        }
    }

    pub fn status(&self) -> Option<LanStatus> {
        let running = self.running()?;
        let wanted = running.wanted.lock().expect("lock").clone();
        let states = running.states.lock().expect("lock");
        let peers = wanted
            .into_iter()
            .map(|peer| {
                let state = states.get(&peer).cloned().unwrap_or(PeerState::Connecting);
                (peer, state)
            })
            .collect();
        Some(LanStatus {
            address: running.address,
            adapter: running.adapter.clone(),
            peers,
        })
    }

    fn running(&self) -> Option<Arc<Running>> {
        self.inner.running.read().expect("lock").clone()
    }
}

impl<P: Peers> ProtocolHandler for LanService<P> {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.answer(peer, conn.clone())).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => debug!(%peer, "LAN handshake: {e:#}"),
            Err(_) => debug!(%peer, "LAN handshake timed out"),
        }
        // Kept until it closes: the link holds a clone and serves it from its own tasks.
        conn.closed().await;
        Ok(())
    }
}

impl<P: Peers> LanService<P> {
    /// The answering side of the handshake.
    async fn answer(&self, peer: EndpointId, conn: Connection) -> anyhow::Result<()> {
        let (mut send, mut recv) = conn.accept_bi().await?;
        let mut hello = [0u8; 8];
        recv.read_exact(&mut hello).await?;
        anyhow::ensure!(&hello[..4] == MAGIC, "not a LAN link");
        let claimed = Ipv4Addr::new(hello[4], hello[5], hello[6], hello[7]);

        let running = self.running();
        let refusal = match &running {
            None => Some(Refusal::Off),
            Some(r) if !r.wanted.lock().expect("lock").contains(&peer) => Some(Refusal::Denied),
            Some(_) if !self.inner.peers.allow(peer).await => Some(Refusal::Denied),
            Some(r) if r.clashes(peer, claimed) => Some(Refusal::Conflict),
            Some(_) => None,
        };
        let (Some(running), None) = (running, refusal) else {
            let refusal = refusal.unwrap_or(Refusal::Off);
            debug!(%peer, %claimed, ?refusal, "LAN: refused");
            send.write_all(&[refusal as u8]).await?;
            send.finish()?;
            // Give the answer time to arrive before the connection goes.
            let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
            return Ok(());
        };
        let mut answer = [0u8; 5];
        answer[1..].copy_from_slice(&running.address.octets());
        send.write_all(&answer).await?;
        running.install(peer, claimed, conn, send, recv, false);
        Ok(())
    }
}

impl Running {
    /// Whether `peer` claiming `address` would clash with this device or another peer.
    fn clashes(&self, peer: EndpointId, address: Ipv4Addr) -> bool {
        !addr::is_device_address(address)
            || address == self.address
            || self
                .links
                .lock()
                .expect("lock")
                .iter()
                .any(|(other, link)| *other != peer && link.address == address)
    }

    fn set_state(&self, peer: EndpointId, state: PeerState) {
        if self.wanted.lock().expect("lock").contains(&peer) {
            self.states.lock().expect("lock").insert(peer, state);
        }
    }

    /// Makes a handshaken connection the link to `peer`.
    ///
    /// Both devices may dial each other at once. Then both keep the connection dialed by
    /// the device with the smaller id, so they agree without talking about it.
    fn install(
        self: &Arc<Self>,
        peer: EndpointId,
        address: Ipv4Addr,
        conn: Connection,
        send: SendStream,
        recv: RecvStream,
        dialed_by_me: bool,
    ) {
        let mut links = self.links.lock().expect("lock");
        if let Some(existing) = links.get(&peer)
            && existing.conn.close_reason().is_none()
        {
            let preferred_dialed_by_me = self.me.as_bytes() < peer.as_bytes();
            if existing.dialed_by_me == preferred_dialed_by_me {
                debug!(%peer, "LAN: a second link, keeping the first");
                conn.close(0u32.into(), b"duplicate");
                return;
            }
            debug!(%peer, "LAN: a second link, replacing the first");
            existing.conn.close(0u32.into(), b"duplicate");
        }
        let id = self.next_link.fetch_add(1, Ordering::Relaxed);
        let (large, large_rx) = mpsc::channel(STREAM_QUEUE);
        let reader = tokio::spawn(read_link(
            Arc::downgrade(self),
            peer,
            id,
            address,
            conn.clone(),
            recv,
        ));
        let writer = tokio::spawn(write_link(send, large_rx));
        links.insert(
            peer,
            Link {
                id,
                address,
                conn,
                dialed_by_me,
                large,
                _tasks: Tasks(vec![reader, writer]),
            },
        );
        drop(links);
        info!(%peer, %address, "LAN link up");
        self.set_state(peer, PeerState::Connected { address });
    }

    /// Takes a packet from a peer to this device's adapter, if it may go there.
    fn deliver(&self, from: Ipv4Addr, packet: Bytes) {
        match packet::parse(&packet) {
            Some(h) if h.src == from && h.dst == self.address => {
                let _ = self.to_os.try_send(packet);
            }
            Some(h) => {
                debug!(src = %h.src, dst = %h.dst, "LAN: dropped a packet not from {from} to this device")
            }
            None => debug!("LAN: dropped a packet that is not IPv4"),
        }
    }
}

/// Reads a link's datagrams and stream frames until the connection ends.
async fn read_link(
    running: std::sync::Weak<Running>,
    peer: EndpointId,
    id: u64,
    address: Ipv4Addr,
    conn: Connection,
    mut recv: RecvStream,
) {
    let mut stream_open = true;
    loop {
        let packet = tokio::select! {
            datagram = conn.read_datagram() => match datagram {
                Ok(d) => d,
                Err(e) => {
                    debug!(%peer, "LAN link closed: {e}");
                    break;
                }
            },
            frame = read_frame(&mut recv), if stream_open => match frame {
                Ok(Some(f)) => f,
                Ok(None) | Err(_) => {
                    stream_open = false;
                    continue;
                }
            },
        };
        let Some(running) = running.upgrade() else {
            return;
        };
        running.deliver(address, packet);
    }
    conn.close(0u32.into(), b"closed");
    let Some(running) = running.upgrade() else {
        return;
    };
    let mut links = running.links.lock().expect("lock");
    if links.get(&peer).is_some_and(|l| l.id == id) {
        links.remove(&peer);
        drop(links);
        info!(%peer, "LAN link down");
        // A link that worked is dialed again at once, whatever an earlier dial left behind
        // (the losing side of two devices dialing each other at once fails, for one).
        running.retry_at.lock().expect("lock").remove(&peer);
        running.set_state(peer, PeerState::Connecting);
        running.wake.notify_one();
    }
}

async fn write_link(mut send: SendStream, mut large: mpsc::Receiver<Bytes>) {
    while let Some(packet) = large.recv().await {
        if write_frame(&mut send, &packet).await.is_err() {
            return;
        }
    }
}

/// Sends what programs on this device send into the LAN to the peer with that address.
async fn route_outbound(running: Arc<Running>, mut device: Device) {
    while let Some(packet) = device.from_os.recv().await {
        let Some(header) = packet::parse(&packet) else {
            continue;
        };
        if header.src != running.address {
            continue;
        }
        let route = {
            let links = running.links.lock().expect("lock");
            links
                .values()
                .find(|l| l.address == header.dst)
                .map(|l| (l.conn.clone(), l.large.clone()))
        };
        let Some((conn, large)) = route else {
            continue;
        };
        if conn
            .max_datagram_size()
            .is_some_and(|max| packet.len() <= max)
        {
            // A datagram that can't go now is dropped, like a packet on a busy network.
            let _ = conn.send_datagram(packet);
        } else {
            let _ = large.try_send(packet);
        }
    }
}

/// Dials the wanted peers that have no link, while LAN mode is on.
async fn dial_loop<P: Peers>(inner: Arc<Inner<P>>, running: std::sync::Weak<Running>) {
    loop {
        let Some(r) = running.upgrade() else { return };
        let due: Vec<EndpointId> = {
            let wanted = r.wanted.lock().expect("lock");
            let links = r.links.lock().expect("lock");
            let dialing = r.dialing.lock().expect("lock");
            let retry_at = r.retry_at.lock().expect("lock");
            let now = Instant::now();
            wanted
                .iter()
                .filter(|p| !links.contains_key(*p) && !dialing.contains(*p))
                .filter(|p| retry_at.get(*p).is_none_or(|at| now >= *at))
                .filter(|p| inner.peers.online(**p))
                .copied()
                .collect()
        };
        for peer in due {
            r.dialing.lock().expect("lock").insert(peer);
            let (inner, r2) = (inner.clone(), r.clone());
            let task = tokio::spawn(async move {
                let r = r2;
                let outcome =
                    tokio::time::timeout(HANDSHAKE_TIMEOUT, dial(&inner.peers, &r, peer)).await;
                let state = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(DialError::Refused(refusal))) => Some(PeerState::Refused(refusal)),
                    Ok(Err(DialError::Failed(e))) => Some(PeerState::Failed(e)),
                    Err(_) => Some(PeerState::Failed("no answer".into())),
                };
                if let Some(state) = state {
                    debug!(%peer, ?state, "LAN: dial did not make a link");
                    r.retry_at
                        .lock()
                        .expect("lock")
                        .insert(peer, Instant::now() + RETRY_AFTER);
                    // A link the peer dialed in the meantime is not undone by this.
                    if !r.links.lock().expect("lock").contains_key(&peer) {
                        r.set_state(peer, state);
                    }
                }
                r.dialing.lock().expect("lock").remove(&peer);
            });
            r.tasks.lock().expect("lock").push(task);
        }
        r.tasks.lock().expect("lock").retain(|t| !t.is_finished());
        let wake = async { r.wake.notified().await };
        tokio::select! {
            () = wake => {}
            () = tokio::time::sleep(DIAL_EVERY) => {}
        }
    }
}

enum DialError {
    Refused(Refusal),
    Failed(String),
}

impl<E: fmt::Display> From<E> for DialError {
    fn from(e: E) -> Self {
        Self::Failed(e.to_string())
    }
}

/// The dialing side of the handshake.
async fn dial<P: Peers>(
    peers: &P,
    running: &Arc<Running>,
    peer: EndpointId,
) -> Result<(), DialError> {
    if !peers.allow(peer).await {
        return Err(DialError::Failed("not allowed on this device".into()));
    }
    let conn = peers.dial(peer).await.map_err(|e| format!("{e:#}"))?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let mut hello = [0u8; 8];
    hello[..4].copy_from_slice(MAGIC);
    hello[4..].copy_from_slice(&running.address.octets());
    send.write_all(&hello).await?;
    let mut status = [0u8; 1];
    recv.read_exact(&mut status).await?;
    if status[0] != 0 {
        conn.close(0u32.into(), b"refused");
        return Err(match Refusal::from_byte(status[0]) {
            Some(refusal) => DialError::Refused(refusal),
            None => DialError::Failed(format!("the other device answered {}", status[0])),
        });
    }
    let mut theirs = [0u8; 4];
    recv.read_exact(&mut theirs).await?;
    let address = Ipv4Addr::from(theirs);
    if running.clashes(peer, address) {
        conn.close(0u32.into(), b"address clash");
        return Err(DialError::Refused(Refusal::Conflict));
    }
    running.install(peer, address, conn, send, recv, true);
    Ok(())
}
