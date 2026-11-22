//! The Chats page: the list of chats, the open conversation and the box to write in.
//!
//! Messages are read from the engine (`Engine::chat_messages`) and read again only when a
//! new snapshot comes out, which is how the engine says something changed.
//!
//! Right-clicking a message offers Reply and Copy text, and on this device's own Edit and
//! Delete. The search box above the list looks through every chat's messages.

use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use deskbeam_engine::{
    Command,
    model::{ChatMessage, Conversation, ConversationKind, Quote, SearchHit, Snapshot},
};
use eframe::egui::{self, Color32, RichText};

use super::{
    DeskBeamApp,
    style::{self, ACCENT, Icon, ONLINE, palette, strong},
};

/// How many of a conversation's newest messages are shown.
const SHOWN: usize = 300;

#[derive(Default)]
pub(super) struct ChatState {
    /// The conversation on the right.
    pub open: Option<String>,
    /// What is being written, per conversation.
    drafts: std::collections::HashMap<String, String>,
    /// The open conversation's messages, as of the snapshot they were read at.
    cache: Option<(String, Arc<Snapshot>, Vec<ChatMessage>)>,
    /// The "New group" dialog, while open.
    new_group: Option<NewGroup>,
    /// The snapshot a "mark read" was last sent for, so it is sent once per change.
    marked: Option<(String, usize)>,
    /// The emoji picker's search and tab.
    picker: super::emoji::Picker,
    /// Files waiting to go with the next message, per conversation.
    pub staged: std::collections::HashMap<String, Vec<std::path::PathBuf>>,
    /// Pictures decoded for showing.
    pictures: super::attach::Pictures,
    /// The picture open in the viewer: its message and the attachment.
    viewing: Option<(String, deskbeam_engine::model::ChatAttachment)>,
    /// The message being answered, per conversation.
    replying: std::collections::HashMap<String, ChatMessage>,
    /// The message being edited: (conversation, message id).
    editing: Option<(String, String)>,
    /// A message waiting for "Delete for everyone?".
    deleting: Option<ChatMessage>,
    /// What the search box holds, and its results as of a snapshot.
    search: String,
    found: Option<(String, Arc<Snapshot>, Vec<SearchHit>)>,
    /// A message to scroll to and mark once, after picking a search result.
    highlight: Option<String>,
    /// When "typing" was last sent for the open chat.
    typing_said: Option<(String, std::time::Instant)>,
    /// The chat shown last, and for how many more frames to hold it at its newest message
    /// (pictures loading in the first frames make it taller).
    opened: Option<(String, u8)>,
}

/// Frames a freshly opened chat is held at its newest message.
const START_AT_BOTTOM: u8 = 6;

/// "Typing" is sent at most this often while writing.
const TYPING_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// What a click on a message asks for.
enum Asked {
    Attachment(super::attach::Action),
    Reply(ChatMessage),
    Edit(ChatMessage),
    Delete(ChatMessage),
}

#[derive(Default)]
struct NewGroup {
    name: String,
    members: BTreeSet<String>,
}

impl DeskBeamApp {
    /// The list of chats, on the left. Call before the central panel.
    pub(super) fn chat_list(&mut self, ui: &mut egui::Ui, snap: &Arc<Snapshot>) {
        egui::Panel::left("chat-list")
            .resizable(true)
            .default_size(230.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Chats").size(20.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let can = snap.devices.iter().any(|d| d.permissions.chat);
                        if ui
                            .add_enabled(can, egui::Button::new("New group"))
                            .on_disabled_hover_text("Pair a device first.")
                            .clicked()
                        {
                            self.chat.new_group = Some(NewGroup::default());
                        }
                    });
                });
                ui.add_space(4.0);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.chat.search)
                        .hint_text("Search messages")
                        .desired_width(f32::INFINITY),
                );
                if field.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.chat.search.clear();
                }
                ui.add_space(4.0);
                if !self.chat.search.trim().is_empty() {
                    self.search_results(ui, snap);
                    return;
                }
                if snap.conversations.is_empty() {
                    ui.label(
                        RichText::new(
                            "No chats yet. Start one with Chat on a paired device, or make a group.",
                        )
                        .color(palette(ui).weak),
                    );
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for conversation in &snap.conversations {
                            self.chat_list_row(ui, snap, conversation);
                        }
                        // Paired devices with no chat yet, so one can be started.
                        let started: Vec<&str> = snap
                            .conversations
                            .iter()
                            .filter_map(|c| match &c.kind {
                                ConversationKind::Direct { endpoint_id } => {
                                    Some(endpoint_id.as_str())
                                }
                                ConversationKind::Group { .. } => None,
                            })
                            .collect();
                        for device in snap.devices.iter().filter(|d| {
                            d.permissions.chat && !started.contains(&d.endpoint_id.as_str())
                        }) {
                            self.new_chat_row(ui, snap, &device.display_name, &device.endpoint_id);
                        }
                    });
            });
    }

    /// The messages found, in place of the list of chats.
    fn search_results(&mut self, ui: &mut egui::Ui, snap: &Arc<Snapshot>) {
        let text = self.chat.search.trim().to_string();
        let fresh =
            !matches!(&self.chat.found, Some((t, at, _)) if *t == text && Arc::ptr_eq(at, snap));
        if fresh {
            let hits = self.engine.chat_search(&text, None, 100);
            self.chat.found = Some((text.clone(), snap.clone(), hits));
        }
        let hits = self
            .chat
            .found
            .as_ref()
            .map(|(_, _, h)| h.clone())
            .unwrap_or_default();
        let pal = palette(ui);
        if hits.is_empty() {
            ui.label(RichText::new("No messages found.").color(pal.weak));
            return;
        }
        ui.label(
            RichText::new(format!("{} found", hits.len()))
                .small()
                .color(pal.weak),
        );
        let mut picked = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for hit in &hits {
                    let selected = self.chat.highlight.as_deref() == Some(&hit.message.id);
                    let clicked = list_row(ui, selected, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&hit.title).strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        RichText::new(when(hit.message.sent_at))
                                            .small()
                                            .color(pal.weak),
                                    );
                                },
                            );
                        });
                        let who = if hit.message.mine {
                            "You".to_string()
                        } else {
                            hit.message.author_name.clone()
                        };
                        ui.label(
                            RichText::new(format!("{who}: {}", snippet(&hit.message.body, &text)))
                                .small()
                                .color(pal.weak),
                        );
                    });
                    if clicked {
                        picked = Some(hit.clone());
                    }
                }
            });
        if let Some(hit) = picked {
            self.chat.open = Some(hit.conversation.clone());
            self.chat.highlight = Some(hit.message.id.clone());
        }
    }

    fn new_chat_row(&mut self, ui: &mut egui::Ui, snap: &Snapshot, name: &str, endpoint_id: &str) {
        let open = self.engine.direct_conversation(endpoint_id);
        let selected = open.is_some() && self.chat.open == open;
        let mut call = false;
        let clicked = list_row(ui, selected, |ui| {
            ui.horizontal(|ui| {
                ui.label(name);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    call = self.phone_button(ui, snap, endpoint_id);
                });
            });
            ui.label(
                RichText::new("No messages yet")
                    .small()
                    .color(palette(ui).weak),
            );
        });
        if call {
            self.start_call(endpoint_id);
        } else if clicked {
            self.open_direct_chat(endpoint_id);
        }
    }

    fn chat_list_row(&mut self, ui: &mut egui::Ui, snap: &Snapshot, conversation: &Conversation) {
        let selected = self.chat.open.as_deref() == Some(&conversation.id);
        let mut call = None;
        let clicked = list_row(ui, selected, |ui| {
            ui.horizontal(|ui| {
                let title = RichText::new(&conversation.title);
                ui.label(if conversation.unread > 0 {
                    title.strong()
                } else {
                    title
                });
                if matches!(conversation.kind, ConversationKind::Group { .. }) {
                    ui.label(RichText::new("group").small().color(palette(ui).weak));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Calls are with one paired device: direct chats only.
                    if let ConversationKind::Direct { endpoint_id } = &conversation.kind
                        && self.phone_button(ui, snap, endpoint_id)
                    {
                        call = Some(endpoint_id.clone());
                    }
                    if conversation.unread > 0 {
                        ui.label(
                            RichText::new(conversation.unread.to_string())
                                .strong()
                                .color(ACCENT),
                        );
                    }
                });
            });
            if !conversation.typing.is_empty() {
                ui.label(
                    RichText::new(typing_line(&conversation.typing))
                        .small()
                        .italics()
                        .color(ACCENT),
                );
            } else if let Some(last) = &conversation.last {
                let first_line = last.body.lines().next().unwrap_or("");
                let preview: String = if last.deleted {
                    "Message deleted".into()
                } else if first_line.is_empty() {
                    match last.attachments.as_slice() {
                        [] => String::new(),
                        [one] if one.is_image() => "Picture".into(),
                        [one] => one.name.clone(),
                        many => format!("{} files", many.len()),
                    }
                } else {
                    first_line.chars().take(40).collect()
                };
                let who = if last.mine { "You: " } else { "" };
                ui.label(
                    RichText::new(format!("{who}{preview}"))
                        .small()
                        .color(palette(ui).weak),
                );
            }
        });
        if let Some(endpoint_id) = call {
            self.start_call(&endpoint_id);
        } else if clicked {
            self.chat.open = Some(conversation.id.clone());
        }
    }

    /// The phone on a paired device's row: enabled while it is online, chat is allowed and
    /// no call is on. Returns whether it was clicked.
    fn phone_button(&self, ui: &mut egui::Ui, snap: &Snapshot, endpoint_id: &str) -> bool {
        let device = snap.device(endpoint_id);
        let in_call = snap.call.as_ref().is_some_and(|c| c.is_live());
        let (enabled, tip) = match device {
            None => (false, "Calls are with paired devices only".to_string()),
            Some(d) if !d.permissions.chat => (false, "Chat is off for this device".to_string()),
            Some(d) if !d.online => (false, format!("{} is offline", d.display_name)),
            Some(_) if in_call => (false, "You are in a call".to_string()),
            Some(d) => (true, format!("Call {}", d.display_name)),
        };
        let pal = palette(ui);
        let (rect, response) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
        let color = if !enabled {
            pal.idle
        } else if response.hovered() {
            ONLINE
        } else {
            pal.weak
        };
        egui::Image::new(Icon::Phone.source())
            .tint(color)
            .paint_at(ui, rect.shrink(3.0));
        let response = if enabled {
            response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(tip)
        } else {
            response.on_hover_text(tip)
        };
        enabled && response.clicked()
    }

    /// The open conversation and the box to write in. Call inside the central panel.
    pub(super) fn chat_view(&mut self, ui: &mut egui::Ui, snap: &Arc<Snapshot>) {
        let Some(open) = self.chat.open.clone() else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Pick a chat on the left.").color(palette(ui).weak));
            });
            return;
        };
        let Some(conversation) = snap.conversation(&open).cloned() else {
            // A chat with no messages yet is not in the list; its device may be.
            self.empty_direct_chat(ui, snap, &open);
            return;
        };

        // Read again only when the engine published something new.
        let fresh =
            !matches!(&self.chat.cache, Some((id, at, _)) if *id == open && Arc::ptr_eq(at, snap));
        if fresh {
            let messages = self.engine.chat_messages(&open, SHOWN);
            self.chat.cache = Some((open.clone(), snap.clone(), messages));
        }

        // Seen while the window has focus: mark read, once per change.
        let focused = ui.ctx().input(|i| i.focused);
        if focused
            && conversation.unread > 0
            && self.chat.marked != Some((open.clone(), conversation.unread))
        {
            self.chat.marked = Some((open.clone(), conversation.unread));
            crate::notify::clear_messages(&open);
            self.dispatch(Command::MarkRead {
                conversation: open.clone(),
            });
        }

        self.chat_heading(ui, &conversation);
        ui.separator();
        self.composer(ui, &open);
        let messages = self
            .chat
            .cache
            .as_ref()
            .map(|(_, _, m)| m.clone())
            .unwrap_or_default();
        let mut asked = None;
        let highlight = self.chat.highlight.clone();
        let mut shown_highlight = false;
        // A chat opens at its newest message (a search result scrolls to its own instead).
        if self.chat.opened.as_ref().is_none_or(|(id, _)| *id != open) {
            self.chat.opened = Some((open.clone(), START_AT_BOTTOM));
        }
        let start = match &mut self.chat.opened {
            Some((_, frames)) if *frames > 0 && highlight.is_none() => {
                *frames -= 1;
                ui.ctx().request_repaint();
                true
            }
            _ => false,
        };
        let mut area = egui::ScrollArea::vertical()
            .id_salt(("messages", open.as_str()))
            .auto_shrink([false, false])
            .stick_to_bottom(highlight.is_none());
        if start {
            // Past the end: egui keeps it at the bottom. Finite, so no arithmetic turns it into
            // infinity before that.
            area = area.vertical_scroll_offset(1.0e9);
        }
        area.show(ui, |ui| {
            ui.add_space(6.0);
            if messages.is_empty() {
                ui.label(RichText::new("No messages yet. Say hello.").color(palette(ui).weak));
            }
            let group = matches!(conversation.kind, ConversationKind::Group { .. });
            for message in &messages {
                let files = Files {
                    pictures: &self.chat.pictures,
                    engine: &self.engine,
                    runtime: &self.runtime,
                };
                let marked = highlight.as_deref() == Some(message.id.as_str());
                shown_highlight |= marked;
                asked = bubble(ui, message, group, &files, marked).or(asked.take());
            }
            ui.add_space(6.0);
        });
        // Scrolled to once; the mark goes when the search is cleared.
        if shown_highlight && self.chat.search.trim().is_empty() {
            self.chat.highlight = None;
        }
        let mut action = None;
        match asked {
            Some(Asked::Attachment(a)) => action = Some(a),
            Some(Asked::Reply(message)) => {
                self.chat.editing = None;
                self.chat.replying.insert(open.clone(), message);
                ui.ctx()
                    .memory_mut(|m| m.request_focus(egui::Id::new(("composer", open.as_str()))));
            }
            Some(Asked::Edit(message)) => {
                self.chat.replying.remove(&open);
                self.chat.drafts.insert(open.clone(), message.body.clone());
                self.chat.editing = Some((open.clone(), message.id));
                ui.ctx()
                    .memory_mut(|m| m.request_focus(egui::Id::new(("composer", open.as_str()))));
            }
            Some(Asked::Delete(message)) => self.chat.deleting = Some(message),
            None => {}
        }
        self.delete_dialog(ui.ctx());
        if let Some((message_id, attachment)) = &self.chat.viewing {
            let (close, clicked) = super::attach::viewer(
                ui.ctx(),
                &(message_id.clone(), attachment.clone()),
                &self.chat.pictures,
                &self.engine,
                &self.runtime,
            );
            if close {
                self.chat.viewing = None;
            }
            action = clicked.or(action);
        }
        if let Some(action) = action {
            self.attachment_action(action);
        }
    }

    fn attachment_action(&mut self, action: super::attach::Action) {
        use super::attach::Action;
        match action {
            Action::Fetch { message_id, hash } => {
                self.dispatch(Command::FetchAttachment { message_id, hash });
            }
            Action::Save { message_id, hash } => self.save_attachment(message_id, hash, None),
            Action::SaveAs {
                message_id,
                hash,
                name,
            } => {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Save the attachment")
                    .set_file_name(&name)
                    .save_file()
                {
                    self.save_attachment(message_id, hash, Some(path.display().to_string()));
                }
            }
            Action::View {
                message_id,
                attachment,
            } => self.chat.viewing = Some((message_id, attachment)),
            Action::Play { message_id, hash } => self.play_attachment(message_id, hash),
        }
    }

    /// Opens a video or sound in the system's player, fetching it first if needed.
    fn play_attachment(&self, message_id: String, hash: String) {
        let (engine, notices, ctx) = (self.engine.clone(), self.notices.clone(), self.ctx.clone());
        self.runtime.spawn(async move {
            match engine
                .execute(Command::OpenAttachment { message_id, hash })
                .await
            {
                Ok(value) => {
                    if let Some(path) = value["path"].as_str() {
                        crate::reveal::open(std::path::Path::new(path));
                    }
                }
                Err(e) => super::push(&notices, &ctx, format!("Not opened: {e}"), true),
            }
        });
    }

    /// Saves an attachment, then shows it in the file manager.
    fn save_attachment(&self, message_id: String, hash: String, path: Option<String>) {
        let (engine, notices, ctx) = (self.engine.clone(), self.notices.clone(), self.ctx.clone());
        self.runtime.spawn(async move {
            let saved = engine
                .execute(Command::SaveAttachment {
                    message_id,
                    hash,
                    path,
                })
                .await;
            match saved {
                Ok(value) => {
                    if let Some(path) = value["path"].as_str() {
                        crate::reveal::reveal(std::path::Path::new(path));
                        super::push(&notices, &ctx, format!("Saved to {path}."), false);
                    }
                }
                Err(e) => super::push(&notices, &ctx, format!("Not saved: {e}"), true),
            }
        });
    }

    fn chat_heading(&mut self, ui: &mut egui::Ui, conversation: &Conversation) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(&conversation.title);
            match &conversation.kind {
                ConversationKind::Direct { endpoint_id } => {
                    let online = self
                        .engine
                        .snapshot()
                        .device(endpoint_id)
                        .is_some_and(|d| d.online);
                    let (dot, text) = if online {
                        (ONLINE, "online")
                    } else {
                        (palette(ui).idle, "offline: messages wait until it is back")
                    };
                    ui.label(RichText::new("●").color(dot));
                    ui.label(RichText::new(text).color(palette(ui).weak));
                }
                ConversationKind::Group { members, .. } => {
                    let names: Vec<_> = members.iter().map(|m| m.display_name.as_str()).collect();
                    ui.label(
                        RichText::new(format!("You, {}", names.join(", "))).color(palette(ui).weak),
                    )
                    .on_hover_text("Members this device is not paired with are left out.");
                }
            }
        });
    }

    /// The box to write in, at the bottom of the conversation.
    /// "Delete for everyone?", while a deletion waits for an answer.
    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(message) = self.chat.deleting.clone() else {
            return;
        };
        let mut answer = None;
        let (dismissed, ()) = style::modal(
            ctx,
            "delete-message",
            "Delete this message?",
            "It is deleted for everyone in the chat. They see that a message was here.",
            |ui| {
                let text = if message.body.is_empty() {
                    format!("{} file(s)", message.attachments.len())
                } else {
                    snippet(&message.body, "")
                };
                ui.label(RichText::new(text).color(palette(ui).weak));
            },
            |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Delete").color(Color32::WHITE))
                            .fill(style::ERROR),
                    )
                    .clicked()
                {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            },
        );
        if answer == Some(true) {
            self.dispatch(Command::DeleteMessage {
                message_id: message.id.clone(),
            });
            if self
                .chat
                .editing
                .as_ref()
                .is_some_and(|(_, id)| *id == message.id)
            {
                self.chat.editing = None;
            }
        }
        if answer.is_some() || dismissed {
            self.chat.deleting = None;
        }
    }

    /// The strip above the text field while replying or editing, with a way out.
    fn composer_strip(&mut self, ui: &mut egui::Ui, open: &str) {
        let pal = palette(ui);
        let editing = self.chat.editing.as_ref().is_some_and(|(c, _)| c == open);
        let (title, text) = if editing {
            ("Editing message".to_string(), String::new())
        } else if let Some(message) = self.chat.replying.get(open) {
            (
                format!("Replying to {}", message.author_name),
                snippet(&message.body, ""),
            )
        } else {
            return;
        };
        let mut cancel = false;
        egui::Frame::new()
            .fill(pal.card)
            .corner_radius(6)
            .inner_margin(egui::Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).small().strong().color(ACCENT));
                    ui.label(RichText::new(text).small().color(pal.weak));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::icon_button(ui, Icon::Close, 14.0, "Cancel (Esc)").clicked() {
                            cancel = true;
                        }
                    });
                });
            });
        if cancel || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            if editing {
                self.chat.editing = None;
                self.chat.drafts.remove(open);
            }
            self.chat.replying.remove(open);
        }
        ui.add_space(4.0);
    }

    fn composer(&mut self, ui: &mut egui::Ui, open: &str) {
        let typing = self
            .engine
            .snapshot()
            .conversation(open)
            .map(|c| c.typing.clone())
            .unwrap_or_default();
        egui::Panel::bottom("composer").show(ui, |ui| {
            ui.add_space(4.0);
            ui.label(
                RichText::new(if typing.is_empty() {
                    " ".to_string()
                } else {
                    typing_line(&typing)
                })
                .small()
                .italics()
                .color(palette(ui).weak),
            );
            self.composer_strip(ui, open);
            let editing = self
                .chat
                .editing
                .as_ref()
                .filter(|(c, _)| c == open)
                .map(|(_, id)| id.clone());
            let field = egui::Id::new(("composer", open));
            let recent = self.engine.snapshot().preferences.recent_emoji.clone();
            let draft = self.chat.drafts.entry(open.to_string()).or_default();
            let staged = self.chat.staged.entry(open.to_string()).or_default();
            let picker = &mut self.chat.picker;
            let mut send = false;
            let mut wrote = false;
            let mut picked = None;
            let mut note = None;
            super::attach::staged_chips(ui, staged);
            ui.horizontal(|ui| {
                let edit = ui.add(
                    egui::TextEdit::multiline(draft)
                        .id(field)
                        .hint_text("Write a message. Enter sends; Shift+Enter starts a new line.")
                        .desired_rows(2)
                        .desired_width(ui.available_width() - 138.0)
                        .return_key(Some(egui::KeyboardShortcut::new(
                            egui::Modifiers::SHIFT,
                            egui::Key::Enter,
                        ))),
                );
                let enter = edit.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                if edit.changed() && !draft.trim().is_empty() {
                    wrote = true;
                }
                // egui hands the paste shortcut to the text field as text only, and with
                // just a picture on the clipboard nothing comes through, so pasting a
                // picture is in the paperclip's menu.
                let clip = style::icon_button(ui, Icon::Paperclip, 22.0, "Attach files");
                egui::Popup::menu(&clip).show(|ui| {
                    if ui.button("Choose files…").clicked()
                        && let Some(files) = rfd::FileDialog::new()
                            .set_title("Attach files")
                            .pick_files()
                    {
                        note = super::attach::stage(staged, files);
                    }
                    if ui.button("Paste picture").clicked() {
                        note = match super::attach::pasted_picture() {
                            Some(path) => super::attach::stage(staged, vec![path]),
                            None => Some("There is no picture on the clipboard.".to_string()),
                        };
                    }
                });
                let smile = style::icon_button(ui, Icon::Smile, 22.0, "Emoji");
                egui::Popup::from_toggle_button_response(&smile)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .align(egui::RectAlign::TOP_END)
                    .width(330.0)
                    .show(|ui| {
                        if let Some(emoji) = picker.show(ui, &recent) {
                            picked = Some(emoji);
                        }
                    });
                let ready = !draft.trim().is_empty() || (!staged.is_empty() && editing.is_none());
                let label = if editing.is_some() { "Save" } else { "Send" };
                if (ui.add_enabled(ready, egui::Button::new(label)).clicked() || enter) && ready {
                    send = true;
                }
                if enter {
                    edit.request_focus();
                }
            });
            let mut commands = Vec::new();
            let said_lately = self
                .chat
                .typing_said
                .as_ref()
                .is_some_and(|(c, at)| c == open && at.elapsed() < TYPING_EVERY);
            if wrote && !send && editing.is_none() && !said_lately {
                self.chat.typing_said = Some((open.to_string(), std::time::Instant::now()));
                commands.push(Command::Typing {
                    conversation: open.to_string(),
                });
            }
            if let Some(emoji) = picked {
                super::emoji::insert_at_cursor(ui.ctx(), field, draft, &emoji);
                commands.push(Command::SetPreferences {
                    close_to_tray: None,
                    notifications: None,
                    window: None,
                    recent_emoji: Some(super::emoji::used(&recent, &emoji)),
                    auto_fetch_images: None,
                    message_sound: None,
                    settings_window: None,
                });
            }
            let body = send.then(|| (std::mem::take(draft), std::mem::take(staged)));
            for command in commands {
                self.dispatch(command);
            }
            if let Some(note) = note {
                super::push(&self.notices, ui.ctx(), note, false);
            }
            if let Some((body, files)) = body {
                if let Some(message_id) = editing {
                    // Files chosen while editing stay for the next message.
                    self.chat.staged.insert(open.to_string(), files);
                    self.chat.editing = None;
                    self.dispatch(Command::EditMessage {
                        message_id,
                        body: body.trim_end().to_string(),
                    });
                } else {
                    let reply_to = self.chat.replying.remove(open).map(|m| m.id);
                    self.dispatch(Command::SendMessage {
                        conversation: open.to_string(),
                        body: body.trim_end().to_string(),
                        attachments: files.iter().map(|p| p.display().to_string()).collect(),
                        reply_to,
                    });
                }
            }
            ui.add_space(6.0);
        });
    }

    /// A direct chat opened from a device, before its first message.
    fn empty_direct_chat(&mut self, ui: &mut egui::Ui, snap: &Snapshot, open: &str) {
        let me = snap
            .this_device
            .as_ref()
            .map(|d| d.endpoint_id.to_lowercase());
        let device = snap.devices.iter().find(|d| {
            me.as_deref()
                .is_some_and(|me| open.contains(&d.endpoint_id.to_lowercase()) && open.contains(me))
        });
        ui.add_space(6.0);
        match device {
            Some(device) => {
                ui.heading(&device.display_name);
                ui.separator();
                self.composer(ui, open);
                ui.label(RichText::new("No messages yet. Say hello.").color(palette(ui).weak));
            }
            None => {
                self.chat.open = None;
            }
        }
    }

    pub(super) fn new_group_dialog(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let Some(group) = &mut self.chat.new_group else {
            return;
        };
        let (mut cancel, mut create) = (false, false);
        let ready = !group.name.trim().is_empty() && !group.members.is_empty();
        let (close, ()) = style::modal(
            ctx,
            "new-group",
            "New group",
            "Each member sees only the members it is paired with itself.",
            |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut group.name)
                        .hint_text("Group name")
                        .margin(egui::Margin::symmetric(12, 10))
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                ui.label(strong("Paired devices in it", 14.0));
                for device in snap.devices.iter().filter(|d| d.permissions.chat) {
                    let mut chosen = group.members.contains(&device.endpoint_id);
                    if ui.checkbox(&mut chosen, &device.display_name).changed() {
                        if chosen {
                            group.members.insert(device.endpoint_id.clone());
                        } else {
                            group.members.remove(&device.endpoint_id);
                        }
                    }
                }
            },
            |ui| {
                if style::primary(ui, "Create", ready).clicked() {
                    create = true;
                }
                if style::secondary(ui, "Cancel").clicked() {
                    cancel = true;
                }
            },
        );
        if create && ready {
            let group = self.chat.new_group.take().expect("open");
            self.dispatch(Command::CreateGroup {
                name: group.name.trim().to_string(),
                members: group.members.into_iter().collect(),
            });
        } else if cancel || close {
            self.chat.new_group = None;
        }
    }

    /// Opens the direct chat with a paired device on the Chats page.
    pub(super) fn open_direct_chat(&mut self, endpoint_id: &str) {
        if let Some(id) = self.engine.direct_conversation(endpoint_id) {
            self.chat.open = Some(id);
            self.tab = super::Tab::Chats;
        }
    }
}

/// What a bubble needs to show its files.
struct Files<'a> {
    pictures: &'a super::attach::Pictures,
    engine: &'a deskbeam_engine::Engine,
    runtime: &'a tokio::runtime::Handle,
}

/// One message. Returns what was clicked among its files.
fn bubble(
    ui: &mut egui::Ui,
    message: &ChatMessage,
    group: bool,
    files: &Files,
    marked: bool,
) -> Option<Asked> {
    let mut asked = None;
    let layout = if message.mine {
        egui::Layout::top_down(egui::Align::Max)
    } else {
        egui::Layout::top_down(egui::Align::Min)
    };
    ui.with_layout(layout, |ui| {
        let pal = palette(ui);
        let fill = if message.mine { pal.mine } else { pal.card };
        let frame = egui::Frame::new()
            .fill(fill)
            .corner_radius(8.0)
            .stroke(if marked {
                egui::Stroke::new(2.0, ACCENT)
            } else {
                egui::Stroke::NONE
            })
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width() * 0.75);
                if group && !message.mine {
                    ui.label(RichText::new(&message.author_name).small().strong());
                }
                if message.deleted {
                    ui.label(RichText::new("Message deleted").italics().color(pal.weak));
                } else {
                    if let Some(quote) = &message.reply {
                        quoted(ui, quote);
                    }
                    if let Some(a) = super::attach::show(
                        ui,
                        message,
                        files.pictures,
                        files.engine,
                        files.runtime,
                    ) {
                        asked = Some(Asked::Attachment(a));
                    }
                    if !message.body.is_empty() {
                        let body = if super::emoji::only_emoji(&message.body) {
                            RichText::new(&message.body).size(36.0)
                        } else {
                            RichText::new(&message.body)
                        };
                        ui.add(egui::Label::new(body).wrap().selectable(true));
                    }
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(when(message.sent_at)).small().color(pal.weak));
                    if message.edited {
                        ui.label(RichText::new("edited").small().color(pal.weak));
                    }
                    if let Some(delivery) = message.delivery.filter(|_| !message.deleted) {
                        let (mark, tip) = match (delivery.delivered, delivery.waiting) {
                            (_, 0) => (Icon::CheckCheck, "Delivered".to_string()),
                            (0, w) => (
                                Icon::Check,
                                format!("Waiting for {w} device{} to come online", plural(w)),
                            ),
                            (d, w) => (Icon::Check, format!("Delivered to {d}, waiting for {w}")),
                        };
                        let color = if delivery.waiting == 0 {
                            ONLINE
                        } else {
                            pal.weak
                        };
                        style::icon(ui, mark, 14.0, color).on_hover_text(tip);
                    }
                });
            });
        if marked {
            frame.response.scroll_to_me(Some(egui::Align::Center));
        }
        if !message.deleted {
            frame
                .response
                .interact(egui::Sense::click())
                .context_menu(|ui| {
                    if ui.button("Reply").clicked() {
                        asked = Some(Asked::Reply(message.clone()));
                    }
                    if !message.body.is_empty() && ui.button("Copy text").clicked() {
                        ui.ctx().copy_text(message.body.clone());
                    }
                    if message.mine {
                        ui.separator();
                        if !message.body.is_empty() && ui.button("Edit").clicked() {
                            asked = Some(Asked::Edit(message.clone()));
                        }
                        if ui.button("Delete…").clicked() {
                            asked = Some(Asked::Delete(message.clone()));
                        }
                    }
                });
        }
    });
    ui.add_space(4.0);
    asked
}

/// "Ana is typing…", "Ana and Ben are typing…".
fn typing_line(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("{one} is typing…"),
        [one, two] => format!("{one} and {two} are typing…"),
        [one, rest @ ..] => format!("{one} and {} others are typing…", rest.len()),
    }
}

/// The message a reply answers, small, above its text.
fn quoted(ui: &mut egui::Ui, quote: &Quote) {
    let pal = palette(ui);
    let text = if quote.deleted {
        "Message deleted".to_string()
    } else if quote.author_name.is_empty() {
        "A message not on this device".to_string()
    } else {
        quote.text.clone()
    };
    egui::Frame::new()
        .fill(pal.line)
        .corner_radius(4)
        .inner_margin(egui::Margin {
            left: 8,
            right: 8,
            top: 3,
            bottom: 3,
        })
        .show(ui, |ui| {
            let left = ui.max_rect().left_top();
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    left - egui::vec2(8.0, 3.0),
                    egui::vec2(3.0, ui.max_rect().height() + 6.0),
                ),
                0.0,
                ACCENT,
            );
            if !quote.author_name.is_empty() {
                ui.label(RichText::new(&quote.author_name).small().strong());
            }
            ui.label(RichText::new(text).small().color(pal.weak));
        });
    ui.add_space(2.0);
}

/// One line of a message for a list, around where `text` is (or its start).
fn snippet(body: &str, text: &str) -> String {
    let flat = body.replace('\n', " ");
    let needle = text.to_lowercase();
    // The character `text` starts at, compared without case in the text as it is.
    let start_char = if needle.is_empty() {
        0
    } else {
        // Lowercasing only ever lengthens text, so as many characters as the search has
        // are enough to compare.
        let wanted = text.chars().count();
        flat.char_indices()
            .position(|(at, _)| {
                flat[at..]
                    .chars()
                    .take(wanted)
                    .collect::<String>()
                    .to_lowercase()
                    .starts_with(&needle)
            })
            .unwrap_or(0)
    };
    let start = start_char.saturating_sub(20);
    let mut out: String = flat.chars().skip(start).take(70).collect();
    if start > 0 {
        out.insert(0, '…');
    }
    if flat.chars().count() > start + 70 {
        out.push('…');
    }
    out
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
fn when(at_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(at_ms);
    let mins = now.saturating_sub(at_ms) / 60_000;
    match mins {
        0 => "just now".into(),
        1..=59 => format!("{mins} min ago"),
        60..=1439 => format!("{} h ago", mins / 60),
        _ => format!("{} days ago", mins / 1440),
    }
}

/// A row in the chat list, clickable as a whole; widgets inside it (the phone) take their
/// own clicks, since the row's is registered before them. Returns whether the row was
/// clicked.
fn list_row(ui: &mut egui::Ui, selected: bool, content: impl FnOnce(&mut egui::Ui)) -> bool {
    let pal = palette(ui);
    let row = ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
        let hovered = ui.response().hovered();
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(10, 6))
            .corner_radius(6.0)
            .fill(if selected {
                pal.selected
            } else if hovered {
                pal.hover
            } else {
                Color32::TRANSPARENT
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                content(ui);
            });
    });
    row.response.clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_show_the_found_text_and_never_split_a_character() {
        let long = format!("{} needle {}", "a".repeat(60), "b".repeat(60));
        let around = snippet(&long, "NEEDLE");
        assert!(
            around.starts_with('…') && around.contains("needle"),
            "{around}"
        );
        assert_eq!(snippet("short", ""), "short");
        assert_eq!(snippet("line one\nline two", ""), "line one line two");
        // Lowercasing İ makes it longer in bytes; the cut must still be on a character.
        let tricky = format!("{}x{}", "İ".repeat(40), "y".repeat(10));
        let cut = snippet(&tricky, "x");
        assert!(cut.contains('x'), "{cut}");
    }
}
