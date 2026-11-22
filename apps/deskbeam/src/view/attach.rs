//! Attachments in the window: the files waiting in the composer, how a message shows its
//! files (pictures as pictures, other files as cards), and the picture viewer.
//!
//! Pictures are decoded by the engine (`Engine::attachment_picture`, turned upright and
//! scaled down) on its runtime, and kept here as textures by hash. Until a picture is here,
//! its preview from the message stands in.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use deskbeam_engine::{
    Engine, Picture,
    model::{AttachmentState, ChatAttachment, ChatMessage},
};
use eframe::egui::{self, RichText, Vec2};

use super::{
    files::size,
    style::{self, Icon, palette},
};

/// The widest a picture shows in a bubble, and how large it is decoded for the viewer.
const BUBBLE_WIDTH: f32 = 280.0;
const VIEWER_SIDE: u32 = 2048;
/// At most this many files per message (the engine's limit too).
pub(super) const MAX_FILES: usize = 10;

/// What a click on an attachment asks for.
pub(super) enum Action {
    Fetch {
        message_id: String,
        hash: String,
    },
    /// Save to Downloads/<author>/ and show it there.
    Save {
        message_id: String,
        hash: String,
    },
    SaveAs {
        message_id: String,
        hash: String,
        name: String,
    },
    View {
        message_id: String,
        attachment: ChatAttachment,
    },
    /// Open in the system's own player (videos, sounds).
    Play {
        message_id: String,
        hash: String,
    },
}

/// Pictures decoded so far, as textures, by "full:<hash>", "preview:<hash>" or, while the
/// viewer shows one at its own size, "largest:<hash>".
#[derive(Default)]
pub(super) struct Pictures {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
    /// The viewer shows the picture at its own pixels, scrolled, instead of fitted.
    actual: std::cell::Cell<bool>,
}

enum Slot {
    Loading,
    Decoded(Picture),
    Ready(egui::TextureHandle),
    Failed,
}

impl Pictures {
    /// The best picture there is for `attachment` now: the full one if it is here (asking
    /// the engine for it the first time), else its preview.
    fn texture(
        &self,
        ctx: &egui::Context,
        engine: &Engine,
        runtime: &tokio::runtime::Handle,
        attachment: &ChatAttachment,
    ) -> Option<egui::TextureHandle> {
        if attachment.state == AttachmentState::Ready
            && let Some(full) = self.full(ctx, engine, runtime, &attachment.hash)
        {
            return Some(full);
        }
        let preview = attachment.preview.as_deref()?;
        let key = format!("preview:{}", attachment.hash);
        let mut slots = self.slots.lock().expect("lock");
        if !slots.contains_key(&key) {
            let slot =
                deskbeam_engine::preview_picture(preview).map_or(Slot::Failed, Slot::Decoded);
            slots.insert(key.clone(), slot);
        }
        upload(ctx, &mut slots, &key)
    }

    fn full(
        &self,
        ctx: &egui::Context,
        engine: &Engine,
        runtime: &tokio::runtime::Handle,
        hash: &str,
    ) -> Option<egui::TextureHandle> {
        self.decoded(ctx, engine, runtime, hash, "full", VIEWER_SIDE)
    }

    /// The picture at its full size, or as large as the graphics card takes a texture
    /// (often 8192 or 16384 px a side). Large: kept only while the viewer shows it.
    fn largest(
        &self,
        ctx: &egui::Context,
        engine: &Engine,
        runtime: &tokio::runtime::Handle,
        hash: &str,
    ) -> Option<egui::TextureHandle> {
        let side = ctx.input(|i| i.max_texture_side).max(VIEWER_SIDE as usize) as u32;
        self.decoded(ctx, engine, runtime, hash, "largest", side)
    }

    /// Drops the full-size pictures, when the viewer closes.
    pub(super) fn forget_largest(&self) {
        self.actual.set(false);
        self.slots
            .lock()
            .expect("lock")
            .retain(|key, _| !key.starts_with("largest:"));
    }

    fn decoded(
        &self,
        ctx: &egui::Context,
        engine: &Engine,
        runtime: &tokio::runtime::Handle,
        hash: &str,
        kind: &str,
        side: u32,
    ) -> Option<egui::TextureHandle> {
        let key = format!("{kind}:{hash}");
        let mut slots = self.slots.lock().expect("lock");
        if !slots.contains_key(&key) {
            slots.insert(key.clone(), Slot::Loading);
            let (slots, engine, ctx, hash, key) = (
                self.slots.clone(),
                engine.clone(),
                ctx.clone(),
                hash.to_string(),
                key.clone(),
            );
            runtime.spawn(async move {
                let slot = match engine.attachment_picture(&hash, side).await {
                    Some(picture) => Slot::Decoded(picture),
                    None => Slot::Failed,
                };
                slots.lock().expect("lock").insert(key, slot);
                ctx.request_repaint();
            });
            return None;
        }
        upload(ctx, &mut slots, &key)
    }
}

/// Turns a decoded picture into a texture once, and returns it.
fn upload(
    ctx: &egui::Context,
    slots: &mut HashMap<String, Slot>,
    key: &str,
) -> Option<egui::TextureHandle> {
    let slot = slots.get_mut(key)?;
    if let Slot::Decoded(picture) = slot {
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [picture.width as usize, picture.height as usize],
            &picture.rgba,
        );
        *slot = Slot::Ready(ctx.load_texture(key, image, egui::TextureOptions::LINEAR));
    }
    match slot {
        Slot::Ready(texture) => Some(texture.clone()),
        _ => None,
    }
}

/// A message's files, under its text. Returns what was clicked.
pub(super) fn show(
    ui: &mut egui::Ui,
    message: &ChatMessage,
    pictures: &Pictures,
    engine: &Engine,
    runtime: &tokio::runtime::Handle,
) -> Option<Action> {
    let mut action = None;
    for attachment in &message.attachments {
        let clicked = if attachment.is_image() && attachment.preview.is_some() {
            picture(ui, message, attachment, pictures, engine, runtime)
        } else {
            card(ui, message, attachment)
        };
        action = action.or(clicked);
    }
    action
}

fn picture(
    ui: &mut egui::Ui,
    message: &ChatMessage,
    attachment: &ChatAttachment,
    pictures: &Pictures,
    engine: &Engine,
    runtime: &tokio::runtime::Handle,
) -> Option<Action> {
    let (w, h) = (
        attachment.width.unwrap_or(4) as f32,
        attachment.height.unwrap_or(3) as f32,
    );
    let width = BUBBLE_WIDTH.min(ui.available_width()).min(w.max(48.0));
    let shown = Vec2::new(width, (width * h / w.max(1.0)).min(360.0));
    let texture = pictures.texture(ui.ctx(), engine, runtime, attachment);
    let response = match &texture {
        Some(texture) => ui.add(
            egui::Image::new((texture.id(), shown))
                .corner_radius(6)
                .sense(egui::Sense::click()),
        ),
        None => {
            let (rect, response) = ui.allocate_exact_size(shown, egui::Sense::click());
            ui.painter().rect_filled(rect, 6.0, palette(ui).line);
            response
        }
    };
    let tip = match &attachment.state {
        AttachmentState::Fetching { bytes } => {
            format!(
                "{}: fetching, {} of {}",
                attachment.name,
                size(*bytes),
                size(attachment.size)
            )
        }
        AttachmentState::Failed { reason } => format!("{}: {reason}", attachment.name),
        _ => format!("{} · {}", attachment.name, size(attachment.size)),
    };
    let response = response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip);
    if !matches!(attachment.state, AttachmentState::Ready) {
        state_line(ui, message, attachment);
    }
    response.clicked().then(|| Action::View {
        message_id: message.id.clone(),
        attachment: attachment.clone(),
    })
}

/// A file: icon, name and size, and what can be done with it now.
fn card(ui: &mut egui::Ui, message: &ChatMessage, attachment: &ChatAttachment) -> Option<Action> {
    let pal = palette(ui);
    let mut action = None;
    egui::Frame::new()
        .fill(pal.hover)
        .corner_radius(8)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_max_width(BUBBLE_WIDTH);
            ui.horizontal(|ui| {
                let icon = if attachment.is_image() {
                    Icon::Image
                } else {
                    Icon::File
                };
                style::icon(ui, icon, 22.0, pal.weak);
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(RichText::new(&attachment.name).strong()).truncate());
                    ui.label(RichText::new(size(attachment.size)).small().color(pal.weak));
                });
            });
            action = state_line(ui, message, attachment);
        });
    action
}

/// Fetch, its progress, or Save / Save as… once here.
fn state_line(
    ui: &mut egui::Ui,
    message: &ChatMessage,
    attachment: &ChatAttachment,
) -> Option<Action> {
    let (message_id, hash) = (message.id.clone(), attachment.hash.clone());
    let mut action = None;
    ui.horizontal(|ui| match &attachment.state {
        _ if attachment.plays()
            && !matches!(attachment.state, AttachmentState::Fetching { .. }) =>
        {
            if ui
                .small_button("▶ Play")
                .on_hover_text("Opens in your system's player; downloads it first if needed")
                .clicked()
            {
                action = Some(Action::Play {
                    message_id: message_id.clone(),
                    hash: hash.clone(),
                });
            }
            if let AttachmentState::Failed { reason } = &attachment.state {
                ui.label(RichText::new(reason).small().color(style::ERROR));
            }
            if attachment.state == AttachmentState::Ready {
                if ui.small_button("Save").clicked() {
                    action = Some(Action::Save {
                        message_id: message_id.clone(),
                        hash: hash.clone(),
                    });
                }
                if ui.small_button("Save as…").clicked() {
                    action = Some(Action::SaveAs {
                        message_id,
                        hash,
                        name: attachment.name.clone(),
                    });
                }
            }
        }
        AttachmentState::Missing => {
            if ui.small_button("Download").clicked() {
                action = Some(Action::Fetch { message_id, hash });
            }
        }
        AttachmentState::Fetching { bytes } => {
            let fraction = if attachment.size > 0 {
                *bytes as f32 / attachment.size as f32
            } else {
                0.0
            };
            ui.add(
                egui::ProgressBar::new(fraction)
                    .desired_width(140.0)
                    .show_percentage(),
            );
        }
        AttachmentState::Failed { reason } => {
            ui.label(RichText::new(reason).small().color(style::ERROR));
            if ui.small_button("Try again").clicked() {
                action = Some(Action::Fetch { message_id, hash });
            }
        }
        AttachmentState::Ready => {
            if ui
                .small_button("Save")
                .on_hover_text("Into Downloads, in a folder named after who sent it")
                .clicked()
            {
                action = Some(Action::Save {
                    message_id: message_id.clone(),
                    hash: hash.clone(),
                });
            }
            if ui.small_button("Save as…").clicked() {
                action = Some(Action::SaveAs {
                    message_id,
                    hash,
                    name: attachment.name.clone(),
                });
            }
        }
    });
    action
}

/// The picture viewer: the picture as large as the window allows, with Save and Save as….
pub(super) fn viewer(
    ctx: &egui::Context,
    open: &(String, ChatAttachment),
    pictures: &Pictures,
    engine: &Engine,
    runtime: &tokio::runtime::Handle,
) -> (bool, Option<Action>) {
    let (message_id, attachment) = open;
    let mut action = None;
    let screen = ctx.content_rect().size();
    let modal = egui::Modal::new(egui::Id::new("picture-viewer")).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&attachment.name).strong());
            ui.label(RichText::new(size(attachment.size)).color(palette(ui).weak));
        });
        let (w, h) = (
            attachment.width.unwrap_or(4) as f32,
            attachment.height.unwrap_or(3) as f32,
        );
        let room = Vec2::new(screen.x - 120.0, screen.y - 200.0).max(Vec2::splat(120.0));
        let scale = (room.x / w).min(room.y / h).min(1.0);
        let shown = Vec2::new(w * scale, h * scale);
        // Bigger than it can be shown here: offer its own size, scrolled.
        let shrunk = scale < 1.0 || w.max(h) > VIEWER_SIDE as f32;
        let ready = attachment.state == AttachmentState::Ready;
        let largest = (pictures.actual.get() && ready)
            .then(|| pictures.largest(ctx, engine, runtime, &attachment.hash))
            .flatten();
        match (pictures.actual.get() && ready, largest) {
            (true, Some(texture)) => {
                let own = texture.size_vec2() / ctx.pixels_per_point();
                egui::ScrollArea::both()
                    .max_width(room.x)
                    .max_height(room.y)
                    .show(ui, |ui| {
                        ui.add(egui::Image::new((texture.id(), own)));
                    });
            }
            (true, None) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Opening at full size…").color(palette(ui).weak));
                });
                ui.allocate_exact_size(shown, egui::Sense::hover());
            }
            (false, _) => match pictures.texture(ctx, engine, runtime, attachment) {
                Some(texture) => {
                    ui.add(egui::Image::new((texture.id(), shown)));
                }
                None => {
                    ui.allocate_exact_size(shown, egui::Sense::hover());
                }
            },
        }
        if ready && shrunk {
            let label = if pictures.actual.get() {
                "Fit to window"
            } else {
                "Actual size"
            };
            if ui.small_button(label).clicked() {
                pictures.actual.set(!pictures.actual.get());
            }
        }
        let message = ChatMessage {
            id: message_id.clone(),
            attachments: Vec::new(),
            ..placeholder()
        };
        action = state_line(ui, &message, attachment);
        ui.add_space(4.0);
        ui.button("Close").clicked()
    });
    let close = modal.should_close() || modal.inner;
    if close {
        pictures.forget_largest();
    }
    (close, action)
}

fn placeholder() -> ChatMessage {
    ChatMessage {
        id: String::new(),
        conversation: String::new(),
        author: String::new(),
        author_name: String::new(),
        mine: false,
        sent_at: 0,
        body: String::new(),
        delivery: None,
        attachments: Vec::new(),
        edited: false,
        deleted: false,
        reply: None,
    }
}

/// The files waiting in the composer, as chips with a remove button.
pub(super) fn staged_chips(ui: &mut egui::Ui, staged: &mut Vec<PathBuf>) {
    if staged.is_empty() {
        return;
    }
    let pal = palette(ui);
    let mut remove = None;
    ui.horizontal_wrapped(|ui| {
        for (i, path) in staged.iter().enumerate() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            egui::Frame::new()
                .fill(pal.hover)
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        style::icon(ui, Icon::Paperclip, 14.0, pal.weak);
                        ui.label(RichText::new(name).small())
                            .on_hover_text(path.display().to_string());
                        if style::icon_button(ui, Icon::Close, 12.0, "Remove").clicked() {
                            remove = Some(i);
                        }
                    });
                });
        }
    });
    if let Some(i) = remove {
        staged.remove(i);
    }
}

/// Adds files (not folders) to a composer's list, once each, up to the limit. Returns how
/// many were left out, and why.
pub(super) fn stage(staged: &mut Vec<PathBuf>, paths: Vec<PathBuf>) -> Option<String> {
    let mut folders = 0;
    let mut over = 0;
    for path in paths {
        if path.is_dir() {
            folders += 1;
        } else if staged.contains(&path) {
            continue;
        } else if staged.len() >= MAX_FILES {
            over += 1;
        } else {
            staged.push(path);
        }
    }
    match (folders, over) {
        (0, 0) => None,
        (f, 0) => Some(format!(
            "{f} folder{} left out: folders go through the Files page.",
            if f == 1 { "" } else { "s" }
        )),
        (_, o) => Some(format!(
            "{o} left out: at most {MAX_FILES} files per message."
        )),
    }
}

/// A picture on the clipboard, written to a PNG in the temp folder so it can be attached
/// (the engine copies it into its store when sending).
pub(super) fn pasted_picture() -> Option<PathBuf> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let picture = clipboard.get_image().ok()?;
    let image = image::RgbaImage::from_raw(
        picture.width as u32,
        picture.height as u32,
        picture.bytes.into_owned(),
    )?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join("deskbeam-pasted");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("Pasted picture {stamp}.png"));
    image.save(&path).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_skips_folders_repeats_and_extras() {
        let dir = tempfile::tempdir().unwrap();
        let file = |n: usize| {
            let path = dir.path().join(format!("{n}.txt"));
            std::fs::write(&path, "x").unwrap();
            path
        };
        let mut staged = Vec::new();
        assert_eq!(stage(&mut staged, vec![file(0), file(0)]), None);
        assert_eq!(staged.len(), 1);
        let note = stage(&mut staged, vec![dir.path().to_path_buf()]).unwrap();
        assert!(note.contains("Files page"));
        let note = stage(&mut staged, (1..15).map(file).collect()).unwrap();
        assert!(note.contains("at most"));
        assert_eq!(staged.len(), MAX_FILES);
    }
}
