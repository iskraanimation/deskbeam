//! Video in datagrams: an encoded picture split into pieces that each fit one.
//!
//! `[2][flags 1][frame 4][index 2][count 2][data …]`, big-endian. The first byte tells it
//! from audio (`1`), so builds without video ignore it. Flags: 1 = keyframe.

/// The header's length in bytes.
pub const HEADER: usize = 10;
pub const KIND: u8 = 2;
const KEYFRAME: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPacket {
    /// Counts pictures from 0.
    pub frame: u32,
    pub keyframe: bool,
    /// This piece, of `count`.
    pub index: u16,
    pub count: u16,
    pub data: Vec<u8>,
}

impl VideoPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER + self.data.len());
        bytes.push(KIND);
        bytes.push(if self.keyframe { KEYFRAME } else { 0 });
        bytes.extend_from_slice(&self.frame.to_be_bytes());
        bytes.extend_from_slice(&self.index.to_be_bytes());
        bytes.extend_from_slice(&self.count.to_be_bytes());
        bytes.extend_from_slice(&self.data);
        bytes
    }

    /// `None` for anything that is not a video packet.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER || bytes[0] != KIND {
            return None;
        }
        let index = u16::from_be_bytes([bytes[6], bytes[7]]);
        let count = u16::from_be_bytes([bytes[8], bytes[9]]);
        if count == 0 || index >= count {
            return None;
        }
        Some(Self {
            frame: u32::from_be_bytes(bytes[2..6].try_into().expect("4 bytes")),
            keyframe: bytes[1] & KEYFRAME != 0,
            index,
            count,
            data: bytes[HEADER..].to_vec(),
        })
    }

    /// One encoded picture as datagrams of at most `most` bytes each.
    pub fn split(frame: u32, keyframe: bool, data: &[u8], most: usize) -> Vec<Vec<u8>> {
        let room = most.saturating_sub(HEADER).max(1);
        let count = data.len().div_ceil(room).max(1);
        let count = count.min(u16::MAX as usize);
        (0..count)
            .map(|index| {
                let piece =
                    &data[(index * room).min(data.len())..((index + 1) * room).min(data.len())];
                VideoPacket {
                    frame,
                    keyframe,
                    index: index as u16,
                    count: count as u16,
                    data: piece.to_vec(),
                }
                .encode()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_split_to_fit_and_decode() {
        let data: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
        let pieces = VideoPacket::split(7, true, &data, 1000);
        assert_eq!(pieces.len(), 3);
        assert!(pieces.iter().all(|p| p.len() <= 1000));
        let back: Vec<VideoPacket> = pieces
            .iter()
            .map(|p| VideoPacket::decode(p).unwrap())
            .collect();
        assert!(
            back.iter()
                .all(|p| p.frame == 7 && p.keyframe && p.count == 3)
        );
        let joined: Vec<u8> = back.iter().flat_map(|p| p.data.clone()).collect();
        assert_eq!(joined, data);
        // Audio and junk are not video.
        assert_eq!(
            VideoPacket::decode(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 9]),
            None
        );
        assert_eq!(VideoPacket::decode(&pieces[0][..5]), None);
    }
}
