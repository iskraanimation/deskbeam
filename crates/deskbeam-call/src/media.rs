//! The media loop of a call: every 10 ms it takes 10 ms from the microphone, cleans it up
//! (echo, noise, level: WebRTC's processing through `sonora`), and every 20 ms sends an Opus
//! packet and plays the next one from the [`JitterBuffer`].
//!
//! The loop keeps its own clock rather than the devices', so it runs with no microphone or
//! no speaker too; when the speaker's clock drifts, the queue to it is trimmed.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{
        AdaptiveDigital, EchoCanceller, FixedDigital, GainController2, HighPassFilter,
        NoiseSuppression, NoiseSuppressionLevel,
    },
};

use crate::{
    FRAME, RATE, STEP,
    codec::{Decoder, Encoder},
    devices::{Devices, Port},
    jitter::{JitterBuffer, Playout},
    packet::AudioPacket,
};

/// The speaker is kept this far ahead (and trimmed past `MOST_AHEAD`).
const AHEAD: usize = FRAME * 2;
const MOST_AHEAD: usize = FRAME * 6;
/// What the echo canceller assumes between playing a sample and hearing it back, to start
/// with; it finds the real delay itself.
const ECHO_DELAY_MS: i32 = 60;
/// How much of the meter's level is kept from one packet (20 ms) to the next.
const LEVEL_FALL: f32 = 0.85;

/// How the microphone's audio is cleaned up before it is sent: WebRTC's processing, through
/// `sonora`. The defaults are what calls were tuned to by hand (plan 6a).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Processing {
    /// Takes what the speaker plays out of the microphone. Off only with headphones.
    pub echo_cancellation: bool,
    /// `None` is off.
    pub noise_suppression: Option<NoiseLevel>,
    /// Raises a quiet voice by itself, up to `max_gain_db`.
    pub auto_gain: bool,
    pub max_gain_db: f32,
    /// A fixed boost before the limiter, for a quiet microphone.
    pub boost_db: f32,
}

/// How hard noise is suppressed: about 6, 12, 18 and 21 dB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseLevel {
    Low,
    Moderate,
    High,
    VeryHigh,
}

impl Default for Processing {
    fn default() -> Self {
        Self {
            echo_cancellation: true,
            noise_suppression: Some(NoiseLevel::Moderate),
            auto_gain: true,
            max_gain_db: 50.0,
            boost_db: 0.0,
        }
    }
}

impl Processing {
    /// The most `max_gain_db` and `boost_db` may be.
    pub const MOST_GAIN_DB: f32 = 50.0;
    pub const MOST_BOOST_DB: f32 = 20.0;

    /// `sonora`'s configuration for these settings. The high-pass filter (taking out hum and
    /// rumble) and the limiter are always on.
    pub fn config(&self) -> Config {
        let max_gain_db = self.max_gain_db.clamp(0.0, Self::MOST_GAIN_DB);
        Config {
            high_pass_filter: Some(HighPassFilter::default()),
            echo_canceller: self.echo_cancellation.then(EchoCanceller::default),
            noise_suppression: self.noise_suppression.map(|level| NoiseSuppression {
                level: match level {
                    NoiseLevel::Low => NoiseSuppressionLevel::Low,
                    NoiseLevel::Moderate => NoiseSuppressionLevel::Moderate,
                    NoiseLevel::High => NoiseSuppressionLevel::High,
                    NoiseLevel::VeryHigh => NoiseSuppressionLevel::VeryHigh,
                },
                ..Default::default()
            }),
            // The adaptive part is off in `sonora`'s default, which leaves only the limiter: a
            // laptop's microphone then reaches the other end some 50 dB below full scale.
            gain_controller2: Some(GainController2 {
                adaptive_digital: self.auto_gain.then(|| {
                    let defaults = AdaptiveDigital::default();
                    AdaptiveDigital {
                        max_gain_db,
                        initial_gain_db: defaults.initial_gain_db.min(max_gain_db),
                        ..defaults
                    }
                }),
                fixed_digital: FixedDigital {
                    gain_db: self.boost_db.clamp(0.0, Self::MOST_BOOST_DB),
                },
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

/// How a call's audio is processed.
#[derive(Debug, Clone)]
pub struct Options {
    /// `None` sends the microphone as it is: only tests that check the audio path itself.
    pub processing: Option<Processing>,
    /// A diagnostic: record the call's audio into this folder as WAV files (the microphone,
    /// the processed microphone, what was decoded, what the speaker got) with a summary,
    /// written when the call ends. At most [`MOST_RECORDED`] of each.
    pub record: Option<std::path::PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            processing: Some(Processing::default()),
            record: None,
        }
    }
}

/// Longest recording kept per track.
pub const MOST_RECORDED: Duration = Duration::from_secs(300);

/// How the call is going, for showing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub sent: u64,
    pub received: u64,
    pub played: u64,
    pub lost: u64,
    pub late: u64,
    /// The microphone's level after processing, 0 to 1, for a meter: the loudest recent
    /// 10 ms, falling away over a few tenths of a second.
    pub level: f32,
    /// Breaks between the devices and the loop, in samples: microphone short, microphone
    /// dropped, speaker short, speaker trimmed.
    pub glitches: [u64; 4],
}

impl Stats {
    /// Lost or late as a share of what should have played.
    pub fn loss(&self) -> f32 {
        let missing = (self.lost + self.late) as f32;
        let total = self.played as f32 + missing;
        if total == 0.0 { 0.0 } else { missing / total }
    }
}

/// A running call's audio. Dropping it stops the loop and closes the devices.
pub struct Media {
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    inbox: Arc<Mutex<JitterBuffer>>,
    stats: Arc<Mutex<Stats>>,
    thread: Option<JoinHandle<()>>,
    port: Port,
    /// New processing settings for the loop to take up.
    changed: Arc<Mutex<Option<Processing>>>,
    _devices: Box<dyn std::any::Any + Send + Sync>,
}

impl Media {
    /// Opens the devices and starts the loop. `send` gets each packet to send as a datagram.
    pub fn start(
        devices: &dyn Devices,
        options: Options,
        send: impl FnMut(Vec<u8>) + Send + 'static,
    ) -> anyhow::Result<Self> {
        let port = Port::default();
        let recorder = options.record.clone().map(|folder| {
            *port.speaker_tap.lock().expect("lock") = Some(Vec::new());
            Recorder {
                folder,
                ..Default::default()
            }
        });
        let guard = devices.open(port.clone())?;
        let stop = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let inbox = Arc::new(Mutex::new(JitterBuffer::default()));
        let stats = Arc::new(Mutex::new(Stats::default()));
        let changed = Arc::new(Mutex::new(None));
        let looping = Loop {
            changed: changed.clone(),
            port: port.clone(),
            recorder,
            stop: stop.clone(),
            muted: muted.clone(),
            inbox: inbox.clone(),
            stats: stats.clone(),
            encoder: Encoder::new()?,
            decoder: Decoder::new()?,
            processing: options.processing.map(processing),
            send: Box::new(send),
        };
        let thread = std::thread::Builder::new()
            .name("call-media".into())
            .spawn(move || looping.run())?;
        Ok(Self {
            stop,
            muted,
            inbox,
            stats,
            thread: Some(thread),
            port,
            changed,
            _devices: guard,
        })
    }

    /// The whole path played back to this device's own speaker, with no network: the
    /// microphone, processing and Opus, then the jitter buffer and the speaker. For trying
    /// the settings; best with headphones.
    pub fn loopback(devices: &dyn Devices, options: Options) -> anyhow::Result<Arc<Self>> {
        let me: Arc<std::sync::OnceLock<std::sync::Weak<Media>>> = Arc::default();
        let back = me.clone();
        let media = Arc::new(Self::start(devices, options, move |packet| {
            if let Some(media) = back.get().and_then(std::sync::Weak::upgrade) {
                media.receive(&packet);
            }
        })?);
        let _ = me.set(Arc::downgrade(&media));
        Ok(media)
    }

    /// New processing settings, taken up within 10 ms. Ignored when started without
    /// processing.
    pub fn set_processing(&self, processing: Processing) {
        *self.changed.lock().expect("lock") = Some(processing);
    }

    /// A datagram from the other end. Anything that is not an audio packet is ignored.
    pub fn receive(&self, datagram: &[u8]) {
        if let Some(packet) = AudioPacket::decode(datagram) {
            self.inbox
                .lock()
                .expect("lock")
                .push(packet.sequence, packet.payload);
            self.stats.lock().expect("lock").received += 1;
        }
    }

    /// Muted, the other end hears silence.
    pub fn set_muted(&self, on: bool) {
        self.muted.store(on, Ordering::SeqCst);
    }

    pub fn stats(&self) -> Stats {
        let mut stats = *self.stats.lock().expect("lock");
        let counts = self.inbox.lock().expect("lock").counts();
        stats.played = counts.played;
        stats.lost = counts.lost;
        stats.late = counts.late;
        stats.glitches = self.port.glitches.read();
        stats
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take()
            // A loopback's last handle can go in the loop itself; it ends on its own then.
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

fn processing(settings: Processing) -> AudioProcessing {
    let mut apm = AudioProcessing::builder()
        .config(settings.config())
        .capture_config(StreamConfig::new(RATE, 1))
        .render_config(StreamConfig::new(RATE, 1))
        .build();
    let _ = apm.set_stream_delay_ms(ECHO_DELAY_MS);
    apm
}

/// The diagnostic recording: each track at 48 kHz mono.
#[derive(Default)]
struct Recorder {
    folder: std::path::PathBuf,
    microphone: Vec<f32>,
    processed: Vec<f32>,
    decoded: Vec<f32>,
    /// One line per 20 ms played: what the jitter buffer gave.
    log: Vec<String>,
}

impl Recorder {
    fn keep(track: &mut Vec<f32>, samples: &[f32]) {
        if track.len() < RATE as usize * MOST_RECORDED.as_secs() as usize {
            track.extend_from_slice(samples);
        }
    }

    fn save(self, port: &Port, stats: Stats) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let folder = self.folder.join(format!("call-{stamp}"));
        let speaker = port
            .speaker_tap
            .lock()
            .expect("lock")
            .take()
            .unwrap_or_default();
        let result = std::fs::create_dir_all(&folder).and_then(|()| {
            write_wav(&folder.join("1-microphone.wav"), &self.microphone)?;
            write_wav(&folder.join("2-processed.wav"), &self.processed)?;
            write_wav(&folder.join("3-decoded.wav"), &self.decoded)?;
            write_wav(&folder.join("4-speaker.wav"), &speaker)?;
            let [
                capture_short,
                capture_dropped,
                playout_short,
                playout_trimmed,
            ] = stats.glitches;
            let summary = format!(
                "sent {}\nreceived {}\nplayed {}\nlost {}\nlate {}\n\
                 capture_short {capture_short}\ncapture_dropped {capture_dropped}\n\
                 playout_short {playout_short}\nplayout_trimmed {playout_trimmed}\n\n{}\n",
                stats.sent,
                stats.received,
                stats.played,
                stats.lost,
                stats.late,
                self.log.join("\n"),
            );
            std::fs::write(folder.join("summary.txt"), summary)
        });
        match result {
            Ok(()) => tracing::info!("call recorded in {}", folder.display()),
            Err(e) => tracing::warn!("recording the call: {e}"),
        }
    }
}

/// 32-bit float WAV, 48 kHz mono.
fn write_wav(path: &std::path::Path, samples: &[f32]) -> std::io::Result<()> {
    let data = (samples.len() * 4) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out)
}

struct Loop {
    port: Port,
    recorder: Option<Recorder>,
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    inbox: Arc<Mutex<JitterBuffer>>,
    stats: Arc<Mutex<Stats>>,
    encoder: Encoder,
    decoder: Decoder,
    processing: Option<AudioProcessing>,
    changed: Arc<Mutex<Option<Processing>>>,
    send: Box<dyn FnMut(Vec<u8>) + Send>,
}

impl Loop {
    fn run(mut self) {
        let step = Duration::from_micros(1_000_000 * STEP as u64 / RATE as u64);
        let mut next = Instant::now();
        let mut pending: Vec<f32> = Vec::with_capacity(FRAME);
        let (mut sequence, mut timestamp) = (0u32, 0u32);
        let mut tick = 0u64;
        while !self.stop.load(Ordering::SeqCst) {
            if let Some(settings) = self.changed.lock().expect("lock").take()
                && let Some(apm) = &mut self.processing
            {
                apm.apply_config(settings.config());
            }
            // 10 ms from the microphone.
            let input = self.port.take_capture(STEP);
            let mut cleaned = vec![0.0f32; STEP];
            match &mut self.processing {
                Some(apm) => {
                    if apm
                        .process_capture_f32(&[&input], &mut [&mut cleaned])
                        .is_err()
                    {
                        cleaned.copy_from_slice(&input);
                    }
                }
                None => cleaned.copy_from_slice(&input),
            }
            if self.muted.load(Ordering::SeqCst) {
                cleaned.fill(0.0);
            }
            if let Some(r) = &mut self.recorder {
                Recorder::keep(&mut r.microphone, &input);
                Recorder::keep(&mut r.processed, &cleaned);
            }
            let level = (cleaned.iter().map(|s| s * s).sum::<f32>() / STEP as f32).sqrt();
            pending.extend_from_slice(&cleaned);
            if pending.len() >= FRAME {
                match self.encoder.encode(&pending[..FRAME]) {
                    Ok(payload) => {
                        let packet = AudioPacket {
                            sequence,
                            timestamp,
                            payload,
                        };
                        (self.send)(packet.encode());
                        sequence = sequence.wrapping_add(1);
                        timestamp = timestamp.wrapping_add(FRAME as u32);
                        let mut stats = self.stats.lock().expect("lock");
                        stats.sent += 1;
                        stats.level = level.min(1.0).max(stats.level * LEVEL_FALL);
                    }
                    Err(e) => tracing::debug!("encoding: {e:#}"),
                }
                pending.drain(..FRAME);
            }

            // Every 10 ms, keep the speaker fed. It takes audio in its own chunks (512
            // samples on a Mac); checked only every 20 ms, it ran dry just before a refill.
            {
                if self.port.playout_len() < AHEAD {
                    let playout = self.inbox.lock().expect("lock").pop();
                    if let Some(r) = &mut self.recorder {
                        let what = match &playout {
                            Playout::Wait => "wait".to_string(),
                            Playout::Packet(_) => "packet".to_string(),
                            Playout::Lost { next } => {
                                format!("lost (next here: {})", next.is_some())
                            }
                        };
                        r.log.push(format!(
                            "{:>8} ms {what} speaker queue {}",
                            tick * 10,
                            self.port.playout_len()
                        ));
                    }
                    let frame = match playout {
                        Playout::Wait => vec![0.0; FRAME],
                        Playout::Packet(payload) => self.decoder.decode(&payload),
                        Playout::Lost { next } => self.decoder.lost(next.as_deref()),
                    };
                    if let Some(r) = &mut self.recorder {
                        Recorder::keep(&mut r.decoded, &frame);
                    }
                    if let Some(apm) = &mut self.processing {
                        // What is about to play, for the echo canceller to take out again.
                        let mut scratch = vec![0.0f32; STEP];
                        for half in frame.chunks(STEP) {
                            let _ = apm.process_render_f32(&[half], &mut [&mut scratch]);
                        }
                    }
                    self.port.push_playout(&frame);
                }
                if self.port.playout_len() > MOST_AHEAD {
                    self.port.trim_playout(STEP);
                }
            }

            tick += 1;
            next += step;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else if now - next > Duration::from_millis(200) {
                // Far behind (the machine slept, say): start the clock again.
                next = now;
            }
        }
        if let Some(recorder) = self.recorder.take() {
            let mut stats = *self.stats.lock().expect("lock");
            let counts = self.inbox.lock().expect("lock").counts();
            stats.played = counts.played;
            stats.lost = counts.lost;
            stats.late = counts.late;
            stats.glitches = self.port.glitches.read();
            recorder.save(&self.port, stats);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::devices::ToneDevices;

    /// How strong `hz` is in `samples` (Goertzel), against their overall level.
    fn share_at(samples: &[f32], hz: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * hz / RATE as f32;
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

    /// Two ends joined by channels in place of datagrams; `drop_every` loses packets on the
    /// way from a to b.
    fn call(processing: bool, drop_every: Option<u64>, seconds: f32) -> (Vec<f32>, Stats) {
        let a_devices = ToneDevices {
            tone: Some(440.0),
            ..Default::default()
        };
        let b_devices = ToneDevices::default();
        let (to_b, at_b) = mpsc::channel::<Vec<u8>>();
        let mut n = 0u64;
        let options = Options {
            processing: processing.then(Processing::default),
            ..Default::default()
        };
        let a = Media::start(&a_devices, options.clone(), move |packet| {
            n += 1;
            if drop_every.is_none_or(|k| !n.is_multiple_of(k)) {
                let _ = to_b.send(packet);
            }
        })
        .unwrap();
        let b = Media::start(&b_devices, options, |_| {}).unwrap();
        let end = Instant::now() + Duration::from_secs_f32(seconds);
        while Instant::now() < end {
            while let Ok(packet) = at_b.try_recv() {
                b.receive(&packet);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let stats = b.stats();
        drop(a);
        drop(b);
        let heard = b_devices.heard.lock().unwrap().clone();
        (heard, stats)
    }

    #[test]
    fn a_tone_crosses_a_call() {
        let (heard, stats) = call(false, None, 2.0);
        assert!(stats.received > 80, "{stats:?}");
        // The last second, well after the buffer filled.
        let tail = &heard[heard.len() - RATE as usize..];
        let share = share_at(tail, 440.0);
        assert!(share > 0.5, "440 Hz is {share} of what was heard");
    }

    #[test]
    fn losing_a_packet_in_ten_still_plays_on() {
        let (heard, stats) = call(false, Some(10), 2.0);
        assert!(stats.lost >= 5, "{stats:?}");
        let tail = &heard[heard.len() - RATE as usize..];
        // No silent stretches longer than 20 ms in the last second.
        let longest_quiet = tail
            .chunks(STEP)
            .map(|c| c.iter().map(|s| s.abs()).fold(0.0, f32::max) < 0.01)
            .fold((0, 0), |(run, most), quiet| {
                let run = if quiet { run + 1 } else { 0 };
                (run, most.max(run))
            })
            .1;
        assert!(
            longest_quiet <= 2,
            "{longest_quiet} silent 10 ms steps in a row"
        );
        assert!(share_at(tail, 440.0) > 0.3);
    }

    #[test]
    fn settings_reach_the_processing_config() {
        let config = Processing::default().config();
        assert!(config.echo_canceller.is_some());
        assert_eq!(
            config.noise_suppression.map(|n| n.level),
            Some(NoiseSuppressionLevel::Moderate)
        );
        let gain = config.gain_controller2.unwrap();
        assert_eq!(gain.adaptive_digital.unwrap().max_gain_db, 50.0);
        assert_eq!(gain.fixed_digital.gain_db, 0.0);

        let config = Processing {
            echo_cancellation: false,
            noise_suppression: None,
            auto_gain: false,
            max_gain_db: 10.0,
            boost_db: 99.0,
        }
        .config();
        assert!(config.echo_canceller.is_none());
        assert!(config.noise_suppression.is_none());
        assert!(config.high_pass_filter.is_some(), "always on");
        let gain = config.gain_controller2.unwrap();
        assert!(gain.adaptive_digital.is_none());
        assert_eq!(gain.fixed_digital.gain_db, Processing::MOST_BOOST_DB);

        let low = Processing {
            max_gain_db: 6.0,
            ..Default::default()
        }
        .config();
        let adaptive = low.gain_controller2.unwrap().adaptive_digital.unwrap();
        assert_eq!(adaptive.initial_gain_db, 6.0, "never starts above the most");
    }

    /// A fixed boost set while running raises the level the loop measures.
    #[test]
    fn a_boost_set_while_running_takes_effect() {
        let quiet = ToneDevices {
            tone: Some(440.0),
            ..Default::default()
        };
        let plain = Processing {
            echo_cancellation: false,
            noise_suppression: None,
            auto_gain: false,
            max_gain_db: 0.0,
            boost_db: 0.0,
        };
        let media = Media::start(
            &quiet,
            Options {
                processing: Some(plain),
                ..Default::default()
            },
            |_| {},
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let before = media.stats().level;
        media.set_processing(Processing {
            boost_db: 12.0,
            ..plain
        });
        std::thread::sleep(Duration::from_millis(600));
        let after = media.stats().level;
        assert!(after > before * 1.5, "{before} → {after}");
    }

    #[test]
    fn processing_keeps_speech_like_sound_and_mute_silences_it() {
        let (heard, _) = call(true, None, 3.0);
        let tail = &heard[heard.len() - RATE as usize..];
        let level = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(level > 0.02, "processed audio is too quiet: {level}");

        let a = Media::start(
            &ToneDevices {
                tone: Some(440.0),
                ..Default::default()
            },
            Options::default(),
            |_| {},
        )
        .unwrap();
        a.set_muted(true);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(a.stats().level, 0.0);
        assert!(a.stats().sent > 5);
    }
}
