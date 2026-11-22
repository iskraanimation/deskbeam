//! DeskBeam's calls (plan Phase 6): voice between two paired devices, with no call server.
//!
//! - **Setup** runs on one QUIC bi-stream on [`ALPN`]: length-prefixed JSON [`Frame`]s
//!   (offer, ringing, accept, decline, hang up, mute).
//! - **Audio** goes as QUIC datagrams on the same connection, one [`packet::AudioPacket`] of
//!   20 ms of Opus each. Datagrams are never resent, which suits live audio: a late packet
//!   is no use. The receiver's [`jitter::JitterBuffer`] evens out arrival times and fills
//!   gaps from Opus's error correction or concealment.
//! - **Processing** ([`media`]): echo cancellation, noise suppression and gain control on the
//!   microphone (WebRTC's, through `sonora`), Opus at 48 kHz mono (`opus1`), and the devices
//!   through `cpal` ([`devices`]) or a test stand-in.
//!
//! This crate knows nothing of iroh: the engine carries the frames and datagrams.

pub mod codec;
pub mod devices;
pub mod jitter;
pub mod media;
pub mod packet;
pub mod video;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"deskbeam/call/1";

/// Audio runs at 48 kHz, mono.
pub const RATE: u32 = 48_000;
/// One packet holds 20 ms.
pub const FRAME: usize = (RATE / 50) as usize;
/// Processing works in 10 ms steps, as WebRTC's does.
pub const STEP: usize = (RATE / 100) as usize;

/// Setup frames are tiny; anything this large is not one.
const MAX_FRAME: usize = 64 * 1024;

/// What the two ends say to set up and run a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum Frame {
    /// The caller asks. `id` names the call in both ends' history. `video` says this build
    /// can do video (missing from older builds: false, so they never get video frames).
    Offer {
        id: String,
        #[serde(default)]
        video: bool,
    },
    /// The callee rings: its user is being asked.
    Ringing,
    Accept {
        #[serde(default)]
        video: bool,
    },
    /// Not now: declined by the user, or not allowed.
    Decline { reason: String },
    /// Already in a call.
    Busy,
    /// Either end ends the call (or the caller stops ringing).
    Hangup,
    /// The other end muted or unmuted its microphone, for showing.
    Mute { on: bool },
    /// This end's camera went on or off. Only when both ends can do video.
    Video { on: bool },
    /// This end's video is now a shared screen (or the camera again). The receiver fits
    /// a shared screen rather than filling the window with it.
    Share { on: bool },
    /// The receiver lost its place in the video: send a keyframe.
    KeyframeRequest,
    /// The share of video pictures lost over the last second, in percent, for the sender's
    /// bitrate.
    VideoReport { loss: u8 },
}

/// Writes one frame: a 4-byte big-endian length, then JSON.
pub async fn write_frame(w: &mut (impl AsyncWrite + Unpin), frame: &Frame) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one frame written by [`write_frame`].
pub async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Frame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    anyhow::ensure!(len <= MAX_FRAME, "frame too large");
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let frames = [
            Frame::Offer {
                id: "c1".into(),
                video: true,
            },
            Frame::Ringing,
            Frame::Accept { video: true },
            Frame::Decline {
                reason: "busy now".into(),
            },
            Frame::Busy,
            Frame::Mute { on: true },
            Frame::Video { on: true },
            Frame::Share { on: true },
            Frame::KeyframeRequest,
            Frame::VideoReport { loss: 7 },
            Frame::Hangup,
        ];
        for frame in &frames {
            write_frame(&mut a, frame).await.unwrap();
        }
        for frame in &frames {
            assert_eq!(&read_frame(&mut b).await.unwrap(), frame);
        }
    }

    /// Builds from before video send these; they read as "no video".
    #[test]
    fn frames_from_builds_without_video_still_read() {
        let offer: Frame = serde_json::from_str(r#"{"type":"offer","id":"x"}"#).unwrap();
        assert_eq!(
            offer,
            Frame::Offer {
                id: "x".into(),
                video: false
            }
        );
        let accept: Frame = serde_json::from_str(r#"{"type":"accept"}"#).unwrap();
        assert_eq!(accept, Frame::Accept { video: false });
    }
}
