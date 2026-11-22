//! Two engines pair, set a permission and unpair, through commands only, the way the app
//! and the CLI drive them. Goes over the internet through n0's public relays, like
//! DashBeam's own pairing test.

use std::{sync::Arc, time::Duration};

use deskbeam_engine::{
    Command, CommandError, Engine, EngineConfig,
    model::{NodeStatus, Permission, Snapshot},
};
use deskbeam_store::SqliteStore;

const START_TIMEOUT: Duration = Duration::from_secs(60);
const JOIN_DEADLINE: Duration = Duration::from_secs(90);
const SETTLE_DEADLINE: Duration = Duration::from_secs(30);

async fn start(dir: &tempfile::TempDir) -> Engine {
    tokio::time::timeout(
        START_TIMEOUT,
        Engine::start_and_wait(
            EngineConfig::new(dir.path()),
            Arc::new(SqliteStore::open_in_memory().unwrap()),
        ),
    )
    .await
    .expect("engine start timed out")
    .expect("engine start failed")
}

/// Waits until the engine's snapshot satisfies `check`.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engines_pair_set_permissions_and_unpair() {
    let host_dir = tempfile::tempdir().unwrap();
    let joiner_dir = tempfile::tempdir().unwrap();
    let (host, joiner) = tokio::join!(start(&host_dir), start(&joiner_dir));
    assert!(matches!(host.snapshot().node, NodeStatus::Running { .. }));
    let host_id = my_id(&host);
    let joiner_id = my_id(&joiner);
    assert_ne!(
        host_id, joiner_id,
        "each data directory has its own identity"
    );

    // Commands for a device that is not paired name the problem.
    let unknown = joiner
        .execute(Command::ForgetDevice {
            endpoint_id: host_id.clone(),
        })
        .await;
    assert!(matches!(unknown, Err(CommandError::UnknownDevice(_))));

    let opened = host
        .execute(Command::OpenPairing {
            ttl_secs: Some(300),
        })
        .await
        .unwrap();
    let code = opened["code"].as_str().expect("pairing code").to_string();
    assert_eq!(
        host.snapshot().pairing.as_ref().map(|p| p.code.as_str()),
        Some(code.as_str())
    );

    // Address discovery can lag just after the endpoints come online.
    let end = tokio::time::Instant::now() + JOIN_DEADLINE;
    loop {
        match tokio::time::timeout(
            Duration::from_secs(30),
            joiner.execute(Command::JoinPairing { code: code.clone() }),
        )
        .await
        {
            Ok(Ok(_)) => break,
            Ok(Err(e)) => eprintln!("join failed, retrying: {e}"),
            Err(_) => eprintln!("join timed out, retrying"),
        }
        assert!(
            tokio::time::Instant::now() < end,
            "pairing did not succeed within {JOIN_DEADLINE:?}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    wait_for(&joiner, "the joiner to list the host", |s| {
        s.device(&host_id).is_some()
    })
    .await;
    wait_for(&host, "the host to list the joiner", |s| {
        s.device(&joiner_id).is_some()
    })
    .await;

    // Remote desktop starts off, and a command turns it on.
    let listed = joiner.snapshot();
    let device = listed.device(&host_id).unwrap();
    assert!(!device.permissions.remote_desktop);
    joiner
        .execute(Command::SetPermission {
            endpoint_id: host_id.clone(),
            permission: Permission::RemoteDesktop,
            allowed: true,
        })
        .await
        .unwrap();
    assert!(
        joiner
            .snapshot()
            .device(&host_id)
            .unwrap()
            .permissions
            .remote_desktop
    );

    // Auto-accept goes through DashBeam's trusted flag.
    joiner
        .execute(Command::SetPermission {
            endpoint_id: host_id.clone(),
            permission: Permission::AutoAcceptFiles,
            allowed: true,
        })
        .await
        .unwrap();
    assert!(
        joiner
            .snapshot()
            .device(&host_id)
            .unwrap()
            .permissions
            .auto_accept_files
    );

    host.execute(Command::ClosePairing).await.unwrap();
    assert!(host.snapshot().pairing.is_none());

    joiner
        .execute(Command::ForgetDevice {
            endpoint_id: host_id.clone(),
        })
        .await
        .unwrap();
    wait_for(&joiner, "the joiner to drop the host", |s| {
        s.device(&host_id).is_none()
    })
    .await;

    host.shutdown().await;
    joiner.shutdown().await;
    assert_eq!(host.snapshot().node, NodeStatus::Stopped);
    assert!(matches!(
        host.execute(Command::ClosePairing).await,
        Err(CommandError::NodeNotRunning)
    ));
}
