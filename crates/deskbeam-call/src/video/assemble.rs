//! Joins video packets back into encoded pictures.
//!
//! A picture is complete when all its pieces are in. H.264 pictures depend on the ones
//! before, so once a picture is lost (or the decoder fails) nothing can be shown until the
//! next keyframe: the assembler holds pictures back until then and says when to ask for one.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use super::packet::VideoPacket;

/// Pictures being put together at once; older ones are given up.
const MOST_PARTIAL: usize = 8;
/// How often to ask again for a keyframe that has not come.
const ASK_EVERY: Duration = Duration::from_millis(500);

/// A complete encoded picture, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Complete {
    pub frame: u32,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Pictures handed on.
    pub complete: u64,
    /// Pictures that never came whole.
    pub lost: u64,
    /// Whole pictures held back while waiting for a keyframe.
    pub held: u64,
}

impl Counts {
    /// Lost as a share of all pictures, for the sender's bitrate.
    pub fn loss_since(&self, before: &Counts) -> f32 {
        let lost = self.lost - before.lost;
        let all = lost + (self.complete - before.complete) + (self.held - before.held);
        if all == 0 {
            0.0
        } else {
            lost as f32 / all as f32
        }
    }
}

struct Partial {
    keyframe: bool,
    pieces: Vec<Option<Vec<u8>>>,
    have: usize,
}

pub struct Assembler {
    partial: BTreeMap<u32, Partial>,
    /// The last picture handed on (or given up).
    last: Option<u32>,
    /// Lost its place: only a keyframe helps.
    need_keyframe: bool,
    asked_at: Option<Instant>,
    counts: Counts,
}

impl Default for Assembler {
    fn default() -> Self {
        Self {
            partial: BTreeMap::new(),
            last: None,
            // The first picture shown must be a keyframe.
            need_keyframe: true,
            asked_at: None,
            counts: Counts::default(),
        }
    }
}

impl Assembler {
    pub fn push(&mut self, packet: VideoPacket) -> Option<Complete> {
        if self.last.is_some_and(|last| packet.frame <= last) {
            return None;
        }
        let partial = self.partial.entry(packet.frame).or_insert_with(|| Partial {
            keyframe: packet.keyframe,
            pieces: vec![None; packet.count as usize],
            have: 0,
        });
        let index = packet.index as usize;
        if index >= partial.pieces.len() || partial.pieces[index].is_some() {
            return None;
        }
        partial.pieces[index] = Some(packet.data);
        partial.have += 1;
        if partial.have < partial.pieces.len() {
            while self.partial.len() > MOST_PARTIAL {
                self.give_up_oldest();
            }
            return None;
        }

        let done = self.partial.remove(&packet.frame).expect("just here");
        // Anything older still in pieces will not be shown now.
        let older: Vec<u32> = self
            .partial
            .range(..packet.frame)
            .map(|(f, _)| *f)
            .collect();
        for frame in older {
            self.partial.remove(&frame);
        }
        let skipped = self.last.map_or(0, |last| {
            packet.frame.saturating_sub(last).saturating_sub(1)
        });
        if skipped > 0 {
            self.counts.lost += skipped as u64;
            self.need_keyframe = true;
        }
        self.last = Some(packet.frame);
        if done.keyframe {
            self.need_keyframe = false;
            self.asked_at = None;
        } else if self.need_keyframe {
            self.counts.held += 1;
            return None;
        }
        self.counts.complete += 1;
        Some(Complete {
            frame: packet.frame,
            keyframe: done.keyframe,
            data: done.pieces.into_iter().flatten().flatten().collect(),
        })
    }

    fn give_up_oldest(&mut self) {
        if let Some((&frame, _)) = self.partial.first_key_value() {
            self.partial.remove(&frame);
        }
    }

    /// The decoder could not use a picture: wait for a keyframe.
    pub fn lost_place(&mut self) {
        self.need_keyframe = true;
    }

    /// Whether to ask the sender for a keyframe now (not more often than every 500 ms).
    pub fn ask_for_keyframe(&mut self, now: Instant) -> bool {
        if !self.need_keyframe || self.asked_at.is_some_and(|at| now - at < ASK_EVERY) {
            return false;
        }
        self.asked_at = Some(now);
        true
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(frame: u32, keyframe: bool, n: u16) -> Vec<VideoPacket> {
        (0..n)
            .map(|index| VideoPacket {
                frame,
                keyframe,
                index,
                count: n,
                data: vec![frame as u8, index as u8],
            })
            .collect()
    }

    #[test]
    fn pieces_join_in_any_order() {
        let mut a = Assembler::default();
        let mut p = pieces(0, true, 3);
        p.reverse();
        assert_eq!(a.push(p[0].clone()), None);
        assert_eq!(a.push(p[1].clone()), None);
        let done = a.push(p[2].clone()).unwrap();
        assert_eq!(done.data, vec![0, 0, 0, 1, 0, 2]);
        assert!(done.keyframe);
    }

    #[test]
    fn a_lost_picture_holds_the_stream_until_a_keyframe() {
        let mut a = Assembler::default();
        let now = Instant::now();
        assert!(a.ask_for_keyframe(now), "nothing to start from yet");
        assert!(!a.ask_for_keyframe(now), "not again at once");
        // A picture before the first keyframe is held.
        assert_eq!(a.push(pieces(0, false, 1)[0].clone()), None);
        assert!(a.push(pieces(1, true, 1)[0].clone()).is_some());
        assert!(a.push(pieces(2, false, 1)[0].clone()).is_some());
        // 3 never comes whole; 4 is held.
        a.push(pieces(3, false, 2)[0].clone());
        assert_eq!(a.push(pieces(4, false, 1)[0].clone()), None);
        assert!(a.ask_for_keyframe(now + Duration::from_secs(1)));
        assert_eq!(a.counts().lost, 1);
        // A late piece of 3 is no use now.
        assert_eq!(a.push(pieces(3, false, 2)[1].clone()), None);
        assert!(a.push(pieces(5, true, 1)[0].clone()).is_some());
        assert!(a.push(pieces(6, false, 1)[0].clone()).is_some());
        assert!(!a.ask_for_keyframe(now + Duration::from_secs(2)));
    }

    #[test]
    fn loss_is_a_share_of_pictures() {
        let before = Counts::default();
        let after = Counts {
            complete: 18,
            lost: 2,
            held: 0,
        };
        assert!((after.loss_since(&before) - 0.1).abs() < 1e-6);
    }
}
