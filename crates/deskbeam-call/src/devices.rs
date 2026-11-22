//! The microphone and the speaker, as the media loop sees them: a [`Port`] of two queues of
//! 48 kHz mono samples. [`Speakers`] fills and empties them from the system's devices through
//! `cpal`, converting rate and channels; [`ToneDevices`] stands in for tests.

use std::{
    any::Any,
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::{RATE, STEP};

/// Most microphone audio kept waiting before the oldest is dropped (the loop takes it every
/// 10 ms; a backlog means a stall, and old audio is worse than none).
const MOST_CAPTURE: usize = STEP * 20;

/// The two queues between the devices and the media loop.
#[derive(Clone, Default)]
pub struct Port {
    capture: Arc<Mutex<VecDeque<f32>>>,
    playout: Arc<Mutex<VecDeque<f32>>>,
    /// Where the queues ran short or over, in samples: each is a break in the sound.
    pub glitches: Arc<Glitches>,
    /// When set, everything handed to the speaker is copied here (a diagnostic recording).
    pub speaker_tap: Arc<Mutex<Option<Vec<f32>>>>,
}

/// Samples the queues had to make up or throw away.
#[derive(Debug, Default)]
pub struct Glitches {
    /// The loop wanted microphone audio that had not come.
    pub capture_short: AtomicU64,
    /// Microphone audio dropped because the loop fell behind.
    pub capture_dropped: AtomicU64,
    /// The speaker wanted audio the loop had not made yet: silence played instead.
    pub playout_short: AtomicU64,
    /// Speaker audio dropped because the queue grew too long.
    pub playout_trimmed: AtomicU64,
}

impl Glitches {
    pub fn read(&self) -> [u64; 4] {
        [
            self.capture_short.load(Ordering::Relaxed),
            self.capture_dropped.load(Ordering::Relaxed),
            self.playout_short.load(Ordering::Relaxed),
            self.playout_trimmed.load(Ordering::Relaxed),
        ]
    }
}

impl Port {
    /// From the microphone.
    pub fn push_capture(&self, samples: impl IntoIterator<Item = f32>) {
        let mut queue = self.capture.lock().expect("lock");
        queue.extend(samples);
        let extra = queue.len().saturating_sub(MOST_CAPTURE);
        queue.drain(..extra);
        self.glitches
            .capture_dropped
            .fetch_add(extra as u64, Ordering::Relaxed);
    }

    /// Microphone samples waiting for the loop.
    pub fn capture_len(&self) -> usize {
        self.capture.lock().expect("lock").len()
    }

    /// `n` samples for the loop, silence where the microphone gave none.
    pub fn take_capture(&self, n: usize) -> Vec<f32> {
        let mut queue = self.capture.lock().expect("lock");
        let take = n.min(queue.len());
        let mut out: Vec<f32> = queue.drain(..take).collect();
        self.glitches
            .capture_short
            .fetch_add((n - take) as u64, Ordering::Relaxed);
        out.resize(n, 0.0);
        out
    }

    /// For the speaker.
    pub fn push_playout(&self, samples: &[f32]) {
        self.playout.lock().expect("lock").extend(samples);
    }

    /// `n` samples for the speaker, silence where the loop has none yet.
    pub fn pull_playout(&self, n: usize) -> Vec<f32> {
        let mut queue = self.playout.lock().expect("lock");
        let take = n.min(queue.len());
        let mut out: Vec<f32> = queue.drain(..take).collect();
        drop(queue);
        self.glitches
            .playout_short
            .fetch_add((n - take) as u64, Ordering::Relaxed);
        out.resize(n, 0.0);
        if let Some(tap) = self.speaker_tap.lock().expect("lock").as_mut() {
            tap.extend_from_slice(&out);
        }
        out
    }

    pub fn playout_len(&self) -> usize {
        self.playout.lock().expect("lock").len()
    }

    /// Drops `n` samples waiting for the speaker, when the devices' clocks drift apart.
    pub fn trim_playout(&self, n: usize) {
        let mut queue = self.playout.lock().expect("lock");
        let n = n.min(queue.len());
        queue.drain(..n);
        self.glitches
            .playout_trimmed
            .fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// Something that connects a [`Port`] to a microphone and a speaker while the returned
/// guard lives.
pub trait Devices: Send + Sync {
    fn open(&self, port: Port) -> anyhow::Result<Box<dyn Any + Send + Sync>>;
}

/// The system's microphone and speaker. Names choose devices; `None` is the default one.
#[derive(Debug, Clone, Default)]
pub struct Speakers {
    pub input: Option<String>,
    pub output: Option<String>,
}

/// The audio devices there are, by name, for a settings list.
pub fn device_names() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let names = |devices: Result<_, _>| -> Vec<String> {
        match devices {
            Ok(devices) => {
                let devices: Vec<cpal::Device> = devices;
                devices
                    .iter()
                    .filter_map(|d| d.description().ok().map(|d| d.name().to_string()))
                    .collect()
            }
            Err(_) => Vec::new(),
        }
    };
    (
        names(host.input_devices().map(|d| d.collect())),
        names(host.output_devices().map(|d| d.collect())),
    )
}

impl Devices for Speakers {
    fn open(&self, port: Port) -> anyhow::Result<Box<dyn Any + Send + Sync>> {
        // cpal streams may not move between threads, so one thread holds them until the
        // guard is dropped.
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let choice = self.clone();
        std::thread::Builder::new()
            .name("call-audio".into())
            .spawn(move || {
                // The streams play only while they live: keep them here until stopped.
                match open_streams(&choice, port) {
                    Ok(_streams) => {
                        let _ = ready_tx.send(Ok(()));
                        let _ = stop_rx.recv();
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                    }
                }
            })?;
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("the audio devices did not open in time"))?
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(Box::new(stop_tx))
    }
}

fn find(devices: impl Iterator<Item = cpal::Device>, name: &str) -> Option<cpal::Device> {
    devices
        .into_iter()
        .find(|d| d.description().is_ok_and(|desc| desc.name() == name))
}

/// Opens both streams. Either may be missing (no microphone, say): the call goes on
/// without it.
fn open_streams(
    choice: &Speakers,
    port: Port,
) -> anyhow::Result<(Option<cpal::Stream>, Option<cpal::Stream>)> {
    let host = cpal::default_host();
    let input = match &choice.input {
        Some(name) => host.input_devices().ok().and_then(|d| find(d, name)),
        None => None,
    }
    .or_else(|| host.default_input_device());
    let output = match &choice.output {
        Some(name) => host.output_devices().ok().and_then(|d| find(d, name)),
        None => None,
    }
    .or_else(|| host.default_output_device());
    anyhow::ensure!(
        input.is_some() || output.is_some(),
        "no microphone and no speaker"
    );

    let input = match input {
        Some(device) => match input_stream(&device, port.clone()) {
            Ok(stream) => Some(stream),
            Err(e) => {
                tracing::warn!("no microphone: {e:#}");
                None
            }
        },
        None => None,
    };
    let output = match output {
        Some(device) => match output_stream(&device, port) {
            Ok(stream) => Some(stream),
            Err(e) => {
                tracing::warn!("no speaker: {e:#}");
                None
            }
        },
        None => None,
    };
    anyhow::ensure!(
        input.is_some() || output.is_some(),
        "neither the microphone nor the speaker opened"
    );
    Ok((input, output))
}

fn input_stream(device: &cpal::Device, port: Port) -> anyhow::Result<cpal::Stream> {
    let supported = device.default_input_config()?;
    let config = supported.config();
    let channels = config.channels.max(1) as usize;
    let mut resampler = Resampler::new(config.sample_rate, RATE);
    let errors = |e| tracing::debug!("microphone: {e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _: &_| port.push_capture(resampler.run(&mono(data, channels))),
            errors,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _: &_| {
                let floats: Vec<f32> = data.iter().map(|s| *s as f32 / 32768.0).collect();
                port.push_capture(resampler.run(&mono(&floats, channels)));
            },
            errors,
            None,
        )?,
        other => anyhow::bail!("the microphone gives {other:?} samples"),
    };
    stream.play()?;
    Ok(stream)
}

fn output_stream(device: &cpal::Device, port: Port) -> anyhow::Result<cpal::Stream> {
    let supported = device.default_output_config()?;
    let config = supported.config();
    let channels = config.channels.max(1) as usize;
    let rate = config.sample_rate;
    let mut resampler = Resampler::new(RATE, rate);
    let mut fill = move |frames: usize| -> Vec<f32> {
        // How many 48 kHz samples make `frames` at the device's rate.
        let needed = (frames as u64 * RATE as u64).div_ceil(rate as u64) as usize;
        let mut out = resampler.run(&port.pull_playout(needed));
        out.resize(frames, *out.last().unwrap_or(&0.0));
        out
    };
    let errors = |e| tracing::debug!("speaker: {e}");
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _: &_| {
                let samples = fill(data.len() / channels);
                for (frame, sample) in data.chunks_mut(channels).zip(samples) {
                    frame.fill(sample);
                }
            },
            errors,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _: &_| {
                let samples = fill(data.len() / channels);
                for (frame, sample) in data.chunks_mut(channels).zip(samples) {
                    frame.fill((sample.clamp(-1.0, 1.0) * 32767.0) as i16);
                }
            },
            errors,
            None,
        )?,
        other => anyhow::bail!("the speaker takes {other:?} samples"),
    };
    stream.play()?;
    Ok(stream)
}

/// Averages interleaved channels into one.
fn mono(data: &[f32], channels: usize) -> Vec<f32> {
    if channels == 1 {
        return data.to_vec();
    }
    data.chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// A linear resampler that keeps its place between calls: plain, but enough for speech.
struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Where the next output sample falls, in input samples after `last`.
    position: f64,
    last: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: from as f64 / to as f64,
            position: 0.0,
            last: 0.0,
        }
    }

    fn run(&mut self, input: &[f32]) -> Vec<f32> {
        if (self.step - 1.0).abs() < f64::EPSILON || input.is_empty() {
            return input.to_vec();
        }
        let n = input.len();
        // Index -1 is the previous call's last sample.
        let at = |i: f64| -> f32 {
            if i < 0.0 {
                self.last
            } else {
                input[(i as usize).min(n - 1)]
            }
        };
        let mut out = Vec::with_capacity((n as f64 / self.step) as usize + 2);
        while self.position <= (n - 1) as f64 {
            let base = self.position.floor();
            let frac = (self.position - base) as f32;
            out.push(at(base) * (1.0 - frac) + at(base + 1.0) * frac);
            self.position += self.step;
        }
        self.position -= n as f64;
        self.last = input[n - 1];
        out
    }
}

/// Test devices: the "microphone" plays a tone (or silence), and what reaches the "speaker"
/// is recorded, both in real time.
#[derive(Clone, Default)]
pub struct ToneDevices {
    /// Hz; `None` for silence.
    pub tone: Option<f32>,
    pub heard: Arc<Mutex<Vec<f32>>>,
}

impl Devices for ToneDevices {
    fn open(&self, port: Port) -> anyhow::Result<Box<dyn Any + Send + Sync>> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tone, heard, flag) = (self.tone, self.heard.clone(), stop.clone());
        std::thread::Builder::new()
            .name("tone-devices".into())
            .spawn(move || {
                let start = Instant::now();
                let mut n = 0u64;
                while !flag.load(Ordering::SeqCst) {
                    let samples: Vec<f32> = (0..STEP)
                        .map(|i| match tone {
                            Some(hz) => {
                                let t = (n + i as u64) as f32 / RATE as f32;
                                // Rises and falls like speech, so noise suppression keeps it.
                                let swell =
                                    0.6 + 0.4 * (2.0 * std::f32::consts::PI * 3.0 * t).sin();
                                0.4 * swell * (2.0 * std::f32::consts::PI * hz * t).sin()
                            }
                            None => 0.0,
                        })
                        .collect();
                    port.push_capture(samples);
                    heard.lock().expect("lock").extend(port.pull_playout(STEP));
                    n += STEP as u64;
                    let due = start + Duration::from_micros(n * 1_000_000 / RATE as u64);
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                }
            })?;
        Ok(Box::new(StopOnDrop(stop)))
    }
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_length_and_shape() {
        let mut down = Resampler::new(48_000, 44_100);
        let input: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(480) {
            out.extend(down.run(chunk));
        }
        let expected = 4800.0 * 44_100.0 / 48_000.0;
        assert!(
            (out.len() as f64 - expected).abs() < 3.0,
            "{} samples",
            out.len()
        );
        // Same rate passes straight through.
        let mut same = Resampler::new(48_000, 48_000);
        assert_eq!(same.run(&input[..10]), &input[..10]);
    }

    #[test]
    fn the_port_pads_with_silence_and_drops_old_capture() {
        let port = Port::default();
        assert_eq!(port.take_capture(4), vec![0.0; 4]);
        port.push_capture((0..MOST_CAPTURE + 10).map(|i| i as f32));
        let first = port.take_capture(1)[0];
        assert_eq!(first, 10.0, "the oldest went");
        port.push_playout(&[1.0, 2.0]);
        assert_eq!(port.pull_playout(3), vec![1.0, 2.0, 0.0]);
    }

    /// The system's speaker keeps pulling while the guard lives (it once closed at once,
    /// and calls were silent). Skipped on machines with no speaker.
    #[test]
    fn the_system_speaker_stays_open() {
        if cpal::default_host().default_output_device().is_none() {
            return;
        }
        let port = Port::default();
        let Ok(guard) = Speakers::default().open(port.clone()) else {
            return;
        };
        port.push_playout(&vec![0.0; RATE as usize]);
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            port.playout_len() < RATE as usize,
            "the speaker took nothing"
        );
        drop(guard);
    }

    #[test]
    fn stereo_becomes_mono() {
        assert_eq!(mono(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
    }
}
