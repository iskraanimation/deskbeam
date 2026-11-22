//! Video in calls through engines over n0's public relays, with moving test patterns in
//! place of the cameras: a camera turned on shows on the other end at its size, both ways;
//! turning it off clears it there; a call starts without video. A shared "screen" (a larger
//! test pattern) replaces the camera, arrives at full size marked as shared, and the camera
//! comes back when sharing stops.

use std::{path::Path, sync::Arc, time::Duration};

use deskbeam_engine::{
    CallAudio, CallCamera, Command, Engine, EngineConfig,
    calls::{devices::ToneDevices, video::source::TestPattern},
    model::{CallState, Snapshot},
};
use deskbeam_store::SqliteStore;

const START_TIMEOUT: Duration = Duration::from_secs(60);
const JOIN_DEADLINE: Duration = Duration::from_secs(90);
const DEADLINE: Duration = Duration::from_secs(60);

async fn start(dir: &Path, width: usize, height: usize) -> Engine {
    let mut config = EngineConfig::new(dir);
    config.audio = CallAudio(Some(Arc::new(ToneDevices::default())));
    config.camera = CallCamera(Some(Arc::new(TestPattern {
        width,
        height,
        fps: 15,
    })));
    // A "screen" larger than a camera sends: it arrives at full size.
    config.screen = CallCamera(Some(Arc::new(TestPattern {
        width: 1280,
        height: 720,
        fps: 10,
    })));
    let store = Arc::new(SqliteStore::open_in(dir).unwrap());
    tokio::time::timeout(START_TIMEOUT, Engine::start_and_wait(config, store))
        .await
        .expect("engine start timed out")
        .expect("engine start failed")
}

/// Waits until `engine` shows the other end's picture at `size`; returns its number.
async fn their_picture(engine: &Engine, size: (usize, usize)) -> u64 {
    let waited = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Some((n, frame)) = engine.call_video().theirs()
                && (frame.width, frame.height) == size
            {
                return n;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    waited.expect("their picture to show")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cameras_show_on_the_other_end_and_go_off() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b) = tokio::join!(
        start(dirs[0].path(), 320, 180),
        start(dirs[1].path(), 640, 480)
    );
    pair(&a, &b).await;

    a.execute(Command::StartCall {
        endpoint_id: my_id(&b),
    })
    .await
    .unwrap();
    wait_for(&b, "b to ring", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.state == CallState::Ringing)
    })
    .await;
    b.execute(Command::AnswerCall { accept: true })
        .await
        .unwrap();
    for (engine, who) in [(&a, "a"), (&b, "b")] {
        wait_for(
            engine,
            &format!("{who}'s call to be on with video possible"),
            |s| {
                s.call
                    .as_ref()
                    .is_some_and(|c| c.state == CallState::Active && c.video && !c.camera)
            },
        )
        .await;
    }
    assert!(
        a.call_video().theirs().is_none(),
        "no video until a camera goes on"
    );

    // a's camera on: b sees it, at a's size.
    a.execute(Command::CallCamera { on: true }).await.unwrap();
    wait_for(&a, "a's camera to be on", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.camera && !c.camera_opening)
    })
    .await;
    wait_for(&b, "b to see a's camera on", |s| {
        s.call.as_ref().is_some_and(|c| c.peer_camera)
    })
    .await;
    let first = their_picture(&b, (320, 180)).await;
    assert!(a.call_video().mine().is_some(), "a sees its own picture");

    // b's camera on too: 640×480 arrives scaled to fit 640×360.
    b.execute(Command::CallCamera { on: true }).await.unwrap();
    their_picture(&a, (480, 360)).await;

    // Pictures keep coming.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let later = b.call_video().theirs().map_or(0, |(n, _)| n);
    assert!(later > first + 10, "{first} → {later}");

    // a's camera off: b's view of it clears.
    a.execute(Command::CallCamera { on: false }).await.unwrap();
    wait_for(&b, "b to see a's camera off", |s| {
        s.call.as_ref().is_some_and(|c| !c.peer_camera)
    })
    .await;
    assert!(b.call_video().theirs().is_none());

    a.execute(Command::HangUp).await.unwrap();
    wait_for(&b, "b's call to end", |s| {
        s.call.as_ref().is_none_or(|c| !c.is_live())
    })
    .await;
    assert!(b.call_video().theirs().is_none() && b.call_video().mine().is_none());
    for engine in [a, b] {
        engine.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_screen_replaces_the_camera_and_stops_with_the_call() {
    use deskbeam_engine::model::ShareTarget;
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b) = tokio::join!(
        start(dirs[0].path(), 320, 180),
        start(dirs[1].path(), 320, 180)
    );
    pair(&a, &b).await;
    a.execute(Command::StartCall {
        endpoint_id: my_id(&b),
    })
    .await
    .unwrap();
    wait_for(&b, "b to ring", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.state == CallState::Ringing)
    })
    .await;
    b.execute(Command::AnswerCall { accept: true })
        .await
        .unwrap();
    wait_for(&a, "a's call to be on", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.state == CallState::Active && c.video)
    })
    .await;

    // The camera first, then a share in its place.
    a.execute(Command::CallCamera { on: true }).await.unwrap();
    their_picture(&b, (320, 180)).await;
    a.execute(Command::ListShareSources).await.unwrap();
    a.execute(Command::CallShare {
        target: ShareTarget::Display { id: 1 },
    })
    .await
    .unwrap();
    wait_for(&a, "a to be sharing", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.sharing.is_some() && !c.share_opening)
    })
    .await;
    wait_for(&b, "b to see a sharing", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.peer_sharing && c.peer_camera)
    })
    .await;
    their_picture(&b, (1280, 720)).await;

    // Stopping brings the camera back.
    a.execute(Command::CallStopSharing).await.unwrap();
    wait_for(&b, "b to see the share end", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| !c.peer_sharing && c.peer_camera)
    })
    .await;
    their_picture(&b, (320, 180)).await;

    // Sharing again, then the call ends: nothing is shared any more.
    a.execute(Command::CallShare {
        target: ShareTarget::Window { id: 9 },
    })
    .await
    .unwrap();
    their_picture(&b, (1280, 720)).await;
    b.execute(Command::HangUp).await.unwrap();
    wait_for(&a, "a's call to end", |s| {
        s.call.as_ref().is_none_or(|c| !c.is_live())
    })
    .await;
    assert!(
        a.call_video().mine().is_none(),
        "a no longer sends anything"
    );
    for engine in [a, b] {
        engine.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hd_sends_the_camera_at_720p() {
    use deskbeam_engine::model::{AudioSettings, CameraQuality};
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b) = tokio::join!(
        start(dirs[0].path(), 1280, 720),
        start(dirs[1].path(), 320, 180)
    );
    pair(&a, &b).await;
    a.execute(Command::StartCall {
        endpoint_id: my_id(&b),
    })
    .await
    .unwrap();
    wait_for(&b, "b to ring", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.state == CallState::Ringing)
    })
    .await;
    b.execute(Command::AnswerCall { accept: true })
        .await
        .unwrap();
    wait_for(&a, "a's call to be on", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.state == CallState::Active && c.video)
    })
    .await;

    // Standard: a 720p camera is sent at 640×360.
    a.execute(Command::CallCamera { on: true }).await.unwrap();
    their_picture(&b, (640, 360)).await;

    // HD, from the next time the camera goes on: 1280×720.
    a.execute(Command::SetAudio {
        settings: AudioSettings {
            camera_quality: CameraQuality::Hd,
            ..Default::default()
        },
    })
    .await
    .unwrap();
    a.execute(Command::CallCamera { on: false }).await.unwrap();
    wait_for(&b, "b to see a's camera off", |s| {
        s.call.as_ref().is_some_and(|c| !c.peer_camera)
    })
    .await;
    a.execute(Command::CallCamera { on: true }).await.unwrap();
    their_picture(&b, (1280, 720)).await;

    a.execute(Command::HangUp).await.unwrap();
    for engine in [a, b] {
        engine.shutdown().await;
    }
}

async fn wait_for(engine: &Engine, what: &str, check: impl Fn(&Snapshot) -> bool) {
    let mut rx = engine.watch();
    let waited = tokio::time::timeout(DEADLINE, async {
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
        .unwrap()
        .endpoint_id
        .clone()
}

async fn pair(host: &Engine, joiner: &Engine) {
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
            joiner.execute(Command::JoinPairing { code: code.clone() }),
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
    let (host_id, joiner_id) = (my_id(host), my_id(joiner));
    wait_for(host, "the host to see the joiner online", |s| {
        s.device(&joiner_id).is_some_and(|d| d.online)
    })
    .await;
    wait_for(joiner, "the joiner to see the host online", |s| {
        s.device(&host_id).is_some_and(|d| d.online)
    })
    .await;
}
