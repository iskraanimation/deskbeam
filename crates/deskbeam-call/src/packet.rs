//! One audio datagram: a small header and 20 ms of Opus.
//!
//! `[version 1][flags 1][sequence 4][timestamp 4][opus …]`, big-endian. The sequence counts
//! packets from 0; the timestamp counts samples at 48 kHz, for the receiver's jitter
//! measurements.

/// The header's length in bytes.
pub const HEADER: usize = 10;
const VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub sequence: u32,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

impl AudioPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER + self.payload.len());
        bytes.push(VERSION);
        bytes.push(0);
        bytes.extend_from_slice(&self.sequence.to_be_bytes());
        bytes.extend_from_slice(&self.timestamp.to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        bytes
    }

    /// `None` for anything that is not a packet of this version.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER || bytes[0] != VERSION {
            return None;
        }
        let word = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        Some(Self {
            sequence: word(2),
            timestamp: word(6),
            payload: bytes[HEADER..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_round_trip_and_junk_is_refused() {
        let packet = AudioPacket {
            sequence: 70_000,
            timestamp: 960 * 70_000,
            payload: vec![1, 2, 3],
        };
        let bytes = packet.encode();
        assert_eq!(bytes.len(), HEADER + 3);
        assert_eq!(AudioPacket::decode(&bytes), Some(packet));
        assert_eq!(AudioPacket::decode(&bytes[..5]), None);
        let mut other = bytes.clone();
        other[0] = 9;
        assert_eq!(AudioPacket::decode(&other), None);
    }
}
