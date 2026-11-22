//! The engine: one per process, owning the iroh node, the store and the published snapshot.

use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use deskbeam_model::{
    DesktopHost, DesktopRequest, DesktopSession, Device, Discoverability, LanMode, Link, LinkKind,
    NearbyDevice, NodeStatus, PairRequest, PairingCode, PairingState, Permission, Preferences,
    Snapshot, ThisDevice,
};
use deskbeam_store::Store;
use native::{NodeService, PairedDeviceInfo};
use protocol::{DiscoveryModeOption, PairingStatus, RelayModeOption, identity::short_fingerprint};
use serde_json::Value;
use tokio::sync::{RwLock, broadcast, mpsc, watch};
use tracing::{debug, warn};

use crate::{
    chat,
    command::{Command, CommandError},
    desktop,
    emitter::{ChannelEmitter, NodeEvent},
    event::Event,
    lan::{self, LanBackend},
    transfer,
};

/// How often the snapshot is rebuilt without an event asking for it. Nearby devices and the
/// path a connection takes change without the node reporting them.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// The store keys of the settings the engine keeps.
const DISCOVERABILITY_KEY: &str = "discoverability";
/// At most this many search results.
const SEARCH_LIMIT: usize = 200;

pub(crate) const PREFERENCES_KEY: &str = "preferences";
/// Chosen sounds live here in the data directory.
const SOUNDS_DIR: &str = "sounds";
const SOUND_EXTENSIONS: [&str; 5] = ["wav", "mp3", "ogg", "flac", "oga"];
const MAX_SOUND: u64 = 10 * 1024 * 1024;

/// The default data directory: `$DESKBEAM_DATA_DIR` when set (for development, or a second
/// identity on one machine), else `DeskBeam` in the OS's per-user data folder
/// (`~/Library/Application Support/DeskBeam`, `%APPDATA%\DeskBeam`, `~/.local/share/DeskBeam`).
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DESKBEAM_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("DeskBeam")
}

/// How to start the engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub data_dir: PathBuf,
    /// Which relays to use (plan §2b). n0's public relays by default.
    pub relay: RelayModeOption,
    /// Who on the local network can see this device. `None` takes the stored setting, or
    /// everyone when there is none.
    pub discoverability: Option<Discoverability>,
    /// Where this machine's Remote Desktop server listens: `127.0.0.1:3389`.
    pub desktop_target: SocketAddr,
    /// Where received files go. `None` is the OS's downloads folder.
    pub downloads_dir: Option<PathBuf>,
    /// Where LAN mode's network adapter comes from.
    pub lan: LanBackend,
    /// The microphone and speaker calls use.
    pub audio: crate::call::CallAudio,
    /// The camera calls use.
    pub camera: crate::call::CallCamera,
    /// What a share captures in place of the screen, for tests.
    pub screen: crate::call::CallCamera,
    /// How long an incoming call rings before it counts as missed.
    pub ring_timeout: std::time::Duration,
}

impl EngineConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            relay: RelayModeOption::Default,
            discoverability: None,
            desktop_target: SocketAddr::from((Ipv4Addr::LOCALHOST, deskbeam_rdp::RDP_PORT)),
            downloads_dir: None,
            lan: LanBackend::default(),
            audio: crate::call::CallAudio::default(),
            camera: crate::call::CallCamera::default(),
            screen: crate::call::CallCamera::default(),
            ring_timeout: std::time::Duration::from_secs(45),
        }
    }
}

/// A handle to the engine. Cheap to clone; every clone is the same engine.
#[derive(Clone)]
pub struct Engine {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) node: RwLock<Option<Arc<NodeService>>>,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) data_dir: PathBuf,
    desktop_target: SocketAddr,
    status: Mutex<NodeStatus>,
    pairing: Mutex<Option<PairingCode>>,
    discoverability: Mutex<Discoverability>,
    pair_requests: Mutex<Vec<PairRequest>>,
    pub(crate) preferences: Mutex<Preferences>,
    pub(crate) snapshot: watch::Sender<Arc<Snapshot>>,
    pub(crate) events: broadcast::Sender<Event>,
    // Remote desktop (see `desktop`).
    pub(crate) forwards: tokio::sync::Mutex<HashMap<String, deskbeam_tunnel::Forward>>,
    pub(crate) desktop_requests: Mutex<Vec<DesktopRequest>>,
    pub(crate) desktop_waiters: Mutex<HashMap<String, watch::Sender<Option<bool>>>>,
    pub(crate) desktop_allowed_until: Mutex<HashMap<String, Instant>>,
    desktop_host: Mutex<DesktopHost>,
    // Messaging (see `chat`) and its attached files (see `attachments`).
    pub(crate) lamport: Mutex<u64>,
    pub(crate) attachments: crate::attachments::Attachments,
    // Calls (see `call`).
    pub(crate) call: Mutex<Option<crate::call::Slot>>,
    pub(crate) call_audio: crate::call::CallAudio,
    pub(crate) audio: Mutex<crate::audio::LiveAudio>,
    pub(crate) call_camera: crate::call::CallCamera,
    pub(crate) call_screen: crate::call::CallCamera,
    pub(crate) share_sources: Mutex<Option<crate::call::Shareable>>,
    pub(crate) video: Mutex<crate::call::CallVideo>,
    pub(crate) ring_timeout: std::time::Duration,
    pub(crate) chat_wake: chat::Wake,
    /// Who is writing where: (conversation, author) → when last heard.
    pub(crate) typing: Mutex<HashMap<(String, String), Instant>>,
    /// When this device last said it was writing, per conversation.
    pub(crate) typing_sent: Mutex<HashMap<String, Instant>>,
    // File transfers (see `transfer`).
    pub(crate) transfers: Mutex<transfer::Transfers>,
    pub(crate) relay: RelayModeOption,
    pub(crate) downloads_dir: Option<PathBuf>,
    // LAN mode (see `lan`).
    pub(crate) lan: lan::Lan,
    pub(crate) lan_mode: Mutex<LanMode>,
    pub(crate) lan_backend: LanBackend,
}

impl Inner {
    /// The node, if it runs and nobody is replacing it right now.
    pub(crate) fn node_now(&self) -> Option<Arc<NodeService>> {
        self.node.try_read().ok().and_then(|n| n.clone())
    }
}

/// Now, in Unix milliseconds.
pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Engine {
    /// Starts the engine and, in the background, its node. Returns at once: the snapshot says
    /// `Starting` until the node runs. Must be called inside a Tokio runtime.
    pub fn start(config: EngineConfig, store: Arc<dyn Store>) -> Engine {
        let discoverability = config
            .discoverability
            .unwrap_or_else(|| stored(store.as_ref(), DISCOVERABILITY_KEY));
        let preferences = stored(store.as_ref(), PREFERENCES_KEY);
        let lamport = store.max_lamport().unwrap_or_else(|e| {
            warn!("reading the message clock: {e}");
            0
        });
        let (snapshot, _) = watch::channel(Arc::new(Snapshot::default()));
        let (events, _) = broadcast::channel(256);
        let engine = Engine {
            inner: Arc::new_cyclic(|weak| Inner {
                node: RwLock::new(None),
                store,
                data_dir: config.data_dir.clone(),
                desktop_target: config.desktop_target,
                status: Mutex::new(NodeStatus::Starting),
                pairing: Mutex::new(None),
                discoverability: Mutex::new(discoverability),
                pair_requests: Mutex::new(Vec::new()),
                preferences: Mutex::new(preferences),
                snapshot,
                events,
                forwards: tokio::sync::Mutex::new(HashMap::new()),
                desktop_requests: Mutex::new(Vec::new()),
                desktop_waiters: Mutex::new(HashMap::new()),
                desktop_allowed_until: Mutex::new(HashMap::new()),
                desktop_host: Mutex::new(DesktopHost::default()),
                lamport: Mutex::new(lamport),
                attachments: Default::default(),
                call: Mutex::new(None),
                call_audio: config.audio.clone(),
                audio: Mutex::default(),
                call_camera: config.camera.clone(),
                call_screen: config.screen.clone(),
                share_sources: Mutex::default(),
                video: Mutex::default(),
                ring_timeout: config.ring_timeout,
                chat_wake: Arc::new(tokio::sync::Notify::new()),
                typing: Mutex::default(),
                typing_sent: Mutex::default(),
                transfers: Mutex::new(transfer::Transfers::default()),
                relay: config.relay.clone(),
                downloads_dir: config.downloads_dir.clone(),
                lan: deskbeam_lan::LanService::new(lan::EnginePeers {
                    inner: weak.clone(),
                }),
                lan_mode: Mutex::new(LanMode::Off),
                lan_backend: config.lan.clone(),
            }),
        };

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(node_event_loop(Arc::downgrade(&engine.inner), rx));
        tokio::spawn(refresh_loop(Arc::downgrade(&engine.inner)));
        tokio::spawn(chat::delivery_loop(Arc::downgrade(&engine.inner)));

        let weak = Arc::downgrade(&engine.inner);
        let lan = engine.inner.lan.clone();
        tokio::spawn(async move {
            let blobs = match weak.upgrade() {
                Some(inner) => crate::attachments::open(&inner, weak.clone()).await,
                None => return,
            };
            let mut protocols = native::deskbeam_ext::ExtraProtocols::new()
                .with(deskbeam_lan::ALPN, lan)
                .with(
                    deskbeam_tunnel::ALPN,
                    desktop::server(weak.clone(), config.desktop_target),
                )
                .with(deskbeam_chat::ALPN, chat::ChatServer::new(weak.clone()))
                .with(
                    deskbeam_call::ALPN,
                    crate::call::CallServer::new(weak.clone()),
                );
            if let Some(blobs) = blobs {
                protocols = protocols.with(iroh_blobs::ALPN, blobs);
            }
            let started = start_node(&config, discoverability, protocols, tx).await;
            let Some(inner) = weak.upgrade() else { return };
            let engine = Engine { inner };
            match started {
                Ok(node) => {
                    name_after_computer(&node);
                    *engine.inner.node.write().await = Some(Arc::new(node));
                    engine.set_status(NodeStatus::Running {
                        network_ready: false,
                    });
                }
                Err(e) => {
                    warn!("node failed to start: {e:#}");
                    engine.set_status(NodeStatus::Failed {
                        reason: format!("{e:#}"),
                    });
                }
            }
            engine.refresh().await;
        });

        engine
    }

    /// [`Engine::start`], then waits until the node runs. For the CLI and tests.
    pub async fn start_and_wait(
        config: EngineConfig,
        store: Arc<dyn Store>,
    ) -> anyhow::Result<Engine> {
        let engine = Engine::start(config, store);
        engine.wait_until_running().await?;
        Ok(engine)
    }

    /// Waits until the node runs, or fails if it could not start.
    pub async fn wait_until_running(&self) -> anyhow::Result<()> {
        let mut rx = self.watch();
        loop {
            match &rx.borrow_and_update().node {
                NodeStatus::Running { .. } => return Ok(()),
                NodeStatus::Failed { reason } => anyhow::bail!("node failed to start: {reason}"),
                NodeStatus::Stopped => anyhow::bail!("engine was shut down"),
                NodeStatus::Starting => {}
            }
            rx.changed().await?;
        }
    }

    /// The latest snapshot.
    /// The sound file the user chose for `kind`, if any and it is still there.
    pub fn sound_file(&self, kind: deskbeam_model::SoundKind) -> Option<PathBuf> {
        let preferences = self.inner.preferences.lock().expect("lock");
        let tone = match kind {
            deskbeam_model::SoundKind::Message => preferences.message_tone.as_ref(),
            deskbeam_model::SoundKind::Ring => preferences.ring_tone.as_ref(),
        }?;
        let path = self.inner.data_dir.join(SOUNDS_DIR).join(&tone.file);
        path.is_file().then_some(path)
    }

    /// Copies a chosen sound file in for `kind`, or goes back to the built-in sound.
    fn set_sound(
        &self,
        kind: deskbeam_model::SoundKind,
        path: Option<&str>,
    ) -> Result<Value, CommandError> {
        let dir = self.inner.data_dir.join(SOUNDS_DIR);
        let tone = match path {
            None => None,
            Some(path) => {
                let source = PathBuf::from(path);
                let invalid = |why: String| CommandError::InvalidParams("sounds.set".into(), why);
                let extension = source
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
                    .filter(|e| SOUND_EXTENSIONS.contains(&e.as_str()))
                    .ok_or_else(|| invalid("choose a WAV, MP3, OGG or FLAC file".into()))?;
                let meta = std::fs::metadata(&source)
                    .map_err(|e| CommandError::Failed(format!("{path}: {e}")))?;
                if !meta.is_file() || meta.len() > MAX_SOUND {
                    return Err(invalid("the sound must be a file of at most 10 MB".into()));
                }
                std::fs::create_dir_all(&dir).map_err(anyhow::Error::from)?;
                let file = format!("{}.{extension}", kind.name());
                std::fs::copy(&source, dir.join(&file)).map_err(anyhow::Error::from)?;
                let name = source
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| file.clone());
                Some(deskbeam_model::Tone { file, name })
            }
        };
        let mut preferences = self.inner.preferences.lock().expect("lock").clone();
        let slot = match kind {
            deskbeam_model::SoundKind::Message => &mut preferences.message_tone,
            deskbeam_model::SoundKind::Ring => &mut preferences.ring_tone,
        };
        // The file of a sound replaced by one of another type would stay behind.
        if let Some(old) = slot.take()
            && tone.as_ref().is_none_or(|t| t.file != old.file)
        {
            let _ = std::fs::remove_file(dir.join(&old.file));
        }
        *slot = tone;
        let json = serde_json::to_string(&preferences).map_err(anyhow::Error::from)?;
        self.inner.store.set_setting(PREFERENCES_KEY, &json)?;
        *self.inner.preferences.lock().expect("lock") = preferences;
        Ok(Value::Null)
    }

    /// The preferences as stored, ready before the first snapshot (the window's place is
    /// needed to open the window).
    pub fn preferences(&self) -> Preferences {
        self.inner.preferences.lock().expect("lock").clone()
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.inner.snapshot.borrow().clone()
    }

    /// A receiver that wakes on every new snapshot.
    pub fn watch(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.inner.snapshot.subscribe()
    }

    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Runs a command, then publishes the new snapshot and a `command.finished` event.
    pub async fn execute(&self, command: Command) -> Result<serde_json::Value, CommandError> {
        let id = command.id();
        debug!(command = id, "execute");
        let result = self.run(command).await;
        self.refresh().await;
        let _ = self.inner.events.send(Event::CommandFinished {
            id: id.to_string(),
            error: result.as_ref().err().map(ToString::to_string),
        });
        result
    }

    /// Stops the node. The engine answers `NodeNotRunning` from then on.
    pub async fn shutdown(&self) {
        self.lan_stop();
        let node = self.inner.node.write().await.take();
        if let Some(node) = node
            && let Err(e) = node.shutdown().await
        {
            warn!("node shutdown: {e:#}");
        }
        self.set_status(NodeStatus::Stopped);
        self.refresh().await;
    }

    pub(crate) async fn node(&self) -> Result<Arc<NodeService>, CommandError> {
        self.inner
            .node
            .read()
            .await
            .clone()
            .ok_or(CommandError::NodeNotRunning)
    }

    fn set_status(&self, status: NodeStatus) {
        *self.inner.status.lock().expect("status lock") = status;
    }

    fn set_pairing(&self, pairing: Option<PairingCode>) {
        *self.inner.pairing.lock().expect("pairing lock") = pairing;
    }

    /// Takes a pending pair request out of the list, or says there was none.
    fn take_pair_request(&self, endpoint_id: &str) -> Result<PairRequest, CommandError> {
        let mut requests = self.inner.pair_requests.lock().expect("requests lock");
        let at = requests
            .iter()
            .position(|r| r.endpoint_id.eq_ignore_ascii_case(endpoint_id))
            .ok_or_else(|| CommandError::NoPairRequest(endpoint_id.to_string()))?;
        Ok(requests.remove(at))
    }

    async fn run(&self, command: Command) -> Result<serde_json::Value, CommandError> {
        // Answers to desktop requests and preferences are the engine's own; they don't need
        // the node.
        if let Command::AnswerDesktopRequest {
            endpoint_id,
            allow,
            always,
        } = &command
        {
            return self.answer_desktop(endpoint_id, *allow, *always);
        }
        if let Command::SetPreferences {
            close_to_tray,
            notifications,
            window,
            recent_emoji,
            auto_fetch_images,
            message_sound,
            settings_window,
        } = command
        {
            let mut preferences = self.inner.preferences.lock().expect("lock").clone();
            if let Some(on) = close_to_tray {
                preferences.close_to_tray = on;
            }
            if let Some(on) = notifications {
                preferences.notifications = on;
            }
            if let Some(place) = window {
                preferences.window = Some(place);
            }
            if let Some(recent) = recent_emoji {
                let mut kept: Vec<String> = Vec::new();
                for emoji in recent {
                    let emoji = emoji.trim();
                    // An emoji is a few characters at most; anything longer is not one.
                    if !emoji.is_empty() && emoji.len() <= 32 && !kept.iter().any(|k| k == emoji) {
                        kept.push(emoji.to_string());
                    }
                }
                kept.truncate(deskbeam_model::RECENT_EMOJI);
                preferences.recent_emoji = kept;
            }
            if let Some(on) = auto_fetch_images {
                preferences.auto_fetch_images = on;
            }
            if let Some(on) = message_sound {
                preferences.message_sound = on;
            }
            if let Some(place) = settings_window {
                preferences.settings_window = Some(place);
            }
            let json = serde_json::to_string(&preferences).map_err(anyhow::Error::from)?;
            self.inner.store.set_setting(PREFERENCES_KEY, &json)?;
            *self.inner.preferences.lock().expect("lock") = preferences;
            return Ok(Value::Null);
        }
        if let Command::SetSound { kind, path } = &command {
            return self.set_sound(*kind, path.as_deref());
        }
        match command {
            Command::SetAudio { settings } => return self.set_audio(settings),
            Command::ListAudioDevices => return self.list_audio_devices().await,
            Command::TestAudio { on } => return self.test_audio(on).await,
            _ => {}
        }
        match command {
            Command::StartLan => return self.lan_start().await,
            Command::StopLan => return Ok(self.lan_stop()),
            _ => {}
        }

        let node = self.node().await?;
        match command {
            Command::RenameThisDevice { name } => {
                let name = name.trim();
                if name.is_empty() {
                    return Err(CommandError::InvalidParams(
                        "device.rename".into(),
                        "the name is empty".into(),
                    ));
                }
                node.set_device_display_name(name)?;
                Ok(Value::Null)
            }
            Command::OpenPairing { ttl_secs } => {
                let code = node.start_pairing_host(ttl_secs).await?;
                self.set_pairing(Some(PairingCode { code: code.clone() }));
                Ok(serde_json::json!({ "code": code }))
            }
            Command::ClosePairing => {
                node.stop_pairing_host().await;
                self.set_pairing(None);
                Ok(Value::Null)
            }
            Command::JoinPairing { code } => {
                node.join_pairing(code.trim()).await?;
                Ok(Value::Null)
            }
            Command::SetDeviceName { endpoint_id, name } => {
                let id = paired_id(&node, &endpoint_id)?;
                let name = name.trim();
                self.inner
                    .store
                    .set_device_name(&id.to_lowercase(), (!name.is_empty()).then_some(name))?;
                Ok(Value::Null)
            }
            Command::ForgetDevice { endpoint_id } => {
                let id = paired_id(&node, &endpoint_id)?;
                node.forget_paired(&id).await?;
                self.inner.store.forget_device(&id)?;
                Ok(Value::Null)
            }
            Command::SetPermission {
                endpoint_id,
                permission,
                allowed,
            } => {
                let id = paired_id(&node, &endpoint_id)?;
                if permission == Permission::AutoAcceptFiles {
                    // DashBeam's "trusted" flag is what makes its receiver skip the prompt.
                    node.trust_paired(&id, allowed)?;
                }
                let mut permissions = self.inner.store.permissions(&id)?;
                permissions.set(permission, allowed);
                self.inner.store.set_permissions(&id, &permissions)?;
                Ok(Value::Null)
            }
            Command::SetDiscoverability { setting } => {
                node.set_discoverability(to_node_discoverability(setting))
                    .await?;
                let json = serde_json::to_string(&setting).map_err(anyhow::Error::from)?;
                self.inner.store.set_setting(DISCOVERABILITY_KEY, &json)?;
                *self.inner.discoverability.lock().expect("lock") = setting;
                Ok(Value::Null)
            }
            Command::RequestNearbyPair { endpoint_id } => {
                if !node.request_nearby_pair(&endpoint_id).await? {
                    return Err(CommandError::Failed(format!(
                        "the request did not reach {}",
                        deskbeam_model::short_id(&endpoint_id)
                    )));
                }
                Ok(Value::Null)
            }
            Command::AcceptPairRequest { endpoint_id } => {
                let request = self.take_pair_request(&endpoint_id)?;
                node.accept_nearby_invite(&request.endpoint_id).await?;
                Ok(Value::Null)
            }
            Command::DeclinePairRequest { endpoint_id, block } => {
                let request = self.take_pair_request(&endpoint_id)?;
                node.decline_nearby_invite(&request.endpoint_id, block)
                    .await?;
                Ok(Value::Null)
            }
            Command::ConnectDesktop {
                endpoint_id,
                launch,
            } => self.connect_desktop(&endpoint_id, launch).await,
            Command::DisconnectDesktop { endpoint_id } => {
                self.disconnect_desktop(&endpoint_id).await
            }
            Command::SendMessage {
                conversation,
                body,
                attachments,
                reply_to,
            } => {
                self.chat_send(&conversation, &body, &attachments, reply_to)
                    .await
            }
            Command::EditMessage { message_id, body } => self.chat_edit(&message_id, &body).await,
            Command::DeleteMessage { message_id } => self.chat_delete(&message_id).await,
            Command::Typing { conversation } => self.chat_typing(&conversation).await,
            Command::FreeAttachmentSpace => self.free_attachment_space().await,
            Command::OpenAttachment { message_id, hash } => {
                self.open_attachment(&message_id, &hash).await
            }
            Command::SearchMessages { text, conversation } => {
                let hits = self.chat_search(&text, conversation.as_deref(), SEARCH_LIMIT);
                Ok(serde_json::json!({ "hits": hits }))
            }
            Command::StartCall { endpoint_id } => self.call_start(&endpoint_id).await,
            Command::AnswerCall { accept } => self.call_answer(accept),
            Command::HangUp => self.call_hangup(),
            Command::MuteCall { on } => self.call_mute(on).await,
            Command::CallCamera { on } => self.call_camera(on),
            Command::CallShare { target } => self.call_share(Some(target)),
            Command::CallStopSharing => self.call_share(None),
            Command::ListShareSources => self.list_share_sources().await,
            Command::FetchAttachment { message_id, hash } => {
                self.fetch_attachment(&message_id, &hash).await
            }
            Command::SaveAttachment {
                message_id,
                hash,
                path,
            } => {
                self.save_attachment(&message_id, &hash, path.as_deref())
                    .await
            }
            Command::CreateGroup { name, members } => self.chat_create_group(&name, &members).await,
            Command::MarkRead { conversation } => self.chat_mark_read(&conversation),
            Command::SendFiles { endpoint_id, paths } => {
                self.send_files(&endpoint_id, &paths).await
            }
            Command::AcceptFiles { transfer_id } => self.accept_files(&transfer_id).await,
            Command::DeclineFiles { transfer_id } => self.decline_files(&transfer_id).await,
            Command::CancelTransfer { transfer_id } => self.cancel_transfer(&transfer_id),
            Command::ClearTransfers => self.clear_transfers(),
            Command::SetPreferences { .. }
            | Command::SetSound { .. }
            | Command::SetAudio { .. }
            | Command::ListAudioDevices
            | Command::TestAudio { .. }
            | Command::AnswerDesktopRequest { .. }
            | Command::StartLan
            | Command::StopLan => {
                unreachable!("handled above")
            }
        }
    }

    /// Rebuilds the snapshot from the node and the store, and publishes it if it changed.
    pub(crate) async fn refresh(&self) {
        let node = self.inner.node.read().await.clone();
        let mut status = self.inner.status.lock().expect("status lock").clone();
        if let (NodeStatus::Running { network_ready }, Some(node)) = (&mut status, &node) {
            *network_ready = node.is_network_ready();
        }

        let this_device = node.as_ref().map(|n| {
            let info = n.device_info();
            ThisDevice {
                fingerprint: short_fingerprint(&info.endpoint_id).unwrap_or_default(),
                endpoint_id: info.endpoint_id,
                display_name: info.display_name,
                device_type: info.device_type,
                os: info.os,
            }
        });

        let sessions: HashMap<String, DesktopSession> = self
            .inner
            .forwards
            .lock()
            .await
            .iter()
            .map(|(id, f)| {
                (
                    id.clone(),
                    DesktopSession {
                        port: f.local_addr().port(),
                        connections: f.active(),
                    },
                )
            })
            .collect();
        let mut devices = Vec::new();
        let mut nearby = Vec::new();
        if let Some(n) = &node {
            match n.list_paired() {
                Ok(list) => {
                    for d in list {
                        let link = if d.online {
                            n.link_to(&d.endpoint_id).await
                        } else {
                            None
                        };
                        let session = sessions.get(&d.endpoint_id).copied();
                        let mut device = self.device_from(d, link);
                        device.desktop = session;
                        devices.push(device);
                    }
                }
                Err(e) => warn!("listing paired devices: {e:#}"),
            }
            nearby = n
                .list_nearby()
                .await
                .into_iter()
                .map(|d| NearbyDevice {
                    endpoint_id: d.endpoint_id,
                    fingerprint: d.fingerprint,
                    display_name: d.display_name.filter(|_| d.identified),
                    device_type: d.device_type,
                    os: d.os,
                })
                .collect();
        }

        let members = self.lan_members(&devices);
        for device in &mut devices {
            device.lan = members.get(&device.endpoint_id).cloned();
        }

        let conversations = match &this_device {
            Some(me) => {
                let paired = devices
                    .iter()
                    .map(|d| d.endpoint_id.to_lowercase())
                    .collect();
                self.conversations(&me.endpoint_id.to_lowercase(), &paired)
            }
            None => Vec::new(),
        };

        let transfers = self.inner.transfers.lock().expect("lock").views();
        let snapshot = Snapshot {
            transfers,
            conversations,
            node: status,
            this_device,
            devices,
            pairing: self.inner.pairing.lock().expect("pairing lock").clone(),
            discoverability: *self.inner.discoverability.lock().expect("lock"),
            nearby,
            pair_requests: self.inner.pair_requests.lock().expect("lock").clone(),
            preferences: self.inner.preferences.lock().expect("lock").clone(),
            desktop_requests: self.inner.desktop_requests.lock().expect("lock").clone(),
            desktop_host: self.inner.desktop_host.lock().expect("lock").clone(),
            lan: self.inner.lan_mode.lock().expect("lock").clone(),
            call: self.call_view(),
            audio_devices: self.audio_device_list(),
            audio_test: self.audio_test_level(),
            share_sources: self.share_sources_view(),
        };

        let changed = self.inner.snapshot.send_if_modified(|current| {
            if **current == snapshot {
                false
            } else {
                *current = Arc::new(snapshot);
                true
            }
        });
        if changed {
            let _ = self.inner.events.send(Event::SnapshotChanged);
        }
    }

    fn device_from(&self, d: PairedDeviceInfo, link: Option<native::deskbeam_ext::Link>) -> Device {
        let mut permissions = self
            .inner
            .store
            .permissions(&d.endpoint_id)
            .unwrap_or_else(|e| {
                warn!("reading permissions of {}: {e}", d.endpoint_id);
                Default::default()
            });
        permissions.auto_accept_files = d.trusted;
        let display_name = self.local_name(&d.endpoint_id, d.display_name);
        Device {
            endpoint_id: d.endpoint_id,
            display_name,
            device_type: d.device_type,
            os: d.os,
            online: d.online,
            paired_at: d.paired_at,
            last_seen_at: d.last_seen_at,
            pairing: match d.pairing_status {
                PairingStatus::Active => PairingState::Active,
                PairingStatus::UnpairedRemotely => PairingState::UnpairedRemotely,
                PairingStatus::StaleLocalIdentity => PairingState::StaleLocalIdentity,
            },
            permissions,
            link: link.map(|l| Link {
                kind: match l.kind {
                    native::deskbeam_ext::LinkKind::Direct => LinkKind::Direct,
                    native::deskbeam_ext::LinkKind::Relayed => LinkKind::Relayed,
                },
                rtt_ms: l.rtt_ms,
            }),
            desktop: None,
            lan: None,
        }
    }

    /// What this device calls a paired one: the name given to it here, else `own` (the name
    /// the device gives itself).
    pub(crate) fn local_name(&self, endpoint_id: &str, own: String) -> String {
        match self.inner.store.device_name(&endpoint_id.to_lowercase()) {
            Ok(Some(name)) => name,
            Ok(None) => own,
            Err(e) => {
                warn!("reading the name of {endpoint_id}: {e}");
                own
            }
        }
    }

    /// A setting kept as JSON in the store, or its default.
    pub(crate) fn stored<T: serde::de::DeserializeOwned + Default>(&self, key: &str) -> T {
        stored(self.inner.store.as_ref(), key)
    }

    /// Looks again for this machine's Remote Desktop server. Blocking, so off the runtime.
    async fn check_desktop_host(&self) {
        let target = self.inner.desktop_target;
        if let Ok(support) =
            tokio::task::spawn_blocking(move || deskbeam_rdp::host_support_at(target)).await
        {
            *self.inner.desktop_host.lock().expect("lock") = DesktopHost {
                listening: support.listening,
                hint: support.hint,
            };
        }
    }

    /// Turns one of the node's events into the engine's, and keeps pair requests.
    fn on_node_event(&self, name: &str, payload: Option<&Value>) {
        let field = |key: &str| {
            payload
                .and_then(|p| p.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        match name {
            "pairing-host-expired" => self.set_pairing(None),
            // A device came or went: messages may be waiting for it.
            "paired-device-presence" => self.inner.chat_wake.notify_one(),
            "device-paired" => {
                self.inner.chat_wake.notify_one();
                let _ = self.inner.events.send(Event::Paired {
                    display_name: field("display_name").unwrap_or_default(),
                });
            }
            "nearby-pair-request-received" => {
                let Some(endpoint_id) = field("remote_endpoint_id") else {
                    return;
                };
                let request = PairRequest {
                    fingerprint: short_fingerprint(&endpoint_id).unwrap_or_default(),
                    display_name: field("sender_name").unwrap_or_default(),
                    device_type: field("device_type").unwrap_or_default(),
                    os: field("os").unwrap_or_default(),
                    endpoint_id,
                };
                let event = Event::PairRequested {
                    endpoint_id: request.endpoint_id.clone(),
                    display_name: request.display_name.clone(),
                };
                {
                    let mut requests = self.inner.pair_requests.lock().expect("lock");
                    // A repeated request replaces the older one rather than queueing twice.
                    requests.retain(|r| !r.endpoint_id.eq_ignore_ascii_case(&request.endpoint_id));
                    requests.push(request);
                }
                let _ = self.inner.events.send(event);
            }
            "paired-invite-received" => {
                if let Some(payload) = payload {
                    self.offer_received(payload);
                }
            }
            "paired-invite-response" => {
                let Some(endpoint_id) = field("endpoint_id") else {
                    return;
                };
                self.offer_answered(
                    &endpoint_id,
                    field("response").as_deref() == Some("accepted"),
                );
                let _ = self.inner.events.send(Event::InviteAnswered {
                    endpoint_id,
                    display_name: field("display_name"),
                    accepted: field("response").as_deref() == Some("accepted"),
                });
            }
            _ => {}
        }
    }
}

/// The paired device's id as the node stores it, or `UnknownDevice`.
pub(crate) fn paired_id(node: &NodeService, endpoint_id: &str) -> Result<String, CommandError> {
    node.list_paired()?
        .into_iter()
        .find(|d| d.endpoint_id.eq_ignore_ascii_case(endpoint_id))
        .map(|d| d.endpoint_id)
        .ok_or_else(|| CommandError::UnknownDevice(endpoint_id.to_string()))
}

/// A setting the engine keeps as JSON in the store, or its default.
fn stored<T: serde::de::DeserializeOwned + Default>(store: &dyn Store, key: &str) -> T {
    match store.setting(key) {
        Ok(Some(json)) => serde_json::from_str(&json).unwrap_or_else(|e| {
            warn!("stored {key} {json:?} is not valid: {e}");
            T::default()
        }),
        Ok(None) => T::default(),
        Err(e) => {
            warn!("reading {key}: {e}");
            T::default()
        }
    }
}

fn to_node_discoverability(setting: Discoverability) -> protocol::Discoverability {
    match setting {
        Discoverability::Everyone => protocol::Discoverability::Everyone,
        Discoverability::PairedOnly => protocol::Discoverability::PairedOnly,
        Discoverability::Off => protocol::Discoverability::Off,
    }
}

/// Names the device after the computer while it still has a placeholder name ("DeskBeam
/// Device"). DashBeam takes the name from `$HOSTNAME`, which macOS does not export, and left
/// the rest to its web UI.
fn name_after_computer(node: &NodeService) {
    if !protocol::is_placeholder_display_name(&node.device_info().display_name) {
        return;
    }
    let Some(name) = computer_name() else { return };
    if let Err(e) = node.set_device_display_name(&name) {
        warn!("naming the device {name:?}: {e:#}");
    }
}

/// The machine's host name without the `.local` mDNS suffix.
fn computer_name() -> Option<String> {
    let raw = gethostname::gethostname().into_string().ok()?;
    let name = raw.trim().trim_end_matches(".local").trim();
    (!name.is_empty() && !name.eq_ignore_ascii_case("localhost")).then(|| name.to_string())
}

async fn start_node(
    config: &EngineConfig,
    discoverability: Discoverability,
    protocols: native::deskbeam_ext::ExtraProtocols,
    tx: mpsc::UnboundedSender<NodeEvent>,
) -> anyhow::Result<NodeService> {
    std::fs::create_dir_all(&config.data_dir)?;
    let relay: iroh::endpoint::RelayMode = config.relay.clone().into();
    NodeService::start_with_protocols(
        Path::new(&config.data_dir),
        relay,
        DiscoveryModeOption::Default,
        to_node_discoverability(discoverability),
        Some(Arc::new(ChannelEmitter { tx })),
        protocols,
    )
    .await
}

/// Turns the node's events into engine events and snapshot updates, until the engine is gone.
async fn node_event_loop(inner: Weak<Inner>, mut rx: mpsc::UnboundedReceiver<NodeEvent>) {
    while let Some(ev) = rx.recv().await {
        let Some(inner) = inner.upgrade() else { return };
        let engine = Engine { inner };
        let payload = ev
            .payload
            .map(|p| serde_json::from_str(&p).unwrap_or(Value::String(p)));
        engine.on_node_event(&ev.name, payload.as_ref());
        let _ = engine.inner.events.send(Event::Node {
            name: ev.name,
            payload,
        });
        engine.refresh().await;
    }
}

/// Rebuilds the snapshot every [`REFRESH_EVERY`], and looks for the Remote Desktop server
/// every [`DESKTOP_HOST_EVERY`] ticks, until the engine is gone.
async fn refresh_loop(inner: Weak<Inner>) {
    let mut tick = tokio::time::interval(REFRESH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    for n in 0u64.. {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else { return };
        let engine = Engine { inner };
        if n % DESKTOP_HOST_EVERY == 0 {
            engine.check_desktop_host().await;
        }
        engine.refresh().await;
    }
}

/// Remote Desktop is turned on and off rarely; looking every 30 seconds is plenty.
const DESKTOP_HOST_EVERY: u64 = 15;
