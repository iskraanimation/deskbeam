//! The camera, through `nokhwa` (AVFoundation, Media Foundation, V4L2). Asks for the closest
//! to 640×480 at 30 fps: asking for the highest frame rate picks 1080p on a Mac, far too
//! much to convert and encode.

use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::Context;
use nokhwa::{
    Camera,
    pixel_format::RgbFormat,
    utils::{
        ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
    },
};

use super::{Picture, source::VideoSource};

/// How long the system's "allow the camera?" question may wait for an answer.
const ASK_FOR: Duration = Duration::from_secs(60);

/// The cameras there are, by name.
pub fn camera_names() -> Vec<String> {
    nokhwa::query(ApiBackend::Auto)
        .map(|cameras| cameras.iter().map(|c| c.human_name()).collect())
        .unwrap_or_default()
}

/// A camera by name; `None` is the first. `size` is what to ask it for (the closest it has
/// at 30 fps is used).
#[derive(Debug, Clone)]
pub struct Webcam {
    pub name: Option<String>,
    pub size: (u32, u32),
}

impl Webcam {
    /// Standard: 640×480, sent at 480×360 or up to 640×360.
    pub const STANDARD: (u32, u32) = (640, 480);
    /// HD: 1280×720.
    pub const HD: (u32, u32) = (1280, 720);
}

impl Default for Webcam {
    fn default() -> Self {
        Self {
            name: None,
            size: Self::STANDARD,
        }
    }
}

impl VideoSource for Webcam {
    fn start(
        &self,
        mut deliver: Box<dyn FnMut(Picture) + Send>,
    ) -> anyhow::Result<Box<dyn Any + Send + Sync>> {
        allowed()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (flag, name, size) = (stop.clone(), self.name.clone(), self.size);
        // The camera stays on the thread that opened it.
        std::thread::Builder::new()
            .name("camera".into())
            .spawn(move || {
                let mut camera = match open(name.as_deref(), size) {
                    Ok(camera) => {
                        let _ = ready_tx.send(Ok(()));
                        camera
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                while !flag.load(Ordering::SeqCst) {
                    match camera.frame() {
                        Ok(buffer) => {
                            if let Some(picture) = to_picture(&buffer) {
                                deliver(picture);
                            }
                        }
                        Err(e) => {
                            tracing::debug!("camera: {e}");
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
                let _ = camera.stop_stream();
            })?;
        ready_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| anyhow::anyhow!("the camera did not open in time"))?
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(Box::new(StopOnDrop(stop)))
    }
}

/// On macOS the system asks once whether DeskBeam may use the camera.
fn allowed() -> anyhow::Result<()> {
    if nokhwa::nokhwa_check() {
        return Ok(());
    }
    let (tx, rx) = mpsc::sync_channel(1);
    nokhwa::nokhwa_initialize(move |ok| {
        let _ = tx.try_send(ok);
    });
    match rx.recv_timeout(ASK_FOR) {
        Ok(true) => Ok(()),
        Ok(false) => anyhow::bail!(
            "DeskBeam may not use the camera: allow it in System Settings → Privacy & Security \
             → Camera"
        ),
        Err(_) => anyhow::bail!("no answer about the camera"),
    }
}

fn open(name: Option<&str>, size: (u32, u32)) -> anyhow::Result<Camera> {
    let cameras = nokhwa::query(ApiBackend::Auto).context("looking for cameras")?;
    let index = match name {
        Some(name) => cameras
            .iter()
            .find(|c| c.human_name() == name)
            .or(cameras.first())
            .map(|c| c.index().clone()),
        None => cameras.first().map(|c| c.index().clone()),
    }
    .ok_or_else(|| anyhow::anyhow!("there is no camera"))?;
    let wanted = CameraFormat::new(Resolution::new(size.0, size.1), FrameFormat::YUYV, 30);
    let mut camera = Camera::new(
        index.clone(),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(wanted)),
    )
    .or_else(|_| {
        Camera::new(
            index,
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::None),
        )
    })
    .context("opening the camera")?;
    camera.open_stream().context("starting the camera")?;
    Ok(camera)
}

/// The camera's buffer as I420, at its size rounded down to even numbers.
fn to_picture(buffer: &nokhwa::Buffer) -> Option<Picture> {
    let size = buffer.resolution();
    let (w, h) = (size.width_x as usize, size.height_y as usize);
    let data = buffer.buffer();
    let mut picture = Picture::new(w, h);
    let (pw, ph) = (picture.width, picture.height);
    match buffer.source_frame_format() {
        FrameFormat::YUYV if data.len() >= w * h * 2 => {
            let (y, u, v) = picture.planes_mut();
            for row in 0..ph {
                for col in 0..pw {
                    y[row * pw + col] = data[(row * w + col) * 2];
                }
            }
            for row in (0..ph).step_by(2) {
                for col in (0..pw).step_by(2) {
                    let at = (row * w + col) * 2;
                    let c = (row / 2) * (pw / 2) + col / 2;
                    u[c] = data[at + 1];
                    v[c] = data[at + 3];
                }
            }
        }
        FrameFormat::NV12 if data.len() >= w * h * 3 / 2 => {
            let (y, u, v) = picture.planes_mut();
            for row in 0..ph {
                y[row * pw..(row + 1) * pw].copy_from_slice(&data[row * w..row * w + pw]);
            }
            let chroma = &data[w * h..];
            for row in 0..ph / 2 {
                for col in 0..pw / 2 {
                    let at = row * w + col * 2;
                    u[row * (pw / 2) + col] = chroma[at];
                    v[row * (pw / 2) + col] = chroma[at + 1];
                }
            }
        }
        FrameFormat::GRAY if data.len() >= w * h => {
            let (y, _, _) = picture.planes_mut();
            for row in 0..ph {
                y[row * pw..(row + 1) * pw].copy_from_slice(&data[row * w..row * w + pw]);
            }
        }
        // MJPEG and the RGB forms: let nokhwa make RGB, then convert.
        _ => {
            let rgb = buffer.decode_image::<RgbFormat>().ok()?;
            rgb_into(&mut picture, rgb.as_raw(), w);
        }
    }
    Some(picture)
}

/// RGB (BT.601, limited range) into the picture's planes.
fn rgb_into(picture: &mut Picture, rgb: &[u8], stride_px: usize) {
    let pw = picture.width;
    let ph = picture.height;
    let (y, u, v) = picture.planes_mut();
    let px = |row: usize, col: usize| {
        let at = (row * stride_px + col) * 3;
        (rgb[at] as f32, rgb[at + 1] as f32, rgb[at + 2] as f32)
    };
    for row in 0..ph {
        for col in 0..pw {
            let (r, g, b) = px(row, col);
            y[row * pw + col] = (16.0 + 0.257 * r + 0.504 * g + 0.098 * b) as u8;
        }
    }
    for row in (0..ph).step_by(2) {
        for col in (0..pw).step_by(2) {
            let (r, g, b) = px(row, col);
            let c = (row / 2) * (pw / 2) + col / 2;
            u[c] = (128.0 - 0.148 * r - 0.291 * g + 0.439 * b) as u8;
            v[c] = (128.0 + 0.439 * r - 0.368 * g - 0.071 * b) as u8;
        }
    }
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_formats_become_i420() {
        // YUYV: Y0 U Y1 V, a 4×2 picture.
        let yuyv: Vec<u8> = [10, 100, 20, 200].repeat(4);
        let buffer = nokhwa::Buffer::new(Resolution::new(4, 2), &yuyv, FrameFormat::YUYV);
        let picture = to_picture(&buffer).unwrap();
        let (y, u, v) = picture.planes();
        assert_eq!(y, [10, 20, 10, 20, 10, 20, 10, 20]);
        assert_eq!((u, v), (&[100, 100][..], &[200, 200][..]));

        // NV12: Y plane, then U V interleaved.
        let mut nv12 = vec![50u8; 8];
        nv12.extend([90, 160, 91, 161]);
        let buffer = nokhwa::Buffer::new(Resolution::new(4, 2), &nv12, FrameFormat::NV12);
        let picture = to_picture(&buffer).unwrap();
        let (_, u, v) = picture.planes();
        assert_eq!((u, v), (&[90, 91][..], &[160, 161][..]));

        // White RGB is bright luma, neutral chroma.
        let mut white = Picture::new(2, 2);
        rgb_into(&mut white, &[255u8; 12], 2);
        let (y, u, v) = white.planes();
        assert!(y.iter().all(|&l| l >= 234), "{y:?}");
        assert!(u[0].abs_diff(128) < 3 && v[0].abs_diff(128) < 3);
    }
}
