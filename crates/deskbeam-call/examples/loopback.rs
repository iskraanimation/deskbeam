//! Hear yourself through a call's whole audio path, with no network: the system's
//! microphone → processing → Opus → jitter buffer → the system's speaker. Prints the
//! figures each second. Use headphones, or the echo canceller fights the loop.
//!
//! `cargo run --release -p deskbeam-call --example loopback [seconds] [raw]`
//!
//! `DESKBEAM_CALL_RECORD=<folder>` records it, as in a call.

use std::time::Duration;

use deskbeam_call::{
    devices::{Speakers, device_names},
    media::{Media, Options, Processing},
};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let processing = args.next().as_deref() != Some("raw");
    let (inputs, outputs) = device_names();
    println!("microphones: {inputs:?}\nspeakers: {outputs:?}");

    let media = Media::loopback(
        &Speakers::default(),
        Options {
            processing: processing.then(Processing::default),
            record: std::env::var_os("DESKBEAM_CALL_RECORD").map(Into::into),
        },
    )?;
    for _ in 0..seconds {
        std::thread::sleep(Duration::from_secs(1));
        println!("{:?}", media.stats());
    }
    // Stops the loop, which writes the recording if one was asked for.
    drop(media);
    Ok(())
}
