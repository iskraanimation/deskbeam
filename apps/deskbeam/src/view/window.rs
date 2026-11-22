//! The windows open where they were last: the main window's and the Settings window's place,
//! size and maximized state are kept in the engine's preferences (so in `deskbeam-store`),
//! saved shortly after the user stops moving or resizing one, and once more on closing the
//! Settings window or quitting.

use std::time::{Duration, Instant};

use deskbeam_engine::{Command, model::WindowPlace};
use eframe::egui;

use super::DeskBeamApp;

/// How long the window has to stay put before its place is saved.
const SETTLE: Duration = Duration::from_millis(800);

/// Opens the window at `place`, if one was saved.
pub fn placed(builder: egui::ViewportBuilder, place: Option<WindowPlace>) -> egui::ViewportBuilder {
    let Some(place) = place else {
        return builder;
    };
    let mut builder = builder
        .with_inner_size([place.width as f32, place.height as f32])
        .with_maximized(place.maximized);
    if let (Some(x), Some(y)) = (place.x, place.y) {
        builder = builder.with_position([x as f32, y as f32]);
    }
    builder
}

/// What the window tracker remembers between frames.
#[derive(Default)]
pub(super) struct Tracker {
    /// The place last saved, or loaded at start.
    saved: Option<WindowPlace>,
    /// A changed place, and since when it has stayed so.
    pending: Option<(WindowPlace, Instant)>,
    /// The monitor has been compared with the saved one.
    checked_monitor: bool,
}

impl Tracker {
    pub(super) fn new(saved: Option<WindowPlace>) -> Self {
        Self {
            saved,
            ..Self::default()
        }
    }
}

/// Which window a place belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Which {
    Main,
    Settings,
}

impl DeskBeamApp {
    /// Notices the main window moving or resizing, and saves its place once it settles.
    pub(super) fn track_window(&mut self, ctx: &egui::Context) {
        if let Some(place) = self.window.observe(ctx) {
            self.dispatch(window_command(Which::Main, place));
        }
    }

    /// The same for the Settings window; `ctx` is in its viewport.
    pub(super) fn track_settings_window(&mut self, ctx: &egui::Context) {
        if let Some(place) = self.settings_place.observe(ctx) {
            self.dispatch(window_command(Which::Settings, place));
        }
    }

    /// The Settings window is closing: keep where it was, even if it moved just now.
    pub(super) fn settings_window_closed(&mut self) {
        if let Some(place) = self.settings_place.take_pending() {
            self.dispatch(window_command(Which::Settings, place));
        }
        // Checked again next time it opens, maybe on other monitors.
        self.settings_place.checked_monitor = false;
    }

    /// Saves places not yet saved, before the app quits.
    pub(super) fn save_window_now(&mut self) {
        let pending = [
            (Which::Main, self.window.take_pending()),
            (Which::Settings, self.settings_place.take_pending()),
        ];
        for (which, place) in pending {
            let Some(place) = place else { continue };
            let engine = self.engine.clone();
            if let Err(e) = self
                .runtime
                .block_on(engine.execute(window_command(which, place)))
            {
                tracing::debug!("window place not saved: {e}");
            }
        }
    }
}

impl Tracker {
    /// The place last saved, to open the window there.
    pub(super) fn saved(&self) -> Option<WindowPlace> {
        self.saved
    }

    /// Watches the window `ctx` belongs to; returns its place once it has settled somewhere
    /// new, to save.
    fn observe(&mut self, ctx: &egui::Context) -> Option<WindowPlace> {
        let info = ctx.input(|i| i.viewport().clone());
        self.check_monitor(ctx, &info);
        let place = self.place_now(&info)?;
        if self.saved == Some(place) {
            self.pending = None;
            return None;
        }
        let now = Instant::now();
        match self.pending {
            Some((pending, since)) if pending == place => {
                if now.duration_since(since) >= SETTLE {
                    self.saved = Some(place);
                    self.pending = None;
                    return Some(place);
                }
                ctx.request_repaint_after(SETTLE - now.duration_since(since));
            }
            _ => {
                self.pending = Some((place, now));
                ctx.request_repaint_after(SETTLE);
            }
        }
        None
    }

    /// A changed place not saved yet, now marked as saved.
    fn take_pending(&mut self) -> Option<WindowPlace> {
        let (place, _) = self.pending.take()?;
        self.saved = Some(place);
        Some(place)
    }

    /// A window put back on a monitor that is gone (or changed) could open out of sight; if
    /// the monitor is not the one it was saved on, it is moved near the top-left corner.
    fn check_monitor(&mut self, ctx: &egui::Context, info: &egui::ViewportInfo) {
        if self.checked_monitor {
            return;
        }
        let Some(size) = info.monitor_size else {
            return;
        };
        self.checked_monitor = true;
        let now = (size.x.round() as u32, size.y.round() as u32);
        if let Some(saved) = self.saved
            && saved.x.is_some()
            && saved.monitor.is_some_and(|m| m != now)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(40.0, 40.0)));
        }
    }

    /// Where the window is now, or `None` while that should not be saved (minimized, full
    /// screen, not known yet).
    fn place_now(&self, info: &egui::ViewportInfo) -> Option<WindowPlace> {
        if info.minimized == Some(true) || info.fullscreen == Some(true) {
            return None;
        }
        let monitor = info
            .monitor_size
            .map(|s| (s.x.round() as u32, s.y.round() as u32));
        if info.maximized == Some(true) {
            // Keep the size and place to go back to when it is no longer maximized.
            let normal = self
                .pending
                .map(|(p, _)| p)
                .or(self.saved)
                .unwrap_or_default();
            return Some(WindowPlace {
                maximized: true,
                monitor,
                ..normal
            });
        }
        let inner = info.inner_rect?;
        // `with_position` places the frame (tried on macOS: given the content's corner, the
        // window crept down by its title bar on every start).
        let corner = info.outer_rect.map(|r| r.min);
        Some(WindowPlace {
            x: corner.map(|c| c.x.round() as i32),
            y: corner.map(|c| c.y.round() as i32),
            width: inner.width().round().max(1.0) as u32,
            height: inner.height().round().max(1.0) as u32,
            maximized: false,
            monitor,
        })
    }
}

fn window_command(which: Which, place: WindowPlace) -> Command {
    let (main, settings) = match which {
        Which::Main => (Some(place), None),
        Which::Settings => (None, Some(place)),
    };
    Command::SetPreferences {
        close_to_tray: None,
        notifications: None,
        window: main,
        recent_emoji: None,
        auto_fetch_images: None,
        message_sound: None,
        settings_window: settings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(x: f32, y: f32, w: f32, h: f32) -> egui::ViewportInfo {
        let inner = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h));
        egui::ViewportInfo {
            inner_rect: Some(inner),
            outer_rect: Some(inner.expand2(egui::vec2(0.0, 14.0))),
            monitor_size: Some(egui::vec2(1440.0, 900.0)),
            maximized: Some(false),
            minimized: Some(false),
            ..Default::default()
        }
    }

    #[test]
    fn a_maximized_window_keeps_its_normal_size() {
        let mut tracker = Tracker::default();
        let normal = tracker.place_now(&info(100.0, 80.0, 800.0, 620.0)).unwrap();
        assert_eq!((normal.width, normal.height), (800, 620));
        tracker.saved = Some(normal);

        let mut maximized = info(0.0, 0.0, 1440.0, 900.0);
        maximized.maximized = Some(true);
        let place = tracker.place_now(&maximized).unwrap();
        assert!(place.maximized);
        assert_eq!((place.x, place.width, place.height), (normal.x, 800, 620));
    }

    #[test]
    fn minimized_and_unknown_windows_are_not_saved() {
        let tracker = Tracker::default();
        let mut minimized = info(1.0, 2.0, 3.0, 4.0);
        minimized.minimized = Some(true);
        assert_eq!(tracker.place_now(&minimized), None);
        assert_eq!(tracker.place_now(&egui::ViewportInfo::default()), None);
    }
}
