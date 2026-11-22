//! What the engine tells its clients, besides the snapshot.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    /// A new [`crate::Snapshot`] is out.
    #[serde(rename = "snapshot.changed")]
    SnapshotChanged,

    /// A command finished. `error` is set when it failed.
    #[serde(rename = "command.finished")]
    CommandFinished { id: String, error: Option<String> },

    /// A device was paired, either way round.
    #[serde(rename = "pairing.paired", rename_all = "camelCase")]
    Paired { display_name: String },

    /// A nearby device asks to pair; it is in the snapshot's `pairRequests` until answered.
    #[serde(rename = "pairing.requested", rename_all = "camelCase")]
    PairRequested {
        endpoint_id: String,
        display_name: String,
    },

    /// A device answered this one's invite: a request to pair, or (later) a file share.
    #[serde(rename = "invite.answered", rename_all = "camelCase")]
    InviteAnswered {
        endpoint_id: String,
        display_name: Option<String>,
        accepted: bool,
    },

    /// A paired device asks to control this desktop; it is in the snapshot's
    /// `desktopRequests` until answered (`rdp.answer`) or for 60 seconds.
    #[serde(rename = "rdp.requested", rename_all = "camelCase")]
    DesktopRequested {
        endpoint_id: String,
        display_name: String,
    },

    /// A forwarded connection reached the other device's Remote Desktop.
    #[serde(rename = "rdp.opened", rename_all = "camelCase")]
    DesktopOpened { endpoint_id: String },

    /// A forwarded connection did not get through, and why.
    #[serde(rename = "rdp.refused", rename_all = "camelCase")]
    DesktopRefused { endpoint_id: String, reason: String },

    /// A message from another device arrived.
    #[serde(rename = "chat.message", rename_all = "camelCase")]
    ChatMessage {
        conversation: String,
        id: String,
        author: String,
    },

    /// A paired device offers files; answer with `files.accept` or `files.decline`.
    #[serde(rename = "files.offered", rename_all = "camelCase")]
    FilesOffered {
        transfer_id: String,
        endpoint_id: String,
        display_name: String,
        file_count: u32,
        total: u64,
    },

    /// A receive ended: done, failed or cancelled (see the snapshot's transfer).
    #[serde(rename = "files.finished", rename_all = "camelCase")]
    TransferFinished { transfer_id: String },

    /// A paired device calls; the snapshot's `call` rings until answered (`call.answer`).
    #[serde(rename = "call.incoming", rename_all = "camelCase")]
    CallIncoming {
        endpoint_id: String,
        display_name: String,
    },

    /// A call to this device stopped ringing unanswered.
    #[serde(rename = "call.missed", rename_all = "camelCase")]
    CallMissed {
        endpoint_id: String,
        display_name: String,
    },

    /// An event from DashBeam's node, as it named it (`device-paired`,
    /// `paired-device-presence`, …). Kept untyped until a feature needs it typed.
    #[serde(rename = "node.event")]
    Node {
        name: String,
        payload: Option<serde_json::Value>,
    },
}
