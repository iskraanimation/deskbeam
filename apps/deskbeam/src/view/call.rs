//! Calls in the window: the bar across the top while a call rings or runs, the dialog for a
//! call coming in, and the ring that goes with it. The phone that starts a call is on each
//! paired device's row in the chat list (`chats.rs`).

use std::time::Duration;

use deskbeam_engine::{
    Command,
    model::{Call, CallState, Snapshot},
};
use eframe::egui::{self, Color32, RichText, Vec2};

use super::{
    DeskBeamApp,
    style::{self, ERROR, Icon, ONLINE, WARN, palette, strong},
};

/// Above this share of lost audio, the call is shown as poor.
const POOR: f32 = 0.08;

impl DeskBeamApp {
    pub(super) fn start_call(&self, endpoint_id: &str) {
        self.dispatch(Command::StartCall {
            endpoint_id: endpoint_id.to_string(),
        });
    }

    /// Rings while a call comes in, and stops when it is answered or gone.
    pub(super) fn ring(&mut self, snap: &Snapshot) {
        let ringing = snap
            .call
            .as_ref()
            .is_some_and(|c| c.state == CallState::Ringing);
        if ringing != self.ringing {
            if ringing {
                self.sounds.start_ringing();
            } else {
                self.sounds.stop_ringing();
            }
            self.ringing = ringing;
        }
    }

    /// The bar under the header while there is a call.
    pub(super) fn call_bar(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let Some(call) = &snap.call else { return };
        let pal = palette(ui);
        egui::Panel::top("call-bar")
            .frame(
                egui::Frame::new()
                    .fill(pal.card)
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (color, text) = describe(call);
                    style::dot(ui, color, 5.0);
                    ui.label(strong(&call.display_name, 15.0));
                    ui.label(RichText::new(text).color(pal.weak));
                    if call.state == CallState::Active {
                        ui.ctx().request_repaint_after(Duration::from_millis(250));
                        if call.peer_muted {
                            ui.label(RichText::new("· muted their microphone").color(pal.weak));
                        }
                        if let Some(error) = &call.camera_error {
                            ui.label(RichText::new(format!("· {error}")).color(ERROR));
                        }
                        if let Some(error) = &call.share_error {
                            ui.label(RichText::new(format!("· {error}")).color(ERROR));
                        }
                        if call.peer_sharing {
                            ui.label(RichText::new("· sharing their screen").color(pal.weak));
                        }
                        let showing = call.camera || call.peer_camera;
                        if showing && !self.video.open && ui.link("Show video").clicked() {
                            self.video.open = true;
                        }
                        if call.loss > POOR {
                            ui.label(RichText::new("· poor connection").color(WARN))
                                .on_hover_text(format!(
                                    "{:.0}% of their audio is lost or late",
                                    call.loss * 100.0
                                ));
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !call.is_live() {
                            return;
                        }
                        let hang_up = match call.state {
                            CallState::Calling => "Cancel",
                            CallState::Ringing => "Decline",
                            _ => "Hang up",
                        };
                        if red_button(ui, hang_up).clicked() {
                            self.dispatch(if call.state == CallState::Ringing {
                                Command::AnswerCall { accept: false }
                            } else {
                                Command::HangUp
                            });
                        }
                        if call.state == CallState::Ringing
                            && style::primary(ui, "Answer", true).clicked()
                        {
                            self.dispatch(Command::AnswerCall { accept: true });
                        }
                        if call.state == CallState::Active {
                            let (icon, tip) = if call.muted {
                                (Icon::MicOff, "Unmute")
                            } else {
                                (Icon::Mic, "Mute")
                            };
                            if style::icon_button(ui, icon, 22.0, tip).clicked() {
                                self.dispatch(Command::MuteCall { on: !call.muted });
                            }
                            self.share_button(ui, call);
                            self.camera_button(ui, call);
                            level_meter(ui, if call.muted { 0.0 } else { call.level });
                        }
                    });
                });
                if call.sharing.is_some() {
                    ui.add_space(6.0);
                    self.sharing_bar(ui, call);
                }
            });
    }

    /// The dialog for a call coming in. Returns whether it is up.
    pub(super) fn incoming_call(&mut self, ctx: &egui::Context, snap: &Snapshot) -> bool {
        let Some(call) = snap.call.as_ref().filter(|c| c.state == CallState::Ringing) else {
            return false;
        };
        let mut answer = None;
        style::modal(
            ctx,
            "incoming-call",
            &format!("{} is calling", call.display_name),
            "A voice call from your paired device.",
            |ui| {
                ui.horizontal(|ui| {
                    style::icon(ui, Icon::Phone, 28.0, ONLINE);
                    ui.label(RichText::new("Ringing…").color(palette(ui).weak));
                });
            },
            |ui| {
                if style::primary(ui, "Answer", true).clicked() {
                    answer = Some(true);
                }
                if red_button(ui, "Decline").clicked() {
                    answer = Some(false);
                }
            },
        );
        if let Some(accept) = answer {
            self.dispatch(Command::AnswerCall { accept });
        }
        true
    }
}

/// The colour and words for where a call is.
fn describe(call: &Call) -> (Color32, String) {
    match &call.state {
        CallState::Calling => (WARN, "Calling…".into()),
        CallState::Ringing => (WARN, "is calling".into()),
        CallState::Active => {
            let seconds = call
                .started_at
                .map(|at| now_ms().saturating_sub(at) / 1000)
                .unwrap_or(0);
            (ONLINE, format!("{:02}:{:02}", seconds / 60, seconds % 60))
        }
        CallState::Ended { reason } => (Color32::GRAY, reason.clone()),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A red button with the hang-up phone.
pub(super) fn red_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let image = egui::Image::new(Icon::PhoneOff.source())
        .fit_to_exact_size(Vec2::splat(16.0))
        .tint(Color32::WHITE);
    ui.add(
        egui::Button::new((RichText::new(text).color(Color32::WHITE).size(15.0), image))
            .fill(ERROR)
            .corner_radius(10)
            .min_size(Vec2::new(0.0, 36.0)),
    )
}

/// A small bar showing how loud the microphone is.
pub(super) fn level_meter(ui: &mut egui::Ui, level: f32) {
    let pal = palette(ui);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(48.0, 6.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 3.0, pal.line);
    // In decibels, as loudness is heard: -60 dBFS (a quiet room) is empty, full scale full.
    let fill = if level > 0.0 {
        ((20.0 * level.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut lit = rect;
    lit.set_width(rect.width() * fill);
    ui.painter().rect_filled(lit, 3.0, ONLINE);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(state: CallState) -> Call {
        Call {
            id: "c".into(),
            endpoint_id: "e".into(),
            display_name: "Mac".into(),
            outgoing: true,
            state,
            muted: false,
            peer_muted: false,
            started_at: Some(now_ms() - 125_000),
            loss: 0.0,
            level: 0.0,
            video: false,
            camera: false,
            camera_opening: false,
            peer_camera: false,
            camera_error: None,
            sharing: None,
            share_opening: false,
            peer_sharing: false,
            share_error: None,
        }
    }

    #[test]
    fn calls_read_well() {
        assert_eq!(describe(&call(CallState::Calling)).1, "Calling…");
        assert_eq!(describe(&call(CallState::Active)).1, "02:05");
        let ended = CallState::Ended {
            reason: "Mac hung up".into(),
        };
        assert_eq!(describe(&call(ended)).1, "Mac hung up");
    }
}
