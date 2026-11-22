//! Messaging (plan §5): chats and group chats between devices, with no message server.
//!
//! - Sending stores the message and puts it in the outbox for each recipient. The delivery
//!   loop hands it over when the recipient is reachable: paired devices once they are
//!   online.
//! - Only paired devices count as group members (plan §5, changed 2026-11-20). Each device
//!   sees, sends to and syncs with only the members it is paired with itself; a member it is
//!   not paired with is left out, and that member's messages are dropped here.
//! - Whenever two paired members of a group talk, they also swap what each has of the
//!   group's messages (catch-up sync). A message thus reaches a member who was offline even
//!   after its author has gone offline, as long as some member who has it is online.
//! - What is accepted: chat links from paired devices only; direct messages only from their
//!   author with "Chat" allowed; group messages only from paired members, written by this
//!   device or a paired member, in groups this device is in; groups only when signed by a
//!   paired creator, or by the creator of the group already held. Every message's signature
//!   is checked, whoever passes it on.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use deskbeam_chat::{ALPN, ConversationId, Draft, Frame, Group, Message, read_frame, write_frame};
use deskbeam_model::{
    ChatMessage, Conversation, ConversationKind, Delivery, Member, Quote, SearchHit,
};
use deskbeam_store::StoredMessage;
use iroh::{
    EndpointId,
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::{
    Event,
    command::CommandError,
    engine::{Engine, Inner, paired_id, unix_ms},
};

/// How many of a group's newest messages a catch-up sync compares.
const SYNC_WINDOW: usize = 500;
/// How many outbox messages go in one delivery.
const DELIVER_BATCH: usize = 200;
/// How often the delivery loop looks at its peers, unless woken sooner.
const LOOP_EVERY: Duration = Duration::from_secs(3);
/// How often members that have nothing waiting still sync.
const SYNC_EVERY: Duration = Duration::from_secs(60);
/// Backoff after a peer could not be reached.
const RETRY_FIRST: Duration = Duration::from_secs(10);
const RETRY_MOST: Duration = Duration::from_secs(300);
/// How long one session may take.
const SESSION_TIMEOUT: Duration = Duration::from_secs(30);

/// The host side of the chat protocol, on the node's endpoint.
pub(crate) struct ChatServer {
    inner: Weak<Inner>,
}

impl ChatServer {
    pub(crate) fn new(inner: Weak<Inner>) -> Self {
        Self { inner }
    }
}

impl std::fmt::Debug for ChatServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatServer")
    }
}

impl ProtocolHandler for ChatServer {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id().to_string();
        // Group members this device is not paired with are not talked to.
        let paired = self
            .inner
            .upgrade()
            .and_then(|inner| inner.node_now())
            .is_some_and(|node| {
                node.list_paired()
                    .unwrap_or_default()
                    .iter()
                    .any(|d| d.endpoint_id.eq_ignore_ascii_case(&peer))
            });
        if !paired {
            debug!(%peer, "chat link from a device that is not paired refused");
            connection.close(1u32.into(), b"not paired");
            return Ok(());
        }
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let Some(inner) = self.inner.upgrade() else {
                break;
            };
            let engine = Engine { inner };
            let peer = peer.clone();
            tokio::spawn(async move {
                let served = tokio::time::timeout(SESSION_TIMEOUT, async {
                    loop {
                        match read_frame(&mut recv).await? {
                            Frame::Deliver { groups, messages } => {
                                let ids = engine.take_in(&peer, groups, messages).await;
                                write_frame(&mut send, &Frame::Ack { ids }).await?;
                            }
                            Frame::Sync { group, have } => {
                                let reply = engine.sync_reply(&peer, &group, &have);
                                write_frame(&mut send, &reply).await?;
                                // Its Ack; nothing to do with it here.
                                let _ = read_frame(&mut recv).await?;
                            }
                            Frame::Typing { conversation } => {
                                engine.take_typing(&peer, &conversation).await;
                            }
                            Frame::Ack { .. } => {}
                            Frame::Done => break,
                        }
                    }
                    let _ = send.finish();
                    anyhow::Ok(())
                })
                .await;
                match served {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => debug!(%peer, "chat session ended: {e:#}"),
                    Err(_) => debug!(%peer, "chat session timed out"),
                }
            });
        }
        Ok(())
    }
}

/// When to try a peer next, and how long to wait after the next failure.
#[derive(Clone, Copy)]
struct Schedule {
    next: Instant,
    backoff: Duration,
}

impl Engine {
    pub(crate) fn my_id(&self) -> Option<String> {
        self.inner
            .node_now()
            .map(|n| n.device_info().endpoint_id.to_lowercase())
    }

    /// The devices paired with this one, lowercase.
    pub(crate) fn paired_ids(&self) -> HashSet<String> {
        self.inner
            .node_now()
            .map(|node| {
                node.list_paired()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| d.endpoint_id.to_lowercase())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The groups this device holds, checked.
    fn held_groups(&self) -> Vec<Group> {
        match self.inner.store.groups() {
            Ok(list) => list
                .iter()
                .filter_map(|json| serde_json::from_str::<Group>(json).ok())
                .collect(),
            Err(e) => {
                warn!("reading groups: {e}");
                Vec::new()
            }
        }
    }

    pub(crate) fn held_group(&self, id: &str) -> Option<Group> {
        let json = self.inner.store.group(id).ok().flatten()?;
        serde_json::from_str(&json).ok()
    }

    /// The next Lamport time for a message this device writes or receives at `seen`.
    fn tick_lamport(&self, seen: u64) -> u64 {
        let mut clock = self.inner.lamport.lock().expect("lock");
        *clock = (*clock).max(seen) + 1;
        *clock
    }

    /// Keeps what a peer delivered, where allowed. Returns the ids of the messages this
    /// device now holds, so the peer can mark them delivered.
    async fn take_in(&self, peer: &str, groups: Vec<Group>, messages: Vec<Message>) -> Vec<String> {
        let Ok(node) = self.node().await else {
            return Vec::new();
        };
        let Some(me) = self.my_id() else {
            return Vec::new();
        };
        let paired: HashMap<String, bool> = node
            .list_paired()
            .unwrap_or_default()
            .into_iter()
            .map(|d| {
                let chat = self
                    .inner
                    .store
                    .permissions(&d.endpoint_id)
                    .map(|p| p.chat)
                    .unwrap_or(false);
                (d.endpoint_id.to_lowercase(), chat)
            })
            .collect();

        for group in groups {
            if let Err(e) = self.take_group(&me, &paired, group) {
                debug!(%peer, "group not taken: {e:#}");
            }
        }

        let mut held = Vec::new();
        let mut arrived = false;
        for message in messages {
            match self.take_message(&me, peer, &paired, &message) {
                Ok(new) => {
                    arrived |= new;
                    // A message ends its author's "typing".
                    self.inner.typing.lock().expect("lock").remove(&(
                        message.conversation.to_string(),
                        message.author.to_lowercase(),
                    ));
                    // An edit or a deletion is not news: no notification, no sound.
                    if new && message.changes().is_none() {
                        let _ = self.inner.events.send(Event::ChatMessage {
                            conversation: message.conversation.to_string(),
                            id: message.id.clone(),
                            author: message.author.clone(),
                        });
                    }
                    held.push(message.id);
                }
                Err(e) => debug!(%peer, id = %message.id, "message not taken: {e:#}"),
            }
        }
        if arrived {
            self.refresh().await;
        }
        held
    }

    fn take_group(
        &self,
        me: &str,
        paired: &HashMap<String, bool>,
        group: Group,
    ) -> anyhow::Result<()> {
        group.verify()?;
        anyhow::ensure!(group.has_member(me), "this device is not in it");
        let trusted = match self.held_group(&group.id) {
            Some(held) => held.creator == group.creator,
            None => paired.contains_key(&group.creator) || group.creator == me,
        };
        anyhow::ensure!(trusted, "its creator is not known here");
        let json = serde_json::to_string(&group)?;
        if self
            .inner
            .store
            .put_group(&group.id, group.version, &json)?
        {
            debug!(group = %group.id, version = group.version, "group kept");
        }
        Ok(())
    }

    /// Returns whether the message is new here.
    fn take_message(
        &self,
        me: &str,
        peer: &str,
        paired: &HashMap<String, bool>,
        message: &Message,
    ) -> anyhow::Result<bool> {
        message.verify()?;
        let author = message.author.to_lowercase();
        let peer = peer.to_lowercase();
        anyhow::ensure!(paired.contains_key(&peer), "the sender is not paired");
        match &message.conversation {
            ConversationId::Direct { .. } => {
                anyhow::ensure!(
                    author == peer,
                    "direct messages come from their author only"
                );
                anyhow::ensure!(
                    message.conversation == ConversationId::direct(me, &peer),
                    "not a conversation with this device"
                );
                anyhow::ensure!(
                    paired.get(&peer).copied().unwrap_or(false),
                    "chat with this device is not allowed"
                );
            }
            ConversationId::Group(id) => {
                let group = self
                    .held_group(id)
                    .ok_or_else(|| anyhow::anyhow!("an unknown group"))?;
                anyhow::ensure!(group.has_member(me), "this device is not in the group");
                anyhow::ensure!(group.has_member(&author), "the author is not in the group");
                anyhow::ensure!(group.has_member(&peer), "the sender is not in the group");
                anyhow::ensure!(
                    author == me || paired.contains_key(&author),
                    "the author is not paired with this device"
                );
            }
        }
        let stored = StoredMessage {
            id: message.id.clone(),
            conversation: message.conversation.to_string(),
            author,
            lamport: message.lamport,
            json: serde_json::to_string(message)?,
            target: message.changes().map(str::to_string),
        };
        let new = self.inner.store.insert_message(&stored)?;
        if new {
            self.tick_lamport(message.lamport);
            self.note_attachments(message, stored.author == me);
        }
        Ok(new)
    }

    /// The group's messages `peer` lacks, if both are members.
    fn sync_reply(&self, peer: &str, group_id: &str, have: &[String]) -> Frame {
        let empty = Frame::Deliver {
            groups: Vec::new(),
            messages: Vec::new(),
        };
        let (Some(me), Some(group)) = (self.my_id(), self.held_group(group_id)) else {
            return empty;
        };
        let peer = peer.to_lowercase();
        if !group.has_member(&me) || !group.has_member(&peer) || !self.paired_ids().contains(&peer)
        {
            return empty;
        }
        let have: HashSet<&str> = have.iter().map(String::as_str).collect();
        let messages = self
            .inner
            .store
            .messages(&group.conversation().to_string(), SYNC_WINDOW)
            .unwrap_or_default()
            .into_iter()
            .filter(|m| !have.contains(m.id.as_str()))
            .filter_map(|m| serde_json::from_str::<Message>(&m.json).ok())
            .collect();
        Frame::Deliver {
            groups: vec![group],
            messages,
        }
    }

    // --- Commands ---

    pub(crate) async fn chat_send(
        &self,
        conversation: &str,
        body: &str,
        attachments: &[String],
        reply_to: Option<String>,
    ) -> Result<Value, CommandError> {
        let body = body.trim_end();
        if body.trim().is_empty() && attachments.is_empty() {
            return Err(CommandError::InvalidParams(
                "chat.send".into(),
                "the message is empty".into(),
            ));
        }
        if body.len() > deskbeam_chat::MAX_BODY {
            return Err(CommandError::InvalidParams(
                "chat.send".into(),
                "the message is too long".into(),
            ));
        }
        let conversation: ConversationId = conversation
            .parse()
            .map_err(|e: String| CommandError::InvalidParams("chat.send".into(), e))?;
        if let Some(id) = &reply_to {
            let answered = self.inner.store.message(id)?;
            if answered.is_none_or(|m| m.conversation != conversation.to_string()) {
                return Err(CommandError::InvalidParams(
                    "chat.send".into(),
                    "the message replied to is not in this chat".into(),
                ));
            }
        }
        let attachments = self.import_attachments(attachments).await?;
        self.publish(
            "chat.send",
            conversation,
            Draft {
                body: body.to_string(),
                attachments,
                reply_to,
                ..Default::default()
            },
        )
        .await
    }

    /// Tells the others in `conversation` that this device is writing, at most every
    /// [`TYPING_EVERY`], to those online.
    pub(crate) async fn chat_typing(&self, conversation: &str) -> Result<Value, CommandError> {
        let conversation: ConversationId = conversation
            .parse()
            .map_err(|e: String| CommandError::InvalidParams("chat.typing".into(), e))?;
        let key = conversation.to_string();
        {
            let mut sent = self.inner.typing_sent.lock().expect("lock");
            if sent.get(&key).is_some_and(|at| at.elapsed() < TYPING_EVERY) {
                return Ok(Value::Null);
            }
            sent.insert(key, Instant::now());
        }
        let node = self.node().await?;
        let me = node.device_info().endpoint_id.to_lowercase();
        let online: HashSet<String> = node
            .list_paired()?
            .into_iter()
            .filter(|d| d.online)
            .map(|d| d.endpoint_id.to_lowercase())
            .collect();
        let recipients = self.recipients("chat.typing", &node, &conversation, &me)?;
        for peer in recipients.into_iter().filter(|p| online.contains(p)) {
            let engine = self.clone();
            let conversation = conversation.clone();
            tokio::spawn(async move {
                let said =
                    tokio::time::timeout(TYPING_EVERY, engine.say_typing(&peer, conversation));
                if let Ok(Err(e)) = said.await {
                    debug!(%peer, "typing not said: {e:#}");
                }
            });
        }
        Ok(Value::Null)
    }

    async fn say_typing(&self, peer: &str, conversation: ConversationId) -> anyhow::Result<()> {
        let node = self.node().await?;
        let remote: EndpointId = peer.parse()?;
        let endpoint = node.endpoint().await;
        let connection = endpoint.connect(remote, ALPN).await?;
        let (mut send, _recv) = connection.open_bi().await?;
        write_frame(&mut send, &Frame::Typing { conversation }).await?;
        write_frame(&mut send, &Frame::Done).await?;
        send.finish()?;
        // Give the frames time to leave before the connection goes.
        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
        Ok(())
    }

    /// Someone says they are writing: kept if they may write in that conversation.
    async fn take_typing(&self, peer: &str, conversation: &ConversationId) {
        let Some(me) = self.my_id() else { return };
        let peer = peer.to_lowercase();
        let allowed = match conversation {
            ConversationId::Direct { .. } => {
                *conversation == ConversationId::direct(&me, &peer)
                    && self.inner.store.permissions(&peer).is_ok_and(|p| p.chat)
            }
            ConversationId::Group(id) => self
                .held_group(id)
                .is_some_and(|g| g.has_member(&me) && g.has_member(&peer)),
        };
        if !allowed {
            return;
        }
        self.inner
            .typing
            .lock()
            .expect("lock")
            .insert((conversation.to_string(), peer), Instant::now());
        self.refresh().await;
    }

    /// Who is writing in `conversation` now, by name.
    fn typing_in(&self, conversation: &str, names: &HashMap<String, String>) -> Vec<String> {
        let mut typing = self.inner.typing.lock().expect("lock");
        typing.retain(|_, at| at.elapsed() < TYPING_SHOWN);
        let mut who: Vec<String> = typing
            .keys()
            .filter(|(c, _)| c == conversation)
            .map(|(_, author)| {
                names
                    .get(author)
                    .cloned()
                    .unwrap_or_else(|| deskbeam_model::short_id(author).to_string())
            })
            .collect();
        who.sort();
        who
    }

    /// A new text for one of this device's messages.
    pub(crate) async fn chat_edit(&self, id: &str, body: &str) -> Result<Value, CommandError> {
        let body = body.trim_end();
        if body.trim().is_empty() {
            return Err(CommandError::InvalidParams(
                "chat.edit".into(),
                "the new text is empty: delete the message instead".into(),
            ));
        }
        if body.len() > deskbeam_chat::MAX_BODY {
            return Err(CommandError::InvalidParams(
                "chat.edit".into(),
                "the message is too long".into(),
            ));
        }
        let target = self.own_message("chat.edit", id)?;
        self.publish(
            "chat.edit",
            target.conversation,
            Draft {
                body: body.to_string(),
                edits: Some(target.id),
                ..Default::default()
            },
        )
        .await
    }

    /// Deletes one of this device's messages, for everyone in the chat.
    pub(crate) async fn chat_delete(&self, id: &str) -> Result<Value, CommandError> {
        let target = self.own_message("chat.delete", id)?;
        self.publish(
            "chat.delete",
            target.conversation,
            Draft {
                deletes: Some(target.id),
                ..Default::default()
            },
        )
        .await
    }

    /// A message this device wrote and hasn't deleted, to change.
    fn own_message(&self, command: &str, id: &str) -> Result<Message, CommandError> {
        let me = self.my_id().ok_or(CommandError::NodeNotRunning)?;
        let stored = self
            .inner
            .store
            .message(id)?
            .filter(|m| m.target.is_none())
            .ok_or_else(|| CommandError::InvalidParams(command.into(), "no such message".into()))?;
        if stored.author != me {
            return Err(CommandError::InvalidParams(
                command.into(),
                "only its author can change a message".into(),
            ));
        }
        let deleted = self
            .inner
            .store
            .changes_to(std::slice::from_ref(&stored.id))?
            .iter()
            .filter_map(|c| serde_json::from_str::<Message>(&c.json).ok())
            .any(|c| c.deletes.is_some() && c.author.eq_ignore_ascii_case(&me));
        if deleted {
            return Err(CommandError::InvalidParams(
                command.into(),
                "the message is deleted".into(),
            ));
        }
        serde_json::from_str(&stored.json).map_err(|e| CommandError::Failed(e.to_string()))
    }

    /// Signs, keeps and queues a message for everyone in its conversation.
    async fn publish(
        &self,
        command: &str,
        conversation: ConversationId,
        draft: Draft,
    ) -> Result<Value, CommandError> {
        let node = self.node().await?;
        let me = node.device_info().endpoint_id.to_lowercase();
        let recipients = self.recipients(command, &node, &conversation, &me)?;
        let lamport = self.tick_lamport(0);
        let message = Message::write(&node.secret_key(), conversation, lamport, unix_ms(), draft);
        let stored = StoredMessage {
            id: message.id.clone(),
            conversation: message.conversation.to_string(),
            author: me,
            lamport,
            json: serde_json::to_string(&message).map_err(anyhow::Error::from)?,
            target: message.changes().map(str::to_string),
        };
        self.inner.store.insert_message(&stored)?;
        self.note_attachments(&message, true);
        for recipient in &recipients {
            self.inner.store.add_outbox(&message.id, recipient)?;
        }
        // Reading up to one's own message.
        self.inner.store.set_read(&stored.conversation, lamport)?;
        self.inner.chat_wake.notify_one();
        Ok(json!({ "id": message.id, "conversation": stored.conversation }))
    }

    /// Who a message in `conversation` goes to.
    fn recipients(
        &self,
        command: &str,
        node: &native::NodeService,
        conversation: &ConversationId,
        me: &str,
    ) -> Result<Vec<String>, CommandError> {
        Ok(match conversation {
            ConversationId::Direct { .. } => {
                let other = conversation
                    .other_than(me)
                    .ok_or_else(|| {
                        CommandError::InvalidParams(
                            command.into(),
                            "a direct conversation of other devices".into(),
                        )
                    })?
                    .to_string();
                let id = paired_id(node, &other)?;
                if !self.inner.store.permissions(&id)?.chat {
                    return Err(CommandError::Failed(format!(
                        "chat with {} is turned off",
                        deskbeam_model::short_id(&id)
                    )));
                }
                vec![id.to_lowercase()]
            }
            ConversationId::Group(id) => {
                let group = self
                    .held_group(id)
                    .ok_or_else(|| CommandError::Failed(format!("no group {id}")))?;
                if !group.has_member(me) {
                    return Err(CommandError::Failed("this device left the group".into()));
                }
                let paired = self.paired_ids();
                group
                    .members
                    .into_iter()
                    .filter(|m| m != me && paired.contains(m))
                    .collect()
            }
        })
    }

    pub(crate) async fn chat_create_group(
        &self,
        name: &str,
        members: &[String],
    ) -> Result<Value, CommandError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CommandError::InvalidParams(
                "chat.createGroup".into(),
                "the group has no name".into(),
            ));
        }
        let node = self.node().await?;
        let mut named = Vec::new();
        for member in members {
            let id = paired_id(&node, member)?;
            let name = node
                .list_paired()?
                .into_iter()
                .find(|d| d.endpoint_id == id)
                .map(|d| d.display_name)
                .unwrap_or_default();
            named.push((id, name));
        }
        if named.is_empty() {
            return Err(CommandError::InvalidParams(
                "chat.createGroup".into(),
                "a group needs at least one other device".into(),
            ));
        }
        let me = node.device_info();
        named.push((me.endpoint_id.clone(), me.display_name.clone()));
        let group = Group::new(
            &node.secret_key(),
            deskbeam_chat::new_id(),
            name.to_string(),
            named,
            1,
        );
        let json = serde_json::to_string(&group).map_err(anyhow::Error::from)?;
        self.inner
            .store
            .put_group(&group.id, group.version, &json)?;
        self.inner.chat_wake.notify_one();
        Ok(json!({ "conversation": group.conversation().to_string() }))
    }

    pub(crate) fn chat_mark_read(&self, conversation: &str) -> Result<Value, CommandError> {
        let newest = self
            .inner
            .store
            .messages(conversation, 1)?
            .first()
            .map(|m| m.lamport);
        if let Some(lamport) = newest {
            self.inner.store.set_read(conversation, lamport)?;
        }
        Ok(Value::Null)
    }

    /// The conversation id for chatting with a paired device.
    pub fn direct_conversation(&self, endpoint_id: &str) -> Option<String> {
        let me = self.my_id()?;
        Some(ConversationId::direct(&me, endpoint_id).to_string())
    }

    /// A conversation's newest `limit` messages, oldest first, with their edits and
    /// deletions applied.
    pub fn chat_messages(&self, conversation: &str, limit: usize) -> Vec<ChatMessage> {
        match self.inner.store.shown_messages(conversation, limit) {
            Ok(list) => self.render(&list),
            Err(e) => {
                warn!("reading messages: {e}");
                Vec::new()
            }
        }
    }

    /// Messages whose current text contains `text`, newest first.
    pub fn chat_search(
        &self,
        text: &str,
        conversation: Option<&str>,
        limit: usize,
    ) -> Vec<SearchHit> {
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let found = match self.inner.store.search(text, conversation, limit * 2) {
            Ok(found) => found,
            Err(e) => {
                warn!("searching messages: {e}");
                return Vec::new();
            }
        };
        // An edit found stands for the message it changed; each message once.
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        for row in &found {
            let id = row.target.clone().unwrap_or_else(|| row.id.clone());
            if seen.insert(id.clone()) {
                ids.push(id);
            }
        }
        let rows: Vec<StoredMessage> = ids
            .iter()
            .filter_map(|id| self.inner.store.message(id).ok().flatten())
            .filter(|m| m.target.is_none())
            .collect();
        let titles: HashMap<String, String> = self
            .my_id()
            .map(|me| {
                self.conversations(&me, &self.paired_ids())
                    .into_iter()
                    .map(|c| (c.id, c.title))
                    .collect()
            })
            .unwrap_or_default();
        let needle = text.to_lowercase();
        self.render(&rows)
            .into_iter()
            // The text as it reads now: not an edit that was changed again, not deleted.
            .filter(|m| !m.deleted && m.body.to_lowercase().contains(&needle))
            .take(limit)
            .map(|message| SearchHit {
                title: titles
                    .get(&message.conversation)
                    .cloned()
                    .unwrap_or_default(),
                conversation: message.conversation.clone(),
                message,
            })
            .collect()
    }

    /// Messages for the window: each with its newest edit, or as deleted, and with the
    /// message it answers quoted. A change counts only from the message's own author.
    fn render(&self, stored: &[StoredMessage]) -> Vec<ChatMessage> {
        let names = self.names();
        let me = self.my_id().unwrap_or_default();
        let ids: Vec<String> = stored.iter().map(|m| m.id.clone()).collect();
        let changes = self.changes_by_target(&ids);
        stored
            .iter()
            .filter_map(|m| {
                let mut shown = self.chat_message(m, &me, &names)?;
                apply_changes(&mut shown, changes.get(&m.id));
                if let Some(answered) = serde_json::from_str::<Message>(&m.json)
                    .ok()
                    .and_then(|message| message.reply_to)
                {
                    shown.reply = Some(self.quote(&answered, &me, &names));
                }
                Some(shown)
            })
            .collect()
    }

    /// The edits and deletions of these messages, oldest first, by the message they change.
    fn changes_by_target(&self, ids: &[String]) -> HashMap<String, Vec<(String, Message)>> {
        let mut by_target: HashMap<String, Vec<(String, Message)>> = HashMap::new();
        for row in self.inner.store.changes_to(ids).unwrap_or_default() {
            if let (Some(target), Ok(message)) = (
                row.target.clone(),
                serde_json::from_str::<Message>(&row.json),
            ) {
                by_target
                    .entry(target)
                    .or_default()
                    .push((row.conversation, message));
            }
        }
        by_target
    }

    /// How a reply shows the message it answers.
    fn quote(&self, id: &str, me: &str, names: &HashMap<String, String>) -> Quote {
        let Some(stored) = self.inner.store.message(id).ok().flatten() else {
            return Quote {
                id: id.to_string(),
                author_name: String::new(),
                text: String::new(),
                deleted: false,
            };
        };
        let changes = self.changes_by_target(std::slice::from_ref(&stored.id));
        let Some(mut shown) = self.chat_message(&stored, me, names) else {
            return Quote {
                id: id.to_string(),
                author_name: String::new(),
                text: String::new(),
                deleted: false,
            };
        };
        apply_changes(&mut shown, changes.get(&stored.id));
        let text = if !shown.body.trim().is_empty() {
            let first = shown.body.lines().next().unwrap_or_default();
            let mut short: String = first.chars().take(QUOTE_CHARS).collect();
            if short.len() < first.len() || shown.body.lines().count() > 1 {
                short.push('…');
            }
            short
        } else {
            match shown.attachments.len() {
                0 => String::new(),
                1 if shown.attachments[0].mime.starts_with("image/") => "Photo".into(),
                1 => shown.attachments[0].name.clone(),
                n => format!("{n} files"),
            }
        };
        Quote {
            id: shown.id,
            author_name: shown.author_name,
            text,
            deleted: shown.deleted,
        }
    }

    fn chat_message(
        &self,
        stored: &StoredMessage,
        me: &str,
        names: &HashMap<String, String>,
    ) -> Option<ChatMessage> {
        let message: Message = serde_json::from_str(&stored.json).ok()?;
        let mine = stored.author == me;
        let delivery = mine
            .then(|| self.inner.store.delivery(&stored.id).ok())
            .flatten()
            .map(|(delivered, waiting)| Delivery { delivered, waiting });
        Some(ChatMessage {
            id: message.id,
            conversation: stored.conversation.clone(),
            author_name: if mine {
                "You".into()
            } else {
                names
                    .get(&stored.author)
                    .cloned()
                    .unwrap_or_else(|| deskbeam_model::short_id(&stored.author).to_string())
            },
            author: stored.author.clone(),
            mine,
            sent_at: message.sent_at,
            attachments: message
                .attachments
                .iter()
                .map(|a| self.inner.attachments.view(a))
                .collect(),
            body: message.body,
            delivery,
            edited: false,
            deleted: false,
            reply: None,
        })
    }

    /// Names for endpoint ids: paired devices' own, then what groups say.
    pub(crate) fn names(&self) -> HashMap<String, String> {
        let mut names = HashMap::new();
        for group in self.held_groups() {
            for (id, name) in group.names {
                names.insert(id, name);
            }
        }
        if let Some(node) = self.inner.node_now() {
            for d in node.list_paired().unwrap_or_default() {
                let name = self.local_name(&d.endpoint_id, d.display_name);
                names.insert(d.endpoint_id.to_lowercase(), name);
            }
            let me = node.device_info();
            names.insert(me.endpoint_id.to_lowercase(), me.display_name);
        }
        names
    }

    /// The list of chats for the snapshot.
    pub(crate) fn conversations(&self, me: &str, paired: &HashSet<String>) -> Vec<Conversation> {
        let names = self.names();
        let rows = match self.inner.store.conversations(me) {
            Ok(rows) => rows,
            Err(e) => {
                warn!("reading conversations: {e}");
                return Vec::new();
            }
        };
        rows.into_iter()
            .filter_map(|row| {
                let id: ConversationId = row.conversation.parse().ok()?;
                let (title, kind) = match &id {
                    ConversationId::Direct { .. } => {
                        let other = id.other_than(me)?.to_string();
                        let title = names
                            .get(&other)
                            .cloned()
                            .unwrap_or_else(|| deskbeam_model::short_id(&other).to_string());
                        (title, ConversationKind::Direct { endpoint_id: other })
                    }
                    ConversationId::Group(gid) => {
                        let group = self.held_group(gid)?;
                        let members = group
                            .members
                            .iter()
                            .filter(|m| m.as_str() != me && paired.contains(*m))
                            .map(|m| Member {
                                endpoint_id: m.clone(),
                                display_name: names
                                    .get(m)
                                    .cloned()
                                    .unwrap_or_else(|| deskbeam_model::short_id(m).to_string()),
                                paired: paired.contains(m),
                            })
                            .collect();
                        (
                            group.name.clone(),
                            ConversationKind::Group {
                                members,
                                mine: group.creator == me,
                            },
                        )
                    }
                };
                let last = row.last.as_ref().and_then(|json| {
                    let message: Message = serde_json::from_str(json).ok()?;
                    let stored = StoredMessage {
                        id: message.id.clone(),
                        conversation: row.conversation.clone(),
                        author: message.author.to_lowercase(),
                        lamport: message.lamport,
                        json: json.clone(),
                        target: None,
                    };
                    let mut shown = self.chat_message(&stored, me, &names)?;
                    let changes = self.changes_by_target(std::slice::from_ref(&stored.id));
                    apply_changes(&mut shown, changes.get(&stored.id));
                    Some(shown)
                });
                Some(Conversation {
                    typing: self.typing_in(&row.conversation, &names),
                    id: row.conversation,
                    title,
                    kind,
                    last,
                    unread: row.unread,
                })
            })
            .collect()
    }

    // --- Delivery ---

    /// One session with `peer`: deliver what waits for it, then catch up on shared groups.
    async fn session(&self, peer: &str, shared_groups: &[Group]) -> anyhow::Result<()> {
        let node = self.node().await?;
        let remote: EndpointId = peer.parse()?;
        let endpoint = node.endpoint().await;
        let connection = tokio::time::timeout(SESSION_TIMEOUT, endpoint.connect(remote, ALPN))
            .await
            .map_err(|_| anyhow::anyhow!("connecting timed out"))??;
        let (mut send, mut recv) = connection.open_bi().await?;

        let run = async {
            let outbox = self.inner.store.outbox_for(peer)?;
            let messages: Vec<Message> = outbox
                .iter()
                .take(DELIVER_BATCH)
                .filter_map(|m| serde_json::from_str(&m.json).ok())
                .collect();
            if !messages.is_empty() || !shared_groups.is_empty() {
                write_frame(
                    &mut send,
                    &Frame::Deliver {
                        groups: shared_groups.to_vec(),
                        messages,
                    },
                )
                .await?;
                if let Frame::Ack { ids } = read_frame(&mut recv).await? {
                    let now = unix_ms();
                    for id in ids {
                        self.inner.store.mark_delivered(&id, peer, now)?;
                    }
                }
            }
            for group in shared_groups {
                let have = self
                    .inner
                    .store
                    .message_ids(&group.conversation().to_string(), SYNC_WINDOW)?;
                write_frame(
                    &mut send,
                    &Frame::Sync {
                        group: group.id.clone(),
                        have,
                    },
                )
                .await?;
                if let Frame::Deliver { groups, messages } = read_frame(&mut recv).await? {
                    let ids = self.take_in(peer, groups, messages).await;
                    write_frame(&mut send, &Frame::Ack { ids }).await?;
                }
            }
            write_frame(&mut send, &Frame::Done).await?;
            let _ = send.finish();
            anyhow::Ok(())
        };
        let ran = tokio::time::timeout(SESSION_TIMEOUT, run)
            .await
            .map_err(|_| anyhow::anyhow!("the session timed out"))?;
        connection.close(0u32.into(), b"done");
        ran
    }
}

/// Delivers outboxes and syncs groups, until the engine is gone.
pub(crate) async fn delivery_loop(inner: Weak<Inner>) {
    let mut schedule: HashMap<String, Schedule> = HashMap::new();
    let mut was_online: HashSet<String> = HashSet::new();
    loop {
        let wake = match inner.upgrade() {
            Some(inner) => inner.chat_wake.clone(),
            None => return,
        };
        let _ = tokio::time::timeout(LOOP_EVERY, wake.notified()).await;
        let Some(strong) = inner.upgrade() else {
            return;
        };
        let engine = Engine { inner: strong };
        let (Ok(node), Some(me)) = (engine.node().await, engine.my_id()) else {
            continue;
        };

        let paired = node.list_paired().unwrap_or_default();
        let online: HashSet<String> = paired
            .iter()
            .filter(|d| d.online)
            .map(|d| d.endpoint_id.to_lowercase())
            .collect();
        let is_paired: HashSet<String> = paired
            .iter()
            .map(|d| d.endpoint_id.to_lowercase())
            .collect();
        let waiting: HashSet<String> = engine
            .inner
            .store
            .outbox_recipients()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let groups = engine.held_groups();

        // Every paired device this device has something to say to, or a group with.
        let mut peers: HashSet<String> = waiting.intersection(&is_paired).cloned().collect();
        for group in &groups {
            if group.has_member(&me) {
                peers.extend(
                    group
                        .members
                        .iter()
                        .filter(|m| **m != me && is_paired.contains(*m))
                        .cloned(),
                );
            }
        }

        let now = Instant::now();
        for peer in peers {
            // A paired device that just came online, or one with new messages for it, now.
            let entry = schedule.entry(peer.clone()).or_insert(Schedule {
                next: now,
                backoff: RETRY_FIRST,
            });
            let came_online = online.contains(&peer) && !was_online.contains(&peer);
            if came_online || (waiting.contains(&peer) && entry.backoff == RETRY_FIRST) {
                entry.next = entry.next.min(now);
            }
            if entry.next > now {
                continue;
            }
            if !online.contains(&peer) {
                // Its presence says when it is back.
                continue;
            }
            let shared: Vec<Group> = groups
                .iter()
                .filter(|g| g.has_member(&me) && g.has_member(&peer))
                .cloned()
                .collect();
            match engine.session(&peer, &shared).await {
                Ok(())
                    if engine
                        .inner
                        .store
                        .outbox_for(&peer)
                        .is_ok_and(|o| !o.is_empty()) =>
                {
                    // Reached, but it did not take everything (it refused some, or there
                    // was more than one batch): try again, more slowly each time.
                    entry.next = now + entry.backoff;
                    entry.backoff = (entry.backoff * 2).min(RETRY_MOST);
                    engine.refresh().await;
                }
                Ok(()) => {
                    entry.backoff = RETRY_FIRST;
                    entry.next = now + SYNC_EVERY;
                    engine.refresh().await;
                }
                Err(e) => {
                    debug!(%peer, "chat delivery failed: {e:#}");
                    entry.next = now + entry.backoff;
                    entry.backoff = (entry.backoff * 2).min(RETRY_MOST);
                }
            }
        }
        was_online = online;
    }
}

/// A shared wake-up for the delivery loop.
pub(crate) type Wake = Arc<tokio::sync::Notify>;

/// "Typing" is said at most this often, and shown this long after it was last heard.
const TYPING_EVERY: Duration = Duration::from_secs(3);
const TYPING_SHOWN: Duration = Duration::from_secs(5);

/// How long a quoted message's text may be.
const QUOTE_CHARS: usize = 120;

/// Applies a message's edits and deletion, from its own author in its own conversation
/// only: the newest edit's text, or nothing once deleted.
fn apply_changes(shown: &mut ChatMessage, changes: Option<&Vec<(String, Message)>>) {
    let Some(changes) = changes else { return };
    let own = changes.iter().filter(|(conversation, change)| {
        *conversation == shown.conversation && change.author.eq_ignore_ascii_case(&shown.author)
    });
    let mut newest_edit: Option<&Message> = None;
    for (_, change) in own {
        if change.deletes.is_some() {
            shown.deleted = true;
        } else if change.edits.is_some()
            && newest_edit.is_none_or(|e| change.order_key() > e.order_key())
        {
            newest_edit = Some(change);
        }
    }
    if shown.deleted {
        shown.body.clear();
        shown.attachments.clear();
        shown.edited = false;
    } else if let Some(edit) = newest_edit {
        shown.body = edit.body.clone();
        shown.edited = true;
    }
}
