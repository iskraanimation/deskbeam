//! The tunnel end to end in one process: two iroh endpoints talking directly (no relay), a
//! stand-in for the RDP server that echoes, and a gatekeeper that allows or denies.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use deskbeam_tunnel::{ALPN, Dialer, Forward, Gatekeeper, Outcome, Service, Status, TunnelServer};
use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, RelayMode, presets},
    protocol::Router,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

#[derive(Debug)]
struct Gate {
    allowed: Option<EndpointId>,
}

impl Gatekeeper for Gate {
    async fn allow(&self, peer: EndpointId, service: Service) -> bool {
        service == Service::RemoteDesktop && Some(peer) == self.allowed
    }
}

struct Direct {
    endpoint: Endpoint,
    host: EndpointAddr,
}

impl Dialer for Direct {
    async fn dial(&self) -> anyhow::Result<Connection> {
        Ok(self.endpoint.connect(self.host.clone(), ALPN).await?)
    }
}

/// A TCP server that sends back what it receives, standing in for Remote Desktop.
async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = tcp.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    address
}

async fn endpoint(alpns: Vec<Vec<u8>>) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(alpns)
        .bind()
        .await
        .unwrap()
}

/// A host serving `target` as Remote Desktop to `allowed`, and a client's forward to it.
async fn setup(
    target: SocketAddr,
    allow_client: bool,
) -> (Router, Forward, Arc<Mutex<Vec<Outcome>>>) {
    let host = endpoint(vec![ALPN.to_vec()]).await;
    let client = endpoint(vec![]).await;
    let gate = Gate {
        allowed: allow_client.then(|| client.id()),
    };
    let targets = HashMap::from([(Service::RemoteDesktop, target)]);
    let host_addr = host.addr();
    let router = Router::builder(host)
        .accept(ALPN, TunnelServer::new(gate, targets))
        .spawn();

    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let heard = outcomes.clone();
    let forward = Forward::start(
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        Service::RemoteDesktop,
        Direct {
            endpoint: client,
            host: host_addr,
        },
        move |outcome| heard.lock().unwrap().push(outcome),
    )
    .await
    .unwrap();
    (router, forward, outcomes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bytes_go_through_and_back() {
    let (_router, forward, outcomes) = setup(echo_server().await, true).await;

    // Two connections at once, as an RDP client may open.
    let mut sessions = Vec::new();
    for n in 0..2u8 {
        let mut tcp = TcpStream::connect(forward.local_addr()).await.unwrap();
        let message: Vec<u8> = (0..200_000u32).map(|i| (i as u8).wrapping_add(n)).collect();
        sessions.push(tokio::spawn(async move {
            let (mut r, mut w) = tcp.split();
            let send = async {
                w.write_all(&message).await.unwrap();
                w.shutdown().await.unwrap();
            };
            let mut back = Vec::new();
            let receive = r.read_to_end(&mut back);
            let (_, read) = tokio::join!(send, receive);
            read.unwrap();
            assert_eq!(back, message, "connection {n} came back unchanged");
        }));
    }
    for session in sessions {
        tokio::time::timeout(Duration::from_secs(30), session)
            .await
            .expect("echo timed out")
            .unwrap();
    }
    let outcomes = outcomes.lock().unwrap();
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Opened))
            .count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_not_allowed_is_refused() {
    let (_router, forward, outcomes) = setup(echo_server().await, false).await;
    let mut tcp = TcpStream::connect(forward.local_addr()).await.unwrap();
    let mut buf = Vec::new();
    // The forward closes the connection without passing anything through.
    tokio::time::timeout(Duration::from_secs(15), tcp.read_to_end(&mut buf))
        .await
        .expect("close timed out")
        .unwrap();
    assert!(buf.is_empty());
    assert!(matches!(
        outcomes.lock().unwrap().as_slice(),
        [Outcome::Refused(Status::Denied)]
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_without_the_service_says_so() {
    // Nothing listens here once the listener is dropped.
    let closed = {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        listener.local_addr().unwrap()
    };
    let (_router, forward, outcomes) = setup(closed, true).await;
    let mut tcp = TcpStream::connect(forward.local_addr()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), tcp.read_to_end(&mut buf))
        .await
        .expect("close timed out")
        .unwrap();
    assert!(matches!(
        outcomes.lock().unwrap().as_slice(),
        [Outcome::Refused(Status::Unavailable)]
    ));
}
