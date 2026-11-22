//! Files attached to chat messages (plan Phase 4b).
//!
//! - The files live in one iroh-blobs store, `attachments/` in the data directory, for as
//!   long as the messages do: sent files are copied in (so moving the original doesn't break
//!   the chat), received ones are fetched into it. Each is tagged, so nothing is collected.
//! - The node serves that store on its endpoint, to paired devices only, and only files
//!   attached to a message the asking device may see: a direct chat with it, or a group it
//!   is in with this device (see [`Engine::may_fetch`]).
//! - A file is fetched from its message's author first, then from any other paired member
//!   online, so in a group it still arrives after the author has gone offline. Pictures up to
//!   [`AUTO_FETCH_LIMIT`] come on their own when the message arrives; the rest when asked.
//! - Pictures get a small JPEG preview inside the message, made here when sending.

use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use data_encoding::BASE64;
use deskbeam_chat::{Attachment, ConversationId, ImageSize, MAX_ATTACHMENTS, MAX_PREVIEW, Message};
use deskbeam_model::{AUTO_FETCH_LIMIT, AttachmentState, ChatAttachment};
use iroh::EndpointId;
use iroh_blobs::{
    BlobsProtocol, Hash, HashAndFormat,
    api::{blobs::BlobStatus, remote::GetProgressItem},
    provider::events::{
        AbortReason, ConnectMode, EventMask, EventSender, ObserveMode, ProviderMessage,
        RequestMode, ThrottleMode,
    },
    store::{
        GcConfig, ProtectOutcome,
        fs::{FsStore, options::Options},
    },
};
use n0_future::StreamExt;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::{
    command::CommandError,
    engine::{Engine, Inner},
    transfer::folder_name,
};

/// The store's folder in the data directory.
const DIR: &str = "attachments";
/// Where copies of files opened in other apps go, in the data directory.
const OPENED: &str = "opened";
/// Tags that keep attached files: `att/<hash>`.
const TAG_PREFIX: &str = "att/";
/// The longest side of a preview, in pixels, and its JPEG quality; smaller if too big.
const PREVIEW_SIDES: [(u32, u8); 3] = [(320, 70), (240, 60), (160, 50)];
/// Pictures larger than this get no preview: decoding them would take too long.
const PREVIEW_SOURCE_LIMIT: u64 = 40 * 1024 * 1024;
/// How long connecting to one member for a file may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How often fetch progress may republish the snapshot.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// The provider asks about every connection and request; it may push nothing.
const SERVE_MASK: EventMask = EventMask {
    connected: ConnectMode::Intercept,
    get: RequestMode::Intercept,
    get_many: RequestMode::Disabled,
    push: RequestMode::Disabled,
    observe: ObserveMode::Intercept,
    throttle: ThrottleMode::None,
};

/// The engine's attachments: the store once open, which files are here, and fetches.
#[derive(Default)]
pub(crate) struct Attachments {
    store: OnceLock<FsStore>,
    /// Hashes (hex) held whole in the store.
    complete: Mutex<HashSet<String>>,
    /// Fetches running or failed, by hash.
    progress: Mutex<HashMap<String, Progress>>,
    /// "Free up space" asked for a garbage collection run; the store's collector checks
    /// every [`GC_CHECK`] and skips its run unless this is set.
    gc_wanted: Arc<AtomicBool>,
    /// Files the run must keep although untagged: those being fetched.
    gc_keep: Arc<Mutex<std::collections::HashSet<Hash>>>,
    /// Space is being freed: no new fetches start meanwhile.
    freeing: AtomicBool,
}

/// How often the store's collector looks whether a run was asked for.
const GC_CHECK: Duration = Duration::from_secs(2);
/// The longest "Free up space" waits for the run.
const GC_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone)]
enum Progress {
    Fetching(u64),
    Failed(String),
}

impl Attachments {
    fn store(&self) -> Result<&FsStore, CommandError> {
        self.store
            .get()
            .ok_or_else(|| CommandError::Failed("attachments are not available".into()))
    }

    fn is_complete(&self, hash: &str) -> bool {
        self.complete.lock().expect("lock").contains(hash)
    }

    /// What clients see of an attachment.
    pub(crate) fn view(&self, attachment: &Attachment) -> ChatAttachment {
        let state = if self.is_complete(&attachment.hash) {
            AttachmentState::Ready
        } else {
            match self.progress.lock().expect("lock").get(&attachment.hash) {
                Some(Progress::Fetching(bytes)) => AttachmentState::Fetching { bytes: *bytes },
                Some(Progress::Failed(reason)) => AttachmentState::Failed {
                    reason: reason.clone(),
                },
                None => AttachmentState::Missing,
            }
        };
        ChatAttachment {
            hash: attachment.hash.clone(),
            name: attachment.name.clone(),
            size: attachment.size,
            mime: attachment.mime.clone(),
            width: attachment.image.map(|i| i.width),
            height: attachment.image.map(|i| i.height),
            preview: attachment.preview.clone(),
            state,
        }
    }
}

/// Opens the store and returns the protocol that serves it, or `None` if it can't be opened
/// (messages still work then, without files).
pub(crate) async fn open(inner: &Inner, weak: Weak<Inner>) -> Option<BlobsProtocol> {
    let dir = inner.data_dir.join(DIR);
    // Garbage collection runs only when "Free up space" asks; it keeps tagged files (sent
    // ones, and received ones not being freed) and files being fetched.
    let mut options = Options::new(&dir);
    let (wanted, keep) = (
        inner.attachments.gc_wanted.clone(),
        inner.attachments.gc_keep.clone(),
    );
    options.gc = Some(GcConfig {
        interval: GC_CHECK,
        add_protected: Some(Arc::new(move |live| {
            let (wanted, keep) = (wanted.clone(), keep.clone());
            Box::pin(async move {
                if !wanted.swap(false, Ordering::SeqCst) {
                    return ProtectOutcome::Abort;
                }
                live.extend(keep.lock().expect("lock").iter().copied());
                ProtectOutcome::Continue
            })
        })),
    });
    let store = match FsStore::load_with_opts(dir.join("blobs.db"), options).await {
        Ok(store) => store,
        Err(e) => {
            warn!("attachments store at {} not opened: {e:#}", dir.display());
            return None;
        }
    };
    match store.blobs().list().hashes().await {
        Ok(hashes) => {
            let mut complete = HashSet::new();
            for hash in hashes {
                if matches!(
                    store.blobs().status(hash).await,
                    Ok(BlobStatus::Complete { .. })
                ) {
                    complete.insert(hash.to_hex());
                }
            }
            *inner.attachments.complete.lock().expect("lock") = complete;
        }
        Err(e) => warn!("listing attachments: {e:#}"),
    }
    let (events, rx) = EventSender::channel(64, SERVE_MASK);
    tokio::spawn(guard(weak, rx));
    let protocol = BlobsProtocol::new(&store, Some(events));
    let _ = inner.attachments.store.set(store);
    Some(protocol)
}

/// Answers the provider: who may connect, and which files they may have.
async fn guard(inner: Weak<Inner>, mut rx: tokio::sync::mpsc::Receiver<ProviderMessage>) {
    let mut peers: HashMap<u64, String> = HashMap::new();
    while let Some(message) = rx.recv().await {
        let Some(strong) = inner.upgrade() else {
            break;
        };
        let engine = Engine { inner: strong };
        match message {
            ProviderMessage::ClientConnected(msg) => {
                let peer = msg
                    .inner
                    .endpoint_id
                    .map(|id| id.to_string().to_lowercase());
                let answer = match peer {
                    Some(peer) if engine.paired_ids().contains(&peer) => {
                        peers.insert(msg.inner.connection_id, peer);
                        Ok(())
                    }
                    other => {
                        debug!(peer = ?other, "attachment fetch from a device that is not paired refused");
                        Err(AbortReason::Permission)
                    }
                };
                msg.tx.send(answer).await.ok();
            }
            ProviderMessage::ConnectionClosed(msg) => {
                peers.remove(&msg.inner.connection_id);
            }
            ProviderMessage::GetRequestReceived(msg) => {
                let hash = msg.inner.request.hash.to_hex();
                let answer = allowed(&engine, peers.get(&msg.inner.connection_id), &hash);
                msg.tx.send(answer).await.ok();
            }
            ProviderMessage::ObserveRequestReceived(msg) => {
                let hash = msg.inner.request.hash.to_hex();
                let answer = allowed(&engine, peers.get(&msg.inner.connection_id), &hash);
                msg.tx.send(answer).await.ok();
            }
            // Not asked for in `SERVE_MASK`, or nothing to answer.
            _ => {}
        }
    }
}

fn allowed(engine: &Engine, peer: Option<&String>, hash: &str) -> Result<(), AbortReason> {
    match peer {
        Some(peer) if engine.may_fetch(peer, hash) => Ok(()),
        _ => {
            debug!(?peer, %hash, "attachment request refused");
            Err(AbortReason::Permission)
        }
    }
}

impl Engine {
    /// Whether `peer` (paired, lowercase) may have the file `hash`: it is attached to a
    /// message in a direct chat with `peer` that allows chat, or in a group both are in.
    pub(crate) fn may_fetch(&self, peer: &str, hash: &str) -> bool {
        let Some(me) = self.my_id() else {
            return false;
        };
        let conversations = self
            .inner
            .store
            .attachment_conversations(hash)
            .unwrap_or_default();
        conversations
            .iter()
            .any(|c| match c.parse::<ConversationId>() {
                Ok(conversation @ ConversationId::Direct { .. }) => {
                    conversation.other_than(&me) == Some(peer)
                        && self.inner.store.permissions(peer).is_ok_and(|p| p.chat)
                }
                Ok(ConversationId::Group(id)) => self
                    .held_group(&id)
                    .is_some_and(|g| g.has_member(&me) && g.has_member(peer)),
                Err(_) => false,
            })
    }

    /// Copies files into the store for a message, with previews for pictures.
    pub(crate) async fn import_attachments(
        &self,
        paths: &[String],
    ) -> Result<Vec<Attachment>, CommandError> {
        if paths.len() > MAX_ATTACHMENTS {
            return Err(CommandError::InvalidParams(
                "chat.send".into(),
                format!("at most {MAX_ATTACHMENTS} attachments per message"),
            ));
        }
        let store = self.inner.attachments.store()?;
        let mut attachments = Vec::new();
        for path in paths {
            let path = std::fs::canonicalize(path)
                .map_err(|e| CommandError::Failed(format!("{path}: {e}")))?;
            let meta = std::fs::metadata(&path)
                .map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?;
            if !meta.is_file() {
                return Err(CommandError::InvalidParams(
                    "chat.send".into(),
                    format!(
                        "{} is not a file; folders go through the Files page",
                        path.display()
                    ),
                ));
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into());
            let mime = mime_for(&name).to_string();
            let tag = store
                .blobs()
                .add_path(&path)
                .temp_tag()
                .await
                .map_err(|e| CommandError::Failed(format!("{name}: {e}")))?;
            let hash = tag.hash();
            store
                .tags()
                .set(tag_name(hash), tag.hash_and_format())
                .await
                .map_err(|e| CommandError::Failed(format!("{name}: {e}")))?;
            self.inner
                .attachments
                .complete
                .lock()
                .expect("lock")
                .insert(hash.to_hex());
            let (image, preview) =
                if mime.starts_with("image/") && meta.len() <= PREVIEW_SOURCE_LIMIT {
                    let source = path.clone();
                    match tokio::task::spawn_blocking(move || make_preview(&source)).await {
                        Ok(Some((size, preview))) => (Some(size), Some(preview)),
                        _ => (None, None),
                    }
                } else {
                    (None, None)
                };
            attachments.push(Attachment {
                hash: hash.to_hex(),
                name,
                size: meta.len(),
                mime,
                image,
                preview,
            });
        }
        Ok(attachments)
    }

    /// Notes a message's attachments, and fetches the pictures that come on their own.
    pub(crate) fn note_attachments(&self, message: &Message, mine: bool) {
        let conversation = message.conversation.to_string();
        for attachment in &message.attachments {
            if let Err(e) =
                self.inner
                    .store
                    .add_attachment(&attachment.hash, &message.id, &conversation)
            {
                warn!("noting an attachment: {e}");
            }
        }
        if mine
            || !self
                .inner
                .preferences
                .lock()
                .expect("lock")
                .auto_fetch_images
        {
            return;
        }
        for attachment in &message.attachments {
            if attachment.mime.starts_with("image/") && attachment.size <= AUTO_FETCH_LIMIT {
                let (engine, id, hash) =
                    (self.clone(), message.id.clone(), attachment.hash.clone());
                tokio::spawn(async move {
                    if let Err(e) = engine.fetch_attachment(&id, &hash).await {
                        debug!(%hash, "picture not fetched on its own: {e}");
                    }
                });
            }
        }
    }

    /// The message with `message_id` and its attachment `hash`.
    fn attached(
        &self,
        message_id: &str,
        hash: &str,
    ) -> Result<(Message, Attachment), CommandError> {
        let stored = self
            .inner
            .store
            .message(message_id)?
            .ok_or_else(|| CommandError::Failed(format!("no message {message_id}")))?;
        let message: Message = serde_json::from_str(&stored.json).map_err(anyhow::Error::from)?;
        let attachment = message
            .attachments
            .iter()
            .find(|a| a.hash == hash)
            .cloned()
            .ok_or_else(|| CommandError::Failed(format!("no attachment {hash} on that message")))?;
        Ok((message, attachment))
    }

    pub(crate) async fn fetch_attachment(
        &self,
        message_id: &str,
        hash: &str,
    ) -> Result<Value, CommandError> {
        let (message, attachment) = self.attached(message_id, hash)?;
        let attachments = &self.inner.attachments;
        let store = attachments.store()?.clone();
        if attachments.is_complete(hash) {
            return Ok(Value::Null);
        }
        if attachments.freeing.load(Ordering::SeqCst) {
            return Err(CommandError::Failed(
                "space is being freed; try again in a moment".into(),
            ));
        }
        {
            let mut progress = attachments.progress.lock().expect("lock");
            if matches!(progress.get(hash), Some(Progress::Fetching(_))) {
                return Ok(Value::Null);
            }
            progress.insert(hash.to_string(), Progress::Fetching(0));
        }
        self.refresh().await;

        let blob: Hash = hash
            .parse()
            .map_err(|_| CommandError::Failed(format!("{hash} is not a hash")))?;
        let mut reason = "no member who may have it is online".to_string();
        for peer in self.holders(&message) {
            match self.fetch_from(&store, &peer, blob, &attachment).await {
                Ok(()) => {
                    store
                        .tags()
                        .set(tag_name(blob), HashAndFormat::raw(blob))
                        .await
                        .map_err(|e| CommandError::Failed(e.to_string()))?;
                    attachments
                        .complete
                        .lock()
                        .expect("lock")
                        .insert(hash.to_string());
                    attachments.progress.lock().expect("lock").remove(hash);
                    self.refresh().await;
                    return Ok(Value::Null);
                }
                Err(e) => {
                    debug!(%peer, %hash, "fetching an attachment failed: {e:#}");
                    reason = format!("{e}");
                }
            }
        }
        attachments
            .progress
            .lock()
            .expect("lock")
            .insert(hash.to_string(), Progress::Failed(reason.clone()));
        self.refresh().await;
        Err(CommandError::Failed(reason))
    }

    /// Who to fetch a message's files from, best first: its author, then the other members
    /// this device is paired with; only those online.
    fn holders(&self, message: &Message) -> Vec<String> {
        let Some(node) = self.inner.node_now() else {
            return Vec::new();
        };
        let me = self.my_id().unwrap_or_default();
        let online: HashSet<String> = node
            .list_paired()
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.online)
            .map(|d| d.endpoint_id.to_lowercase())
            .collect();
        let author = message.author.to_lowercase();
        let mut holders = vec![author.clone()];
        match &message.conversation {
            ConversationId::Direct { .. } => {
                if let Some(other) = message.conversation.other_than(&me) {
                    holders.push(other.to_string());
                }
            }
            ConversationId::Group(id) => {
                if let Some(group) = self.held_group(id) {
                    holders.extend(group.members);
                }
            }
        }
        let mut seen = HashSet::new();
        holders
            .into_iter()
            .filter(|h| *h != me && online.contains(h) && seen.insert(h.clone()))
            .collect()
    }

    async fn fetch_from(
        &self,
        store: &FsStore,
        peer: &str,
        hash: Hash,
        attachment: &Attachment,
    ) -> anyhow::Result<()> {
        let node = self.node().await?;
        let remote: EndpointId = peer.parse()?;
        let endpoint = node.endpoint().await;
        let connection =
            tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(remote, iroh_blobs::ALPN))
                .await
                .map_err(|_| anyhow::anyhow!("connecting timed out"))??;
        let mut progress = store
            .remote()
            .fetch(connection.clone(), HashAndFormat::raw(hash))
            .stream();
        let mut last = Instant::now();
        let key = attachment.hash.clone();
        let outcome = loop {
            match progress.next().await {
                Some(GetProgressItem::Progress(bytes)) => {
                    self.inner
                        .attachments
                        .progress
                        .lock()
                        .expect("lock")
                        .insert(key.clone(), Progress::Fetching(bytes));
                    if last.elapsed() >= PROGRESS_EVERY {
                        last = Instant::now();
                        self.refresh().await;
                    }
                }
                Some(GetProgressItem::Done(_)) => break Ok(()),
                Some(GetProgressItem::Error(e)) => break Err(anyhow::anyhow!("{e}")),
                None => break Err(anyhow::anyhow!("the fetch ended early")),
            }
        };
        connection.close(0u32.into(), b"done");
        outcome?;
        anyhow::ensure!(
            matches!(
                store.blobs().status(hash).await?,
                BlobStatus::Complete { .. }
            ),
            "the file did not arrive whole"
        );
        Ok(())
    }

    /// Saves an attached file that is here, to `path` or to Downloads/<author>/<name>.
    pub(crate) async fn save_attachment(
        &self,
        message_id: &str,
        hash: &str,
        path: Option<&str>,
    ) -> Result<Value, CommandError> {
        let (message, attachment) = self.attached(message_id, hash)?;
        let store = self.inner.attachments.store()?;
        if !self.inner.attachments.is_complete(hash) {
            return Err(CommandError::Failed(format!(
                "{} is not here yet; fetch it first",
                attachment.name
            )));
        }
        let target = match path {
            Some(path) => PathBuf::from(path),
            None => {
                let author = message.author.to_lowercase();
                let name = self.author_name(&author);
                let folder = self.downloads_dir().join(folder_name(&name, &author));
                std::fs::create_dir_all(&folder).map_err(anyhow::Error::from)?;
                unused_path(&folder, &attachment.name)
            }
        };
        let target = std::path::absolute(&target).map_err(anyhow::Error::from)?;
        let blob: Hash = hash
            .parse()
            .map_err(|_| CommandError::Failed(format!("{hash} is not a hash")))?;
        store
            .blobs()
            .export(blob, &target)
            .await
            .map_err(|e| CommandError::Failed(format!("saving {}: {e}", attachment.name)))?;
        Ok(json!({ "path": target.display().to_string() }))
    }

    /// A copy of an attached file to open in the system's own app (a video in its player),
    /// fetched first if needed, kept under `<data dir>/opened/` until space is freed.
    /// Returns `{"path"}`.
    pub(crate) async fn open_attachment(
        &self,
        message_id: &str,
        hash: &str,
    ) -> Result<Value, CommandError> {
        let (_, attachment) = self.attached(message_id, hash)?;
        if !self.inner.attachments.is_complete(hash) {
            self.fetch_attachment(message_id, hash).await?;
        }
        let folder = self
            .inner
            .data_dir
            .join(OPENED)
            .join(&hash[..hash.len().min(16)]);
        let target = folder.join(&attachment.name);
        if std::fs::metadata(&target).is_ok_and(|m| m.len() == attachment.size) {
            return Ok(json!({ "path": target.display().to_string() }));
        }
        std::fs::create_dir_all(&folder).map_err(anyhow::Error::from)?;
        let blob: Hash = hash
            .parse()
            .map_err(|_| CommandError::Failed(format!("{hash} is not a hash")))?;
        self.inner
            .attachments
            .store()?
            .blobs()
            .export(blob, &target)
            .await
            .map_err(|e| CommandError::Failed(format!("opening {}: {e}", attachment.name)))?;
        Ok(json!({ "path": target.display().to_string() }))
    }

    /// An attached picture that is here, decoded, turned upright and at most `max_side`
    /// pixels on its longest side, for the window to show.
    pub async fn attachment_picture(&self, hash: &str, max_side: u32) -> Option<Picture> {
        if !self.inner.attachments.is_complete(hash) {
            return None;
        }
        let store = self.inner.attachments.store().ok()?;
        let bytes = store
            .blobs()
            .get_bytes(hash.parse::<Hash>().ok()?)
            .await
            .ok()?;
        tokio::task::spawn_blocking(move || {
            let image = decode_upright(&bytes)?;
            let image = if image.width().max(image.height()) > max_side {
                image.thumbnail(max_side, max_side)
            } else {
                image
            };
            Some(Picture::from(image))
        })
        .await
        .ok()
        .flatten()
    }

    /// A device's name as this device shows it.
    fn author_name(&self, endpoint_id: &str) -> String {
        self.names()
            .get(endpoint_id)
            .cloned()
            .unwrap_or_else(|| deskbeam_model::short_id(endpoint_id).to_string())
    }

    /// How much space attached files take on this device, in bytes.
    pub fn attachments_size(&self) -> u64 {
        dir_size(&self.inner.data_dir.join(DIR)) + dir_size(&self.inner.data_dir.join(OPENED))
    }

    /// Removes files received in chats from this device; files it sent stay, since others
    /// may fetch them from it. The messages stay, and a removed file shows Download again.
    /// Returns `{"files", "freed"}` (bytes).
    pub(crate) async fn free_attachment_space(&self) -> Result<Value, CommandError> {
        let me = self.my_id().ok_or(CommandError::NodeNotRunning)?;
        let attachments = &self.inner.attachments;
        let store = attachments.store()?.clone();
        if attachments.freeing.swap(true, Ordering::SeqCst) {
            return Err(CommandError::Failed("space is being freed already".into()));
        }
        let result = async {
            // Copies made for opening files in other apps go first; they come back on demand.
            let _ = std::fs::remove_dir_all(self.inner.data_dir.join(OPENED));
            let received = self.inner.store.received_attachments(&me)?;
            let chosen: Vec<Hash> = received
                .iter()
                .filter(|h| attachments.is_complete(h))
                .filter_map(|h| h.parse().ok())
                .collect();
            if chosen.is_empty() {
                return Ok(json!({ "files": 0, "freed": 0 }));
            }
            let before = self.attachments_size();
            for hash in &chosen {
                store
                    .tags()
                    .delete(tag_name(*hash))
                    .await
                    .map_err(|e| CommandError::Failed(e.to_string()))?;
            }
            // Keep what is being fetched, then ask for a run.
            *attachments.gc_keep.lock().expect("lock") = attachments
                .progress
                .lock()
                .expect("lock")
                .iter()
                .filter(|(_, p)| matches!(p, Progress::Fetching(_)))
                .filter_map(|(h, _)| h.parse().ok())
                .collect();
            attachments.gc_wanted.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + GC_WAIT;
            loop {
                let mut left = 0;
                for hash in &chosen {
                    if matches!(
                        store.blobs().status(*hash).await,
                        Ok(BlobStatus::Complete { .. })
                    ) {
                        left += 1;
                    }
                }
                if left == 0 {
                    break;
                }
                if Instant::now() > deadline {
                    return Err(CommandError::Failed(
                        "the files were not removed in time; try again".into(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            {
                let mut complete = attachments.complete.lock().expect("lock");
                for hash in &chosen {
                    complete.remove(&hash.to_hex());
                }
            }
            let freed = before.saturating_sub(self.attachments_size());
            Ok(json!({ "files": chosen.len(), "freed": freed }))
        }
        .await;
        attachments.freeing.store(false, Ordering::SeqCst);
        attachments.gc_keep.lock().expect("lock").clear();
        self.refresh().await;
        result
    }
}

/// A decoded picture: RGBA, row by row.
#[derive(Debug, Clone)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl From<image::DynamicImage> for Picture {
    fn from(image: image::DynamicImage) -> Self {
        let rgba = image.into_rgba8();
        Self {
            width: rgba.width(),
            height: rgba.height(),
            rgba: rgba.into_raw(),
        }
    }
}

/// An attachment's preview (base64 JPEG), decoded.
pub fn preview_picture(preview: &str) -> Option<Picture> {
    let bytes = BASE64.decode(preview.as_bytes()).ok()?;
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).ok()?;
    Some(Picture::from(image))
}

fn tag_name(hash: Hash) -> String {
    format!("{TAG_PREFIX}{}", hash.to_hex())
}

/// Decodes a picture and applies its EXIF orientation, so phone photos stand upright.
fn decode_upright(bytes: &[u8]) -> Option<image::DynamicImage> {
    use image::ImageDecoder;
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(1024 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok();
    let mut image = image::DynamicImage::from_decoder(decoder).ok()?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    Some(image)
}

/// A picture's size and a small JPEG of it (base64), under `MAX_PREVIEW`.
fn make_preview(path: &Path) -> Option<(ImageSize, String)> {
    let bytes = std::fs::read(path).ok()?;
    let image = decode_upright(&bytes)?;
    let size = ImageSize {
        width: image.width(),
        height: image.height(),
    };
    for (side, quality) in PREVIEW_SIDES {
        let small = image.thumbnail(side, side).into_rgb8();
        let mut jpeg = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality);
        small.write_with_encoder(encoder).ok()?;
        let encoded = BASE64.encode(&jpeg);
        if encoded.len() <= MAX_PREVIEW {
            return Some((size, encoded));
        }
    }
    None
}

/// The media type for a file name, by its extension.
pub(crate) fn mime_for(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "heic" => "image/heic",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "wmv" => "video/x-ms-wmv",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// `folder/name`, or `folder/name (2).ext` and so on if that is taken.
fn unused_path(folder: &Path, name: &str) -> PathBuf {
    let first = folder.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    (2..)
        .map(|n| folder.join(format!("{stem} ({n}){extension}")))
        .find(|p| !p.exists())
        .expect("some name is free")
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_come_from_extensions() {
        assert_eq!(mime_for("Photo.JPG"), "image/jpeg");
        assert_eq!(mime_for("notes.txt"), "text/plain");
        assert_eq!(mime_for("archive"), "application/octet-stream");
    }

    #[test]
    fn saving_twice_picks_a_new_name() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unused_path(dir.path(), "a.txt"), dir.path().join("a.txt"));
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        assert_eq!(
            unused_path(dir.path(), "a.txt"),
            dir.path().join("a (2).txt")
        );
        std::fs::write(dir.path().join("a (2).txt"), "x").unwrap();
        assert_eq!(
            unused_path(dir.path(), "a.txt"),
            dir.path().join("a (3).txt")
        );
        std::fs::write(dir.path().join("README"), "x").unwrap();
        assert_eq!(
            unused_path(dir.path(), "README"),
            dir.path().join("README (2)")
        );
    }

    #[test]
    fn previews_are_small_upright_jpegs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.png");
        let image = image::RgbImage::from_fn(1600, 900, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8])
        });
        image.save(&path).unwrap();
        let (size, preview) = make_preview(&path).unwrap();
        assert_eq!((size.width, size.height), (1600, 900));
        assert!(preview.len() <= MAX_PREVIEW);
        let picture = preview_picture(&preview).unwrap();
        assert_eq!(picture.width, 320);
        assert_eq!(picture.height, 180);
        assert!(make_preview(&dir.path().join("missing.png")).is_none());
    }
}
