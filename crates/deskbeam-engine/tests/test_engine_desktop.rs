//! Remote desktop through two engines, with an echo server standing in for the host's
//! Remote Desktop. Goes over n0's public relays, like the pairing test.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use deskbeam_engine::{
    Command, Engine, EngineConfig, Event,
    model::{Permission, Snapshot},
};
use deskbeam_store::SqliteStore;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const START_TIMEOUT: Duration = Duration::from_secs(60);
const JOIN_DEADLINE: Duration = Duration::from_secs(90);
const SETTLE_DEADLINE: Duration = Duration::from_secs(45);

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

async fn start(dir: &tempfile::TempDir, desktop: Option<SocketAddr>) -> Engine {
    let mut config = EngineConfig::new(dir.path());
    if let Some(target) = desktop {
        config.desktop_target = target;
    }
    tokio::time::timeout(
        START_TIMEOUT,
        Engine::start_and_wait(config, Arc::new(SqliteStore::open_in_memory().unwrap())),
    )
    .await
    .expect("engine start timed out")
    .expect("engine start failed")
}

async fn wait_for(engine: &Engine, what: &str, check: impl Fn(&Snapshot) -> bool) {
    let mut rx = engine.watch();
    let waited = tokio::time::timeout(SETTLE_DEADLINE, async {
        loop {
            if check(&rx.borrow_and_update()) {
                return;
            }
            rx.changed().await.expect("engine gone");
        }
    })
    .await;
    assert!(waited.is_ok(), "timed out waiting for {what}");
}

fn my_id(engine: &Engine) -> String {
    engine
        .snapshot()
        .this_device
        .as_ref()
        .expect("this device")
        .endpoint_id
        .clone()
}

async fn pair(host: &Engine, client: &Engine) {
    let opened = host
        .execute(Command::OpenPairing {
            ttl_secs: Some(300),
        })
        .await
        .unwrap();
    let code = opened["code"].as_str().unwrap().to_string();
    let end = tokio::time::Instant::now() + JOIN_DEADLINE;
    loop {
        match tokio::time::timeout(
            Duration::from_secs(30),
            client.execute(Command::JoinPairing { code: code.clone() }),
        )
        .await
        {
            Ok(Ok(_)) => break,
            Ok(Err(e)) => eprintln!("join failed, retrying: {e}"),
            Err(_) => eprintln!("join timed out, retrying"),
        }
        assert!(tokio::time::Instant::now() < end, "pairing did not succeed");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    host.execute(Command::ClosePairing).await.unwrap();
}

/// Sends `message` through the tunnel and returns what comes back.
async fn round_trip(port: u16, message: &[u8]) -> Vec<u8> {
    let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let (mut r, mut w) = tcp.split();
    // A refused connection is dropped with our bytes unread, so it may end in a reset
    // rather than a clean close; either way nothing comes back.
    let send = async {
        if w.write_all(message).await.is_ok() {
            let _ = w.shutdown().await;
        }
    };
    let mut back = Vec::new();
    let (_, read) = tokio::join!(send, r.read_to_end(&mut back));
    match read {
        Ok(_) => back,
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => Vec::new(),
        Err(e) => panic!("reading from the tunnel: {e}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_desktop_asks_then_goes_through() {
    let host_dir = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let (host, client) = tokio::join!(
        start(&host_dir, Some(echo_server().await)),
        start(&client_dir, None)
    );
    let (host_id, client_id) = (my_id(&host), my_id(&client));
    pair(&host, &client).await;
    wait_for(&client, "the host to be online", |s| {
        s.device(&host_id).is_some_and(|d| d.online)
    })
    .await;
    wait_for(&host, "the client to be listed", |s| {
        s.device(&client_id).is_some()
    })
    .await;

    // The host's Remote Desktop listens: the echo server.
    wait_for(&host, "the host to see its desktop server", |s| {
        s.desktop_host.listening
    })
    .await;

    let opened = client
        .execute(Command::ConnectDesktop {
            endpoint_id: host_id.clone(),
            launch: false,
        })
        .await
        .unwrap();
    let port = opened["port"].as_u64().unwrap() as u16;
    assert_eq!(port, 13390, "the first device gets the first port");
    assert_eq!(opened["address"], format!("localhost:{port}"));
    assert!(std::path::Path::new(opened["rdpFile"].as_str().unwrap()).exists());
    assert_eq!(
        client
            .snapshot()
            .device(&host_id)
            .unwrap()
            .desktop
            .map(|d| d.port),
        Some(port)
    );

    // Not allowed yet: the host's owner is asked, and allows once.
    let mut host_events = host.events();
    let session = tokio::spawn(round_trip(port, b"hello desktop"));
    wait_for(&host, "the host to be asked", |s| {
        s.desktop_requests
            .iter()
            .any(|r| r.endpoint_id == client_id)
    })
    .await;
    let asked = loop {
        match host_events.recv().await.unwrap() {
            Event::DesktopRequested { endpoint_id, .. } => break endpoint_id,
            _ => continue,
        }
    };
    assert_eq!(asked, client_id);
    host.execute(Command::AnswerDesktopRequest {
        endpoint_id: client_id.clone(),
        allow: true,
        always: false,
    })
    .await
    .unwrap();
    let back = tokio::time::timeout(SETTLE_DEADLINE, session)
        .await
        .expect("session timed out")
        .unwrap();
    assert_eq!(back, b"hello desktop");
    assert!(host.snapshot().desktop_requests.is_empty());
    assert!(
        !host
            .snapshot()
            .device(&client_id)
            .unwrap()
            .permissions
            .remote_desktop,
        "allowing once does not change the permission"
    );

    // Allowed once lasts a while: the next connection goes straight through.
    let back = tokio::time::timeout(SETTLE_DEADLINE, round_trip(port, b"again"))
        .await
        .expect("second session timed out");
    assert_eq!(back, b"again");

    // The port is the device's: disconnecting and connecting again keeps it.
    client
        .execute(Command::DisconnectDesktop {
            endpoint_id: host_id.clone(),
        })
        .await
        .unwrap();
    assert!(
        client
            .snapshot()
            .device(&host_id)
            .unwrap()
            .desktop
            .is_none()
    );
    let reopened = client
        .execute(Command::ConnectDesktop {
            endpoint_id: host_id.clone(),
            launch: false,
        })
        .await
        .unwrap();
    assert_eq!(reopened["port"].as_u64().unwrap() as u16, port);

    host.shutdown().await;
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_desktop_denied_and_always_allowed() {
    let host_dir = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let (host, client) = tokio::join!(
        start(&host_dir, Some(echo_server().await)),
        start(&client_dir, None)
    );
    let (host_id, client_id) = (my_id(&host), my_id(&client));
    pair(&host, &client).await;
    wait_for(&client, "the host to be online", |s| {
        s.device(&host_id).is_some_and(|d| d.online)
    })
    .await;
    wait_for(&host, "the client to be listed", |s| {
        s.device(&client_id).is_some()
    })
    .await;

    let opened = client
        .execute(Command::ConnectDesktop {
            endpoint_id: host_id.clone(),
            launch: false,
        })
        .await
        .unwrap();
    let port = opened["port"].as_u64().unwrap() as u16;

    // Denied: the connection closes with nothing passed through, and the client hears why.
    let mut client_events = client.events();
    let session = tokio::spawn(round_trip(port, b"let me in"));
    wait_for(&host, "the host to be asked", |s| {
        s.desktop_requests
            .iter()
            .any(|r| r.endpoint_id == client_id)
    })
    .await;
    host.execute(Command::AnswerDesktopRequest {
        endpoint_id: client_id.clone(),
        allow: false,
        always: false,
    })
    .await
    .unwrap();
    let back = tokio::time::timeout(SETTLE_DEADLINE, session)
        .await
        .expect("denied session timed out")
        .unwrap();
    assert!(back.is_empty());
    let refused = tokio::time::timeout(SETTLE_DEADLINE, async {
        loop {
            if let Event::DesktopRefused { reason, .. } = client_events.recv().await.unwrap() {
                return reason;
            }
        }
    })
    .await
    .expect("the client to hear the refusal");
    assert!(refused.contains("did not allow"), "{refused}");

    // Always allow: the permission is set, and connections go straight through.
    let session = tokio::spawn(round_trip(port, b"please"));
    wait_for(&host, "the host to be asked again", |s| {
        s.desktop_requests
            .iter()
            .any(|r| r.endpoint_id == client_id)
    })
    .await;
    host.execute(Command::AnswerDesktopRequest {
        endpoint_id: client_id.clone(),
        allow: true,
        always: true,
    })
    .await
    .unwrap();
    assert_eq!(
        tokio::time::timeout(SETTLE_DEADLINE, session)
            .await
            .expect("allowed session timed out")
            .unwrap(),
        b"please"
    );
    assert!(
        host.snapshot()
            .device(&client_id)
            .unwrap()
            .permissions
            .get(Permission::RemoteDesktop)
    );
    assert_eq!(
        tokio::time::timeout(SETTLE_DEADLINE, round_trip(port, b"no question"))
            .await
            .expect("permitted session timed out"),
        b"no question"
    );

    host.shutdown().await;
    client.shutdown().await;
}
