//! Sharing a screen in a call (plan 6d): the button and the red "you are sharing" bar in the
//! call bar, and the picker of displays and windows with their small pictures.

use std::collections::HashMap;

use deskbeam_engine::{
    Command,
    model::{Call, ShareOption, ShareTarget, Snapshot},
};
use eframe::egui::{self, Color32, RichText, TextureHandle, Vec2};

use super::{
    DeskBeamApp,
    style::{self, ERROR, Icon, palette},
};

/// A picture's width in the picker.
const PICTURE: f32 = 172.0;

#[derive(Default)]
pub(super) struct SharePicker {
    pub(super) open: bool,
    pictures: HashMap<ShareTarget, TextureHandle>,
}

impl DeskBeamApp {
    /// The share button in the call bar, when both ends can do video.
    pub(super) fn share_button(&mut self, ui: &mut egui::Ui, call: &Call) {
        if !call.video || call.sharing.is_some() {
            return;
        }
        if call.share_opening {
            ui.spinner().on_hover_text("Starting to share…");
            return;
        }
        if style::icon_button(ui, Icon::ScreenShare, 22.0, "Share your screen").clicked() {
            self.share.open = true;
            self.share.pictures.clear();
            self.dispatch(Command::ListShareSources);
        }
    }

    /// The red bar under the call bar while this device shares, so it is never forgotten.
    pub(super) fn sharing_bar(&self, ui: &mut egui::Ui, call: &Call) {
        let Some(name) = call.sharing.as_ref().filter(|_| call.is_live()) else {
            return;
        };
        egui::Frame::new()
            .fill(ERROR)
            .corner_radius(8)
            .inner_margin(egui::Margin::symmetric(12, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    style::dot(ui, Color32::WHITE, 4.0);
                    ui.label(
                        RichText::new(format!("You are sharing {name}"))
                            .color(Color32::WHITE)
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Stop sharing").color(ERROR).strong(),
                                )
                                .fill(Color32::WHITE),
                            )
                            .clicked()
                        {
                            self.dispatch(Command::CallStopSharing);
                        }
                    });
                });
            });
    }

    /// The picker. Returns whether it is up.
    pub(super) fn share_picker(&mut self, ctx: &egui::Context, snap: &Snapshot) -> bool {
        let live = snap
            .call
            .as_ref()
            .is_some_and(|c| c.is_live() && c.video && c.sharing.is_none());
        if !self.share.open || !live {
            self.share.open = false;
            return false;
        }
        let mut chosen: Option<ShareTarget> = None;
        let mut close = false;
        let (dismissed, ()) = style::modal(
            ctx,
            "share-picker",
            "Share your screen",
            "The other side sees it until you stop. They can't control it.",
            |ui| match &snap.share_sources {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new("Looking for screens and windows…")
                                .color(palette(ui).weak),
                        );
                    });
                }
                Some(sources) if !sources.allowed => {
                    ui.label(
                        "macOS needs your permission for DeskBeam to record the screen. Allow \
                         DeskBeam under System Settings → Privacy & Security → Screen \
                         Recording, then quit and reopen DeskBeam.",
                    );
                    ui.add_space(6.0);
                    if ui.button("Ask macOS for permission").clicked() {
                        // Sharing asks the system when it may not record yet.
                        chosen = Some(
                            sources
                                .options
                                .first()
                                .map(|o| o.target.clone())
                                .unwrap_or(ShareTarget::Display { id: 0 }),
                        );
                    }
                }
                Some(sources) => {
                    let (displays, windows): (Vec<&ShareOption>, Vec<&ShareOption>) = sources
                        .options
                        .iter()
                        .partition(|o| matches!(o.target, ShareTarget::Display { .. }));
                    egui::ScrollArea::vertical()
                        .max_height(420.0)
                        .show(ui, |ui| {
                            for (heading, options) in [("Screens", displays), ("Windows", windows)]
                            {
                                if options.is_empty() {
                                    continue;
                                }
                                style::section(ui, heading);
                                ui.add_space(4.0);
                                egui::Grid::new(heading)
                                    .num_columns(2)
                                    .spacing([12.0, 12.0])
                                    .show(ui, |ui| {
                                        for (n, option) in options.iter().enumerate() {
                                            if self.share_choice(ui, option) {
                                                chosen = Some(option.target.clone());
                                            }
                                            if n % 2 == 1 {
                                                ui.end_row();
                                            }
                                        }
                                    });
                                ui.add_space(10.0);
                            }
                        });
                }
            },
            |ui| {
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            },
        );
        if let Some(target) = chosen {
            self.dispatch(Command::CallShare { target });
            close = true;
        }
        if close || dismissed {
            self.share.open = false;
        }
        true
    }

    /// One thing to share: its picture and name. Returns whether it was picked.
    fn share_choice(&mut self, ui: &mut egui::Ui, option: &ShareOption) -> bool {
        if !self.share.pictures.contains_key(&option.target)
            && let Some(frame) = self.engine.share_thumbnail(&option.target)
        {
            let image =
                egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
            let texture = ui.ctx().load_texture(
                format!("share-{:?}", option.target),
                image,
                egui::TextureOptions::LINEAR,
            );
            self.share.pictures.insert(option.target.clone(), texture);
        }
        let pal = palette(ui);
        let response = ui
            .vertical(|ui| {
                ui.set_width(PICTURE);
                let shape = if option.width > 0 && option.height > 0 {
                    option.height as f32 / option.width as f32
                } else {
                    9.0 / 16.0
                };
                let size = Vec2::new(PICTURE, (PICTURE * shape).clamp(40.0, 130.0));
                match self.share.pictures.get(&option.target) {
                    Some(texture) => {
                        ui.add(
                            egui::Image::new(texture)
                                .fit_to_exact_size(size)
                                .corner_radius(6),
                        );
                    }
                    None => {
                        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        ui.painter().rect_filled(rect, 6.0, pal.line);
                    }
                }
                ui.add(egui::Label::new(RichText::new(&option.name).small()).truncate());
            })
            .response
            .interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(&option.name);
        if response.hovered() {
            ui.painter().rect_stroke(
                response.rect.expand(3.0),
                8.0,
                egui::Stroke::new(1.5, style::ACCENT),
                egui::StrokeKind::Outside,
            );
        }
        response.clicked()
    }
}
