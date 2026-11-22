//! DeskBeam's headless engine (plan §3.1).
//!
//! The engine owns all state. Clients (the egui app, the CLI, tests) do three things only:
//! - send a [`Command`] through [`Engine::execute`],
//! - read the latest [`Snapshot`] ([`Engine::snapshot`], or watch it with [`Engine::watch`]),
//! - listen to [`Event`]s ([`Engine::events`]).
//!
//! Underneath runs DashBeam's `NodeService` (forked into `deskbeam-net`), which does the
//! iroh networking: identity, pairing, presence, nearby discovery and file transfer. Its
//! untyped string events come in through [`emitter`] and become snapshot updates.

mod attachments;
mod audio;
mod call;
mod chat;
pub mod command;
mod desktop;
mod emitter;
pub mod engine;
pub mod event;
mod lan;
pub mod lock;
mod transfer;

pub use attachments::{Picture, preview_picture};
pub use call::{CallAudio, CallCamera, CallVideo};
pub use command::{Command, CommandError};
pub use deskbeam_call as calls;
pub use deskbeam_model as model;
pub use deskbeam_model::Snapshot;
pub use engine::{Engine, EngineConfig, default_data_dir};
pub use event::Event;
pub use lan::LanBackend;

/// Runs LAN mode's admin-rights helper when the program was started as one (`--lan-helper`,
/// see `deskbeam_lan::helper`): call first thing in `main`, and exit with the code it returns.
pub fn run_lan_helper_if_asked() -> Option<i32> {
    deskbeam_lan::helper::run_if_helper()
}
pub use lock::{DataDirLock, LockError};

/// DashBeam's engine API, as its `engine` facade crate exposed it. DashBeam's integration
/// tests use it; new DeskBeam code goes through [`Engine`] instead.
pub mod net {
    pub use native::*;
    pub use protocol::identity::unix_now_ms;
    pub use protocol::{
        ControlMessage, Discoverability, DownloadToStoreResult, METADATA_ALPN, PairedDevice,
        PairingStatus, PairingTicket, RelayConfigArg, RelayFallbackPolicy, RelayStatusResponse,
        ShareSessionOutcome, VerifyRelaysResponse, allows_unpaired_control, build_relay_mode,
        download_to_store, get_relay_status, pairing_host_is_persistent, relay_fallback_policy,
        resolve_relay_mode_with_fallback, run_share_session, sanitize_folder_name,
        should_answer_identity, should_publish_mdns, should_run_background_presence,
        sign_challenge, unpaired_message_allowed, verify_challenge, verify_relays,
    };
}
