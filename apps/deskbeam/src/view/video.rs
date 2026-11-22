//! The call's video, in a window of its own: their camera filling it, this device's own
//! small in a corner (mirrored, as people expect to see themselves), and the call's
//! controls. It opens when either camera goes on; closing it hides the video only, and the
//! call bar offers "Show video" again.

use std::time::Duration;

use deskbeam_engine::{
    Command,
    calls::video::VideoFrame,
    model::{Call, CallState, Snapshot},
};
use eframe::egui::{self, Color32, Pos2, Rect, RichText, TextureHandle, Vec2};

use super::{
    DeskBeamApp,
    call::red_button,
    style::{self, Icon, palette},
};

/// About 30 repaints a second while video shows.
const REPAINT: Duration = Duration::from_millis(33);
/// The own picture's width in the corner.
const OWN_WIDTH: f32 = 168.0;

/// What the window keeps between repaints.
#[derive(Default)]
pub(super) struct VideoView {
    /// The video window is open.
    pub(super) open: bool,
    /// Either camera was on last time: the window opens when one goes on.
    was_on: bool,
    theirs: Option<(u64, TextureHandle)>,
    mine: Option<(u64, TextureHandle)>,
    /// A shared screen at its own pixels, scrolled, instead of fitted.
    actual_size: bool,
}

impl DeskBeamApp {
    /// The camera on/off button in the call bar, when both ends can do video.
    pub(super) fn camera_button(&self, ui: &mut egui::Ui, call: &Call) {
        if !call.video {
            return;
        }
        if call.camera_opening {
            ui.spinner().on_hover_text("Opening the camera…");
            return;
        }
        let (icon, tip) = if call.camera {
            (Icon::Video, "Turn the camera off")
        } else {
            (Icon::VideoOff, "Turn the camera on")
        };
        if style::icon_button(ui, icon, 22.0, tip).clicked() {
            self.dispatch(Command::CallCamera { on: !call.camera });
        }
    }

    pub(super) fn video_window(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some(call) = snap.call.as_ref().filter(|c| c.state == CallState::Active) else {
            self.video = VideoView::default();
            return;
        };
        let on = call.camera || call.peer_camera;
        if on && !self.video.was_on {
            self.video.open = true;
        }
        self.video.was_on = on;
        if !self.video.open {
            return;
        }
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("Call with {}", call.display_name))
            .with_inner_size([720.0, 480.0])
            .with_min_inner_size([360.0, 260.0]);
        let call = call.clone();
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("deskbeam-video"),
            builder,
            |ui, _class| {
                if ui.ctx().input(|i| i.viewport().close_requested()) {
                    self.video.open = false;
                    return;
                }
                ui.ctx().request_repaint_after(REPAINT);
                self.video_textures(ui.ctx());
                egui::Panel::bottom("video-controls")
                    .frame(
                        egui::Frame::new()
                            .fill(palette(ui).card)
                            .inner_margin(egui::Margin::symmetric(16, 8)),
                    )
                    .show(ui, |ui| self.video_controls(ui, &call));
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(Color32::BLACK))
                    .show(ui, |ui| self.video_pictures(ui, &call));
            },
        );
    }

    /// Copies new pictures from the engine into textures.
    fn video_textures(&mut self, ctx: &egui::Context) {
        let frames = self.engine.call_video();
        update(ctx, &mut self.video.theirs, frames.theirs(), "their-video");
        update(ctx, &mut self.video.mine, frames.mine(), "own-video");
    }

    fn video_pictures(&self, ui: &mut egui::Ui, call: &Call) {
        let area = ui.available_rect_before_wrap();
        match &self.video.theirs {
            // A shared screen at its own pixels, to read small text.
            Some((_, texture))
                if call.peer_camera && call.peer_sharing && self.video.actual_size =>
            {
                let size = texture.size_vec2() / ui.ctx().pixels_per_point();
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add(egui::Image::new(texture).fit_to_exact_size(size));
                    });
            }
            Some((_, texture)) if call.peer_camera => {
                let shown = fit(texture.size_vec2(), area);
                ui.painter().image(
                    texture.id(),
                    shown,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            _ => {
                let text = if call.peer_camera {
                    format!("Waiting for {}'s picture…", call.display_name)
                } else {
                    format!("{}'s camera is off", call.display_name)
                };
                let text = if call.peer_sharing && call.peer_camera {
                    format!("Waiting for {}'s screen…", call.display_name)
                } else {
                    text
                };
                ui.painter().text(
                    area.center(),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::FontId::proportional(18.0),
                    Color32::from_gray(170),
                );
            }
        }
        if let Some((_, texture)) = &self.video.mine
            && (call.camera || call.sharing.is_some())
        {
            let size = texture.size_vec2();
            let own = Vec2::new(OWN_WIDTH, OWN_WIDTH * size.y / size.x.max(1.0));
            let rect = Rect::from_min_size(area.right_bottom() - own - Vec2::splat(12.0), own);
            // The camera mirrored, like a mirror; a shared screen as it is.
            let uv = if call.sharing.is_some() {
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0))
            } else {
                Rect::from_min_max(Pos2::new(1.0, 0.0), Pos2::new(0.0, 1.0))
            };
            ui.painter().image(texture.id(), rect, uv, Color32::WHITE);
            ui.painter().rect_stroke(
                rect,
                4.0,
                egui::Stroke::new(1.0, Color32::from_gray(90)),
                egui::StrokeKind::Outside,
            );
        }
    }

    fn video_controls(&mut self, ui: &mut egui::Ui, call: &Call) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&call.display_name).size(15.0));
            if call.peer_sharing {
                ui.label(RichText::new("is sharing their screen").color(palette(ui).weak));
                let fit = if self.video.actual_size {
                    "Fit to window"
                } else {
                    "Actual size"
                };
                if ui.button(fit).clicked() {
                    self.video.actual_size = !self.video.actual_size;
                }
            }
            let full = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
            if ui
                .button(if full {
                    "Exit full screen"
                } else {
                    "Full screen"
                })
                .clicked()
            {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
            }
            if let Some(error) = &call.camera_error {
                ui.label(RichText::new(error).color(style::ERROR));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if red_button(ui, "Hang up").clicked() {
                    self.dispatch(Command::HangUp);
                }
                if call.sharing.is_some() && ui.button("Stop sharing").clicked() {
                    self.dispatch(Command::CallStopSharing);
                }
                self.camera_button(ui, call);
                let (icon, tip) = if call.muted {
                    (Icon::MicOff, "Unmute")
                } else {
                    (Icon::Mic, "Mute")
                };
                if style::icon_button(ui, icon, 22.0, tip).clicked() {
                    self.dispatch(Command::MuteCall { on: !call.muted });
                }
            });
        });
    }
}

/// Puts a new picture into the texture, when there is one.
fn update(
    ctx: &egui::Context,
    slot: &mut Option<(u64, TextureHandle)>,
    latest: Option<(u64, std::sync::Arc<VideoFrame>)>,
    name: &str,
) {
    let Some((n, frame)) = latest else {
        *slot = None;
        return;
    };
    if slot.as_ref().is_some_and(|(shown, _)| *shown == n) {
        return;
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
    match slot {
        Some((shown, texture)) => {
            texture.set(image, egui::TextureOptions::LINEAR);
            *shown = n;
        }
        None => {
            *slot = Some((
                n,
                ctx.load_texture(name, image, egui::TextureOptions::LINEAR),
            ))
        }
    }
}

/// The largest rectangle of `size`'s shape inside `area`, centred.
fn fit(size: Vec2, area: Rect) -> Rect {
    let scale = (area.width() / size.x.max(1.0)).min(area.height() / size.y.max(1.0));
    Rect::from_center_size(area.center(), size * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_fit_keeping_their_shape() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 400.0));
        let shown = fit(Vec2::new(640.0, 360.0), area);
        assert!((shown.height() - 400.0).abs() < 0.01);
        assert!((shown.width() - 400.0 * 640.0 / 360.0).abs() < 0.01);
        assert!((shown.center().x - 400.0).abs() < 0.01);
    }
}
