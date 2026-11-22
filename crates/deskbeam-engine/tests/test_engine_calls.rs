//! Voice calls through engines over n0's public relays, with test devices in place of the
//! microphone and speaker: ring, answer, hear each other, mute, hang up; decline; a call
//! given up while ringing is missed; a busy device says so; an unpaired one is refused.

use std::{path::Path, sync::Arc, time::Duration};

use deskbeam_engine::{
    CallAudio, Command, Engine, EngineConfig, Event,
    calls::devices::ToneDevices,
    model::{CallState, Snapshot},
};
use deskbeam_store::SqliteStore;

const START_TIMEOUT: Duration = Duration::from_secs(60);
const JOIN_DEADLINE: Duration = Duration::from_secs(90);
const DEADLINE: Duration = Duration::from_secs(60);
const RATE: f32 = 48_000.0;

/// An engine whose "microphone" plays `tone` Hz; what its "speaker" gets is in the devices.
async fn start(dir: &Path, tone: f32) -> (Engine, ToneDevices) {
    let devices = ToneDevices {
        tone: Some(tone),
        ..Default::default()
    };
    let mut config = EngineConfig::new(dir);
    config.audio = CallAudio(Some(Arc::new(devices.clone())));
    config.ring_timeout = Duration::from_secs(20);
    let store = Arc::new(SqliteStore::open_in(dir).unwrap());
    let engine = tokio::time::timeout(START_TIMEOUT, Engine::start_and_wait(config, store))
        .await
        .expect("engine start timed out")
        .expect("engine start failed");
    (engine, devices)
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

fn state(engine: &Engine) -> Option<CallState> {
    engine.snapshot().call.as_ref().map(|c| c.state.clone())
}

/// How much of `samples`' energy is at `hz` (Goertzel), 0 to 1.
fn share_at(samples: &[f32], hz: f32) -> f32 {
    let w = 2.0 * std::f32::consts::PI * hz / RATE;
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in samples {
        let s0 = x + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
    let total: f32 = samples.iter().map(|s| s * s).sum::<f32>() * samples.len() as f32 / 2.0;
    if total == 0.0 { 0.0 } else { power / total }
}

/// The last second each side heard.
fn last_second(devices: &ToneDevices) -> Vec<f32> {
    let heard = devices.heard.lock().unwrap();
    heard[heard.len().saturating_sub(RATE as usize)..].to_vec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_rings_connects_and_ends() {
    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let ((a, a_heard), (b, b_heard), (c, _)) = tokio::join!(
        start(dirs[0].path(), 440.0),
        start(dirs[1].path(), 660.0),
        start(dirs[2].path(), 880.0)
    );
    pair(&a, &b).await;
    pair(&c, &b).await;
    let b_id = my_id(&b);
    let mut b_events = b.events();

    // a calls; b rings.
    a.execute(Command::StartCall {
        endpoint_id: b_id.clone(),
    })
    .await
    .unwrap();
    assert_eq!(state(&a), Some(CallState::Calling));
    wait_for(&b, "b to ring", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.state == CallState::Ringing && !c.outgoing)
    })
    .await;
    let incoming = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(Event::CallIncoming { display_name, .. }) = b_events.recv().await {
                return display_name;
            }
        }
    })
    .await
    .expect("an incoming-call event");
    assert!(!incoming.is_empty());
    // Another call to a device that is in one is refused at once.
    assert!(
        a.execute(Command::StartCall {
            endpoint_id: b_id.clone()
        })
        .await
        .is_err()
    );

    b.execute(Command::AnswerCall { accept: true })
        .await
        .unwrap();
    for (engine, who) in [(&a, "a"), (&b, "b")] {
        wait_for(engine, &format!("{who}'s call to be on"), |s| {
            s.call
                .as_ref()
                .is_some_and(|c| c.state == CallState::Active && c.started_at.is_some())
        })
        .await;
    }

    // c calls b while b talks: busy.
    c.execute(Command::StartCall {
        endpoint_id: b_id.clone(),
    })
    .await
    .unwrap();
    wait_for(&c, "c to hear b is busy", |s| {
        matches!(&s.call, Some(call) if matches!(&call.state, CallState::Ended { reason } if reason.contains("another call")))
    })
    .await;

    // Each hears the other's tone.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let at_b = share_at(&last_second(&b_heard), 440.0);
    let at_a = share_at(&last_second(&a_heard), 660.0);
    assert!(at_b > 0.2, "b hears a's 440 Hz at {at_b}");
    assert!(at_a > 0.2, "a hears b's 660 Hz at {at_a}");

    // a mutes: b sees it, and a's sound stops reaching b.
    a.execute(Command::MuteCall { on: true }).await.unwrap();
    wait_for(&b, "b to see a muted", |s| {
        s.call.as_ref().is_some_and(|c| c.peer_muted)
    })
    .await;
    // Past the jitter buffer and the decoder's fade-out, what b hears is silence.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let heard = last_second(&b_heard);
    let quiet = &heard[heard.len() / 2..];
    let level = (quiet.iter().map(|s| s * s).sum::<f32>() / quiet.len() as f32).sqrt();
    assert!(level < 0.02, "b still hears a at {level}");

    // a hangs up: both end, then clear.
    a.execute(Command::HangUp).await.unwrap();
    wait_for(&b, "b's call to end", |s| {
        matches!(&s.call, Some(call) if matches!(&call.state, CallState::Ended { reason } if reason.contains("hung up")))
    })
    .await;
    wait_for(&a, "a's call to clear", |s| s.call.is_none()).await;

    for engine in [a, b, c] {
        engine.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declined_missed_and_unpaired_calls() {
    let dirs: Vec<_> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let ((a, _), (b, _)) = tokio::join!(start(dirs[0].path(), 440.0), start(dirs[1].path(), 660.0));
    pair(&a, &b).await;
    let b_id = my_id(&b);

    // Declined.
    a.execute(Command::StartCall {
        endpoint_id: b_id.clone(),
    })
    .await
    .unwrap();
    wait_for(&b, "b to ring", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.state == CallState::Ringing)
    })
    .await;
    b.execute(Command::AnswerCall { accept: false })
        .await
        .unwrap();
    wait_for(&a, "a to hear it was declined", |s| {
        matches!(&s.call, Some(call) if matches!(&call.state, CallState::Ended { reason } if reason.contains("declined")))
    })
    .await;
    wait_for(&a, "a's call to clear", |s| s.call.is_none()).await;
    wait_for(&b, "b's call to clear", |s| s.call.is_none()).await;

    // Given up while ringing: missed on b.
    let mut b_events = b.events();
    a.execute(Command::StartCall {
        endpoint_id: b_id.clone(),
    })
    .await
    .unwrap();
    wait_for(&b, "b to ring again", |s| {
        s.call
            .as_ref()
            .is_some_and(|c| c.state == CallState::Ringing)
    })
    .await;
    a.execute(Command::HangUp).await.unwrap();
    wait_for(&b, "b to show a missed call", |s| {
        matches!(&s.call, Some(call) if matches!(&call.state, CallState::Ended { reason } if reason.contains("Missed")))
    })
    .await;
    let missed = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(Event::CallMissed { .. }) = b_events.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(missed.is_ok(), "a missed-call event");

    // A device that is not paired is turned away before anything rings.
    let stranger = iroh::Endpoint::bind(iroh::endpoint::presets::N0)
        .await
        .unwrap();
    let b_endpoint: iroh::EndpointId = b_id.parse().unwrap();
    let refused = tokio::time::timeout(Duration::from_secs(60), async {
        let connection = stranger
            .connect(b_endpoint, deskbeam_engine::calls::ALPN)
            .await?;
        let (mut send, mut recv) = connection.open_bi().await?;
        deskbeam_engine::calls::write_frame(
            &mut send,
            &deskbeam_engine::calls::Frame::Offer {
                id: "x".into(),
                video: false,
            },
        )
        .await?;
        deskbeam_engine::calls::read_frame(&mut recv).await
    })
    .await
    .expect("the refusal came");
    assert!(refused.is_err(), "an unpaired device got {refused:?}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    wait_for(&b, "b not to ring", |s| {
        s.call
            .as_ref()
            .is_none_or(|c| c.state != CallState::Ringing)
    })
    .await;

    a.shutdown().await;
    b.shutdown().await;
}
