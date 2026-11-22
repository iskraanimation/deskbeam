//! The receiver's jitter buffer: packets arrive unevenly, out of order or not at all; the
//! speaker needs 20 ms every 20 ms.
//!
//! Playback starts once `target` packets are waiting, so there is room for late ones. Each
//! [`JitterBuffer::pop`] gives the next packet in order, or says it is lost (with the
//! packet after it when that is here, whose error-correction data can rebuild it). Packets
//! that come after their turn are dropped and make the buffer deeper; a buffer that has
//! grown too deep skips ahead, to keep the delay down.

use std::collections::BTreeMap;

/// Packets waiting before playback starts, at first and at most (20 ms each).
const START: usize = 3;
const MOST: usize = 10;
/// How many packets beyond the target are tolerated before skipping ahead.
const SLACK: usize = 4;
/// Every this many packets played on time, the target shrinks by one.
const CALM: u32 = 250;

/// What to play for the next 20 ms.
#[derive(Debug, PartialEq, Eq)]
pub enum Playout {
    /// Nothing has come yet, or the buffer is filling: play silence.
    Wait,
    Packet(Vec<u8>),
    /// The packet is lost; `next` is the one after it, if here, for its correction data.
    Lost {
        next: Option<Vec<u8>>,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub played: u64,
    pub lost: u64,
    pub late: u64,
}

pub struct JitterBuffer {
    waiting: BTreeMap<u32, Vec<u8>>,
    /// The sequence number to play next, once started.
    next: Option<u32>,
    target: usize,
    filling: bool,
    /// Playback has begun; before that, an earlier packet simply comes first.
    begun: bool,
    on_time: u32,
    counts: Counts,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self {
            waiting: BTreeMap::new(),
            next: None,
            target: START,
            filling: true,
            begun: false,
            on_time: 0,
            counts: Counts::default(),
        }
    }
}

impl JitterBuffer {
    pub fn push(&mut self, sequence: u32, payload: Vec<u8>) {
        if let Some(next) = self.next
            && sequence < next
            && self.begun
        {
            // Its turn has passed: allow for more delay from now on.
            self.counts.late += 1;
            self.target = (self.target + 1).min(MOST);
            self.on_time = 0;
            return;
        }
        self.waiting.insert(sequence, payload);
        if !self.begun || self.next.is_none() {
            self.next = self.waiting.keys().next().copied();
        }
    }

    pub fn pop(&mut self) -> Playout {
        let Some(next) = self.next else {
            return Playout::Wait;
        };
        if self.filling {
            if self.waiting.len() < self.target {
                return Playout::Wait;
            }
            // (Re)start at the oldest packet here, after a gap too.
            self.filling = false;
            self.begun = true;
            if let Some(&first) = self.waiting.keys().next() {
                self.next = Some(first.max(next));
            }
        }
        // Too deep: drop the oldest to catch up.
        while self.waiting.len() > self.target + SLACK
            && let Some((&first, _)) = self.waiting.first_key_value()
        {
            self.waiting.remove(&first);
            self.next = Some(first + 1);
        }
        let next = self.next.unwrap_or(next).max(next);
        self.next = Some(next + 1);
        match self.waiting.remove(&next) {
            Some(payload) => {
                self.counts.played += 1;
                self.on_time += 1;
                if self.on_time >= CALM && self.target > START {
                    self.target -= 1;
                    self.on_time = 0;
                }
                Playout::Packet(payload)
            }
            None => {
                self.counts.lost += 1;
                self.on_time = 0;
                if self.waiting.is_empty() {
                    // Nothing more is here: wait for the stream to come back.
                    self.filling = true;
                }
                Playout::Lost {
                    next: self.waiting.get(&(next + 1)).cloned(),
                }
            }
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// How deep the buffer aims to be, in packets.
    pub fn target(&self) -> usize {
        self.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u8) -> Vec<u8> {
        vec![n]
    }

    #[test]
    fn plays_in_order_after_filling() {
        let mut buffer = JitterBuffer::default();
        assert_eq!(buffer.pop(), Playout::Wait);
        // Out of order on the way.
        for n in [1u32, 0, 2] {
            buffer.push(n, p(n as u8));
        }
        assert_eq!(buffer.pop(), Playout::Packet(p(0)));
        assert_eq!(buffer.pop(), Playout::Packet(p(1)));
        assert_eq!(buffer.pop(), Playout::Packet(p(2)));
        assert_eq!(buffer.counts().played, 3);
    }

    #[test]
    fn a_lost_packet_brings_the_next_for_correction() {
        let mut buffer = JitterBuffer::default();
        for n in [0u32, 2, 3] {
            buffer.push(n, p(n as u8));
        }
        assert_eq!(buffer.pop(), Playout::Packet(p(0)));
        assert_eq!(buffer.pop(), Playout::Lost { next: Some(p(2)) });
        assert_eq!(buffer.pop(), Playout::Packet(p(2)));
        assert_eq!(buffer.counts().lost, 1);
    }

    #[test]
    fn late_packets_are_dropped_and_deepen_the_buffer() {
        let mut buffer = JitterBuffer::default();
        for n in 0..3u32 {
            buffer.push(n, p(n as u8));
        }
        buffer.pop();
        buffer.pop();
        buffer.push(0, p(0));
        assert_eq!(buffer.counts().late, 1);
        assert_eq!(buffer.target(), START + 1);
    }

    #[test]
    fn a_deep_buffer_skips_ahead() {
        let mut buffer = JitterBuffer::default();
        for n in 0..20u32 {
            buffer.push(n, p(n as u8));
        }
        // Keeps at most target + slack waiting, so the oldest go.
        let Playout::Packet(first) = buffer.pop() else {
            panic!("a packet");
        };
        assert!(first[0] >= 20 - (START + SLACK) as u8);
    }

    #[test]
    fn after_a_gap_it_fills_again() {
        let mut buffer = JitterBuffer::default();
        for n in 0..3u32 {
            buffer.push(n, p(n as u8));
        }
        for _ in 0..3 {
            buffer.pop();
        }
        assert!(matches!(buffer.pop(), Playout::Lost { next: None }));
        assert_eq!(buffer.pop(), Playout::Wait);
        for n in 10..13u32 {
            buffer.push(n, p(n as u8));
        }
        assert_eq!(buffer.pop(), Playout::Packet(p(10)));
    }
}
