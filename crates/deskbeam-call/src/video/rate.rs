//! The video bitrate, following the loss the receiver reports each second: down hard on
//! loss, up gently when clean. At the floor with loss still high, video pauses (audio goes
//! first).

pub const START: u32 = 600_000;
pub const LEAST: u32 = 150_000;
pub const MOST: u32 = 1_500_000;

/// Loss above this halves the bitrate; below `CLEAN`, it rises by 10 %.
const LOSSY: f32 = 0.05;
const CLEAN: f32 = 0.02;
/// Reports in a row at the floor and still lossy before video pauses.
const PAUSE_AFTER: u32 = 3;

#[derive(Debug, Clone, PartialEq)]
pub struct Rate {
    bps: u32,
    least: u32,
    most: u32,
    starved: u32,
}

impl Default for Rate {
    fn default() -> Self {
        Self::new(START, LEAST, MOST)
    }
}

impl Rate {
    pub fn new(start: u32, least: u32, most: u32) -> Self {
        Self {
            bps: start.clamp(least, most),
            least,
            most,
            starved: 0,
        }
    }

    pub fn bps(&self) -> u32 {
        self.bps
    }

    /// The receiver's loss over the last second, 0 to 1.
    pub fn report(&mut self, loss: f32) {
        if loss > LOSSY {
            if self.bps == self.least {
                self.starved += 1;
            }
            self.bps = (self.bps / 2).max(self.least);
        } else {
            self.starved = 0;
            if loss < CLEAN {
                self.bps = (self.bps as f64 * 1.1).min(self.most as f64) as u32;
            }
        }
    }

    /// Too little gets through even at the floor: stop sending video for now.
    pub fn paused(&self) -> bool {
        self.starved >= PAUSE_AFTER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_on_loss_up_when_clean_pause_at_the_floor() {
        let mut rate = Rate::default();
        rate.report(0.2);
        assert_eq!(rate.bps(), START / 2);
        for _ in 0..40 {
            rate.report(0.0);
        }
        assert_eq!(rate.bps(), MOST);
        rate.report(0.03);
        assert_eq!(rate.bps(), MOST, "a little loss holds it");
        for _ in 0..10 {
            rate.report(0.5);
        }
        assert_eq!(rate.bps(), LEAST);
        assert!(rate.paused());
        rate.report(0.0);
        assert!(!rate.paused());
    }
}
