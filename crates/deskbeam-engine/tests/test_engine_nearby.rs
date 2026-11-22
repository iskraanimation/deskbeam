//! Two engines on one machine find each other on the local network (mDNS), one asks the
//! other to pair, and the other accepts, through commands only.

use std::{sync::Arc, time::Duration};

use deskbeam_engine::{
    Command, CommandError, Engine, EngineConfig, Event,
    model::{Discoverability, Snapshot},
};
use deskbeam_store::{SqliteStore, Store};

const START_TIMEOUT: Duration = Duration::from_secs(60);
const NEARBY_DEADLINE: Duration = Duration::from_secs(45);
const SETTLE_DEADLINE: Duration = Duration::from_secs(30);

async fn start(dir: &tempfile::TempDir, store: Arc<dyn Store>) -> Engine {
    tokio::time::timeout(
        START_TIMEOUT,
        Engine::start_and_wait(EngineConfig::new(dir.path()), store),
    )
    .await
    .expect("engine start timed out")
    .expect("engine start failed")
}

async fn wait_for(
    engine: &Engine,
    what: &str,
    deadline: Duration,
    check: impl Fn(&Snapshot) -> bool,
) {
    let mut rx = engine.watch();
    let waited = tokio::time::timeout(deadline, async {
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
async fn nearby_engines_pair_by_request() {
    let a_dir = tempfile::tempdir().unwrap();
    let b_dir = tempfile::tempdir().unwrap();
    let (a, b) = tokio::join!(
        start(&a_dir, Arc::new(SqliteStore::open_in_memory().unwrap())),
        start(&b_dir, Arc::new(SqliteStore::open_in_memory().unwrap()))
    );
    let a_id = my_id(&a);
    let b_id = my_id(&b);
    let a_fingerprint = a
        .snapshot()
        .this_device
        .as_ref()
        .unwrap()
        .fingerprint
        .clone();
    assert_eq!(
        a_fingerprint.len(),
        14,
        "three groups of four: {a_fingerprint}"
    );

    wait_for(&b, "b to see a nearby", NEARBY_DEADLINE, |s| {
        s.nearby.iter().any(|d| d.endpoint_id == a_id)
    })
    .await;

    // Accepting a request nobody made is refused.
    assert!(matches!(
        a.execute(Command::AcceptPairRequest {
            endpoint_id: b_id.clone()
        })
        .await,
        Err(CommandError::NoPairRequest(_))
    ));

    let mut b_events = b.events();
    b.execute(Command::RequestNearbyPair {
        endpoint_id: a_id.clone(),
    })
    .await
    .expect("request to pair");

    // a sees the request with b's code, which is what b's owner sees on their screen.
    wait_for(&a, "a to receive the request", SETTLE_DEADLINE, |s| {
        s.pair_requests.iter().any(|r| r.endpoint_id == b_id)
    })
    .await;
    let b_fingerprint = b
        .snapshot()
        .this_device
        .as_ref()
        .unwrap()
        .fingerprint
        .clone();
    let shown = a.snapshot();
    let request = shown
        .pair_requests
        .iter()
        .find(|r| r.endpoint_id == b_id)
        .unwrap();
    assert_eq!(request.fingerprint, b_fingerprint);

    a.execute(Command::AcceptPairRequest {
        endpoint_id: b_id.clone(),
    })
    .await
    .expect("accept");
    assert!(a.snapshot().pair_requests.is_empty());

    wait_for(&a, "a to list b as paired", SETTLE_DEADLINE, |s| {
        s.device(&b_id).is_some()
    })
    .await;
    wait_for(&b, "b to list a as paired", SETTLE_DEADLINE, |s| {
        s.device(&a_id).is_some()
    })
    .await;

    // b hears that a accepted.
    let answered = tokio::time::timeout(SETTLE_DEADLINE, async {
        loop {
            match b_events.recv().await {
                Ok(Event::InviteAnswered {
                    endpoint_id,
                    accepted,
                    ..
                }) if endpoint_id == a_id => return accepted,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("events: {e}"),
            }
        }
    })
    .await
    .expect("b to hear the answer");
    assert!(answered);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discoverability_is_kept_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in(dir.path()).unwrap());
    let engine = start(&dir, store.clone()).await;
    assert_eq!(engine.snapshot().discoverability, Discoverability::Everyone);
    engine
        .execute(Command::SetDiscoverability {
            setting: Discoverability::Off,
        })
        .await
        .unwrap();
    assert_eq!(engine.snapshot().discoverability, Discoverability::Off);
    engine.shutdown().await;
    drop(engine);

    let engine = start(&dir, store).await;
    assert_eq!(engine.snapshot().discoverability, Discoverability::Off);
    engine.shutdown().await;
}
