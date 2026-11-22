//! Where pictures come from: the camera, or a moving test pattern for tests. The screen
//! (plan 6d) will be another.

use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use super::Picture;

/// Something that delivers pictures to `deliver`, on a thread of its own, while the
/// returned guard lives.
pub trait VideoSource: Send + Sync {
    fn start(
        &self,
        deliver: Box<dyn FnMut(Picture) + Send>,
    ) -> anyhow::Result<Box<dyn Any + Send + Sync>>;
}

/// A test picture that changes with `n`: bright on the left fading to dark on the right,
/// with a band moving down.
pub fn pattern(width: usize, height: usize, n: usize) -> Picture {
    let mut picture = Picture::new(width, height);
    let (y, u, v) = picture.planes_mut();
    for row in 0..height {
        let band = (row + n * 4) % height < height / 6;
        for col in 0..width {
            let level = 235 - (col * 200 / width.max(1)) as i32;
            y[row * width + col] = if band { 60 } else { level as u8 };
        }
    }
    u.fill(110);
    v.fill(150);
    picture
}

/// [`pattern`] at a steady rate.
#[derive(Debug, Clone, Copy)]
pub struct TestPattern {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
}

impl Default for TestPattern {
    fn default() -> Self {
        Self {
            width: 320,
            height: 180,
            fps: 15,
        }
    }
}

impl VideoSource for TestPattern {
    fn start(
        &self,
        mut deliver: Box<dyn FnMut(Picture) + Send>,
    ) -> anyhow::Result<Box<dyn Any + Send + Sync>> {
        let stop = Arc::new(AtomicBool::new(false));
        let (me, flag) = (*self, stop.clone());
        std::thread::Builder::new()
            .name("test-pattern".into())
            .spawn(move || {
                let start = Instant::now();
                let every = Duration::from_secs(1) / me.fps.max(1);
                let mut n = 0usize;
                while !flag.load(Ordering::SeqCst) {
                    deliver(pattern(me.width, me.height, n));
                    n += 1;
                    let due = start + every * n as u32;
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
