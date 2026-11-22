//! The Files page: browse or drop files to add them to a list, check the device chosen below,
//! then press "Send files to …". Nothing is sent before that, so files don't go to the wrong
//! device. While transfers run, their progress shows above. Offers to this device are answered
//! in a dialog (see `modals` in `view.rs`).

use std::path::PathBuf;

use deskbeam_engine::{
    Command,
    model::{Device, Direction, Snapshot, Transfer, TransferState},
};
use eframe::egui::{self, Color32, CornerRadius, RichText, Sense, Stroke, Vec2};

use super::{
    DeskBeamApp, Tab,
    style::{self, ACCENT, ERROR, Icon, ONLINE, palette, strong},
};

/// A file or folder added to the list, waiting for Send.
pub(super) struct Staged {
    path: PathBuf,
    name: String,
    /// The file's size; `None` for a folder, which is not walked to keep the window quick.
    size: Option<u64>,
}

impl Staged {
    fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let size = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        Self { path, name, size }
    }
}

/// Segments in a progress bar.
const SEGMENTS: usize = 53;

impl DeskBeamApp {
    /// Files dropped onto the window are added to the list on the Files page; they are not sent.
    pub(super) fn take_dropped_files(&mut self, ui: &egui::Ui) {
        let dropped: Vec<PathBuf> = ui.ctx().input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        // Onto an open chat, they are attached to the next message there.
        if self.tab == Tab::Chats
            && let Some(open) = self.chat.open.clone()
        {
            let staged = self.chat.staged.entry(open).or_default();
            if let Some(note) = super::attach::stage(staged, dropped) {
                super::push(&self.notices, ui.ctx(), note, false);
            }
            return;
        }
        self.tab = Tab::Files;
        self.add(dropped);
    }

    /// Adds to the list, skipping anything already in it.
    fn add(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            if !self.staged.iter().any(|s| s.path == path) {
                self.staged.push(Staged::new(path));
            }
        }
    }

    /// While files are dragged over the window, say what dropping them does.
    pub(super) fn drop_hint(&self, ui: &egui::Ui) {
        if ui.ctx().input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop-hint"),
        ));
        let screen = ui.ctx().content_rect();
        painter.rect_filled(screen, 0.0, Color32::from_black_alpha(160));
        let text = if self.tab == Tab::Chats && self.chat.open.is_some() {
            "Drop to attach to the message you are writing"
        } else {
            "Drop to add to the list; nothing is sent yet"
        };
        painter.text(
            screen.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(20.0),
            Color32::WHITE,
        );
    }

    /// The device chosen on this page, while it is online.
    fn chosen_device<'a>(&self, snap: &'a Snapshot) -> Option<&'a Device> {
        let id = self.files_to.as_deref()?;
        snap.devices
            .iter()
            .find(|d| d.endpoint_id == id && d.online)
    }

    fn send(&self, endpoint_id: String, paths: Vec<PathBuf>) {
        self.dispatch(Command::SendFiles {
            endpoint_id,
            paths: paths.iter().map(|p| p.display().to_string()).collect(),
        });
    }

    pub(super) fn files_page(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        // The first online device is chosen until the user picks another.
        if self.chosen_device(snap).is_none() {
            self.files_to = snap
                .devices
                .iter()
                .find(|d| d.online)
                .map(|d| d.endpoint_id.clone());
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(24, 20))
                    .show(ui, |ui| {
                        let active: Vec<&Transfer> = snap
                            .transfers
                            .iter()
                            .filter(|t| {
                                matches!(t.state, TransferState::Offered | TransferState::Running)
                            })
                            .collect();
                        for transfer in &active {
                            self.progress(ui, transfer);
                            ui.add_space(18.0);
                        }
                        // While transfers run, the drop zone gives way to them; a list being
                        // put together stays on screen.
                        if active.is_empty() || !self.staged.is_empty() {
                            self.send_area(ui, snap);
                        }
                        self.finished(ui, snap);
                        ui.add_space(24.0);
                        self.device_list(ui, snap);
                    });
            });
    }

    /// Browse File, Browse Folder and Send in a row, then the drop zone or the list of what
    /// was added.
    fn send_area(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let to = self
            .chosen_device(snap)
            .map(|d| (d.endpoint_id.clone(), d.display_name.clone()));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 1.0;
            if style::dark_icon_button(ui, "Browse File", Icon::File, true).clicked() {
                self.browse_files();
            }
            if style::dark_icon_button(ui, "Browse Folder", Icon::Folder, true).clicked()
                && let Some(folder) = rfd::FileDialog::new()
                    .set_title("Add a folder to send")
                    .pick_folder()
            {
                self.add(vec![folder]);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = match &to {
                    Some((_, name)) => format!("Send files to {name}"),
                    None => "Send files".to_string(),
                };
                let tip = if self.staged.is_empty() {
                    "Add files or folders first."
                } else {
                    "Choose an online device below first."
                };
                let ready = to.is_some() && !self.staged.is_empty();
                if send_button(ui, &label, ready)
                    .on_disabled_hover_text(tip)
                    .clicked()
                    && let Some((id, _)) = &to
                {
                    let paths = self.staged.drain(..).map(|s| s.path).collect();
                    self.send(id.clone(), paths);
                }
            });
        });
        ui.add_space(18.0);
        if self.staged.is_empty() {
            self.drop_zone(ui);
        } else {
            self.staged_list(ui);
        }
    }

    /// The dashed area to drop onto; clicking it browses for files.
    fn drop_zone(&mut self, ui: &mut egui::Ui) {
        let pal = palette(ui);
        let width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 260.0), Sense::click());
        let hovered = response.hovered() || ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let painter = ui.painter();
        painter.rect_filled(
            rect,
            CornerRadius::same(12),
            if hovered { pal.line } else { pal.hover },
        );
        dashed_border(ui, rect);
        let mid = rect.center();
        egui::Image::new(Icon::Upload.source())
            .tint(pal.text)
            .paint_at(
                ui,
                egui::Rect::from_center_size(mid - Vec2::new(0.0, 50.0), Vec2::splat(56.0)),
            );
        let painter = ui.painter();
        painter.text(
            mid + Vec2::new(0.0, 22.0),
            egui::Align2::CENTER_CENTER,
            "Drag & drop",
            egui::FontId::new(20.0, style::bold()),
            pal.text,
        );
        painter.text(
            mid + Vec2::new(0.0, 56.0),
            egui::Align2::CENTER_CENTER,
            "or browse to add files or folders; they are sent when you press Send",
            egui::FontId::proportional(15.0),
            pal.weak,
        );
        if response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            self.browse_files();
        }
    }

    /// What was added: name, size and a remove button each, and Clear. Dropping more adds to it.
    fn staged_list(&mut self, ui: &mut egui::Ui) {
        let pal = palette(ui);
        let hovered = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let frame = egui::Frame::new()
            .fill(if hovered { pal.line } else { pal.hover })
            .corner_radius(12)
            .inner_margin(egui::Margin::symmetric(16, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let n = self.staged.len();
                    let total: u64 = self.staged.iter().filter_map(|s| s.size).sum();
                    let mut head = format!("{n} item{} to send", if n == 1 { "" } else { "s" });
                    if total > 0 {
                        head.push_str(&format!(" · {}", size(total)));
                    }
                    ui.label(strong(&head, 15.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Clear").clicked() {
                            self.staged.clear();
                        }
                    });
                });
                ui.add_space(6.0);
                let mut remove = None;
                for (i, item) in self.staged.iter().enumerate() {
                    style::rule(ui);
                    ui.horizontal(|ui| {
                        let icon = if item.size.is_some() {
                            Icon::File
                        } else {
                            Icon::Folder
                        };
                        ui.add(
                            egui::Image::new(icon.source())
                                .fit_to_exact_size(Vec2::splat(18.0))
                                .tint(pal.weak),
                        );
                        ui.label(RichText::new(&item.name).size(15.0))
                            .on_hover_text(item.path.display().to_string());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if style::icon_button(ui, Icon::Close, 16.0, "Remove").clicked() {
                                remove = Some(i);
                            }
                            let what = match item.size {
                                Some(bytes) => size(bytes),
                                None => "Folder".to_string(),
                            };
                            ui.label(RichText::new(what).color(pal.weak));
                        });
                    });
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Drop or browse to add more.")
                        .small()
                        .color(pal.weak),
                );
                remove
            });
        if let Some(i) = frame.inner {
            self.staged.remove(i);
        }
        dashed_border(ui, frame.response.rect);
    }

    fn browse_files(&mut self) {
        if let Some(files) = rfd::FileDialog::new()
            .set_title("Add files to send")
            .pick_files()
        {
            self.add(files);
        }
    }

    /// A running or offered transfer: what, to or from whom, a segmented bar, speed, stop.
    fn progress(&mut self, ui: &mut egui::Ui, transfer: &Transfer) {
        let pal = palette(ui);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(strong("File:", 14.0));
                    ui.label(what(transfer));
                });
                ui.add_space(4.0);
                let (color, text, hint) = match (&transfer.state, transfer.direction) {
                    (TransferState::Offered, _) => (
                        style::WARN,
                        format!("Waiting for {} to accept", transfer.peer_name),
                        "It starts as soon as they accept.".to_string(),
                    ),
                    (_, Direction::Send) => (
                        ONLINE,
                        format!("Sharing with {}", transfer.peer_name),
                        format!(
                            "Keep DeskBeam open until {} has everything.",
                            transfer.peer_name
                        ),
                    ),
                    (_, Direction::Receive) => (
                        ONLINE,
                        format!("Receiving from {}", transfer.peer_name),
                        "Keep DeskBeam open until everything has arrived.".to_string(),
                    ),
                };
                ui.horizontal(|ui| {
                    style::dot(ui, color, 4.0);
                    ui.label(RichText::new(text).color(color).size(16.0));
                });
                ui.label(RichText::new(hint).small().color(pal.weak));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if stop_button(ui).clicked() {
                    self.dispatch(Command::CancelTransfer {
                        transfer_id: transfer.id.clone(),
                    });
                }
            });
        });
        ui.add_space(12.0);
        let fraction = if transfer.total > 0 {
            (transfer.bytes as f32 / transfer.total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ui.horizontal(|ui| {
            ui.label("Transfer Progress");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!("{:.1}%", fraction * 100.0));
            });
        });
        segmented_bar(ui, fraction);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("Speed: {}/s", size(transfer.bytes_per_sec)))
                    .small()
                    .color(pal.weak),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "{} / {}",
                        size(transfer.bytes),
                        size(transfer.total)
                    ))
                    .small()
                    .color(pal.weak),
                );
            });
        });
    }

    /// Transfers that ended this session, newest first, with Show in folder for received ones.
    fn finished(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let pal = palette(ui);
        let done: Vec<&Transfer> = snap
            .transfers
            .iter()
            .rev()
            .filter(|t| t.state.is_finished())
            .take(5)
            .collect();
        if done.is_empty() {
            return;
        }
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.label(strong("Recent", 14.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Clear").clicked() {
                    self.dispatch(Command::ClearTransfers);
                }
            });
        });
        for transfer in done {
            ui.horizontal(|ui| {
                let way = match transfer.direction {
                    Direction::Send => "to",
                    Direction::Receive => "from",
                };
                ui.label(what(transfer));
                ui.label(RichText::new(format!("{way} {}", transfer.peer_name)).color(pal.weak));
                match &transfer.state {
                    TransferState::Done { path } => {
                        ui.label(RichText::new("Done").color(ONLINE));
                        if let Some(path) = path
                            && ui.small_button("Show in folder").clicked()
                        {
                            crate::reveal::reveal(std::path::Path::new(path));
                        }
                    }
                    TransferState::Declined => {
                        ui.label(RichText::new("Declined").color(pal.weak));
                    }
                    TransferState::Cancelled => {
                        ui.label(RichText::new("Stopped").color(pal.weak));
                    }
                    TransferState::Failed { reason } => {
                        ui.label(RichText::new(format!("Failed: {reason}")).color(ERROR));
                    }
                    _ => {}
                }
            });
        }
    }

    /// The paired devices; an online one is chosen by clicking it.
    fn device_list(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let pal = palette(ui);
        if snap.devices.is_empty() {
            ui.label(
                RichText::new("No paired devices yet. Pair one under Settings.").color(pal.weak),
            );
            return;
        }
        for (i, device) in snap.devices.iter().enumerate() {
            if i > 0 {
                style::rule(ui);
            }
            let chosen = self.files_to.as_deref() == Some(&device.endpoint_id) && device.online;
            let sense = if device.online {
                Sense::click()
            } else {
                Sense::hover()
            };
            let row = ui.scope_builder(egui::UiBuilder::new().sense(sense), |ui| {
                let hovered = ui.response().hovered() && device.online;
                let fade = if device.online { 1.0 } else { 0.45 };
                egui::Frame::new()
                    .fill(if chosen || hovered {
                        pal.hover
                    } else {
                        Color32::TRANSPARENT
                    })
                    .corner_radius(10)
                    .inner_margin(egui::Margin::symmetric(16, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.multiply_opacity(fade);
                        ui.horizontal(|ui| {
                            let (tile, _) =
                                ui.allocate_exact_size(Vec2::splat(48.0), Sense::hover());
                            ui.painter().rect_filled(
                                tile,
                                CornerRadius::same(8),
                                pal.line.gamma_multiply(0.6),
                            );
                            egui::Image::new(Icon::for_device(&device.device_type).source())
                                .tint(pal.weak)
                                .paint_at(
                                    ui,
                                    egui::Rect::from_center_size(tile.center(), Vec2::splat(22.0)),
                                );
                            ui.add_space(8.0);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&device.display_name).size(16.0));
                                ui.label(
                                    RichText::new(super::kind(&device.device_type, &device.os))
                                        .color(pal.weak),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| style::status(ui, device.online),
                            );
                        });
                    });
            });
            let response = row.response;
            if device.online
                && response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
            {
                self.files_to = Some(device.endpoint_id.clone());
            }
        }
    }
}

/// The accent-coloured button that sends the list, named after the chosen device.
fn send_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let image = egui::Image::new(Icon::Upload.source())
        .fit_to_exact_size(Vec2::splat(18.0))
        .tint(Color32::WHITE);
    ui.add_enabled(
        enabled,
        egui::Button::new((RichText::new(text).color(Color32::WHITE).size(15.0), image))
            .fill(ACCENT)
            .corner_radius(4)
            .min_size(Vec2::new(0.0, 38.0)),
    )
}

/// A dashed line around `rect`, as on the drop zone.
fn dashed_border(ui: &egui::Ui, rect: egui::Rect) {
    let pal = palette(ui);
    let r = rect.shrink(1.0);
    let corners = [
        r.left_top(),
        r.right_top(),
        r.right_bottom(),
        r.left_bottom(),
        r.left_top(),
    ];
    ui.painter().extend(egui::Shape::dashed_line(
        &corners,
        Stroke::new(1.5, pal.idle),
        7.0,
        6.0,
    ));
}

/// A red ring with a square in it.
fn stop_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::click());
    let color = if response.hovered() {
        ERROR.gamma_multiply(0.85)
    } else {
        ERROR
    };
    let painter = ui.painter();
    painter.circle_stroke(rect.center(), 18.0, Stroke::new(2.5, color));
    painter.rect_filled(
        egui::Rect::from_center_size(rect.center(), Vec2::splat(15.0)),
        CornerRadius::same(3),
        color,
    );
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Stop")
}

/// A bar of rounded segments, the first `fraction` of them filled.
fn segmented_bar(ui: &mut egui::Ui, fraction: f32) {
    let pal = palette(ui);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 26.0), Sense::hover());
    let gap = 8.0;
    let segment = ((width - gap * (SEGMENTS - 1) as f32) / SEGMENTS as f32).max(2.0);
    let filled = (fraction * SEGMENTS as f32).round() as usize;
    for i in 0..SEGMENTS {
        let x = rect.left() + i as f32 * (segment + gap);
        let r =
            egui::Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(segment, rect.height()));
        let color = if i < filled {
            pal.idle
        } else {
            pal.line.gamma_multiply(0.7)
        };
        ui.painter()
            .rect_filled(r, CornerRadius::same((segment / 2.0).min(6.0) as u8), color);
    }
}

/// "report.txt", "report.txt and 2 more", or "3 files" before the names are known.
pub(super) fn what(transfer: &Transfer) -> String {
    match transfer.names.as_slice() {
        [] => match transfer.file_count {
            0 => "Files".into(),
            1 => "1 file".into(),
            n => format!("{n} files"),
        },
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} and {} more", rest.len()),
    }
}

/// Bytes, for people: "1.4 MB".
pub(super) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::size;

    #[test]
    fn sizes_read_well() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1_400_000), "1.4 MB");
        assert_eq!(size(3_200_000_000), "3.2 GB");
    }
}
