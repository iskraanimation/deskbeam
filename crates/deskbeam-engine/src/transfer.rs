//! File transfers with paired devices, on DashBeam's engine.
//!
//! - Sending shares the files (DashBeam's resumable, BLAKE3-verified iroh-blobs share, on
//!   an endpoint of its own) and hands the share's ticket to the paired device as an
//!   invite over the control connection.
//! - Receiving: an invite becomes an incoming transfer. It is accepted on its own when the
//!   sender may "Accept files without asking", declined on its own when it may not "Send
//!   files", and otherwise waits for `files.accept` or `files.decline`. Accepting downloads
//!   into the downloads folder.
//! - Each transfer gets its own event receiver, so DashBeam's progress events land on the
//!   right one.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use deskbeam_model::{Direction, Transfer, TransferState};
use protocol::{EventEmitter, ReceiveOptions, SendOptions};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use crate::{
    Event,
    command::CommandError,
    engine::{Engine, Inner, paired_id, unix_ms},
};

/// How often progress may republish the snapshot.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// How long a finished share keeps serving, for the receiver's last requests.
const SHARE_LINGER: Duration = Duration::from_secs(10);
/// How long an offer waits for an answer before the share stops.
const OFFER_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What the engine keeps for a transfer beyond what clients see.
pub(crate) struct Running {
    pub(crate) view: Transfer,
    /// An offer's ticket, until it is accepted.
    ticket: Option<String>,
    /// Ends a download.
    cancel: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
pub(crate) struct Transfers {
    pub(crate) list: Vec<Running>,
    /// Shares being served, by transfer id. Dropping one stops it.
    shares: HashMap<String, native::SendResult>,
    last_published: Option<Instant>,
}

impl Transfers {
    fn get(&mut self, id: &str) -> Option<&mut Running> {
        self.list.iter_mut().find(|t| t.view.id == id)
    }

    pub(crate) fn views(&self) -> Vec<Transfer> {
        let mut views: Vec<Transfer> = self.list.iter().map(|t| t.view.clone()).collect();
        views.sort_by_key(|t| std::cmp::Reverse(t.started_at));
        views
    }
}

/// Receives one transfer's events from DashBeam.
struct TransferEmitter {
    id: String,
    inner: Weak<Inner>,
}

impl EventEmitter for TransferEmitter {
    fn emit_event(&self, event_name: &str) -> Result<(), String> {
        self.on(event_name, None);
        Ok(())
    }

    fn emit_event_with_payload(&self, event_name: &str, payload: &str) -> Result<(), String> {
        self.on(event_name, Some(payload));
        Ok(())
    }
}

impl TransferEmitter {
    fn on(&self, name: &str, payload: Option<&str>) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let publish = {
            let mut transfers = inner.transfers.lock().expect("lock");
            let Some(transfer) = transfers.get(&self.id) else {
                return;
            };
            match (name, payload) {
                ("transfer-progress" | "receive-progress", Some(p)) => {
                    // "bytes:total:bytes per second × 1000"
                    let mut parts = p.split(':').map(|n| n.parse::<i64>().unwrap_or(0));
                    let (bytes, total, speed) = (
                        parts.next().unwrap_or(0),
                        parts.next().unwrap_or(0),
                        parts.next().unwrap_or(0),
                    );
                    let view = &mut transfer.view;
                    view.bytes = bytes.max(0) as u64;
                    if total > 0 {
                        view.total = total as u64;
                    }
                    view.bytes_per_sec = (speed.max(0) / 1000) as u64;
                    if !view.state.is_finished() {
                        view.state = TransferState::Running;
                    }
                }
                ("receive-file-names", Some(p)) => {
                    if let Ok(names) = serde_json::from_str::<Vec<String>>(p) {
                        transfer.view.names = names;
                    }
                }
                ("transfer-completed", _) if transfer.view.direction == Direction::Send => {
                    transfer.view.bytes = transfer.view.total;
                    transfer.view.state = TransferState::Done { path: None };
                    let (id, inner) = (self.id.clone(), self.inner.clone());
                    tokio::spawn(async move {
                        tokio::time::sleep(SHARE_LINGER).await;
                        if let Some(inner) = inner.upgrade() {
                            inner.transfers.lock().expect("lock").shares.remove(&id);
                        }
                    });
                }
                _ => return,
            }
            // Progress comes often; publish it a few times a second.
            let now = Instant::now();
            let due = transfers
                .last_published
                .is_none_or(|at| now.duration_since(at) >= PROGRESS_EVERY)
                || name != "transfer-progress" && name != "receive-progress";
            if due {
                transfers.last_published = Some(now);
            }
            due
        };
        if publish {
            let engine = Engine { inner };
            tokio::spawn(async move { engine.refresh().await });
        }
    }
}

impl Engine {
    fn emitter(&self, id: &str) -> protocol::AppHandle {
        Some(Arc::new(TransferEmitter {
            id: id.to_string(),
            inner: Arc::downgrade(&self.inner),
        }))
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut Running)) {
        if let Some(t) = self.inner.transfers.lock().expect("lock").get(id) {
            change(t);
        }
    }

    pub(crate) async fn send_files(
        &self,
        endpoint_id: &str,
        paths: &[String],
    ) -> Result<Value, CommandError> {
        if paths.is_empty() {
            return Err(CommandError::InvalidParams(
                "files.send".into(),
                "no files given".into(),
            ));
        }
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        if let Some(missing) = paths.iter().find(|p| !p.exists()) {
            return Err(CommandError::Failed(format!(
                "{} does not exist",
                missing.display()
            )));
        }
        let node = self.node().await?;
        let id = paired_id(&node, endpoint_id)?;
        let device = node
            .list_paired()?
            .into_iter()
            .find(|d| d.endpoint_id == id)
            .ok_or_else(|| CommandError::UnknownDevice(id.clone()))?;
        if !device.online {
            return Err(CommandError::Offline(device.display_name));
        }

        let transfer_id = deskbeam_chat::new_id();
        let names = paths
            .iter()
            .map(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string())
            })
            .collect();
        self.inner
            .transfers
            .lock()
            .expect("lock")
            .list
            .push(Running {
                view: Transfer {
                    id: transfer_id.clone(),
                    direction: Direction::Send,
                    peer: id.clone(),
                    peer_name: self.local_name(&id, device.display_name.clone()),
                    names,
                    file_count: 0,
                    total: 0,
                    bytes: 0,
                    bytes_per_sec: 0,
                    state: TransferState::Offered,
                    started_at: unix_ms(),
                },
                ticket: None,
                cancel: None,
            });

        let options = SendOptions {
            relay_mode: self.inner.relay.clone(),
            ..Default::default()
        };
        let share = match native::start_share_items(
            paths.clone(),
            options,
            &self.emitter(&transfer_id),
            None,
        )
        .await
        {
            Ok(share) => share,
            Err(e) => {
                let reason = format!("{e:#}");
                self.update(&transfer_id, |t| {
                    t.view.state = TransferState::Failed {
                        reason: reason.clone(),
                    }
                });
                return Err(CommandError::Failed(reason));
            }
        };
        let file_count = count_files(&paths);
        let (ticket, size) = (share.ticket.clone(), share.size);
        {
            let mut transfers = self.inner.transfers.lock().expect("lock");
            if let Some(t) = transfers.get(&transfer_id) {
                t.view.total = size;
                t.view.file_count = file_count;
            }
            transfers.shares.insert(transfer_id.clone(), share);
        }

        match node
            .invite_paired_device(&id, &ticket, file_count, size)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                self.stop_share(
                    &transfer_id,
                    TransferState::Failed {
                        reason: format!("{} could not be reached", device.display_name),
                    },
                );
                return Err(CommandError::Offline(device.display_name));
            }
            Err(e) => {
                let reason = format!("{e:#}");
                self.stop_share(
                    &transfer_id,
                    TransferState::Failed {
                        reason: reason.clone(),
                    },
                );
                return Err(CommandError::Failed(reason));
            }
        }

        // An offer nobody answers stops serving after a while.
        let (engine, waiting) = (self.clone(), transfer_id.clone());
        tokio::spawn(async move {
            tokio::time::sleep(OFFER_TIMEOUT).await;
            let unanswered = engine
                .inner
                .transfers
                .lock()
                .expect("lock")
                .get(&waiting)
                .is_some_and(|t| t.view.state == TransferState::Offered);
            if unanswered {
                engine.stop_share(
                    &waiting,
                    TransferState::Failed {
                        reason: "no answer".into(),
                    },
                );
                engine.refresh().await;
            }
        });
        Ok(json!({ "transferId": transfer_id }))
    }

    fn stop_share(&self, transfer_id: &str, state: TransferState) {
        let mut transfers = self.inner.transfers.lock().expect("lock");
        transfers.shares.remove(transfer_id);
        if let Some(t) = transfers.get(transfer_id) {
            t.view.state = state;
        }
    }

    /// The other device answered an offer this one sent.
    pub(crate) fn offer_answered(&self, peer: &str, accepted: bool) {
        let offered = {
            let transfers = self.inner.transfers.lock().expect("lock");
            transfers
                .list
                .iter()
                .filter(|t| {
                    t.view.direction == Direction::Send
                        && t.view.state == TransferState::Offered
                        && t.view.peer.eq_ignore_ascii_case(peer)
                })
                .max_by_key(|t| t.view.started_at)
                .map(|t| t.view.id.clone())
        };
        let Some(id) = offered else { return };
        if accepted {
            self.update(&id, |t| t.view.state = TransferState::Running);
        } else {
            self.stop_share(&id, TransferState::Declined);
        }
    }

    /// A paired device offers files.
    pub(crate) fn offer_received(&self, payload: &Value) {
        let field = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
        let (Some(peer), Some(ticket)) = (field("remote_endpoint_id"), field("blob_ticket")) else {
            return;
        };
        let peer = peer.to_lowercase();
        let permissions = self.inner.store.permissions(&peer).unwrap_or_default();
        let id = deskbeam_chat::new_id();
        let view = Transfer {
            id: id.clone(),
            direction: Direction::Receive,
            peer: peer.clone(),
            peer_name: self.local_name(&peer, field("sender_name").unwrap_or_default()),
            names: Vec::new(),
            file_count: payload
                .get("file_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            total: payload
                .get("total_size")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            bytes: 0,
            bytes_per_sec: 0,
            state: TransferState::Incoming,
            started_at: unix_ms(),
        };
        let event = Event::FilesOffered {
            transfer_id: id.clone(),
            endpoint_id: peer.clone(),
            display_name: view.peer_name.clone(),
            file_count: view.file_count,
            total: view.total,
        };
        self.inner
            .transfers
            .lock()
            .expect("lock")
            .list
            .push(Running {
                view,
                ticket: Some(ticket),
                cancel: None,
            });

        let engine = self.clone();
        tokio::spawn(async move {
            let answered = if !permissions.send_files {
                debug!(%peer, "files from a device that may not send them: declined");
                engine.decline_files(&id).await
            } else if permissions.auto_accept_files {
                engine.accept_files(&id).await
            } else {
                let _ = engine.inner.events.send(event);
                Ok(Value::Null)
            };
            if let Err(e) = answered {
                warn!("answering a file offer: {e}");
            }
            engine.refresh().await;
        });
    }

    pub(crate) async fn accept_files(&self, transfer_id: &str) -> Result<Value, CommandError> {
        let (peer, ticket) = self.take_offer(transfer_id)?;
        let node = self.node().await?;
        node.respond_paired_invite(&peer, true).await?;
        let sender = self
            .inner
            .transfers
            .lock()
            .expect("lock")
            .get(transfer_id)
            .map(|t| t.view.peer_name.clone())
            .unwrap_or_default();

        let (cancel, cancelled) = oneshot::channel();
        self.update(transfer_id, |t| {
            t.view.state = TransferState::Running;
            t.cancel = Some(cancel);
        });
        let options = ReceiveOptions {
            // Each sender's files in a folder of its own: Downloads/<sender>/.
            output_dir: Some(self.downloads_dir().join(folder_name(&sender, &peer))),
            relay_mode: self.inner.relay.clone(),
            ..Default::default()
        };
        let (engine, id) = (self.clone(), transfer_id.to_string());
        let emitter = self.emitter(transfer_id);
        tokio::spawn(async move {
            let result = native::download(ticket, options, emitter, cancelled).await;
            engine.update(&id, |t| {
                t.cancel = None;
                if t.view.state == TransferState::Cancelled {
                    return;
                }
                t.view.state = match result {
                    Ok(done) => {
                        t.view.bytes = t.view.total.max(t.view.bytes);
                        TransferState::Done {
                            path: Some(done.file_path.display().to_string()),
                        }
                    }
                    Err(e) => TransferState::Failed {
                        reason: format!("{e:#}"),
                    },
                };
            });
            let _ = engine.inner.events.send(Event::TransferFinished {
                transfer_id: id.clone(),
            });
            engine.refresh().await;
        });
        Ok(Value::Null)
    }

    pub(crate) async fn decline_files(&self, transfer_id: &str) -> Result<Value, CommandError> {
        let (peer, _) = self.take_offer(transfer_id)?;
        self.update(transfer_id, |t| t.view.state = TransferState::Declined);
        let node = self.node().await?;
        node.respond_paired_invite(&peer, false).await?;
        Ok(Value::Null)
    }

    /// An incoming offer's peer and ticket, which can be answered once.
    fn take_offer(&self, transfer_id: &str) -> Result<(String, String), CommandError> {
        let mut transfers = self.inner.transfers.lock().expect("lock");
        let t = transfers
            .get(transfer_id)
            .ok_or_else(|| CommandError::Failed(format!("no transfer {transfer_id}")))?;
        if t.view.state != TransferState::Incoming {
            return Err(CommandError::Failed(
                "this transfer was already answered".into(),
            ));
        }
        let ticket = t
            .ticket
            .take()
            .ok_or_else(|| CommandError::Failed("this transfer was already answered".into()))?;
        Ok((t.view.peer.clone(), ticket))
    }

    pub(crate) fn cancel_transfer(&self, transfer_id: &str) -> Result<Value, CommandError> {
        let mut transfers = self.inner.transfers.lock().expect("lock");
        transfers.shares.remove(transfer_id);
        let t = transfers
            .get(transfer_id)
            .ok_or_else(|| CommandError::Failed(format!("no transfer {transfer_id}")))?;
        if t.view.state.is_finished() {
            return Ok(Value::Null);
        }
        if let Some(cancel) = t.cancel.take() {
            let _ = cancel.send(());
        }
        t.view.state = TransferState::Cancelled;
        Ok(Value::Null)
    }

    pub(crate) fn clear_transfers(&self) -> Result<Value, CommandError> {
        self.inner
            .transfers
            .lock()
            .expect("lock")
            .list
            .retain(|t| !t.view.state.is_finished());
        Ok(Value::Null)
    }

    pub(crate) fn downloads_dir(&self) -> PathBuf {
        self.inner
            .downloads_dir
            .clone()
            .or_else(dirs::download_dir)
            .unwrap_or_else(|| self.inner.data_dir.join("Downloads"))
    }
}

/// A folder name for files from `sender`, safe on every OS: characters Windows, macOS or
/// Linux refuse become `_`, and an empty or reserved result falls back to the endpoint id.
pub(crate) fn folder_name(sender: &str, endpoint_id: &str) -> String {
    let cleaned: String = sender
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Windows drops trailing dots and spaces; a name of only dots means a parent folder.
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let reserved = RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&cleaned));
    if cleaned.is_empty() || reserved {
        endpoint_id.chars().take(10).collect()
    } else {
        cleaned.chars().take(80).collect()
    }
}

/// How many files the paths hold, folders counted by their contents.
fn count_files(paths: &[PathBuf]) -> u32 {
    let mut count = 0u32;
    for path in paths {
        if path.is_dir() {
            count += walk(path);
        } else {
            count += 1;
        }
    }
    count
}

fn walk(dir: &std::path::Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let path = e.path();
            if path.is_dir() { walk(&path) } else { 1 }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::folder_name;

    #[test]
    fn sender_folders_are_safe_names() {
        assert_eq!(folder_name("Alice MacBook", "abc"), "Alice MacBook");
        assert_eq!(folder_name("a/b\\c:d*e?", "abc"), "a_b_c_d_e_");
        assert_eq!(folder_name("  Office PC.. ", "abc"), "Office PC");
        assert_eq!(folder_name("..", "abcdef0123456789"), "abcdef0123");
        assert_eq!(folder_name("", "abcdef0123456789"), "abcdef0123");
        assert_eq!(folder_name("con", "abcdef0123456789"), "abcdef0123");
    }
}
