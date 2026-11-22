//! A shared screen as a video source (plan 6d): a whole display or one window, captured
//! through `xcap` (CoreGraphics on macOS, GDI / Windows Graphics Capture on Windows, X11 or
//! PipeWire on Linux) at the screen profile's rate, and converted straight to I420 at the
//! size it is sent at, so only the pixels sent are touched.
//!
//! On macOS capture needs the Screen Recording permission. Without it capture does not fail:
//! it gives the wallpaper and the menu bar only. So [`may_capture`] asks the system first.

use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use super::{Picture, Profile, VideoFrame, source::VideoSource};

/// What can be shared.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ShareSource {
    Display { id: u32 },
    Window { id: u32 },
}

/// A source to pick from, with a small picture of it.
#[derive(Debug, Clone)]
pub struct ShareChoice {
    pub source: ShareSource,
    /// "Built-in Retina Display", or "Safari: DeskBeam on GitHub".
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub thumbnail: Option<VideoFrame>,
}

/// Windows smaller than this are toolbars, menus and the like.
const LEAST_WINDOW: u32 = 120;
/// At most this many windows are offered (each needs a capture for its picture).
const MOST_WINDOWS: usize = 16;
/// Capture failing this many times in a row ends the share (the window closed, say).
const MOST_FAILURES: u32 = 20;

/// Whether this app may capture the screen. Only macOS asks; elsewhere it is always yes.
pub fn may_capture() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2_core_graphics::CGPreflightScreenCaptureAccess()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Asks for the permission: on macOS the system's question the first time, and DeskBeam's
/// entry in System Settings after that. It takes effect once DeskBeam is restarted.
pub fn ask_to_capture() {
    #[cfg(target_os = "macos")]
    {
        let _ = objc2_core_graphics::CGRequestScreenCaptureAccess();
    }
}

/// The displays, then the windows of other apps, with pictures `thumb_width` wide.
pub fn share_sources(thumb_width: usize) -> Vec<ShareChoice> {
    let mut choices = Vec::new();
    for monitor in xcap::Monitor::all().unwrap_or_default() {
        let Ok(id) = monitor.id() else { continue };
        let name = monitor
            .friendly_name()
            .or_else(|_| monitor.name())
            .unwrap_or_else(|_| format!("Display {id}"));
        let thumbnail = monitor
            .capture_image()
            .ok()
            .map(|image| thumbnail(&image, thumb_width));
        choices.push(ShareChoice {
            source: ShareSource::Display { id },
            name,
            width: monitor.width().unwrap_or(0),
            height: monitor.height().unwrap_or(0),
            thumbnail,
        });
    }
    let me = std::process::id();
    let windows = xcap::Window::all()
        .unwrap_or_default()
        .into_iter()
        .filter(|w| {
            w.pid().is_ok_and(|pid| pid != me)
                && !w.is_minimized().unwrap_or(true)
                && w.width().unwrap_or(0) >= LEAST_WINDOW
                && w.height().unwrap_or(0) >= LEAST_WINDOW
                && w.title().is_ok_and(|t| !t.trim().is_empty())
        });
    for window in windows.take(MOST_WINDOWS) {
        let Ok(id) = window.id() else { continue };
        let title = window.title().unwrap_or_default();
        let name = match window.app_name() {
            Ok(app) if !app.is_empty() && app != title => format!("{app}: {title}"),
            _ => title,
        };
        let thumbnail = window
            .capture_image()
            .ok()
            .map(|image| thumbnail(&image, thumb_width));
        choices.push(ShareChoice {
            source: ShareSource::Window { id },
            name,
            width: window.width().unwrap_or(0),
            height: window.height().unwrap_or(0),
            thumbnail,
        });
    }
    choices
}

/// A display or window, captured at `profile`'s rate and size.
#[derive(Debug, Clone)]
pub struct Screen {
    pub source: ShareSource,
    pub profile: Profile,
}

enum Target {
    Display(xcap::Monitor),
    Window(xcap::Window),
}

impl Target {
    fn find(source: &ShareSource) -> anyhow::Result<Self> {
        match source {
            ShareSource::Display { id } => xcap::Monitor::all()
                .context("listing displays")?
                .into_iter()
                .find(|m| m.id().is_ok_and(|m| m == *id))
                .map(Target::Display)
                .ok_or_else(|| anyhow::anyhow!("that display is gone")),
            ShareSource::Window { id } => xcap::Window::all()
                .context("listing windows")?
                .into_iter()
                .find(|w| w.id().is_ok_and(|w| w == *id))
                .map(Target::Window)
                .ok_or_else(|| anyhow::anyhow!("that window is closed")),
        }
    }

    fn capture(&self) -> anyhow::Result<xcap::image::RgbaImage> {
        Ok(match self {
            Target::Display(m) => m.capture_image()?,
            Target::Window(w) => w.capture_image()?,
        })
    }
}

impl VideoSource for Screen {
    fn start(
        &self,
        mut deliver: Box<dyn FnMut(Picture) + Send>,
    ) -> anyhow::Result<Box<dyn Any + Send + Sync>> {
        anyhow::ensure!(
            may_capture(),
            "DeskBeam may not record the screen: allow it in System Settings → Privacy & \
             Security → Screen Recording, then quit and reopen DeskBeam"
        );
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (flag, me) = (stop.clone(), self.clone());
        std::thread::Builder::new()
            .name("screen".into())
            .spawn(move || {
                let target = match Target::find(&me.source).and_then(|t| {
                    t.capture()?;
                    Ok(t)
                }) {
                    Ok(target) => {
                        let _ = ready_tx.send(Ok(()));
                        target
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                let every = Duration::from_secs_f32(1.0 / me.profile.fps.max(1.0));
                let mut failures = 0;
                while !flag.load(Ordering::SeqCst) {
                    let started = Instant::now();
                    match target.capture() {
                        Ok(image) => {
                            failures = 0;
                            let (w, h) = me
                                .profile
                                .fit(image.width() as usize, image.height() as usize);
                            deliver(to_picture(
                                image.as_raw(),
                                image.width() as usize,
                                image.height() as usize,
                                w,
                                h,
                            ));
                        }
                        Err(e) => {
                            failures += 1;
                            tracing::debug!("screen: {e}");
                            if failures >= MOST_FAILURES {
                                return;
                            }
                        }
                    }
                    std::thread::sleep(every.saturating_sub(started.elapsed()));
                }
            })?;
        ready_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| anyhow::anyhow!("the screen capture did not start in time"))?
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(Box::new(StopOnDrop(stop)))
    }
}

/// RGBA `sw` × `sh` to I420 `dw` × `dh` (nearest sample; BT.601 limited range).
pub fn to_picture(rgba: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Picture {
    let mut picture = Picture::new(dw, dh);
    let (dw, dh) = (picture.width, picture.height);
    let (y, u, v) = picture.planes_mut();
    let at = |x: usize, row: usize| {
        let sx = (x * sw / dw.max(1)).min(sw.saturating_sub(1));
        let sy = (row * sh / dh.max(1)).min(sh.saturating_sub(1));
        let i = (sy * sw + sx) * 4;
        (rgba[i] as f32, rgba[i + 1] as f32, rgba[i + 2] as f32)
    };
    for row in 0..dh {
        for x in 0..dw {
            let (r, g, b) = at(x, row);
            y[row * dw + x] = (16.0 + 0.257 * r + 0.504 * g + 0.098 * b) as u8;
        }
    }
    for row in (0..dh).step_by(2) {
        for x in (0..dw).step_by(2) {
            let (r, g, b) = at(x, row);
            let c = (row / 2) * (dw / 2) + x / 2;
            u[c] = (128.0 - 0.148 * r - 0.291 * g + 0.439 * b) as u8;
            v[c] = (128.0 + 0.439 * r - 0.368 * g - 0.071 * b) as u8;
        }
    }
    picture
}

/// A small RGBA copy for the picker.
fn thumbnail(image: &xcap::image::RgbaImage, width: usize) -> VideoFrame {
    let (sw, sh) = (image.width() as usize, image.height() as usize);
    let width = width.min(sw).max(1);
    let height = (sh * width / sw.max(1)).max(1);
    let raw = image.as_raw();
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let sy = row * sh / height;
        for x in 0..width {
            let i = (sy * sw + x * sw / width) * 4;
            rgba.extend_from_slice(&raw[i..i + 3]);
            rgba.push(255);
        }
    }
    VideoFrame {
        width,
        height,
        rgba,
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
    fn screen_pixels_become_i420_at_the_sent_size() {
        // 8×4: the left half white, the right half black.
        let mut rgba = Vec::new();
        for _row in 0..4 {
            for x in 0..8 {
                let c = if x < 4 { 255 } else { 0 };
                rgba.extend_from_slice(&[c, c, c, 255]);
            }
        }
        let picture = to_picture(&rgba, 8, 4, 4, 2);
        assert_eq!((picture.width, picture.height), (4, 2));
        let (y, u, v) = picture.planes();
        assert!(y[0] >= 234 && y[1] >= 234, "{y:?}");
        assert!(y[2] <= 17 && y[3] <= 17, "{y:?}");
        assert!(u.iter().chain(v).all(|c| c.abs_diff(128) < 3));
    }

    #[test]
    fn sources_name_their_kind() {
        let json = serde_json::to_string(&ShareSource::Window { id: 7 }).unwrap();
        assert_eq!(json, r#"{"kind":"window","id":7}"#);
    }

    #[test]
    fn the_screen_profile_keeps_text_sharp() {
        // A Retina display's native pixels fit within 1920×1080 at the same shape.
        assert_eq!(Profile::SCREEN.fit(4112, 2658), (1670, 1080));
        assert_eq!(Profile::SCREEN.fit(1280, 720), (1280, 720));
    }
}
