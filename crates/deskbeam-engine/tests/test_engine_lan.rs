//! LAN mode through two engines, with memory devices standing in for the network
//! adapters. Goes over n0's public relays, like the pairing test.

use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use deskbeam_engine::{
    Command, Engine, EngineConfig, LanBackend,
    model::{DeviceLan, LanMode, Permission, Snapshot},
};
use deskbeam_lan::{MemoryOs, packet::udp_packet};
use deskbeam_store::SqliteStore;
use tokio::sync::mpsc;

const START_TIMEOUT: Duration = Duration::from_secs(60);
const JOIN_DEADLINE: Duration = Duration::from_secs(90);
const SETTLE_DEADLINE: Duration = Duration::from_secs(45);

async fn start(dir: &tempfile::TempDir) -> (Engine, mpsc::UnboundedReceiver<MemoryOs>) {
    let (sink, devices) = mpsc::unbounded_channel();
    let mut config = EngineConfig::new(dir.path());
    config.lan = LanBackend::Memory(sink);
    let engine = tokio::time::timeout(
        START_TIMEOUT,
        Engine::start_and_wait(config, Arc::new(SqliteStore::open_in_memory().unwrap())),
    )
    .await
    .expect("engine start timed out")
    .expect("engine start failed");
    (engine, devices)
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
    assert!(
        waited.is_ok(),
        "timed out waiting for {what}; last {:?}",
        engine
            .snapshot()
            .devices
            .iter()
            .map(|d| &d.lan)
            .collect::<Vec<_>>()
    );
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

async fn allow_lan(engine: &Engine, other: &str) {
    engine
        .execute(Command::SetPermission {
            endpoint_id: other.to_string(),
            permission: Permission::JoinLan,
            allowed: true,
        })
        .await
        .unwrap();
}

fn lan_state(s: &Snapshot, id: &str) -> Option<DeviceLan> {
    s.device(id).and_then(|d| d.lan.clone())
}

fn address(s: &Snapshot) -> Ipv4Addr {
    match &s.lan {
        LanMode::On { address, .. } => address.parse().unwrap(),
        other => panic!("LAN mode is {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_devices_share_a_lan_once_both_allow_it() {
    let (a_dir, b_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let ((a, mut a_devices), (b, mut b_devices)) = tokio::join!(start(&a_dir), start(&b_dir));
    let (a_id, b_id) = (my_id(&a), my_id(&b));
    pair(&a, &b).await;
    wait_for(&a, "B to be online", |s| {
        s.device(&b_id).is_some_and(|d| d.online)
    })
    .await;
    wait_for(&b, "A to be online", |s| {
        s.device(&a_id).is_some_and(|d| d.online)
    })
    .await;

    // On, but nobody is allowed in yet: no members.
    let started = a.execute(Command::StartLan).await.unwrap();
    let mut a_os = a_devices.recv().await.unwrap();
    let a_address = address(&a.snapshot());
    assert_eq!(started["address"], a_address.to_string());
    assert!(deskbeam_lan::addr::is_device_address(a_address));
    assert_eq!(lan_state(&a.snapshot(), &b_id), None);

    // A lets B in; B has LAN mode off.
    allow_lan(&a, &b_id).await;
    wait_for(&a, "B to say LAN mode is off", |s| {
        matches!(lan_state(s, &b_id), Some(DeviceLan::Refused { .. }))
    })
    .await;

    // B turns it on but hasn't let A in: still refused.
    b.execute(Command::StartLan).await.unwrap();
    let mut b_os = b_devices.recv().await.unwrap();
    let b_address = address(&b.snapshot());
    assert_ne!(a_address, b_address);

    // B lets A in: they link up, and packets pass both ways.
    allow_lan(&b, &a_id).await;
    wait_for(&a, "the link to B", |s| {
        lan_state(s, &b_id)
            == Some(DeviceLan::Connected {
                address: b_address.to_string(),
            })
    })
    .await;
    wait_for(&b, "the link to A", |s| {
        matches!(lan_state(s, &a_id), Some(DeviceLan::Connected { .. }))
    })
    .await;

    let ping = udp_packet(a_address, b_address, b"ping");
    a_os.send.send(ping.clone().into()).await.unwrap();
    let got = tokio::time::timeout(SETTLE_DEADLINE, b_os.delivered.recv())
        .await
        .expect("no packet at B")
        .unwrap();
    assert_eq!(got, ping);
    let pong = udp_packet(b_address, a_address, &[5u8; 3000]);
    b_os.send.send(pong.clone().into()).await.unwrap();
    let got = tokio::time::timeout(SETTLE_DEADLINE, a_os.delivered.recv())
        .await
        .expect("no packet at A")
        .unwrap();
    assert_eq!(got, pong);

    // B takes the permission back: the link goes.
    b.execute(Command::SetPermission {
        endpoint_id: a_id.clone(),
        permission: Permission::JoinLan,
        allowed: false,
    })
    .await
    .unwrap();
    assert_eq!(lan_state(&b.snapshot(), &a_id), None);
    wait_for(&a, "B to turn A away", |s| {
        matches!(lan_state(s, &b_id), Some(DeviceLan::Refused { .. }))
    })
    .await;

    // Off: no adapter and no members.
    a.execute(Command::StopLan).await.unwrap();
    assert_eq!(a.snapshot().lan, LanMode::Off);
    assert_eq!(lan_state(&a.snapshot(), &b_id), None);
    assert!(
        a_os.delivered.recv().await.is_none(),
        "the device was closed"
    );

    a.shutdown().await;
    b.shutdown().await;
}
