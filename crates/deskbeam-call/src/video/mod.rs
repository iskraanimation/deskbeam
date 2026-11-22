//! Video for calls (plan 6b): pictures from a [`source::VideoSource`] (the camera, a test
//! pattern, later the screen), H.264 through OpenH264 ([`codec`]), split across datagrams
//! ([`packet`]) and joined again ([`assemble`]), with the bitrate following the loss the
//! other end reports ([`rate`]). [`stream`] runs both directions.
//!
//! Like audio, this knows nothing of iroh: the engine carries the datagrams.

pub mod assemble;
pub mod camera;
pub mod codec;
pub mod packet;
pub mod rate;
pub mod screen;
pub mod source;
pub mod stream;

/// The camera's largest picture, scaled down to fit and keeping its shape.
pub const MOST_WIDTH: usize = 640;
pub const MOST_HEIGHT: usize = 360;

/// How a kind of video is sent: the camera's moving faces, or a shared screen's fine
/// detail (more pixels, fewer pictures, more bits).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Profile {
    pub most_width: usize,
    pub most_height: usize,
    pub fps: f32,
    /// Bits a second: to start with, and the range the loss reports move it in.
    pub start_bps: u32,
    pub least_bps: u32,
    pub most_bps: u32,
    /// OpenH264's screen-content mode: sharper text, fewer bits for still areas.
    pub screen: bool,
}

impl Profile {
    pub const CAMERA: Profile = Profile {
        most_width: MOST_WIDTH,
        most_height: MOST_HEIGHT,
        fps: 30.0,
        start_bps: rate::START,
        least_bps: rate::LEAST,
        most_bps: rate::MOST,
        screen: false,
    };

    /// The camera in HD: up to 1280×720 at 30 fps.
    pub const CAMERA_HD: Profile = Profile {
        most_width: 1280,
        most_height: 720,
        fps: 30.0,
        start_bps: 1_500_000,
        least_bps: 300_000,
        most_bps: 3_000_000,
        screen: false,
    };

    pub const SCREEN: Profile = Profile {
        most_width: 1920,
        most_height: 1080,
        fps: 10.0,
        start_bps: 1_200_000,
        least_bps: 300_000,
        most_bps: 2_500_000,
        screen: true,
    };

    /// The size a `width` × `height` picture is sent at: within the profile's largest, the
    /// same shape, even numbers.
    pub fn fit(&self, width: usize, height: usize) -> (usize, usize) {
        Picture::fitted_within(width, height, self.most_width, self.most_height)
    }
}

/// A picture in I420: the full-size Y plane, then U and V at half size each way. Width and
/// height are even.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
}

impl Picture {
    /// A mid-grey picture.
    pub fn new(width: usize, height: usize) -> Self {
        let (width, height) = (width & !1, height & !1);
        let mut data = vec![128u8; width * height * 3 / 2];
        data[..width * height].fill(128);
        Self {
            width,
            height,
            data,
        }
    }

    pub fn planes(&self) -> (&[u8], &[u8], &[u8]) {
        let y = self.width * self.height;
        let c = y / 4;
        (
            &self.data[..y],
            &self.data[y..y + c],
            &self.data[y + c..y + 2 * c],
        )
    }

    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let y = self.width * self.height;
        let c = y / 4;
        let (luma, chroma) = self.data.split_at_mut(y);
        let (u, v) = chroma.split_at_mut(c);
        (luma, u, &mut v[..c])
    }

    /// The camera's size for a picture: within `MOST_WIDTH` × `MOST_HEIGHT`.
    pub fn fitted_size(width: usize, height: usize) -> (usize, usize) {
        Self::fitted_within(width, height, MOST_WIDTH, MOST_HEIGHT)
    }

    /// Within `most_w` × `most_h`, the same shape, even numbers.
    pub fn fitted_within(
        width: usize,
        height: usize,
        most_w: usize,
        most_h: usize,
    ) -> (usize, usize) {
        let scale = (most_w as f64 / width.max(1) as f64)
            .min(most_h as f64 / height.max(1) as f64)
            .min(1.0);
        let w = ((width as f64 * scale) as usize).max(2) & !1;
        let h = ((height as f64 * scale) as usize).max(2) & !1;
        (w, h)
    }

    /// Scaled to `width` × `height` (nearest sample: plain, quick, enough for a camera).
    pub fn scaled(&self, width: usize, height: usize) -> Picture {
        if (width, height) == (self.width, self.height) {
            return self.clone();
        }
        let mut out = Picture::new(width, height);
        let (sy, su, sv) = self.planes();
        let (dy, du, dv) = out.planes_mut();
        scale_plane(sy, self.width, self.height, dy, width, height);
        scale_plane(
            su,
            self.width / 2,
            self.height / 2,
            du,
            width / 2,
            height / 2,
        );
        scale_plane(
            sv,
            self.width / 2,
            self.height / 2,
            dv,
            width / 2,
            height / 2,
        );
        out
    }

    /// For showing: RGBA, BT.601 limited range (what the camera and OpenH264 use).
    pub fn to_rgba(&self) -> VideoFrame {
        let (y, u, v) = self.planes();
        let mut rgba = vec![255u8; self.width * self.height * 4];
        for row in 0..self.height {
            for col in 0..self.width {
                let c = (row / 2) * (self.width / 2) + col / 2;
                let (yy, uu, vv) = (
                    y[row * self.width + col] as f32 - 16.0,
                    u[c] as f32 - 128.0,
                    v[c] as f32 - 128.0,
                );
                let at = (row * self.width + col) * 4;
                rgba[at] = (1.164 * yy + 1.596 * vv).clamp(0.0, 255.0) as u8;
                rgba[at + 1] = (1.164 * yy - 0.392 * uu - 0.813 * vv).clamp(0.0, 255.0) as u8;
                rgba[at + 2] = (1.164 * yy + 2.017 * uu).clamp(0.0, 255.0) as u8;
            }
        }
        VideoFrame {
            width: self.width,
            height: self.height,
            rgba,
        }
    }
}

fn scale_plane(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], dw: usize, dh: usize) {
    for y in 0..dh {
        let from = (y * sh / dh.max(1)).min(sh.saturating_sub(1)) * sw;
        for x in 0..dw {
            dst[y * dw + x] = src[from + (x * sw / dw.max(1)).min(sw.saturating_sub(1))];
        }
    }
}

/// A picture ready to show: RGBA, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_fit_and_scale() {
        assert_eq!(Picture::fitted_size(640, 480), (480, 360));
        assert_eq!(Picture::fitted_size(1920, 1080), (640, 360));
        assert_eq!(
            Picture::fitted_size(320, 241),
            (320, 240),
            "small stays, even"
        );
        let mut big = Picture::new(64, 32);
        big.planes_mut().0.fill(200);
        let small = big.scaled(16, 8);
        assert_eq!(small.data.len(), 16 * 8 * 3 / 2);
        assert!(small.planes().0.iter().all(|&y| y == 200));
    }

    #[test]
    fn grey_stays_grey_in_rgba() {
        let frame = Picture::new(4, 4).to_rgba();
        assert_eq!(frame.rgba.len(), 64);
        let px = &frame.rgba[..4];
        assert!(
            px[0].abs_diff(px[1]) < 3 && px[1].abs_diff(px[2]) < 3,
            "{px:?}"
        );
    }
}
