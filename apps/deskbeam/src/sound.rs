//! The app's sounds: a soft ping for new messages and a ring for incoming calls.
//!
//! The built-in ones are DeskBeam's own (`assets/sounds`, synthesized for it). The user may
//! choose a file for each instead; the engine copies it into the data directory
//! (`sounds.set`), and it plays from there. One thread owns the audio output, opened the
//! first time a sound plays.

use std::{io::Cursor, sync::mpsc, time::Duration};

use deskbeam_engine::{Engine, model::SoundKind};
use rodio::Source;

const MESSAGE: &[u8] = include_bytes!("../assets/sounds/message.wav");
const RING: &[u8] = include_bytes!("../assets/sounds/ring.wav");
/// A ring starts this often, and is queued this many times.
const RING_EVERY: Duration = Duration::from_secs(3);
const RING_REPEATS: usize = 30;

enum Request {
    Play(SoundKind),
    StartRinging,
    StopRinging,
}

/// Plays sounds without waiting. Cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct Sounds {
    tx: mpsc::Sender<Request>,
}

impl Sounds {
    pub fn new(engine: Engine) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("sounds".into())
            .spawn(move || run(engine, rx))
            .expect("a thread for sounds");
        Self { tx }
    }

    /// Plays `kind` once.
    pub fn play(&self, kind: SoundKind) {
        let _ = self.tx.send(Request::Play(kind));
    }

    /// Rings until [`Self::stop_ringing`], for an incoming call.
    pub fn start_ringing(&self) {
        let _ = self.tx.send(Request::StartRinging);
    }

    pub fn stop_ringing(&self) {
        let _ = self.tx.send(Request::StopRinging);
    }
}

fn run(engine: Engine, rx: mpsc::Receiver<Request>) {
    let mut output: Option<rodio::MixerDeviceSink> = None;
    let mut ringing: Option<rodio::Player> = None;
    while let Ok(request) = rx.recv() {
        if output.is_none() {
            match rodio::DeviceSinkBuilder::open_default_sink() {
                Ok(mut sink) => {
                    sink.log_on_drop(false);
                    output = Some(sink);
                }
                Err(e) => {
                    tracing::debug!("no sound output: {e}");
                    continue;
                }
            }
        }
        let Some(sink) = &output else { continue };
        match request {
            Request::Play(kind) => {
                if let Some(source) = decoded(&engine, kind) {
                    sink.mixer().add(source);
                }
            }
            Request::StartRinging => {
                if ringing.is_some() {
                    continue;
                }
                if let Some(source) = decoded(&engine, SoundKind::Ring) {
                    let player = rodio::Player::connect_new(sink.mixer());
                    // A short ring is spaced out to every few seconds; a long chosen one
                    // simply repeats. A call rings for 45 s at most, well within this.
                    let length = source.total_duration().unwrap_or(RING_EVERY);
                    let pause = RING_EVERY.saturating_sub(length);
                    let (channels, rate) = (source.channels(), source.sample_rate());
                    let once = source.buffered();
                    for _ in 0..RING_REPEATS {
                        player.append(once.clone());
                        if !pause.is_zero() {
                            player.append(
                                rodio::source::Zero::new(channels, rate).take_duration(pause),
                            );
                        }
                    }
                    ringing = Some(player);
                }
            }
            Request::StopRinging => {
                if let Some(player) = ringing.take() {
                    player.stop();
                }
            }
        }
    }
}

/// The chosen sound for `kind`, or the built-in one if none was chosen or it doesn't decode.
fn decoded(engine: &Engine, kind: SoundKind) -> Option<rodio::Decoder<Cursor<Vec<u8>>>> {
    if let Some(path) = engine.sound_file(kind)
        && let Ok(bytes) = std::fs::read(&path)
    {
        match rodio::Decoder::new(Cursor::new(bytes)) {
            Ok(source) => return Some(source),
            Err(e) => tracing::debug!("{} does not play: {e}", path.display()),
        }
    }
    let built_in = match kind {
        SoundKind::Message => MESSAGE,
        SoundKind::Ring => RING,
    };
    rodio::Decoder::new(Cursor::new(built_in.to_vec())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_sounds_decode() {
        for bytes in [MESSAGE, RING] {
            let source = rodio::Decoder::new(Cursor::new(bytes.to_vec())).unwrap();
            let length = source.total_duration().unwrap();
            assert!(length > Duration::from_millis(500) && length < Duration::from_secs(4));
        }
    }
}
