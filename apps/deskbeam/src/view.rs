//! The main window: three pages behind the icons at the top (Chats, Files, Settings), and
//! the requests that need an answer, as modal dialogs over whichever page is open.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use deskbeam_engine::{
    Command, Engine, Event,
    model::{
        DesktopRequest, Device, DeviceLan, Discoverability, LanMode, LinkKind, NearbyDevice,
        NodeStatus, PairRequest, PairingState, Permission, Snapshot, SoundKind, Transfer,
        TransferState, short_id,
    },
};
use eframe::egui::{self, RichText, Vec2};

use crate::{
    autostart, notify,
    tray::{self, Tray, TrayAction},
};

mod attach;
mod audio;
mod call;
mod chats;
mod emoji;
mod files;
mod share;
mod style;
mod video;
pub mod window;

use style::{ERROR, Icon, ONLINE, WARN, palette, strong};

/// The pages of the window, in the order of their icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Chats,
    Files,
    Settings,
}

/// Permissions shown per device. Auto-accept sits under "Send files".
const SHOWN: [Permission; 5] = [
    Permission::RemoteDesktop,
    Permission::SendFiles,
    Permission::AutoAcceptFiles,
    Permission::Chat,
    Permission::JoinLan,
];

/// A line at the bottom of the window until dismissed.
struct Notice {
    text: String,
    error: bool,
}

type Notices = Arc<Mutex<Vec<Notice>>>;

fn push(notices: &Notices, ctx: &egui::Context, text: String, error: bool) {
    let mut list = notices.lock().expect("notices lock");
    list.push(Notice { text, error });
    // Keep the last few; older ones are stale by then.
    let extra = list.len().saturating_sub(5);
    list.drain(..extra);
    ctx.request_repaint();
}

pub struct DeskBeamApp {
    engine: Engine,
    runtime: tokio::runtime::Handle,
    ctx: egui::Context,
    notices: Notices,
    // What is being typed or confirmed: view state only.
    /// The "Enter pairing code" dialog and what is typed in it, while open.
    pair_dialog: Option<String>,
    renaming: Option<String>,
    /// The Settings window is open.
    settings_open: bool,
    /// The menu asked for the Settings window.
    open_settings: Arc<AtomicBool>,
    /// "Free up space" waits for its OK, or runs.
    confirm_free: bool,
    freeing_space: Arc<AtomicBool>,
    /// Where the Settings window is, saved like the main window's place.
    settings_place: window::Tracker,
    /// The place the Settings window opens at, fixed while it is open (a builder that
    /// changed would move the window).
    settings_opens_at: Option<deskbeam_engine::model::WindowPlace>,
    /// Audio settings being changed, shown until the snapshot has them.
    audio_draft: Option<deskbeam_engine::model::AudioSettings>,
    /// The audio devices were asked for once.
    audio_listed: bool,
    /// The call's video window.
    video: video::VideoView,
    /// The picker of what to share.
    share: share::SharePicker,
    /// A paired device being renamed on this device: its id and the name typed so far.
    renaming_device: Option<(String, String)>,
    confirm_forget: Option<String>,
    // The desktop shell.
    tray: Option<Tray>,
    /// Set by the tray's Quit, so closing the window then ends the app instead of hiding it.
    quitting: Arc<AtomicBool>,
    /// Read from the OS once; it changes only through this window.
    start_at_login: bool,
    tab: Tab,
    chat: chats::ChatState,
    /// The conversation on screen while the window has focus, so its messages need no
    /// notification.
    viewing: Arc<Mutex<Option<String>>>,
    /// Where the window is, to save it when it moves.
    window: window::Tracker,
    /// The message and ring sounds.
    sounds: crate::sound::Sounds,
    /// The ring is playing for an incoming call.
    ringing: bool,
    /// The space chat files take, and when it was measured (walking the folder each frame
    /// would be slow).
    attachments_size: std::cell::Cell<Option<(u64, std::time::Instant)>>,
    /// Files and folders added on the Files page, sent only when the user presses Send.
    staged: Vec<files::Staged>,
    /// The device chosen on the Files page.
    files_to: Option<String>,
}

impl DeskBeamApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        engine: Engine,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        style::install(&cc.egui_ctx);

        // Redraw whenever the engine publishes a new snapshot.
        let ctx = cc.egui_ctx.clone();
        let mut snapshots = engine.watch();
        runtime.spawn(async move {
            while snapshots.changed().await.is_ok() {
                ctx.request_repaint();
            }
        });

        // Say what happened, for the events a snapshot does not explain by itself. Those that
        // need the user also become system notifications, for when the window is closed.
        let notices: Notices = Arc::default();
        let viewing: Arc<Mutex<Option<String>>> = Arc::default();
        let sounds = crate::sound::Sounds::new(engine.clone());
        let (ctx, said, watched, seen, ping) = (
            cc.egui_ctx.clone(),
            notices.clone(),
            engine.clone(),
            viewing.clone(),
            sounds.clone(),
        );
        let mut events = engine.events();
        runtime.spawn(async move {
            loop {
                let notify_on = watched.snapshot().preferences.notifications;
                let text = match events.recv().await {
                    Ok(Event::PairRequested { display_name, .. }) => {
                        if notify_on {
                            notify::show(
                                format!("{display_name} wants to pair"),
                                "Open DeskBeam to compare the code and accept or decline.".into(),
                            );
                        }
                        continue;
                    }
                    Ok(Event::DesktopRequested { display_name, .. }) => {
                        if notify_on {
                            notify::show(
                                format!("{display_name} wants to control this desktop"),
                                "Open DeskBeam to allow or deny. It is refused in a minute \
                                 otherwise."
                                    .into(),
                            );
                        }
                        continue;
                    }
                    Ok(Event::CallIncoming { display_name, .. }) => {
                        crate::show_window(&ctx);
                        if notify_on {
                            notify::show(
                                format!("{display_name} is calling"),
                                "Open DeskBeam to answer.".into(),
                            );
                        }
                        continue;
                    }
                    Ok(Event::CallMissed { display_name, .. }) => {
                        if notify_on {
                            notify::show(format!("Missed call from {display_name}"), String::new());
                        }
                        continue;
                    }
                    Ok(Event::DesktopRefused { reason, .. }) => {
                        push(&said, &ctx, format!("Remote desktop: {reason}."), true);
                        continue;
                    }
                    Ok(Event::ChatMessage {
                        conversation, id, ..
                    }) => {
                        let on_screen =
                            seen.lock().expect("lock").as_deref() == Some(&conversation);
                        // Not for this device's own messages, synced back from another.
                        let mine = watched
                            .chat_messages(&conversation, 30)
                            .iter()
                            .any(|m| m.id == id && m.mine);
                        if !on_screen && !mine && watched.preferences().message_sound {
                            ping.play(SoundKind::Message);
                        }
                        if notify_on && !on_screen {
                            let snap = watched.snapshot();
                            let chat = snap.conversation(&conversation);
                            let message = watched
                                .chat_messages(&conversation, 30)
                                .into_iter()
                                .find(|m| m.id == id);
                            if let (Some(chat), Some(message)) = (chat, message) {
                                let title = match chat.kind {
                                    deskbeam_engine::model::ConversationKind::Group { .. } => {
                                        format!("{} in {}", message.author_name, chat.title)
                                    }
                                    _ => message.author_name.clone(),
                                };
                                let body: String = message.body.chars().take(200).collect();
                                notify::show_message(conversation.clone(), title, body);
                            }
                        }
                        continue;
                    }
                    Ok(Event::FilesOffered {
                        display_name,
                        file_count,
                        total,
                        ..
                    }) => {
                        if notify_on {
                            let files = if file_count == 1 {
                                "a file".to_string()
                            } else {
                                format!("{file_count} files")
                            };
                            notify::show(
                                format!("{display_name} wants to send {files}"),
                                format!(
                                    "{}. Open DeskBeam to accept or decline.",
                                    files::size(total)
                                ),
                            );
                        }
                        continue;
                    }
                    Ok(Event::Paired { display_name }) => {
                        if notify_on {
                            notify::show(
                                format!("Paired with {display_name}"),
                                "It now appears under Paired devices.".into(),
                            );
                        }
                        format!("Paired with {display_name}.")
                    }
                    Ok(Event::InviteAnswered {
                        endpoint_id,
                        display_name,
                        accepted,
                    }) => {
                        let who = display_name.unwrap_or_else(|| short_id(&endpoint_id).into());
                        if accepted {
                            format!("{who} accepted.")
                        } else {
                            format!("{who} declined.")
                        }
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                push(&said, &ctx, text, false);
            }
        });

        // The tray's menu runs on the main thread; it only sends the window commands.
        let quitting = Arc::new(AtomicBool::new(false));
        let open_settings = Arc::new(AtomicBool::new(false));
        let (ctx, quit, settings) = (cc.egui_ctx.clone(), quitting.clone(), open_settings.clone());
        let tray = match Tray::new(move |action| match action {
            TrayAction::Open => crate::show_window(&ctx),
            TrayAction::Settings => {
                settings.store(true, Ordering::SeqCst);
                crate::show_window(&ctx);
            }
            TrayAction::Quit => {
                quit.store(true, Ordering::SeqCst);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ctx.request_repaint();
            }
        }) {
            Ok(tray) => Some(tray),
            Err(e) => {
                if tray::AVAILABLE {
                    tracing::warn!("no tray icon: {e:#}");
                }
                None
            }
        };

        let engine_has_devices = !engine.snapshot().devices.is_empty();
        let engine_window = engine.preferences().window;
        let settings_window_place = engine.preferences().settings_window;
        Self {
            engine,
            runtime,
            ctx: cc.egui_ctx.clone(),
            notices,
            pair_dialog: None,
            renaming: None,
            settings_open: false,
            open_settings,
            confirm_free: false,
            freeing_space: Arc::default(),
            audio_draft: None,
            audio_listed: false,
            video: video::VideoView::default(),
            share: share::SharePicker::default(),
            renaming_device: None,
            confirm_forget: None,
            tray,
            quitting,
            start_at_login: autostart::is_enabled(),
            // Nothing to chat with before the first pairing.
            tab: if engine_has_devices {
                Tab::Chats
            } else {
                Tab::Settings
            },
            chat: chats::ChatState::default(),
            viewing,
            staged: Vec::new(),
            window: window::Tracker::new(engine_window),
            settings_place: window::Tracker::new(settings_window_place),
            settings_opens_at: None,
            sounds,
            ringing: false,
            attachments_size: std::cell::Cell::new(None),
            files_to: None,
        }
    }

    /// Closing the window hides it to the tray, unless the tray's Quit asked for the close,
    /// there is no tray, or the user chose to quit on close.
    fn close_to_tray(&mut self, ui: &egui::Ui, snap: &Snapshot) {
        let ctx = ui.ctx();
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quitting.load(Ordering::SeqCst)
            && self.tray.is_some()
            && snap.preferences.close_to_tray
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            // The Settings window would stay up alone, and stop repainting.
            if self.settings_open {
                self.settings_window_closed();
            }
            self.settings_open = false;
            if snap.audio_test.is_some() {
                self.dispatch(Command::TestAudio { on: false });
            }
        }
    }

    fn tray_status(&mut self, snap: &Snapshot) {
        let Some(tray) = &mut self.tray else { return };
        let waiting = snap.pair_requests.len() + snap.desktop_requests.len();
        let sharing = snap
            .call
            .as_ref()
            .is_some_and(|c| c.is_live() && c.sharing.is_some());
        let text = match (&snap.node, waiting) {
            _ if sharing => "Sharing your screen".to_string(),
            (NodeStatus::Starting, _) => "Starting…".to_string(),
            (NodeStatus::Failed { .. }, _) => "Not connected".to_string(),
            (NodeStatus::Stopped, _) => "Stopped".to_string(),
            (NodeStatus::Running { .. }, 0) => format!(
                "{} of {} devices online",
                snap.online_count(),
                snap.devices.len()
            ),
            (NodeStatus::Running { .. }, 1) => "1 request is waiting".to_string(),
            (NodeStatus::Running { .. }, n) => format!("{n} requests are waiting"),
        };
        tray.set_status(&text);
    }

    /// Sounds: on or off for messages, and which sound for messages and for calls.
    fn sound_settings(&mut self, ui: &mut egui::Ui, prefs: &deskbeam_engine::model::Preferences) {
        let pal = palette(ui);
        let mut on = prefs.message_sound;
        if ui
            .checkbox(&mut on, "Play a sound for new messages")
            .changed()
        {
            self.dispatch(Command::SetPreferences {
                close_to_tray: None,
                notifications: None,
                window: None,
                recent_emoji: None,
                auto_fetch_images: None,
                message_sound: Some(on),
                settings_window: None,
            });
        }
        for (kind, label, built_in, tone) in [
            (
                SoundKind::Message,
                "Message sound",
                "Soft ping (built in)",
                &prefs.message_tone,
            ),
            (
                SoundKind::Ring,
                "Ring sound",
                "Drin drin (built in)",
                &prefs.ring_tone,
            ),
        ] {
            ui.horizontal(|ui| {
                ui.label(format!("{label}:"));
                let name = tone.as_ref().map_or(built_in, |t| t.name.as_str());
                ui.label(RichText::new(name).color(pal.weak));
                if ui.small_button("Test").clicked() {
                    self.sounds.play(kind);
                }
                if ui
                    .small_button("Choose…")
                    .on_hover_text("A WAV, MP3, OGG or FLAC file; DeskBeam keeps a copy")
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .set_title(format!("Choose the {}", label.to_lowercase()))
                        .add_filter("Sounds", &["wav", "mp3", "ogg", "oga", "flac"])
                        .pick_file()
                {
                    self.dispatch(Command::SetSound {
                        kind,
                        path: Some(path.display().to_string()),
                    });
                }
                if tone.is_some() && ui.small_button("Built-in").clicked() {
                    self.dispatch(Command::SetSound { kind, path: None });
                }
            });
        }
    }

    /// The space chat files take, measured at most every few seconds.
    fn attachments_size(&self) -> u64 {
        match self.attachments_size.get() {
            Some((bytes, at)) if at.elapsed() < std::time::Duration::from_secs(5) => bytes,
            _ => {
                let bytes = self.engine.attachments_size();
                self.attachments_size
                    .set(Some((bytes, std::time::Instant::now())));
                bytes
            }
        }
    }

    /// "Files in chats take …", with "Free up space", which asks first.
    fn free_space_row(&mut self, ui: &mut egui::Ui) {
        let pal = palette(ui);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "Files in chats take {} on this device.",
                    files::size(self.attachments_size())
                ))
                .small()
                .color(pal.weak),
            );
            if self.freeing_space.load(Ordering::SeqCst) {
                ui.spinner();
                ui.label(RichText::new("Freeing space…").small().color(pal.weak));
            } else if self.confirm_free {
                ui.label(
                    RichText::new("Remove the files you received? Your messages stay.").small(),
                );
                if ui.small_button("Remove").clicked() {
                    self.confirm_free = false;
                    self.free_space();
                }
                if ui.small_button("Cancel").clicked() {
                    self.confirm_free = false;
                }
            } else if ui
                .small_button("Free up space…")
                .on_hover_text(
                    "Removes files you received in chats from this device. Files you sent stay, \
                     as others may still fetch them from you. A removed file can be downloaded \
                     again while someone who has it is online.",
                )
                .clicked()
            {
                self.confirm_free = true;
            }
        });
    }

    fn free_space(&self) {
        let (engine, notices, ctx, busy) = (
            self.engine.clone(),
            self.notices.clone(),
            self.ctx.clone(),
            self.freeing_space.clone(),
        );
        busy.store(true, Ordering::SeqCst);
        self.attachments_size.set(None);
        self.runtime.spawn(async move {
            let text = match engine.execute(Command::FreeAttachmentSpace).await {
                Ok(done) => {
                    let files = done["files"].as_u64().unwrap_or(0);
                    let freed = done["freed"].as_u64().unwrap_or(0);
                    if files == 0 {
                        "There were no received files to remove.".to_string()
                    } else {
                        format!(
                            "Freed {} ({files} file{}).",
                            files::size(freed),
                            if files == 1 { "" } else { "s" }
                        )
                    }
                }
                Err(e) => format!("Freeing space: {e}"),
            };
            busy.store(false, Ordering::SeqCst);
            push(&notices, &ctx, text, false);
        });
    }

    /// Sends a command to the engine without waiting for it. A failure becomes a notice.
    fn dispatch(&self, command: Command) {
        let engine = self.engine.clone();
        let notices = self.notices.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let id = command.id();
            if let Err(e) = engine.execute(command).await {
                push(&notices, &ctx, format!("{id}: {e}"), true);
            }
        });
    }

    /// The page icons on the left, this device's name on the right.
    fn header(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let pal = palette(ui);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let waiting_files = snap
                .transfers
                .iter()
                .any(|t| t.state == TransferState::Incoming);
            for (tab, which, tip, flagged) in [
                (Tab::Chats, Icon::Chats, "Chats", snap.unread() > 0),
                (Tab::Files, Icon::Files, "Files", waiting_files),
                (Tab::Settings, Icon::Settings, "Devices and settings", false),
            ] {
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::splat(44.0), egui::Sense::click());
                let color = if self.tab == tab || response.hovered() {
                    pal.text
                } else {
                    pal.idle
                };
                egui::Image::new(which.source())
                    .tint(color)
                    .paint_at(ui, rect);
                if flagged {
                    ui.painter().circle_filled(
                        rect.right_top() + Vec2::new(-3.0, 5.0),
                        4.5,
                        style::ACCENT,
                    );
                }
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.tab = tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                ui.add_space(8.0);
                let name = snap
                    .this_device
                    .as_ref()
                    .map(|d| d.display_name.as_str())
                    .unwrap_or("DeskBeam");
                let color = match &snap.node {
                    NodeStatus::Running {
                        network_ready: true,
                    } => None,
                    NodeStatus::Failed { .. } => Some((ERROR, "Not connected")),
                    NodeStatus::Stopped => Some((pal.idle, "Stopped")),
                    _ => Some((WARN, "Connecting…")),
                };
                ui.label(RichText::new(name).size(18.0));
                if let Some((color, text)) = color {
                    ui.label(RichText::new(text).color(color));
                }
            });
        });
        if let NodeStatus::Failed { reason } = &snap.node {
            ui.colored_label(ERROR, reason);
        }
        ui.add_space(6.0);
    }

    /// The Settings page: this device, pairing, paired and nearby devices. Preferences are
    /// in their own window ([`Self::settings_window`]).
    fn settings_page(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(12, 14))
                    .show(ui, |ui| {
                        self.this_device(ui, snap);
                        ui.add_space(22.0);
                        self.devices(ui, snap);
                        ui.add_space(22.0);
                        self.nearby(ui, snap);
                    });
            });
    }

    fn this_device(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let pal = palette(ui);
        let running = matches!(snap.node, NodeStatus::Running { .. });
        ui.horizontal(|ui| {
            style::section(ui, "This device");
            if tray::APP_MENU {
                // In the menu bar instead: DeskBeam → Settings… (⌘,).
                return;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let gear = egui::Image::new(Icon::Settings.source())
                    .fit_to_exact_size(Vec2::splat(16.0))
                    .tint(pal.text);
                if ui
                    .add(
                        egui::Button::image_and_text(gear, RichText::new("Settings").size(15.0))
                            .frame(false),
                    )
                    .on_hover_text(
                        "Remote Desktop, LAN mode, notifications, sounds, start at login",
                    )
                    .clicked()
                {
                    self.settings_open = true;
                }
            });
        });
        ui.add_space(8.0);
        let Some(me) = &snap.this_device else {
            ui.spinner();
            return;
        };
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| match &mut self.renaming {
                    Some(name) => {
                        let edit = ui.add(egui::TextEdit::singleline(name).desired_width(220.0));
                        let enter =
                            edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.button("Save").clicked() || enter {
                            let name = std::mem::take(name);
                            self.renaming = None;
                            self.dispatch(Command::RenameThisDevice { name });
                        } else if ui.button("Cancel").clicked() {
                            self.renaming = None;
                        }
                    }
                    None => {
                        ui.label(RichText::new(&me.display_name).size(18.0))
                            .on_hover_text(&me.endpoint_id);
                        if style::icon_button(ui, Icon::Pencil, 16.0, "Rename this device")
                            .clicked()
                        {
                            self.renaming = Some(me.display_name.clone());
                        }
                    }
                });
                ui.label(RichText::new(kind(&me.device_type, &me.os)).color(pal.weak));
                ui.horizontal(|ui| {
                    ui.label(RichText::new("This device's code").small().color(pal.weak));
                    ui.label(
                        RichText::new(&me.fingerprint)
                            .monospace()
                            .small()
                            .color(pal.weak),
                    );
                })
                .response
                .on_hover_text("Another device asking to pair shows this code. Compare it.");
            });
            ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                ui.label(RichText::new("Pairing code").size(18.0));
                match &snap.pairing {
                    Some(pairing) => {
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if style::icon_button(ui, Icon::Close, 16.0, "Stop pairing")
                                        .clicked()
                                    {
                                        self.dispatch(Command::ClosePairing);
                                    }
                                    if style::icon_button(ui, Icon::Copy, 16.0, "Copy the code")
                                        .clicked()
                                    {
                                        ui.ctx().copy_text(pairing.code.clone());
                                        push(
                                            &self.notices,
                                            &self.ctx,
                                            "Pairing code copied. It works for 10 minutes.".into(),
                                            false,
                                        );
                                    }
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&pairing.code)
                                                .monospace()
                                                .color(pal.weak),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(
                                        "Enter this on the other device, under Enter pairing code.",
                                    );
                                },
                            );
                        });
                    }
                    None => {
                        if ui
                            .add_enabled(running, egui::Button::new("Show pairing code"))
                            .on_hover_text("A code for another device to pair with this one.")
                            .clicked()
                        {
                            self.dispatch(Command::OpenPairing {
                                ttl_secs: Some(600),
                            });
                        }
                    }
                }
            });
        });
    }

    fn nearby(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let pal = palette(ui);
        ui.horizontal(|ui| {
            style::section(ui, "On this network");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut chosen = snap.discoverability;
                egui::ComboBox::from_id_salt("discoverability")
                    .selected_text(chosen.label())
                    .show_ui(ui, |ui| {
                        for setting in Discoverability::ALL {
                            ui.selectable_value(&mut chosen, setting, setting.label());
                        }
                    });
                if chosen != snap.discoverability {
                    self.dispatch(Command::SetDiscoverability { setting: chosen });
                }
                ui.label(RichText::new("Visible to").color(pal.weak));
            });
        });
        ui.add_space(4.0);
        if snap.nearby.is_empty() {
            ui.label(
                RichText::new(
                    "No other DeskBeam devices found here. Guest Wi-Fi and VPNs often hide them.",
                )
                .color(pal.weak),
            );
            return;
        }
        for device in &snap.nearby {
            self.nearby_row(ui, device);
        }
    }

    fn nearby_row(&mut self, ui: &mut egui::Ui, device: &NearbyDevice) {
        let pal = palette(ui);
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let os = device.os.as_deref().unwrap_or("");
                    style::icon(ui, Icon::for_device(os), 20.0, pal.weak);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(device.name()).size(15.0));
                        ui.horizontal(|ui| {
                            if !os.is_empty() {
                                ui.label(RichText::new(os).color(pal.weak));
                            }
                            ui.label(
                                RichText::new(&device.fingerprint)
                                    .monospace()
                                    .color(pal.weak),
                            )
                            .on_hover_text("This device's code. Its screen shows the same one.");
                        });
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Ask to pair").clicked() {
                            self.dispatch(Command::RequestNearbyPair {
                                endpoint_id: device.endpoint_id.clone(),
                            });
                        }
                    });
                });
            });
        style::rule(ui);
    }

    fn devices(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let pal = palette(ui);
        ui.horizontal(|ui| {
            style::section(ui, "Paired devices");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let running = matches!(snap.node, NodeStatus::Running { .. });
                let plus = egui::Image::new(Icon::Plus.source())
                    .fit_to_exact_size(Vec2::splat(16.0))
                    .tint(pal.text);
                if ui
                    .add_enabled(
                        running,
                        egui::Button::image_and_text(
                            plus,
                            RichText::new("Enter pairing code").size(15.0),
                        )
                        .frame(false),
                    )
                    .clicked()
                {
                    self.pair_dialog = Some(String::new());
                }
            });
        });
        ui.add_space(6.0);
        if snap.devices.is_empty() {
            ui.label(
                RichText::new(
                    "No paired devices yet. Show a pairing code here and enter it on the other \
                     device, or ask a device on this network below.",
                )
                .color(pal.weak),
            );
            return;
        }
        for device in &snap.devices {
            self.device_row(ui, device);
        }
    }

    fn device_row(&mut self, ui: &mut egui::Ui, device: &Device) {
        let pal = palette(ui);
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(8, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(170.0, 40.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_min_width(170.0);
                            self.device_name(ui, device);
                            ui.label(
                                RichText::new(kind(&device.device_type, &device.os))
                                    .color(pal.weak),
                            );
                        },
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if style::icon_button(ui, Icon::Trash, 18.0, "Unpair").clicked() {
                            self.confirm_forget = Some(device.endpoint_id.clone());
                        }
                        ui.add_space(12.0);
                        let status = style::status(ui, device.online);
                        match (device.online, device.link) {
                            (true, Some(link)) => {
                                let how = match link.kind {
                                    LinkKind::Direct => "Straight between the two devices",
                                    LinkKind::Relayed => {
                                        "Through a relay, which only sees encrypted traffic"
                                    }
                                };
                                status.on_hover_text(format!("{how}, {} ms.", link.rtt_ms));
                            }
                            (false, _) => {
                                status.on_hover_text(last_seen(device.last_seen_at));
                            }
                            _ => {}
                        }
                        ui.add_space(8.0);
                        if device.desktop.is_none() && device.online {
                            self.connect_button(ui, device);
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.horizontal_wrapped(|ui| self.permissions(ui, device));
                        });
                    });
                });
                match device.pairing {
                    PairingState::Active => {}
                    PairingState::UnpairedRemotely => {
                        ui.colored_label(WARN, "Unpaired on the other device.");
                    }
                    PairingState::StaleLocalIdentity => {
                        ui.colored_label(WARN, "Pair again: this device's identity changed.");
                    }
                }
                if device.desktop.is_some() {
                    self.desktop_session(ui, device);
                }
                if let Some(lan) = &device.lan {
                    lan_row(ui, lan);
                }
            });
        style::rule(ui);
    }

    /// A paired device's name with a pencil, or the box to rename it on this device.
    fn device_name(&mut self, ui: &mut egui::Ui, device: &Device) {
        let editing = matches!(&self.renaming_device, Some((id, _)) if *id == device.endpoint_id);
        if !editing {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&device.display_name).size(15.0))
                    .on_hover_text(&device.endpoint_id);
                if style::icon_button(
                    ui,
                    Icon::Pencil,
                    15.0,
                    "Rename on this device. Leave it empty for the device's own name.",
                )
                .clicked()
                {
                    self.renaming_device =
                        Some((device.endpoint_id.clone(), device.display_name.clone()));
                }
            });
            return;
        }
        let Some((id, name)) = &mut self.renaming_device else {
            return;
        };
        let edit = ui.add(egui::TextEdit::singleline(name).desired_width(160.0));
        if !edit.has_focus() && !edit.lost_focus() {
            edit.request_focus();
        }
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let mut save = enter;
        let mut cancel = escape;
        ui.horizontal(|ui| {
            save |= ui.small_button("Save").clicked();
            cancel |= ui.small_button("Cancel").clicked();
        });
        if save {
            let command = Command::SetDeviceName {
                endpoint_id: id.clone(),
                name: std::mem::take(name),
            };
            self.renaming_device = None;
            self.dispatch(command);
        } else if cancel {
            self.renaming_device = None;
        }
    }

    fn permissions(&mut self, ui: &mut egui::Ui, device: &Device) {
        for permission in SHOWN {
            let mut allowed = device.permissions.get(permission);
            let enabled =
                permission != Permission::AutoAcceptFiles || device.permissions.send_files;
            let mut response = ui.add_enabled(
                enabled,
                egui::Checkbox::new(&mut allowed, permission.label()),
            );
            if permission == Permission::JoinLan {
                response = response.on_hover_text(
                    "Lets this device into this one's LAN while LAN mode is on. It can then \
                     reach every service on this computer. It must allow this one too.",
                );
            }
            if response.changed() {
                self.dispatch(Command::SetPermission {
                    endpoint_id: device.endpoint_id.clone(),
                    permission,
                    allowed,
                });
            }
        }
    }

    fn connect_button(&mut self, ui: &mut egui::Ui, device: &Device) {
        if ui
            .button("Connect desktop")
            .on_hover_text(
                "Opens your Remote Desktop app on this device's desktop. It must have Remote \
                 Desktop turned on.",
            )
            .clicked()
        {
            self.dispatch(Command::ConnectDesktop {
                endpoint_id: device.endpoint_id.clone(),
                launch: true,
            });
        }
    }

    /// The open tunnel to the device's desktop: its address, Copy, Open again, Disconnect.
    fn desktop_session(&mut self, ui: &mut egui::Ui, device: &Device) {
        let pal = palette(ui);
        let Some(session) = device.desktop else {
            return;
        };
        ui.horizontal(|ui| {
            ui.add_space(178.0);
            let address = format!("localhost:{}", session.port);
            ui.label(strong("Desktop", 14.0));
            ui.label(RichText::new(&address).monospace())
                .on_hover_text("Enter this as the PC name in your Remote Desktop app.");
            let open = match session.connections {
                0 => "no session".to_string(),
                1 => "1 connection".to_string(),
                n => format!("{n} connections"),
            };
            ui.label(RichText::new(open).color(pal.weak));
            if style::icon_button(ui, Icon::Copy, 16.0, "Copy the address").clicked() {
                ui.ctx().copy_text(address);
            }
            if device.online && ui.small_button("Open again").clicked() {
                self.dispatch(Command::ConnectDesktop {
                    endpoint_id: device.endpoint_id.clone(),
                    launch: true,
                });
            }
            if ui.small_button("Disconnect").clicked() {
                self.dispatch(Command::DisconnectDesktop {
                    endpoint_id: device.endpoint_id.clone(),
                });
            }
        });
    }

    /// The Settings window: its own native window while open. Closing it (or Escape) only
    /// closes it.
    fn settings_window(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let id = egui::ViewportId::from_hash_of("deskbeam-settings");
        if self.open_settings.swap(false, Ordering::SeqCst) {
            if self.settings_open {
                // Already open, maybe behind: bring it forward.
                ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
            } else {
                self.settings_opens_at = self.settings_place.saved();
            }
            self.settings_open = true;
        }
        if !self.settings_open {
            return;
        }
        let builder = window::placed(
            egui::ViewportBuilder::default()
                .with_title("DeskBeam Settings")
                .with_inner_size([560.0, 520.0])
                .with_min_inner_size([420.0, 320.0]),
            self.settings_opens_at,
        );
        ctx.show_viewport_immediate(id, builder, |ui, _class| {
            let closing = ui
                .ctx()
                .input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape));
            if !closing {
                self.track_settings_window(&ui.ctx().clone());
            }
            if closing {
                self.settings_open = false;
                self.settings_window_closed();
                if snap.audio_test.is_some() {
                    self.dispatch(Command::TestAudio { on: false });
                }
                return;
            }
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(12, 14))
                            .show(ui, |ui| self.settings(ui, snap));
                    });
            });
        });
    }

    fn settings(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let pal = palette(ui);
        style::section(ui, "General");
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("This desktop:");
            if snap.desktop_host.listening {
                ui.label(
                    RichText::new("Remote Desktop is on; devices you allow can connect.")
                        .color(ONLINE),
                );
            } else {
                ui.label(RichText::new(&snap.desktop_host.hint).color(pal.weak));
            }
        });
        self.lan_mode(ui, snap);
        let mut prefs = snap.preferences.clone();
        ui.add_enabled_ui(self.tray.is_some(), |ui| {
            if ui
                .checkbox(
                    &mut prefs.close_to_tray,
                    "Keep running in the tray when the window is closed",
                )
                .on_disabled_hover_text("No tray on this system yet.")
                .changed()
            {
                self.dispatch(Command::SetPreferences {
                    close_to_tray: Some(prefs.close_to_tray),
                    notifications: None,
                    window: None,
                    recent_emoji: None,
                    auto_fetch_images: None,
                    message_sound: None,
                    settings_window: None,
                });
            }
        });
        if ui
            .checkbox(
                &mut prefs.notifications,
                "Notify me of pair requests and new pairings",
            )
            .changed()
        {
            self.dispatch(Command::SetPreferences {
                close_to_tray: None,
                notifications: Some(prefs.notifications),
                window: None,
                recent_emoji: None,
                auto_fetch_images: None,
                message_sound: None,
                settings_window: None,
            });
        }
        if ui
            .checkbox(
                &mut prefs.auto_fetch_images,
                "Download pictures in chats as they arrive (up to 10 MB)",
            )
            .on_hover_text("Off: pictures show as small previews until you open them.")
            .changed()
        {
            self.dispatch(Command::SetPreferences {
                close_to_tray: None,
                notifications: None,
                window: None,
                recent_emoji: None,
                auto_fetch_images: Some(prefs.auto_fetch_images),
                message_sound: None,
                settings_window: None,
            });
        }
        self.free_space_row(ui);
        self.sound_settings(ui, &prefs);
        let mut login = self.start_at_login;
        if ui
            .checkbox(&mut login, "Start DeskBeam when I log in")
            .changed()
        {
            match autostart::set_enabled(login) {
                Ok(()) => self.start_at_login = login,
                Err(e) => push(
                    &self.notices,
                    &self.ctx,
                    format!("start at login: {e:#}"),
                    true,
                ),
            }
        }
        ui.add_space(22.0);
        style::section(ui, "Calls");
        ui.add_space(6.0);
        self.audio_settings(ui, snap);
    }

    /// LAN mode: off, waiting for admin rights, on with this device's address, or failed.
    fn lan_mode(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        const ABOUT: &str = "Paired devices you allow (\"LAN mode\" on each device) get \
            addresses on a private network with this one, so file shares, ping, games and \
            Remote Desktop to the address work as on a local network. They can reach every \
            service on this computer. Needs administrator rights.";
        let pal = palette(ui);
        ui.horizontal_wrapped(|ui| {
            ui.label("LAN mode:").on_hover_text(ABOUT);
            match &snap.lan {
                LanMode::Off => {
                    ui.label(RichText::new("off").color(pal.weak));
                    if ui.button("Turn on…").on_hover_text(ABOUT).clicked() {
                        self.dispatch(Command::StartLan);
                    }
                }
                LanMode::Starting => {
                    ui.spinner();
                    ui.label(
                        RichText::new(
                            "making the network adapter (the system may ask for your password)",
                        )
                        .color(pal.weak),
                    );
                }
                LanMode::On { address, adapter } => {
                    ui.label(RichText::new("this device is").color(ONLINE));
                    ui.label(RichText::new(address).monospace())
                        .on_hover_text(format!("On the network adapter {adapter}."));
                    if style::icon_button(ui, Icon::Copy, 16.0, "Copy the address").clicked() {
                        ui.ctx().copy_text(address.clone());
                    }
                    if ui.small_button("Turn off").clicked() {
                        self.dispatch(Command::StopLan);
                    }
                }
                LanMode::Failed { reason } => {
                    ui.label(RichText::new(reason).color(ERROR));
                    if ui.small_button("Try again").clicked() {
                        self.dispatch(Command::StartLan);
                    }
                }
            }
        });
    }

    /// One modal at a time, the most urgent first: remote desktop (refused unanswered after
    /// a minute), pairing, file offers, then the dialogs this window opened.
    fn modals(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        if self.incoming_call(ctx, snap) {
            // A call comes first: it can't wait.
        } else if self.share_picker(ctx, snap) {
            // Opened by the user during a call.
        } else if let Some(request) = snap.desktop_requests.first() {
            self.desktop_request(ctx, request);
        } else if let Some(request) = snap.pair_requests.first() {
            self.pair_request(ctx, request);
        } else if let Some(offer) = snap
            .transfers
            .iter()
            .find(|t| t.state == TransferState::Incoming)
        {
            self.file_offer(ctx, offer);
        } else if self.pair_dialog.is_some() {
            self.pair_dialog(ctx, snap);
        } else if let Some(id) = self.confirm_forget.clone() {
            self.forget_dialog(ctx, snap, &id);
        } else {
            self.new_group_dialog(ctx, snap);
        }
    }

    fn desktop_request(&mut self, ctx: &egui::Context, request: &DesktopRequest) {
        let name = if request.display_name.is_empty() {
            short_id(&request.endpoint_id)
        } else {
            &request.display_name
        };
        let answer = |allow, always| Command::AnswerDesktopRequest {
            endpoint_id: request.endpoint_id.clone(),
            allow,
            always,
        };
        let mut chosen = None;
        style::modal(
            ctx,
            "desktop-request",
            &format!("{name} wants to control this desktop"),
            "Over Remote Desktop. It will still need this computer's account and password.",
            |ui| {
                ui.label(
                    RichText::new("Refused by itself in a minute if not answered.")
                        .color(palette(ui).weak),
                );
            },
            |ui| {
                if style::primary(ui, "Allow once", true)
                    .on_hover_text("For the next 30 minutes.")
                    .clicked()
                {
                    chosen = Some(answer(true, false));
                }
                if style::secondary(ui, "Always allow")
                    .on_hover_text("Turns on \"Can control this desktop\" for this device.")
                    .clicked()
                {
                    chosen = Some(answer(true, true));
                }
                if style::secondary(ui, "Deny").clicked() {
                    chosen = Some(answer(false, false));
                }
            },
        );
        if let Some(command) = chosen {
            self.dispatch(command);
        }
    }

    fn pair_request(&mut self, ctx: &egui::Context, request: &PairRequest) {
        let name = if request.display_name.is_empty() {
            short_id(&request.endpoint_id)
        } else {
            &request.display_name
        };
        let id = request.endpoint_id.clone();
        let mut chosen = None;
        let subtitle = if request.os.is_empty() {
            String::new()
        } else {
            request.os.clone()
        };
        style::modal(
            ctx,
            "pair-request",
            &format!("{name} wants to pair"),
            &subtitle,
            |ui| {
                ui.label("Pair only if its screen shows this code:");
                ui.label(RichText::new(&request.fingerprint).monospace().size(18.0));
            },
            |ui| {
                if style::primary(ui, "Accept", true).clicked() {
                    chosen = Some(Command::AcceptPairRequest {
                        endpoint_id: id.clone(),
                    });
                }
                if style::secondary(ui, "Decline").clicked() {
                    chosen = Some(Command::DeclinePairRequest {
                        endpoint_id: id.clone(),
                        block: false,
                    });
                }
                if style::secondary(ui, "Block")
                    .on_hover_text("Decline, and ignore this device until DeskBeam restarts.")
                    .clicked()
                {
                    chosen = Some(Command::DeclinePairRequest {
                        endpoint_id: id.clone(),
                        block: true,
                    });
                }
            },
        );
        if let Some(command) = chosen {
            self.dispatch(command);
        }
    }

    fn file_offer(&mut self, ctx: &egui::Context, offer: &Transfer) {
        let mut chosen = None;
        let size = if offer.total > 0 {
            files::size(offer.total)
        } else {
            String::new()
        };
        style::modal(
            ctx,
            "file-offer",
            &format!("{} wants to send you files", offer.peer_name),
            &size,
            |ui| {
                ui.label(files::what(offer));
            },
            |ui| {
                if style::primary(ui, "Accept", true).clicked() {
                    chosen = Some(Command::AcceptFiles {
                        transfer_id: offer.id.clone(),
                    });
                }
                if style::secondary(ui, "Decline").clicked() {
                    chosen = Some(Command::DeclineFiles {
                        transfer_id: offer.id.clone(),
                    });
                }
            },
        );
        if let Some(command) = chosen {
            if matches!(command, Command::AcceptFiles { .. }) {
                self.tab = Tab::Files;
            }
            self.dispatch(command);
        }
    }

    fn pair_dialog(&mut self, ctx: &egui::Context, snap: &Snapshot) {
        let running = matches!(snap.node, NodeStatus::Running { .. });
        let Some(code) = &mut self.pair_dialog else {
            return;
        };
        let (mut cancel, mut pair) = (false, false);
        let ready = running && !code.trim().is_empty();
        let (close, entered) = style::modal(
            ctx,
            "pair-dialog",
            "Enter pairing code",
            "Paste the pairing code from your other device.",
            |ui| {
                let edit = ui.add(
                    egui::TextEdit::singleline(code)
                        .hint_text("Paste pairing code")
                        .font(egui::TextStyle::Monospace)
                        .margin(egui::Margin::symmetric(12, 10))
                        .desired_width(f32::INFINITY),
                );
                if !edit.has_focus() && code.is_empty() {
                    edit.request_focus();
                }
                edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
            },
            |ui| {
                if style::primary(ui, "Pair", ready).clicked() {
                    pair = true;
                }
                if style::secondary(ui, "Cancel").clicked() {
                    cancel = true;
                }
            },
        );
        if (pair || entered) && running && !code.trim().is_empty() {
            let code = code.trim().to_string();
            self.pair_dialog = None;
            self.dispatch(Command::JoinPairing { code });
        } else if cancel || close {
            self.pair_dialog = None;
        }
    }

    fn forget_dialog(&mut self, ctx: &egui::Context, snap: &Snapshot, id: &str) {
        let Some(device) = snap.device(id) else {
            self.confirm_forget = None;
            return;
        };
        let (mut keep, mut forget) = (false, false);
        let (close, ()) = style::modal(
            ctx,
            "forget-dialog",
            &format!("Unpair {}?", device.display_name),
            "It can no longer reach this device. Chat history stays here.",
            |_| {},
            |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Unpair").color(egui::Color32::WHITE))
                            .fill(ERROR)
                            .corner_radius(10)
                            .min_size(Vec2::new(0.0, 36.0)),
                    )
                    .clicked()
                {
                    forget = true;
                }
                if style::secondary(ui, "Keep").clicked() {
                    keep = true;
                }
            },
        );
        if forget {
            self.dispatch(Command::ForgetDevice {
                endpoint_id: id.to_string(),
            });
            self.confirm_forget = None;
        } else if keep || close {
            self.confirm_forget = None;
        }
    }

    fn notices(&mut self, ui: &mut egui::Ui) {
        let mut notices = self.notices.lock().expect("notices lock");
        if notices.is_empty() {
            return;
        }
        egui::Panel::bottom("notices").show(ui, |ui| {
            ui.add_space(4.0);
            let mut dismiss = None;
            for (i, notice) in notices.iter().enumerate() {
                ui.horizontal(|ui| {
                    if notice.error {
                        ui.colored_label(ERROR, &notice.text);
                    } else {
                        ui.label(&notice.text);
                    }
                    if style::icon_button(ui, Icon::Close, 14.0, "Dismiss").clicked() {
                        dismiss = Some(i);
                    }
                });
            }
            if let Some(i) = dismiss {
                notices.remove(i);
            }
            ui.add_space(4.0);
        });
    }
}

/// "Laptop macOS", from a device's type and system.
fn kind(device_type: &str, os: &str) -> String {
    [device_type, os]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A device's place in this device's LAN.
fn lan_row(ui: &mut egui::Ui, lan: &DeviceLan) {
    let pal = palette(ui);
    ui.horizontal(|ui| {
        ui.add_space(178.0);
        ui.label(strong("LAN", 14.0));
        match lan {
            DeviceLan::Connected { address } => {
                ui.label(RichText::new(address).monospace())
                    .on_hover_text("Programs on this computer reach the device at this address.");
                if style::icon_button(ui, Icon::Copy, 16.0, "Copy the address").clicked() {
                    ui.ctx().copy_text(address.clone());
                }
            }
            DeviceLan::Connecting => {
                ui.label(
                    RichText::new("not linked yet: it is offline, or LAN mode is off there")
                        .color(pal.weak),
                );
            }
            DeviceLan::Refused { reason } => {
                ui.label(RichText::new(format!("not linked: {reason}")).color(WARN));
            }
        }
    });
}

/// "last seen 5 min ago", from Unix milliseconds.
fn last_seen(at_ms: u64) -> String {
    if at_ms == 0 {
        return "Not seen yet".into();
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(at_ms);
    let mins = now_ms.saturating_sub(at_ms) / 60_000;
    match mins {
        0 => "Last seen just now".into(),
        1..=59 => format!("Last seen {mins} min ago"),
        60..=1439 => format!("Last seen {} h ago", mins / 60),
        _ => format!("Last seen {} days ago", mins / 1440),
    }
}

impl eframe::App for DeskBeamApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_window_now();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let snap = self.engine.snapshot();
        self.track_window(&ui.ctx().clone());
        self.close_to_tray(ui, &snap);
        self.tray_status(&snap);
        egui::Panel::top("header").show(ui, |ui| self.header(ui, &snap));
        self.call_bar(ui, &snap);
        self.ring(&snap);
        self.notices(ui);
        self.take_dropped_files(ui);
        self.drop_hint(ui);
        self.modals(&ui.ctx().clone(), &snap);
        self.settings_window(&ui.ctx().clone(), &snap);
        self.video_window(&ui.ctx().clone(), &snap);

        let focused = ui.ctx().input(|i| i.focused);
        *self.viewing.lock().expect("lock") = match self.tab {
            Tab::Chats if focused => self.chat.open.clone(),
            _ => None,
        };
        match self.tab {
            Tab::Chats => {
                self.chat_list(ui, &snap);
                egui::CentralPanel::default().show(ui, |ui| self.chat_view(ui, &snap));
            }
            Tab::Files => {
                egui::CentralPanel::default().show(ui, |ui| self.files_page(ui, &snap));
            }
            Tab::Settings => {
                egui::CentralPanel::default().show(ui, |ui| self.settings_page(ui, &snap));
            }
        }
    }
}
