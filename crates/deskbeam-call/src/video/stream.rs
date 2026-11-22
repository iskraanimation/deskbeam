//! Both directions of a call's video: [`VideoSender`] takes pictures from a source,
//! encodes and splits them; [`VideoReceiver`] joins, decodes and keeps the latest picture
//! for the window. Each runs its own thread, so the codec never holds up the network.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use super::{
    Picture, Profile, VideoFrame,
    assemble::{Assembler, Complete, Counts},
    codec::{VideoDecoder, VideoEncoder},
    packet::VideoPacket,
    rate::Rate,
    source::VideoSource,
};

/// The newest picture of a stream, with a number that grows with each one, so the window
/// copies only new ones into its texture.
#[derive(Default)]
pub struct Latest {
    frame: Mutex<Option<(u64, Arc<VideoFrame>)>>,
    count: AtomicU64,
}

impl Latest {
    fn set(&self, frame: VideoFrame) {
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        *self.frame.lock().expect("lock") = Some((n, Arc::new(frame)));
    }

    pub fn get(&self) -> Option<(u64, Arc<VideoFrame>)> {
        self.frame.lock().expect("lock").clone()
    }

    pub fn clear(&self) {
        *self.frame.lock().expect("lock") = None;
    }
}

/// What the sender has done, for showing.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct SendStats {
    pub pictures: u64,
    pub bytes: u64,
    pub bitrate: u32,
    pub paused: bool,
}

/// Sends a source's pictures, while it lives.
pub struct VideoSender {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    rate: Arc<Mutex<Rate>>,
    preview: Arc<Latest>,
    stats: Arc<Mutex<SendStats>>,
    thread: Option<JoinHandle<()>>,
    _source: Box<dyn std::any::Any + Send + Sync>,
}

impl VideoSender {
    /// `most` gives the largest datagram that fits the path now; `send` sends one.
    pub fn start(
        source: &dyn VideoSource,
        profile: Profile,
        most: impl Fn() -> usize + Send + 'static,
        mut send: impl FnMut(Vec<u8>) + Send + 'static,
    ) -> anyhow::Result<Self> {
        // Only the newest picture waits: an old one is no use.
        let (tx, rx) = mpsc::sync_channel::<Picture>(1);
        let guard = source.start(Box::new(move |picture| {
            let _ = tx.try_send(picture);
        }))?;
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(false));
        let rate = Arc::new(Mutex::new(Rate::new(
            profile.start_bps,
            profile.least_bps,
            profile.most_bps,
        )));
        let preview = Arc::new(Latest::default());
        let stats = Arc::new(Mutex::new(SendStats::default()));
        let (flag, key, shared_rate, own, figures) = (
            stop.clone(),
            keyframe.clone(),
            rate.clone(),
            preview.clone(),
            stats.clone(),
        );
        let thread = std::thread::Builder::new()
            .name("video-send".into())
            .spawn(move || {
                let mut encoder = VideoEncoder::new(profile.start_bps, profile.fps, profile.screen);
                // A screen may deliver faster than it is sent: keep to the profile's rate.
                let every = Duration::from_secs_f32(1.0 / profile.fps.max(1.0));
                let mut last_sent: Option<Instant> = None;
                let mut frame = 0u32;
                let mut was_paused = false;
                while !flag.load(Ordering::SeqCst) {
                    let Ok(picture) = rx.recv_timeout(Duration::from_millis(100)) else {
                        continue;
                    };
                    if last_sent.is_some_and(|at| at.elapsed() < every.mul_f32(0.8)) {
                        continue;
                    }
                    last_sent = Some(Instant::now());
                    let (w, h) = profile.fit(picture.width, picture.height);
                    let picture = picture.scaled(w, h);
                    own.set(picture.to_rgba());

                    let (bps, paused) = {
                        let rate = shared_rate.lock().expect("lock");
                        (rate.bps(), rate.paused())
                    };
                    if paused {
                        was_paused = true;
                        figures.lock().expect("lock").paused = true;
                        continue;
                    }
                    encoder.set_bitrate(bps);
                    if key.swap(false, Ordering::SeqCst) || std::mem::take(&mut was_paused) {
                        encoder.request_keyframe();
                    }
                    let encoded = match encoder.encode(&picture) {
                        Ok(Some(encoded)) => encoded,
                        Ok(None) => continue,
                        Err(e) => {
                            tracing::debug!("video: {e:#}");
                            continue;
                        }
                    };
                    let pieces = VideoPacket::split(frame, encoded.keyframe, &encoded.data, most());
                    frame = frame.wrapping_add(1);
                    for piece in pieces {
                        send(piece);
                    }
                    let mut figures = figures.lock().expect("lock");
                    figures.pictures += 1;
                    figures.bytes += encoded.data.len() as u64;
                    figures.bitrate = encoder.bitrate();
                    figures.paused = false;
                }
            })?;
        Ok(Self {
            stop,
            keyframe,
            rate,
            preview,
            stats,
            thread: Some(thread),
            _source: guard,
        })
    }

    /// The other end lost its place.
    pub fn request_keyframe(&self) {
        self.keyframe.store(true, Ordering::SeqCst);
    }

    /// The other end's loss over the last second.
    pub fn report(&self, loss: f32) {
        self.rate.lock().expect("lock").report(loss);
    }

    /// This end's own picture, as sent.
    pub fn preview(&self) -> &Arc<Latest> {
        &self.preview
    }

    pub fn stats(&self) -> SendStats {
        *self.stats.lock().expect("lock")
    }
}

impl Drop for VideoSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Receives the other end's video.
pub struct VideoReceiver {
    assembler: Arc<Mutex<Assembler>>,
    decode: Option<mpsc::Sender<Complete>>,
    latest: Arc<Latest>,
    reported: Mutex<Counts>,
    thread: Option<JoinHandle<()>>,
}

impl VideoReceiver {
    pub fn start() -> anyhow::Result<Self> {
        let assembler = Arc::new(Mutex::new(Assembler::default()));
        let latest = Arc::new(Latest::default());
        let (tx, rx) = mpsc::channel::<Complete>();
        let (lost, shown) = (assembler.clone(), latest.clone());
        let mut decoder = VideoDecoder::new()?;
        let thread = std::thread::Builder::new()
            .name("video-receive".into())
            .spawn(move || {
                for complete in rx {
                    match decoder.decode(&complete.data) {
                        Ok(Some(frame)) => shown.set(frame),
                        Ok(None) => {}
                        Err(e) => {
                            tracing::debug!("video: {e:#}");
                            lost.lock().expect("lock").lost_place();
                        }
                    }
                }
            })?;
        Ok(Self {
            assembler,
            decode: Some(tx),
            latest,
            reported: Mutex::default(),
            thread: Some(thread),
        })
    }

    /// A datagram from the other end; `false` when it is not video.
    pub fn receive(&self, datagram: &[u8]) -> bool {
        let Some(packet) = VideoPacket::decode(datagram) else {
            return false;
        };
        let complete = self.assembler.lock().expect("lock").push(packet);
        if let (Some(complete), Some(tx)) = (complete, &self.decode) {
            let _ = tx.send(complete);
        }
        true
    }

    /// Whether to ask the other end for a keyframe now.
    pub fn wants_keyframe(&self) -> bool {
        self.assembler
            .lock()
            .expect("lock")
            .ask_for_keyframe(Instant::now())
    }

    /// The share of pictures lost since the last call, for the other end's bitrate.
    pub fn loss_report(&self) -> f32 {
        let now = self.assembler.lock().expect("lock").counts();
        let mut before = self.reported.lock().expect("lock");
        let loss = now.loss_since(&before);
        *before = now;
        loss
    }

    pub fn counts(&self) -> Counts {
        self.assembler.lock().expect("lock").counts()
    }

    pub fn latest(&self) -> &Arc<Latest> {
        &self.latest
    }
}

impl Drop for VideoReceiver {
    fn drop(&mut self) {
        self.decode.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::source::TestPattern;

    /// A sender and a receiver joined by a channel that loses `drop_every`th datagram.
    fn run(seconds: f32, drop_every: Option<u64>) -> (VideoReceiver, VideoSender, u64) {
        let receiver = Arc::new(VideoReceiver::start().unwrap());
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let mut n = 0u64;
        let sender = VideoSender::start(
            &TestPattern::default(),
            Profile::CAMERA,
            || 1100,
            move |d| {
                n += 1;
                if drop_every.is_none_or(|k| !n.is_multiple_of(k)) {
                    let _ = tx.send(d);
                }
            },
        )
        .unwrap();
        let end = Instant::now() + Duration::from_secs_f32(seconds);
        let mut asked = 0;
        while Instant::now() < end {
            while let Ok(d) = rx.try_recv() {
                assert!(receiver.receive(&d));
            }
            if receiver.wants_keyframe() {
                asked += 1;
                sender.request_keyframe();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let receiver = Arc::into_inner(receiver).unwrap();
        (receiver, sender, asked)
    }

    #[test]
    fn pictures_cross_and_show() {
        let (receiver, sender, _) = run(1.5, None);
        let (n, frame) = receiver.latest().get().expect("a picture");
        assert!(n >= 10, "{n} pictures");
        assert_eq!((frame.width, frame.height), (320, 180));
        assert!(sender.preview().get().is_some());
        assert!(sender.stats().pictures >= 10);
        assert_eq!(receiver.counts().lost, 0);
    }

    #[test]
    fn after_loss_a_keyframe_is_asked_for_and_the_stream_recovers() {
        // Pictures are 1–3 datagrams; losing one in 15 loses a picture now and then.
        let (receiver, _sender, asked) = run(3.0, Some(15));
        assert!(receiver.counts().lost > 0, "{:?}", receiver.counts());
        assert!(asked >= 2, "asked {asked} times");
        let shown = receiver.latest().get().map_or(0, |(n, _)| n);
        assert!(shown >= 15, "only {shown} pictures shown");
    }
}
