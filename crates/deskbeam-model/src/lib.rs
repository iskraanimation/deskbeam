//! DeskBeam's plain data types (plan §3.1).
//!
//! No I/O and no async: these are what the engine publishes in its snapshots and what the
//! store persists. Every client (the egui app, the CLI, tests) reads the same types.

use serde::{Deserialize, Serialize};

/// What a paired device is allowed to do with this one.
///
/// Remote desktop and LAN mode are off until the user turns them on for that device
/// (plan §2, security layers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Permissions {
    pub send_files: bool,
    pub auto_accept_files: bool,
    pub remote_desktop: bool,
    pub join_lan: bool,
    pub chat: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            send_files: true,
            auto_accept_files: false,
            remote_desktop: false,
            join_lan: false,
            chat: true,
        }
    }
}

/// One permission, named, so a command can change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Permission {
    SendFiles,
    AutoAcceptFiles,
    RemoteDesktop,
    JoinLan,
    Chat,
}

impl Permission {
    pub const ALL: [Permission; 5] = [
        Self::SendFiles,
        Self::AutoAcceptFiles,
        Self::RemoteDesktop,
        Self::JoinLan,
        Self::Chat,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::SendFiles => "Send files",
            Self::AutoAcceptFiles => "Accept files without asking",
            Self::RemoteDesktop => "Can control this desktop",
            Self::JoinLan => "LAN mode",
            Self::Chat => "Chat",
        }
    }
}

impl Permissions {
    pub fn get(&self, p: Permission) -> bool {
        match p {
            Permission::SendFiles => self.send_files,
            Permission::AutoAcceptFiles => self.auto_accept_files,
            Permission::RemoteDesktop => self.remote_desktop,
            Permission::JoinLan => self.join_lan,
            Permission::Chat => self.chat,
        }
    }

    pub fn set(&mut self, p: Permission, allowed: bool) {
        match p {
            Permission::SendFiles => self.send_files = allowed,
            Permission::AutoAcceptFiles => self.auto_accept_files = allowed,
            Permission::RemoteDesktop => self.remote_desktop = allowed,
            Permission::JoinLan => self.join_lan = allowed,
            Permission::Chat => self.chat = allowed,
        }
    }
}

/// This device, as others see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThisDevice {
    pub endpoint_id: String,
    /// "This device's code", compared on both screens when pairing.
    pub fingerprint: String,
    pub display_name: String,
    pub device_type: String,
    pub os: String,
}

/// A paired device and what it may do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub endpoint_id: String,
    pub display_name: String,
    pub device_type: String,
    pub os: String,
    pub online: bool,
    /// Unix milliseconds.
    pub paired_at: u64,
    /// Unix milliseconds.
    pub last_seen_at: u64,
    pub pairing: PairingState,
    pub permissions: Permissions,
    /// The live connection, while there is one.
    pub link: Option<Link>,
    /// This device's tunnel to that device's desktop, while it is open.
    pub desktop: Option<DesktopSession>,
    /// The device in this device's LAN, while LAN mode is on and it is allowed in.
    pub lan: Option<DeviceLan>,
}

/// LAN mode on this device (plan §2, option B).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum LanMode {
    #[default]
    Off,
    /// Waiting for the network adapter, which may mean the user typing a password.
    Starting,
    #[serde(rename_all = "camelCase")]
    On {
        /// This device's address, e.g. `100.88.17.42`.
        address: String,
        /// The adapter's name as the OS shows it (`utun5`, `tun0`, `DeskBeam`).
        adapter: String,
    },
    Failed {
        reason: String,
    },
}

/// A paired device in this device's LAN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum DeviceLan {
    /// Not linked yet: it may be offline, or have LAN mode off.
    Connecting,
    /// Programs on this device reach it at `address`.
    Connected { address: String },
    /// It turned the link down, and why.
    Refused { reason: String },
}

/// A tunnel from this device to a paired device's Remote Desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSession {
    /// The RDP client connects to `localhost:<port>`.
    pub port: u16,
    /// Forwarded connections open now.
    pub connections: usize,
}

/// A paired device asking to control this desktop, without the permission to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopRequest {
    pub endpoint_id: String,
    pub display_name: String,
}

/// Whether this machine can be controlled over Remote Desktop.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopHost {
    /// A Remote Desktop server answers on this machine's port 3389.
    pub listening: bool,
    /// What to do when it does not, for this OS.
    pub hint: String,
}

/// How the connection to a paired device runs (plan §2b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkKind {
    /// Straight between the two devices.
    Direct,
    /// Through a relay, which only sees encrypted traffic.
    Relayed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    pub kind: LinkKind,
    pub rtt_ms: u32,
}

/// An unpaired DeskBeam device on the same local network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NearbyDevice {
    pub endpoint_id: String,
    /// What its owner sees as "this device's code": compare it before pairing.
    pub fingerprint: String,
    /// `None` until the device has said who it is.
    pub display_name: Option<String>,
    pub device_type: Option<String>,
    pub os: Option<String>,
}

impl NearbyDevice {
    pub fn name(&self) -> &str {
        self.display_name
            .as_deref()
            .unwrap_or_else(|| short_id(&self.endpoint_id))
    }
}

/// Another device on the network asking to pair with this one. It waits for the user to
/// accept or decline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub endpoint_id: String,
    pub display_name: String,
    pub device_type: String,
    pub os: String,
    /// The requesting device's code; its owner sees the same one on their screen.
    pub fingerprint: String,
}

/// A chat, as the list of chats shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    /// `dm:<a>:<b>` or `group:<id>`; what `chat.send` and `chat.markRead` take.
    pub id: String,
    pub title: String,
    pub kind: ConversationKind,
    pub last: Option<ChatMessage>,
    /// Messages from others not read yet.
    pub unread: usize,
    /// Who is writing in it now, by name.
    #[serde(default)]
    pub typing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConversationKind {
    /// With one paired device.
    Direct { endpoint_id: String },
    Group {
        /// Every member but this device.
        members: Vec<Member>,
        /// Whether this device made the group, and so may change it.
        mine: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub endpoint_id: String,
    pub display_name: String,
    /// Paired with this device. Always true now: members this device is not paired with are
    /// left out of the list (plan §5).
    pub paired: bool,
}

/// One message, as clients show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: String,
    pub conversation: String,
    pub author: String,
    pub author_name: String,
    /// Written on this device.
    pub mine: bool,
    /// Unix milliseconds, on the author's clock.
    pub sent_at: u64,
    pub body: String,
    /// For this device's messages: how many recipients have it, and how many it waits for.
    pub delivery: Option<Delivery>,
    /// Files sent with it.
    #[serde(default)]
    pub attachments: Vec<ChatAttachment>,
    /// Its author changed the text after sending.
    #[serde(default)]
    pub edited: bool,
    /// Its author deleted it: no text, no files, only that it was there.
    #[serde(default)]
    pub deleted: bool,
    /// The message this one answers, as quoted above it.
    #[serde(default)]
    pub reply: Option<Quote>,
}

/// A message as quoted in a reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub id: String,
    pub author_name: String,
    /// The start of its text, or what it carried ("Photo", "2 files"); empty when this
    /// device doesn't have that message.
    pub text: String,
    pub deleted: bool,
}

/// A message found by `chat.search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub conversation: String,
    /// The chat's name, as in the list of chats.
    pub title: String,
    pub message: ChatMessage,
}

/// A file sent with a message, and whether this device has it yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAttachment {
    /// Its BLAKE3 hash (hex): what `chat.fetchAttachment` and `chat.saveAttachment` name.
    pub hash: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
    /// A picture's size in pixels.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// A picture's small preview: JPEG, base64.
    pub preview: Option<String>,
    pub state: AttachmentState,
}

impl ChatAttachment {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    /// A video or a sound, which plays in the system's own player.
    pub fn plays(&self) -> bool {
        self.mime.starts_with("video/") || self.mime.starts_with("audio/")
    }
}

/// Whether this device holds an attachment's file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum AttachmentState {
    /// Not fetched: `chat.fetchAttachment` gets it.
    Missing,
    Fetching {
        bytes: u64,
    },
    /// Here, whole and checked.
    Ready,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Delivery {
    pub delivered: usize,
    pub waiting: usize,
}

/// A file transfer with a paired device, this session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    pub id: String,
    pub direction: Direction,
    pub peer: String,
    pub peer_name: String,
    /// The files' and folders' names, once known. A receiver learns them when it starts.
    pub names: Vec<String>,
    pub file_count: u32,
    /// Bytes in all; 0 until known.
    pub total: u64,
    pub bytes: u64,
    pub bytes_per_sec: u64,
    pub state: TransferState,
    /// Unix milliseconds.
    pub started_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    Send,
    Receive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TransferState {
    /// Sent the offer; waiting for the other device to accept.
    Offered,
    /// An offer to this device, waiting for the user (`files.accept` / `files.decline`).
    Incoming,
    Running,
    /// Finished. For a receive, where the files are.
    Done {
        path: Option<String>,
    },
    Declined,
    Cancelled,
    Failed {
        reason: String,
    },
}

impl TransferState {
    pub fn is_finished(&self) -> bool {
        matches!(
            self,
            Self::Done { .. } | Self::Declined | Self::Cancelled | Self::Failed { .. }
        )
    }
}

/// How the desktop app behaves. Kept by the engine so every client sees the same choices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preferences {
    /// Closing the window keeps DeskBeam running in the tray, reachable by paired devices.
    pub close_to_tray: bool,
    /// Pair requests and new pairings show as system notifications.
    pub notifications: bool,
    /// Where the window was last, to open it there again. `None` before the first save.
    pub window: Option<WindowPlace>,
    /// Emoji picked lately in the composer's picker, newest first.
    pub recent_emoji: Vec<String>,
    /// Pictures up to `AUTO_FETCH_LIMIT` sent in chats are fetched as they arrive, so they
    /// show in full at once. Off: only their previews, until asked for.
    pub auto_fetch_images: bool,
    /// A sound plays for new messages when a notification would.
    pub message_sound: bool,
    /// Sounds the user chose instead of the built-in ones.
    pub message_tone: Option<Tone>,
    pub ring_tone: Option<Tone>,
    /// The devices and processing calls use.
    pub audio: AudioSettings,
    /// Where the Settings window was last. `None` before the first save.
    pub settings_window: Option<WindowPlace>,
}

/// The devices and processing calls use, set with `call.setAudio`. The defaults are what
/// calls were tuned to by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AudioSettings {
    /// A microphone by name; `None` is the system's default.
    pub microphone: Option<String>,
    /// A speaker by name; `None` is the system's default.
    pub speaker: Option<String>,
    /// A camera by name; `None` is the first one.
    pub camera: Option<String>,
    /// How much the camera sends: standard 640×360 or HD 1280×720.
    pub camera_quality: CameraQuality,
    /// Takes what the speaker plays out of the microphone. Off only with headphones.
    pub echo_cancellation: bool,
    pub noise_suppression: NoiseSuppression,
    /// Raises a quiet voice by itself, up to `max_gain_db`.
    pub auto_gain: bool,
    /// 0 to `AudioSettings::MOST_GAIN_DB`.
    pub max_gain_db: u8,
    /// A fixed boost for a quiet microphone, 0 to `AudioSettings::MOST_BOOST_DB`.
    pub boost_db: u8,
}

impl AudioSettings {
    pub const MOST_GAIN_DB: u8 = 50;
    pub const MOST_BOOST_DB: u8 = 20;
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            microphone: None,
            speaker: None,
            camera: None,
            camera_quality: CameraQuality::Standard,
            echo_cancellation: true,
            noise_suppression: NoiseSuppression::Moderate,
            auto_gain: true,
            max_gain_db: Self::MOST_GAIN_DB,
            boost_db: 0,
        }
    }
}

/// The camera's size in calls. HD needs a camera that can give 720p, and about 2–3 times
/// the bandwidth and CPU; a camera that can't sends what it can, never scaled up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CameraQuality {
    /// Up to 640×360.
    #[default]
    Standard,
    /// Up to 1280×720.
    Hd,
}

impl CameraQuality {
    pub const ALL: [Self; 2] = [Self::Standard, Self::Hd];

    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "Standard (640×360)",
            Self::Hd => "HD (1280×720)",
        }
    }
}

/// How hard background noise is taken out of the microphone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NoiseSuppression {
    Off,
    /// About 6 dB.
    Low,
    /// About 12 dB.
    #[default]
    Moderate,
    /// About 18 dB.
    High,
    /// About 21 dB.
    VeryHigh,
}

impl NoiseSuppression {
    pub const ALL: [Self; 5] = [
        Self::Off,
        Self::Low,
        Self::Moderate,
        Self::High,
        Self::VeryHigh,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Low => "Low",
            Self::Moderate => "Moderate",
            Self::High => "High",
            Self::VeryHigh => "Very high",
        }
    }
}

/// The microphones and speakers there are, by name, from `call.listDevices`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevices {
    pub microphones: Vec<String>,
    pub speakers: Vec<String>,
    pub cameras: Vec<String>,
}

/// A sound file the user chose, copied into the data directory's `sounds` folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tone {
    /// Its file name in `sounds/`.
    pub file: String,
    /// The name it had when chosen, to show.
    pub name: String,
}

/// Which of the app's sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SoundKind {
    /// A new message.
    Message,
    /// An incoming call.
    Ring,
}

impl SoundKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Ring => "ring",
        }
    }
}

/// The largest picture fetched without being asked for.
pub const AUTO_FETCH_LIMIT: u64 = 10 * 1024 * 1024;

/// How many recent emoji are kept.
pub const RECENT_EMOJI: usize = 24;

/// The desktop app's window: its place, size and whether it was maximized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WindowPlace {
    /// Top-left corner of the window's frame, in points. `None` where it can't be known
    /// (Wayland).
    pub x: Option<i32>,
    pub y: Option<i32>,
    /// Size of the content in points, before any maximizing.
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    /// Size in points of the monitor it was on, to notice when the monitors changed.
    pub monitor: Option<(u32, u32)>,
}

impl Default for WindowPlace {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            width: 720,
            height: 600,
            maximized: false,
            monitor: None,
        }
    }
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            notifications: true,
            window: None,
            recent_emoji: Vec::new(),
            auto_fetch_images: true,
            message_sound: true,
            message_tone: None,
            ring_tone: None,
            audio: AudioSettings::default(),
            settings_window: None,
        }
    }
}

/// Who on the local network can see this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Discoverability {
    /// Every DeskBeam on the network sees this device's name.
    #[default]
    Everyone,
    /// Only paired devices see the name; others see an id.
    PairedOnly,
    /// Not announced at all. This device can still see others.
    Off,
}

impl Discoverability {
    pub const ALL: [Discoverability; 3] = [Self::Everyone, Self::PairedOnly, Self::Off];

    pub fn label(self) -> &'static str {
        match self {
            Self::Everyone => "Everyone on this network",
            Self::PairedOnly => "Paired devices only",
            Self::Off => "Nobody",
        }
    }
}

/// Whether a pairing still holds on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PairingState {
    #[default]
    Active,
    /// The other device unpaired this one.
    UnpairedRemotely,
    /// This device's identity changed, so the other still knows its old id.
    StaleLocalIdentity,
}

impl Device {
    /// The first characters of the endpoint id, for showing next to a name.
    pub fn short_id(&self) -> &str {
        short_id(&self.endpoint_id)
    }
}

pub fn short_id(endpoint_id: &str) -> &str {
    endpoint_id.get(..10).unwrap_or(endpoint_id)
}

/// Where the networking node is.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum NodeStatus {
    #[default]
    Starting,
    /// The node runs. `network_ready` is true once the home relay is reachable.
    Running {
        network_ready: bool,
    },
    Failed {
        reason: String,
    },
    Stopped,
}

/// An open pairing code others can join with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingCode {
    pub code: String,
}

/// Everything a client shows, at one moment. The engine publishes a new one on every change;
/// clients never modify it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub node: NodeStatus,
    pub this_device: Option<ThisDevice>,
    pub devices: Vec<Device>,
    pub pairing: Option<PairingCode>,
    pub discoverability: Discoverability,
    /// Unpaired devices on the local network.
    pub nearby: Vec<NearbyDevice>,
    /// Incoming requests to pair, oldest first.
    pub pair_requests: Vec<PairRequest>,
    pub preferences: Preferences,
    /// Paired devices asking to control this desktop, oldest first.
    pub desktop_requests: Vec<DesktopRequest>,
    pub desktop_host: DesktopHost,
    /// Chats, newest activity first.
    pub conversations: Vec<Conversation>,
    /// File transfers this session, newest first.
    pub transfers: Vec<Transfer>,
    pub lan: LanMode,
    /// The call in progress, ringing, or just ended (kept a few seconds to show why).
    pub call: Option<Call>,
    /// The audio devices, once `call.listDevices` has looked.
    pub audio_devices: Option<AudioDevices>,
    /// While `call.testAudio` plays the microphone back: its level, 0 to 1.
    pub audio_test: Option<f32>,
    /// What can be shared, once `call.listShareSources` has looked.
    pub share_sources: Option<ShareSources>,
}

/// A voice call with a paired device (plan Phase 6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Call {
    pub id: String,
    pub endpoint_id: String,
    pub display_name: String,
    /// This device called.
    pub outgoing: bool,
    pub state: CallState,
    /// This device's microphone is muted.
    pub muted: bool,
    /// The other end muted its microphone.
    pub peer_muted: bool,
    /// Unix milliseconds when it was answered.
    pub started_at: Option<u64>,
    /// While it runs: the share of the other end's audio lost or late, 0 to 1.
    pub loss: f32,
    /// This device's microphone level, 0 to 1, for a meter.
    pub level: f32,
    /// Both ends can do video (an older build on the other end can't).
    pub video: bool,
    /// This device's camera is on in the call.
    pub camera: bool,
    /// The camera is being opened (the system may be asking for permission).
    pub camera_opening: bool,
    /// The other end's camera is on.
    pub peer_camera: bool,
    /// Why the camera did not open, until it is tried again.
    pub camera_error: Option<String>,
    /// What this device is sharing (a display's or window's name), while it does.
    pub sharing: Option<String>,
    /// A share is starting.
    pub share_opening: bool,
    /// The other end is sharing its screen.
    pub peer_sharing: bool,
    /// Why sharing did not start (or stopped), until it is tried again.
    pub share_error: Option<String>,
}

/// A display or window to share in a call.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ShareTarget {
    Display { id: u32 },
    Window { id: u32 },
}

/// One thing that can be shared, from `call.listShareSources`. Its small picture is
/// `Engine::share_thumbnail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareOption {
    pub target: ShareTarget,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// What `call.listShareSources` found.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareSources {
    /// The system lets DeskBeam record the screen (on macOS, Screen Recording).
    pub allowed: bool,
    pub options: Vec<ShareOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum CallState {
    /// This device called; the other is ringing.
    Calling,
    /// The other device calls; answer with `call.answer`.
    Ringing,
    Active,
    Ended {
        reason: String,
    },
}

impl Call {
    /// Ringing, calling or talking: not over.
    pub fn is_live(&self) -> bool {
        !matches!(self.state, CallState::Ended { .. })
    }
}

impl Snapshot {
    pub fn device(&self, endpoint_id: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.endpoint_id == endpoint_id)
    }

    pub fn online_count(&self) -> usize {
        self.devices.iter().filter(|d| d.online).count()
    }

    pub fn unread(&self) -> usize {
        self.conversations.iter().map(|c| c.unread).sum()
    }

    pub fn conversation(&self, id: &str) -> Option<&Conversation> {
        self.conversations.iter().find(|c| c.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_desktop_is_off_by_default() {
        let p = Permissions::default();
        assert!(!p.remote_desktop);
        assert!(!p.join_lan);
        assert!(!p.auto_accept_files);
    }

    #[test]
    fn set_and_get_every_permission() {
        let mut p = Permissions::default();
        for perm in Permission::ALL {
            p.set(perm, true);
            assert!(p.get(perm));
            p.set(perm, false);
            assert!(!p.get(perm));
        }
    }

    #[test]
    fn node_status_fields_are_camel_case() {
        let json = serde_json::to_value(NodeStatus::Running {
            network_ready: true,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"state": "running", "networkReady": true})
        );
    }

    #[test]
    fn permissions_missing_fields_take_defaults() {
        let p: Permissions = serde_json::from_str(r#"{"remoteDesktop":true}"#).unwrap();
        assert!(p.remote_desktop);
        assert!(p.chat);
    }
}
