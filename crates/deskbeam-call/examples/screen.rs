//! Screen sharing's path with no network: a display or window → I420 → H.264 → datagrams →
//! decoder. Lists what can be shared, then shares the first display (or the given number
//! from the list) and prints the figures.
//!
//! `cargo run --release -p deskbeam-call --example screen [seconds] [choice]`
//!
//! On macOS the terminal running it needs the Screen Recording permission.

use std::{sync::mpsc, time::Duration};

use deskbeam_call::video::{
    Profile,
    screen::{Screen, may_capture, share_sources},
    stream::{VideoReceiver, VideoSender},
};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let pick: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    println!("may capture the screen: {}", may_capture());
    let choices = share_sources(160);
    for (n, c) in choices.iter().enumerate() {
        println!("{n}: {:?} {} ({}×{})", c.source, c.name, c.width, c.height);
    }
    let Some(choice) = choices.get(pick) else {
        anyhow::bail!("nothing to share");
    };
    let receiver = VideoReceiver::start()?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let screen = Screen {
        source: choice.source.clone(),
        profile: Profile::SCREEN,
    };
    let sender = VideoSender::start(
        &screen,
        Profile::SCREEN,
        || 1100,
        move |d| {
            let _ = tx.send(d);
        },
    )?;
    for _ in 0..seconds * 10 {
        std::thread::sleep(Duration::from_millis(100));
        while let Ok(d) = rx.try_recv() {
            receiver.receive(&d);
        }
        if receiver.wants_keyframe() {
            sender.request_keyframe();
        }
    }
    let shown = receiver.latest().get();
    println!(
        "sent {:?}\nreceived {:?}\nshown {:?}",
        sender.stats(),
        receiver.counts(),
        shown.map(|(n, f)| (n, f.width, f.height))
    );
    Ok(())
}
