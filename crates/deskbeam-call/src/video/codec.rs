//! H.264 through OpenH264 (Cisco's, built from source by the `openh264` crate).

use std::time::{Duration, Instant};

use anyhow::Context;
use openh264::{
    OpenH264API,
    decoder::Decoder,
    encoder::{
        BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
        UsageType,
    },
    formats::{YUVBuffer, YUVSource},
};

use super::{Picture, VideoFrame};

/// A bitrate change rebuilds the encoder (the crate has no safe way to change it while it
/// runs), and a rebuilt encoder starts with a keyframe: so only for a change this large…
const WORTH_CHANGING: f64 = 0.2;
/// …and at most this often.
const CHANGE_AT_MOST_EVERY: Duration = Duration::from_secs(3);
/// A keyframe at least this often, so a stream always recovers.
const KEYFRAME_EVERY_SECS: f32 = 10.0;

/// One encoded picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub struct VideoEncoder {
    encoder: Option<(Encoder, usize, usize)>,
    fps: f32,
    /// What the encoder was built for, and what is wanted now.
    bitrate: u32,
    wanted: u32,
    changed_at: Option<Instant>,
    keyframe: bool,
    screen: bool,
}

impl VideoEncoder {
    /// `screen`: tuned for a shared screen rather than a camera.
    pub fn new(bitrate: u32, fps: f32, screen: bool) -> Self {
        Self {
            encoder: None,
            screen,
            fps,
            bitrate,
            wanted: bitrate,
            changed_at: None,
            keyframe: false,
        }
    }

    /// The bitrate to aim for; applied when it differs enough and not too often.
    pub fn set_bitrate(&mut self, bps: u32) {
        self.wanted = bps;
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    /// The next picture is sent whole, for a decoder that lost its place.
    pub fn request_keyframe(&mut self) {
        self.keyframe = true;
    }

    /// `None` when the encoder skipped the picture to keep to its bitrate.
    pub fn encode(&mut self, picture: &Picture) -> anyhow::Result<Option<Encoded>> {
        let far = (self.wanted as f64 - self.bitrate as f64).abs() / self.bitrate as f64;
        let may = self
            .changed_at
            .is_none_or(|at| at.elapsed() >= CHANGE_AT_MOST_EVERY);
        if far >= WORTH_CHANGING && may {
            self.bitrate = self.wanted;
            self.changed_at = Some(Instant::now());
            self.encoder = None;
        }
        let fits = self
            .encoder
            .as_ref()
            .is_some_and(|(_, w, h)| (*w, *h) == (picture.width, picture.height));
        if !fits {
            let config = EncoderConfig::new()
                .bitrate(BitRate::from_bps(self.bitrate))
                .max_frame_rate(FrameRate::from_hz(self.fps))
                .usage_type(if self.screen {
                    UsageType::ScreenContentRealTime
                } else {
                    UsageType::CameraVideoRealTime
                })
                .rate_control_mode(RateControlMode::Bitrate)
                .skip_frames(true)
                .intra_frame_period(IntraFramePeriod::from_num_frames(
                    (self.fps * KEYFRAME_EVERY_SECS) as u32,
                ));
            let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
                .context("starting the video encoder")?;
            self.encoder = Some((encoder, picture.width, picture.height));
        }
        let (encoder, _, _) = self.encoder.as_mut().expect("made above");
        if std::mem::take(&mut self.keyframe) {
            encoder.force_intra_frame();
        }
        let yuv = YUVBuffer::from_vec(picture.data.clone(), picture.width, picture.height);
        let stream = encoder.encode(&yuv).context("encoding video")?;
        let keyframe = matches!(stream.frame_type(), FrameType::IDR | FrameType::I);
        if matches!(stream.frame_type(), FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let data = stream.to_vec();
        Ok((!data.is_empty()).then_some(Encoded { data, keyframe }))
    }
}

pub struct VideoDecoder {
    decoder: Decoder,
}

impl VideoDecoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            decoder: Decoder::new().context("starting the video decoder")?,
        })
    }

    /// `Ok(None)` while the decoder waits for more; an error means it lost its place and
    /// needs a keyframe.
    pub fn decode(&mut self, data: &[u8]) -> anyhow::Result<Option<VideoFrame>> {
        let Some(yuv) = self.decoder.decode(data).context("decoding video")? else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        let mut rgba = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut rgba);
        Ok(Some(VideoFrame {
            width,
            height,
            rgba,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::source::pattern;

    #[test]
    fn pictures_round_trip_and_keyframes_come_when_asked() {
        let mut encoder = VideoEncoder::new(600_000, 30.0, false);
        let mut decoder = VideoDecoder::new().unwrap();
        let mut keyframes = Vec::new();
        let mut last = None;
        for n in 0..40 {
            if n == 20 {
                encoder.request_keyframe();
            }
            let picture = pattern(320, 180, n);
            if let Some(encoded) = encoder.encode(&picture).unwrap() {
                if encoded.keyframe {
                    keyframes.push(n);
                }
                if let Some(frame) = decoder.decode(&encoded.data).unwrap() {
                    last = Some(frame);
                }
            }
        }
        assert_eq!(keyframes.first(), Some(&0), "starts with a keyframe");
        assert!(keyframes.contains(&20), "{keyframes:?}");
        let frame = last.unwrap();
        assert_eq!((frame.width, frame.height), (320, 180));
        // The pattern's left edge is bright, its right edge dark.
        let at = |x: usize| frame.rgba[(90 * 320 + x) * 4] as i32;
        assert!(at(4) - at(316) > 60, "{} vs {}", at(4), at(316));
    }

    #[test]
    fn a_large_bitrate_change_rebuilds_with_a_keyframe() {
        let mut encoder = VideoEncoder::new(600_000, 30.0, false);
        encoder.encode(&pattern(160, 90, 0)).unwrap();
        encoder.set_bitrate(620_000);
        encoder.encode(&pattern(160, 90, 1)).unwrap();
        assert_eq!(encoder.bitrate(), 600_000, "too small a change");
        encoder.set_bitrate(300_000);
        let next = encoder.encode(&pattern(160, 90, 2)).unwrap().unwrap();
        assert_eq!(encoder.bitrate(), 300_000);
        assert!(next.keyframe);
    }
}
