//! The camera through the whole video path, with no network: camera → scale → H.264 →
//! datagrams → reassembly → decoder. Prints the figures each second.
//!
//! `cargo run --release -p deskbeam-call --example camera [seconds] [hd] [camera name]`

use std::{sync::mpsc, time::Duration};

use deskbeam_call::video::{
    Profile,
    camera::{Webcam, camera_names},
    stream::{VideoReceiver, VideoSender},
};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let mut name = args.next();
    let hd = name.as_deref() == Some("hd");
    if hd {
        name = args.next();
    }
    let (size, profile) = if hd {
        (Webcam::HD, Profile::CAMERA_HD)
    } else {
        (Webcam::STANDARD, Profile::CAMERA)
    };
    println!("cameras: {:?}", camera_names());
    let receiver = VideoReceiver::start()?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let sender = VideoSender::start(
        &Webcam { name, size },
        profile,
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
