//! Every action is a command (plan §3.1, principle 2).
//!
//! A command has a stable id (`device.setPermission`) and serializable parameters, so the UI,
//! the tray, the CLI and tests all dispatch the same thing. On the wire it is
//! `{"id": "<id>", "params": {...}}`.

use deskbeam_model::{
    AudioSettings, Discoverability, Permission, ShareTarget, SoundKind, WindowPlace,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "id", content = "params")]
pub enum Command {
    /// Renames this device, as paired devices will see it.
    #[serde(rename = "device.rename", rename_all = "camelCase")]
    RenameThisDevice { name: String },

    /// Opens a pairing code another device can join with. With no `ttlSecs` it stays open
    /// until `pairing.close`.
    #[serde(rename = "pairing.open", rename_all = "camelCase")]
    OpenPairing {
        #[serde(default)]
        ttl_secs: Option<u64>,
    },

    #[serde(rename = "pairing.close")]
    ClosePairing,

    /// Pairs with the device whose pairing code this is.
    #[serde(rename = "pairing.join", rename_all = "camelCase")]
    JoinPairing { code: String },

    /// Unpairs a device and deletes what is stored for it.
    #[serde(rename = "device.forget", rename_all = "camelCase")]
    ForgetDevice { endpoint_id: String },

    /// Names a paired device on this device only; the device itself is not told. An empty
    /// name goes back to the name the device gives itself.
    #[serde(rename = "device.setName", rename_all = "camelCase")]
    SetDeviceName { endpoint_id: String, name: String },

    /// Allows or denies one thing for a paired device.
    #[serde(rename = "device.setPermission", rename_all = "camelCase")]
    SetPermission {
        endpoint_id: String,
        permission: Permission,
        allowed: bool,
    },

    /// Sets who on the local network can see this device. Kept across restarts.
    #[serde(rename = "nearby.setDiscoverability", rename_all = "camelCase")]
    SetDiscoverability { setting: Discoverability },

    /// Asks a nearby (unpaired) device to pair. Its owner sees this device's code and accepts
    /// or declines; the answer arrives as an `invite.answered` event.
    #[serde(rename = "nearby.requestPair", rename_all = "camelCase")]
    RequestNearbyPair { endpoint_id: String },

    /// Accepts a nearby device's request to pair.
    #[serde(rename = "pairRequest.accept", rename_all = "camelCase")]
    AcceptPairRequest { endpoint_id: String },

    /// Declines a nearby device's request to pair. With `block`, its further requests are
    /// ignored until the app restarts.
    #[serde(rename = "pairRequest.decline", rename_all = "camelCase")]
    DeclinePairRequest {
        endpoint_id: String,
        #[serde(default)]
        block: bool,
    },

    /// Opens a tunnel to a paired device's Remote Desktop on a local port kept for that
    /// device, writes the `.rdp` file and, unless `launch` is false, opens the RDP client.
    /// Returns `{"port", "address", "rdpFile"}`.
    #[serde(rename = "rdp.connect", rename_all = "camelCase")]
    ConnectDesktop {
        endpoint_id: String,
        #[serde(default = "yes")]
        launch: bool,
    },

    /// Closes the tunnel to a device's desktop. Sessions already open run until they end.
    #[serde(rename = "rdp.disconnect", rename_all = "camelCase")]
    DisconnectDesktop { endpoint_id: String },

    /// Answers a paired device's request to control this desktop. `always` turns its
    /// "Can control this desktop" permission on; otherwise it is allowed for 30 minutes.
    #[serde(rename = "rdp.answer", rename_all = "camelCase")]
    AnswerDesktopRequest {
        endpoint_id: String,
        allow: bool,
        #[serde(default)]
        always: bool,
    },

    /// Sends a message to a conversation (`dm:…` or `group:…`). Returns `{"id",
    /// "conversation"}`. It waits in the outbox for recipients that are offline.
    /// `attachments` are paths of files to send with it (at most 10; no folders). The body
    /// may be empty when there are attachments.
    #[serde(rename = "chat.send", rename_all = "camelCase")]
    SendMessage {
        conversation: String,
        body: String,
        #[serde(default)]
        attachments: Vec<String>,
        /// The message this one answers.
        #[serde(default)]
        reply_to: Option<String>,
    },

    /// Changes the text of a message this device sent. Everyone in the chat sees the new
    /// text, marked as edited.
    #[serde(rename = "chat.edit", rename_all = "camelCase")]
    EditMessage { message_id: String, body: String },

    /// A copy of an attached file to open in the system's own app (a video in its player),
    /// fetched first if needed. Returns `{"path"}`; the client opens it.
    #[serde(rename = "chat.openAttachment", rename_all = "camelCase")]
    OpenAttachment { message_id: String, hash: String },

    /// Removes files received in chats from this device to free space; files it sent stay.
    /// The messages stay, and a removed file can be downloaded again while someone has it.
    /// Returns `{"files", "freed"}`.
    #[serde(rename = "chat.freeSpace")]
    FreeAttachmentSpace,

    /// Tells the others in the chat that this device is writing. Sent at most every few
    /// seconds whatever the calls; they show it briefly. Nothing is stored.
    #[serde(rename = "chat.typing", rename_all = "camelCase")]
    Typing { conversation: String },

    /// Deletes a message this device sent, for everyone in the chat. Its place shows that a
    /// message was deleted.
    #[serde(rename = "chat.delete", rename_all = "camelCase")]
    DeleteMessage { message_id: String },

    /// Finds messages whose text contains `text`, newest first, in one chat or in all.
    /// Returns `{"hits": [...]}`.
    #[serde(rename = "chat.search", rename_all = "camelCase")]
    SearchMessages {
        text: String,
        #[serde(default)]
        conversation: Option<String>,
    },

    /// Fetches a file attached to a message, from its author or another member who has it.
    /// Returns when it is here.
    #[serde(rename = "chat.fetchAttachment", rename_all = "camelCase")]
    FetchAttachment { message_id: String, hash: String },

    /// Saves an attached file that is here to `path`, or when left out to the downloads
    /// folder under the author's name (as received files are). Returns `{"path"}`.
    #[serde(rename = "chat.saveAttachment", rename_all = "camelCase")]
    SaveAttachment {
        message_id: String,
        hash: String,
        #[serde(default)]
        path: Option<String>,
    },

    /// Makes a group of this device and paired devices. Returns `{"conversation"}`.
    #[serde(rename = "chat.createGroup", rename_all = "camelCase")]
    CreateGroup { name: String, members: Vec<String> },

    /// Marks a conversation read up to its newest message.
    #[serde(rename = "chat.markRead", rename_all = "camelCase")]
    MarkRead { conversation: String },

    /// Offers files and folders to a paired, online device. Returns `{"transferId"}`.
    #[serde(rename = "files.send", rename_all = "camelCase")]
    SendFiles {
        endpoint_id: String,
        paths: Vec<String>,
    },

    /// Accepts files offered to this device; they go to the downloads folder.
    #[serde(rename = "files.accept", rename_all = "camelCase")]
    AcceptFiles { transfer_id: String },

    #[serde(rename = "files.decline", rename_all = "camelCase")]
    DeclineFiles { transfer_id: String },

    /// Stops a transfer, either way round.
    #[serde(rename = "files.cancel", rename_all = "camelCase")]
    CancelTransfer { transfer_id: String },

    /// Takes finished transfers off the list.
    #[serde(rename = "files.clear")]
    ClearTransfers,

    /// Turns LAN mode on: this device joins a virtual network with the paired devices that
    /// have "LAN mode" allowed (and allow it back). Makes a network adapter, which needs
    /// admin rights: the OS may ask for them. Returns `{"address"}`.
    #[serde(rename = "lan.start")]
    StartLan,

    /// Turns LAN mode off: closes the links and removes the network adapter.
    #[serde(rename = "lan.stop")]
    StopLan,

    /// Changes the app's preferences. Fields left out keep their value.
    #[serde(rename = "preferences.set", rename_all = "camelCase")]
    SetPreferences {
        #[serde(default)]
        close_to_tray: Option<bool>,
        #[serde(default)]
        notifications: Option<bool>,
        /// Where the window is now, saved by the desktop app as it moves and resizes.
        #[serde(default)]
        window: Option<WindowPlace>,
        /// Emoji picked lately, newest first; kept to the newest `RECENT_EMOJI`.
        #[serde(default)]
        recent_emoji: Option<Vec<String>>,
        #[serde(default)]
        auto_fetch_images: Option<bool>,
        #[serde(default)]
        message_sound: Option<bool>,
        /// Where the Settings window is now, saved as it moves and resizes.
        #[serde(default)]
        settings_window: Option<WindowPlace>,
    },

    /// Calls a paired device that allows chat. Returns `{"id"}` at once; the snapshot's
    /// `call` shows it ringing, then talking or why it ended.
    #[serde(rename = "call.start", rename_all = "camelCase")]
    StartCall { endpoint_id: String },

    /// Answers the call ringing on this device.
    #[serde(rename = "call.answer", rename_all = "camelCase")]
    AnswerCall { accept: bool },

    /// Ends the call, or stops ringing out.
    #[serde(rename = "call.hangup")]
    HangUp,

    /// Mutes or unmutes this device's microphone in the call.
    #[serde(rename = "call.mute", rename_all = "camelCase")]
    MuteCall { on: bool },

    /// Turns this device's camera on or off in the call (both ends need a build with
    /// video). The snapshot's call shows `cameraOpening`, then `camera` or `cameraError`.
    #[serde(rename = "call.camera", rename_all = "camelCase")]
    CallCamera { on: bool },

    /// Shares a display or window in the call, in place of the camera while it lasts. On
    /// macOS, without the Screen Recording permission this asks for it and fails with what
    /// to do.
    #[serde(rename = "call.share", rename_all = "camelCase")]
    CallShare { target: ShareTarget },

    /// Stops sharing; the camera comes back if it was on.
    #[serde(rename = "call.stopSharing")]
    CallStopSharing,

    /// Looks up the displays and windows that can be shared, into the snapshot's
    /// `shareSources` (their small pictures through `Engine::share_thumbnail`).
    #[serde(rename = "call.listShareSources")]
    ListShareSources,

    /// The devices and processing calls use. Processing changes reach a call (or an audio
    /// test) at once; a new microphone or speaker, the next one.
    #[serde(rename = "call.setAudio", rename_all = "camelCase")]
    SetAudio { settings: AudioSettings },

    /// Looks up the microphones and speakers there are, into the snapshot's `audioDevices`.
    #[serde(rename = "call.listDevices")]
    ListAudioDevices,

    /// Plays the microphone back through the whole call path (processing, Opus, the
    /// speaker) for up to 15 s, its level in the snapshot's `audioTest`. Not during a call.
    #[serde(rename = "call.testAudio", rename_all = "camelCase")]
    TestAudio { on: bool },

    /// Uses the sound file at `path` (WAV, MP3, OGG or FLAC, at most 10 MB) for `kind`,
    /// copied into the data directory; no path goes back to the built-in sound.
    #[serde(rename = "sounds.set", rename_all = "camelCase")]
    SetSound {
        kind: SoundKind,
        #[serde(default)]
        path: Option<String>,
    },
}

fn yes() -> bool {
    true
}

impl Command {
    /// Every command id, for the CLI's listing and for tests.
    pub const IDS: &'static [&'static str] = &[
        "device.rename",
        "pairing.open",
        "pairing.close",
        "pairing.join",
        "device.forget",
        "device.setName",
        "device.setPermission",
        "nearby.setDiscoverability",
        "nearby.requestPair",
        "pairRequest.accept",
        "pairRequest.decline",
        "preferences.set",
        "rdp.connect",
        "rdp.disconnect",
        "rdp.answer",
        "chat.send",
        "chat.edit",
        "chat.typing",
        "chat.freeSpace",
        "chat.openAttachment",
        "chat.delete",
        "chat.search",
        "chat.createGroup",
        "chat.markRead",
        "chat.fetchAttachment",
        "sounds.set",
        "call.start",
        "call.answer",
        "call.hangup",
        "call.mute",
        "call.camera",
        "call.share",
        "call.stopSharing",
        "call.listShareSources",
        "call.setAudio",
        "call.listDevices",
        "call.testAudio",
        "chat.saveAttachment",
        "files.send",
        "files.accept",
        "files.decline",
        "files.cancel",
        "files.clear",
        "lan.start",
        "lan.stop",
    ];

    pub fn id(&self) -> &'static str {
        match self {
            Self::RenameThisDevice { .. } => "device.rename",
            Self::OpenPairing { .. } => "pairing.open",
            Self::ClosePairing => "pairing.close",
            Self::JoinPairing { .. } => "pairing.join",
            Self::ForgetDevice { .. } => "device.forget",
            Self::SetDeviceName { .. } => "device.setName",
            Self::SetPermission { .. } => "device.setPermission",
            Self::SetDiscoverability { .. } => "nearby.setDiscoverability",
            Self::RequestNearbyPair { .. } => "nearby.requestPair",
            Self::AcceptPairRequest { .. } => "pairRequest.accept",
            Self::DeclinePairRequest { .. } => "pairRequest.decline",
            Self::SetPreferences { .. } => "preferences.set",
            Self::ConnectDesktop { .. } => "rdp.connect",
            Self::DisconnectDesktop { .. } => "rdp.disconnect",
            Self::AnswerDesktopRequest { .. } => "rdp.answer",
            Self::SendMessage { .. } => "chat.send",
            Self::EditMessage { .. } => "chat.edit",
            Self::Typing { .. } => "chat.typing",
            Self::FreeAttachmentSpace => "chat.freeSpace",
            Self::OpenAttachment { .. } => "chat.openAttachment",
            Self::DeleteMessage { .. } => "chat.delete",
            Self::SearchMessages { .. } => "chat.search",
            Self::CreateGroup { .. } => "chat.createGroup",
            Self::MarkRead { .. } => "chat.markRead",
            Self::FetchAttachment { .. } => "chat.fetchAttachment",
            Self::SetSound { .. } => "sounds.set",
            Self::StartCall { .. } => "call.start",
            Self::AnswerCall { .. } => "call.answer",
            Self::HangUp => "call.hangup",
            Self::MuteCall { .. } => "call.mute",
            Self::CallCamera { .. } => "call.camera",
            Self::CallShare { .. } => "call.share",
            Self::CallStopSharing => "call.stopSharing",
            Self::ListShareSources => "call.listShareSources",
            Self::SetAudio { .. } => "call.setAudio",
            Self::ListAudioDevices => "call.listDevices",
            Self::TestAudio { .. } => "call.testAudio",
            Self::SaveAttachment { .. } => "chat.saveAttachment",
            Self::SendFiles { .. } => "files.send",
            Self::AcceptFiles { .. } => "files.accept",
            Self::DeclineFiles { .. } => "files.decline",
            Self::CancelTransfer { .. } => "files.cancel",
            Self::ClearTransfers => "files.clear",
            Self::StartLan => "lan.start",
            Self::StopLan => "lan.stop",
        }
    }

    /// Builds a command from its id and JSON parameters, as the CLI receives them.
    /// `params` may be `null` or `{}` for commands that take none.
    pub fn parse(id: &str, params: serde_json::Value) -> Result<Command, CommandError> {
        if !Self::IDS.contains(&id) {
            return Err(CommandError::UnknownCommand(id.to_string()));
        }
        let empty = params.is_null() || params.as_object().is_some_and(|o| o.is_empty());
        let mut wire = serde_json::json!({ "id": id });
        if !empty {
            wire["params"] = params;
        }
        match serde_json::from_value(wire) {
            Ok(cmd) => Ok(cmd),
            // A command whose parameters are all optional, given none.
            Err(_) if empty => {
                serde_json::from_value(serde_json::json!({ "id": id, "params": {} }))
                    .map_err(|e| CommandError::InvalidParams(id.to_string(), e.to_string()))
            }
            Err(e) => Err(CommandError::InvalidParams(id.to_string(), e.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("no command has the id `{0}`")]
    UnknownCommand(String),
    #[error("invalid parameters for `{0}`: {1}")]
    InvalidParams(String, String),
    #[error("the node is not running")]
    NodeNotRunning,
    #[error("no paired device has the id `{0}`")]
    UnknownDevice(String),
    #[error("`{0}` has not asked to pair")]
    NoPairRequest(String),
    #[error("`{0}` has not asked to control this desktop")]
    NoDesktopRequest(String),
    #[error("{0} is offline")]
    Offline(String),
    #[error("{0}")]
    Failed(String),
}

impl From<anyhow::Error> for CommandError {
    fn from(e: anyhow::Error) -> Self {
        Self::Failed(format!("{e:#}"))
    }
}

impl From<deskbeam_store::StoreError> for CommandError {
    fn from(e: deskbeam_store::StoreError) -> Self {
        Self::Failed(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn every_command() -> Vec<Command> {
        vec![
            Command::RenameThisDevice {
                name: "Office PC".into(),
            },
            Command::OpenPairing {
                ttl_secs: Some(300),
            },
            Command::ClosePairing,
            Command::JoinPairing { code: "abc".into() },
            Command::ForgetDevice {
                endpoint_id: "e".into(),
            },
            Command::SetDeviceName {
                endpoint_id: "e".into(),
                name: "Den".into(),
            },
            Command::SetPermission {
                endpoint_id: "e".into(),
                permission: Permission::RemoteDesktop,
                allowed: true,
            },
            Command::SetDiscoverability {
                setting: Discoverability::PairedOnly,
            },
            Command::RequestNearbyPair {
                endpoint_id: "e".into(),
            },
            Command::AcceptPairRequest {
                endpoint_id: "e".into(),
            },
            Command::DeclinePairRequest {
                endpoint_id: "e".into(),
                block: true,
            },
            Command::SetPreferences {
                close_to_tray: Some(false),
                notifications: None,
                window: None,
                recent_emoji: None,
                auto_fetch_images: None,
                message_sound: None,
                settings_window: None,
            },
            Command::ConnectDesktop {
                endpoint_id: "e".into(),
                launch: false,
            },
            Command::DisconnectDesktop {
                endpoint_id: "e".into(),
            },
            Command::AnswerDesktopRequest {
                endpoint_id: "e".into(),
                allow: true,
                always: false,
            },
            Command::SendMessage {
                conversation: "group:g".into(),
                body: "hi".into(),
                attachments: vec!["/tmp/a.jpg".into()],
                reply_to: Some("m0".into()),
            },
            Command::EditMessage {
                message_id: "m".into(),
                body: "hello".into(),
            },
            Command::DeleteMessage {
                message_id: "m".into(),
            },
            Command::Typing {
                conversation: "group:g".into(),
            },
            Command::FreeAttachmentSpace,
            Command::OpenAttachment {
                message_id: "m".into(),
                hash: "h".into(),
            },
            Command::SearchMessages {
                text: "hi".into(),
                conversation: None,
            },
            Command::CreateGroup {
                name: "Team".into(),
                members: vec!["e".into()],
            },
            Command::MarkRead {
                conversation: "group:g".into(),
            },
            Command::FetchAttachment {
                message_id: "m".into(),
                hash: "h".into(),
            },
            Command::SetSound {
                kind: SoundKind::Ring,
                path: Some("/tmp/ring.mp3".into()),
            },
            Command::StartCall {
                endpoint_id: "e".into(),
            },
            Command::AnswerCall { accept: true },
            Command::HangUp,
            Command::MuteCall { on: true },
            Command::SetAudio {
                settings: AudioSettings {
                    microphone: Some("USB Mic".into()),
                    max_gain_db: 30,
                    ..Default::default()
                },
            },
            Command::ListAudioDevices,
            Command::TestAudio { on: true },
            Command::CallCamera { on: true },
            Command::CallShare {
                target: ShareTarget::Window { id: 7 },
            },
            Command::CallStopSharing,
            Command::ListShareSources,
            Command::SaveAttachment {
                message_id: "m".into(),
                hash: "h".into(),
                path: None,
            },
            Command::SendFiles {
                endpoint_id: "e".into(),
                paths: vec!["/tmp/a".into()],
            },
            Command::AcceptFiles {
                transfer_id: "t".into(),
            },
            Command::DeclineFiles {
                transfer_id: "t".into(),
            },
            Command::CancelTransfer {
                transfer_id: "t".into(),
            },
            Command::ClearTransfers,
            Command::StartLan,
            Command::StopLan,
        ]
    }

    #[test]
    fn ids_are_listed_and_round_trip() {
        let cmds = every_command();
        assert_eq!(cmds.len(), Command::IDS.len());
        for cmd in cmds {
            assert!(Command::IDS.contains(&cmd.id()));
            let wire = serde_json::to_value(&cmd).unwrap();
            assert_eq!(wire["id"], cmd.id());
            let back: Command = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(back, cmd);
            let parsed =
                Command::parse(cmd.id(), wire.get("params").cloned().unwrap_or_default()).unwrap();
            assert_eq!(parsed, cmd);
        }
    }

    #[test]
    fn parse_accepts_missing_optional_params() {
        assert_eq!(
            Command::parse("pairing.open", json!(null)).unwrap(),
            Command::OpenPairing { ttl_secs: None }
        );
        assert_eq!(
            Command::parse("pairing.open", json!({})).unwrap(),
            Command::OpenPairing { ttl_secs: None }
        );
        assert_eq!(
            Command::parse("pairing.close", json!({})).unwrap(),
            Command::ClosePairing
        );
    }

    #[test]
    fn parse_names_the_problem() {
        assert!(matches!(
            Command::parse("nope", json!({})),
            Err(CommandError::UnknownCommand(_))
        ));
        assert!(matches!(
            Command::parse("pairing.join", json!({})),
            Err(CommandError::InvalidParams(..))
        ));
        let p = Command::parse(
            "device.setPermission",
            json!({"endpointId": "e", "permission": "remoteDesktop", "allowed": true}),
        )
        .unwrap();
        assert_eq!(
            p,
            Command::SetPermission {
                endpoint_id: "e".into(),
                permission: Permission::RemoteDesktop,
                allowed: true
            }
        );
    }
}
