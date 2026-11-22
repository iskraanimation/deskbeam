//! DeskBeam's messaging wire format (plan §5).
//!
//! There is no message server. A message is held by the devices that have it and moves
//! when one of them and a device it is for are online together:
//!
//! - Every message is signed by its author's device key, so any group member can pass it
//!   on and nobody can change it on the way.
//! - A group is a member list signed by its creator. Only members paired with each other
//!   talk (the engine's rule since 2026-11-20).
//! - A message may carry attachments: each names a file by its BLAKE3 hash, which members
//!   fetch over iroh-blobs, and may hold a small preview. Messages with attachments are
//!   signed in a second format ([`Message::signed_bytes`]); text-only messages are signed as
//!   they always were, so older builds still read them.
//! - Order comes from a Lamport clock, then the author, then the id, so every device sorts a
//!   conversation the same way.
//! - Messages are never changed once sent. An edit or a deletion is a message of its own that
//!   names the message it changes ([`Message::edits`], [`Message::deletes`]); every device
//!   applies it only when its author wrote that message too. A reply names the message it
//!   answers ([`Message::reply_to`]). These are signed in a third format, so builds from
//!   before them drop them while still reading everything else.
//!
//! On [`ALPN`] a session is one bi-stream of length-prefixed JSON [`Frame`]s.

use std::{collections::BTreeMap, fmt};

use data_encoding::HEXLOWER;
use iroh::{PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"deskbeam/chat/1";

/// Frames are small; anything this large is not one.
const MAX_FRAME: usize = 4 * 1024 * 1024;
/// Message bodies longer than this are refused.
pub const MAX_BODY: usize = 64 * 1024;
/// At most this many attachments per message.
pub const MAX_ATTACHMENTS: usize = 10;
/// An attachment's preview, as base64, is at most this long.
pub const MAX_PREVIEW: usize = 48 * 1024;
/// File names and media types longer than these are refused.
const MAX_NAME: usize = 255;
const MAX_MIME: usize = 127;

/// A file sent with a message. The file itself travels over iroh-blobs, fetched by its hash
/// from any member who has it; the message carries only this description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// The file's BLAKE3 hash (hex), as iroh-blobs names it.
    pub hash: String,
    /// The file name, without any folder.
    pub name: String,
    pub size: u64,
    /// Its media type, such as `image/jpeg`; `application/octet-stream` when unknown.
    pub mime: String,
    /// For a picture: its size in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSize>,
    /// For a picture: a small JPEG of it, base64, to show before the file is fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

impl Attachment {
    fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.hash.len() == 64 && self.hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "an attachment's hash is not a BLAKE3 hash"
        );
        anyhow::ensure!(
            !self.name.is_empty()
                && self.name.len() <= MAX_NAME
                && !self.name.contains(['/', '\\', '\0']),
            "an attachment's name is not a file name"
        );
        anyhow::ensure!(
            self.mime.len() <= MAX_MIME,
            "an attachment's type is too long"
        );
        if let Some(preview) = &self.preview {
            anyhow::ensure!(
                preview.len() <= MAX_PREVIEW,
                "an attachment's preview is too big"
            );
        }
        Ok(())
    }
}

/// Which conversation a message belongs to.
///
/// On the wire and in storage it is a string: `dm:<a>:<b>` for two devices (the two ids in
/// sorted order, so both ends name it the same) or `group:<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum ConversationId {
    Direct { a: String, b: String },
    Group(String),
}

impl ConversationId {
    pub fn direct(one: &str, other: &str) -> Self {
        let (one, other) = (one.to_lowercase(), other.to_lowercase());
        if one <= other {
            Self::Direct { a: one, b: other }
        } else {
            Self::Direct { a: other, b: one }
        }
    }

    /// For a direct conversation, the device that is not `me`.
    pub fn other_than(&self, me: &str) -> Option<&str> {
        match self {
            Self::Direct { a, b } if a.eq_ignore_ascii_case(me) => Some(b),
            Self::Direct { a, b } if b.eq_ignore_ascii_case(me) => Some(a),
            _ => None,
        }
    }

    pub fn group_id(&self) -> Option<&str> {
        match self {
            Self::Group(id) => Some(id),
            Self::Direct { .. } => None,
        }
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct { a, b } => write!(f, "dm:{a}:{b}"),
            Self::Group(id) => write!(f, "group:{id}"),
        }
    }
}

impl From<ConversationId> for String {
    fn from(c: ConversationId) -> Self {
        c.to_string()
    }
}

impl TryFrom<String> for ConversationId {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl std::str::FromStr for ConversationId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        if let Some(rest) = s.strip_prefix("dm:") {
            let (a, b) = rest
                .split_once(':')
                .ok_or_else(|| format!("not a conversation: {s}"))?;
            return Ok(Self::direct(a, b));
        }
        if let Some(id) = s.strip_prefix("group:")
            && !id.is_empty()
        {
            return Ok(Self::Group(id.to_string()));
        }
        Err(format!("not a conversation: {s}"))
    }
}

/// A new random id, for messages and groups.
pub fn new_id() -> String {
    let bytes: [u8; 16] = rand::random();
    HEXLOWER.encode(&bytes)
}

/// A message as it travels and as it is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub conversation: ConversationId,
    /// The author's endpoint id (hex), whose key signed this.
    pub author: String,
    /// Unix milliseconds, on the author's clock. For showing, not ordering.
    pub sent_at: u64,
    /// The author's Lamport clock when sending.
    pub lamport: u64,
    pub body: String,
    /// Files sent with it. Empty for a text message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// The message this one answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// This is a new text for that message (by the same author), not a message to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<String>,
    /// This deletes that message (by the same author); its body is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletes: Option<String>,
    /// Hex Ed25519 signature over [`Message::signed_bytes`].
    pub signature: String,
}

/// What a new message says, besides where and when.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub body: String,
    pub attachments: Vec<Attachment>,
    pub reply_to: Option<String>,
    pub edits: Option<String>,
    pub deletes: Option<String>,
}

/// The fields a message signature covers, in a fixed order.
#[derive(Serialize)]
struct SignedMessage<'a> {
    kind: &'static str,
    id: &'a str,
    conversation: String,
    author: &'a str,
    sent_at: u64,
    lamport: u64,
    body: &'a str,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    attachments: &'a [Attachment],
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    edits: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deletes: Option<&'a str>,
}

impl Message {
    /// Writes and signs a message with this device's key.
    pub fn new(
        key: &SecretKey,
        conversation: ConversationId,
        lamport: u64,
        sent_at: u64,
        body: String,
    ) -> Self {
        Self::with_attachments(key, conversation, lamport, sent_at, body, Vec::new())
    }

    /// Writes and signs a message with files.
    pub fn with_attachments(
        key: &SecretKey,
        conversation: ConversationId,
        lamport: u64,
        sent_at: u64,
        body: String,
        attachments: Vec<Attachment>,
    ) -> Self {
        Self::write(
            key,
            conversation,
            lamport,
            sent_at,
            Draft {
                body,
                attachments,
                ..Default::default()
            },
        )
    }

    /// Writes and signs any message: text, files, a reply, an edit or a deletion.
    pub fn write(
        key: &SecretKey,
        conversation: ConversationId,
        lamport: u64,
        sent_at: u64,
        draft: Draft,
    ) -> Self {
        let mut message = Self {
            id: new_id(),
            conversation,
            author: key.public().to_string(),
            sent_at,
            lamport,
            body: draft.body,
            attachments: draft.attachments,
            reply_to: draft.reply_to,
            edits: draft.edits,
            deletes: draft.deletes,
            signature: String::new(),
        };
        message.signature = HEXLOWER.encode(&key.sign(&message.signed_bytes()).to_bytes());
        message
    }

    /// What the signature covers. A text message keeps the first format, byte for byte, so
    /// builds from before attachments still verify it; one with attachments adds them and
    /// says so in its kind, so it can't be passed off as the text alone.
    fn signed_bytes(&self) -> Vec<u8> {
        let kind = if self.reply_to.is_some() || self.edits.is_some() || self.deletes.is_some() {
            "deskbeam-chat-message-v3"
        } else if self.attachments.is_empty() {
            "deskbeam-chat-message-v1"
        } else {
            "deskbeam-chat-message-v2"
        };
        serde_json::to_vec(&SignedMessage {
            kind,
            id: &self.id,
            conversation: self.conversation.to_string(),
            author: &self.author,
            sent_at: self.sent_at,
            lamport: self.lamport,
            body: &self.body,
            attachments: &self.attachments,
            reply_to: self.reply_to.as_deref(),
            edits: self.edits.as_deref(),
            deletes: self.deletes.as_deref(),
        })
        .expect("a message serializes")
    }

    /// Checks the signature against the author's key, the body's size and the attachments.
    pub fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.body.len() <= MAX_BODY, "the message is too long");
        anyhow::ensure!(!self.id.is_empty(), "the message has no id");
        anyhow::ensure!(
            self.attachments.len() <= MAX_ATTACHMENTS,
            "too many attachments"
        );
        for attachment in &self.attachments {
            attachment.check()?;
        }
        let names = [&self.reply_to, &self.edits, &self.deletes];
        for id in names.into_iter().flatten() {
            anyhow::ensure!(
                !id.is_empty() && id.len() <= 64 && *id != self.id,
                "a message refers to a bad id"
            );
        }
        if self.edits.is_some() || self.deletes.is_some() {
            anyhow::ensure!(
                self.edits.is_none() || self.deletes.is_none(),
                "a message can't both edit and delete"
            );
            anyhow::ensure!(
                self.attachments.is_empty() && self.reply_to.is_none(),
                "an edit or deletion carries only text"
            );
        }
        if self.deletes.is_some() {
            anyhow::ensure!(self.body.is_empty(), "a deletion has no text");
        }
        verify(&self.author, &self.signed_bytes(), &self.signature)
    }

    /// The message this one edits or deletes, if it does.
    pub fn changes(&self) -> Option<&str> {
        self.edits.as_deref().or(self.deletes.as_deref())
    }

    /// The order every device shows a conversation in.
    pub fn order_key(&self) -> (u64, &str, &str) {
        (self.lamport, &self.author, &self.id)
    }
}

/// A group: who is in it, signed by whoever made it. A newer version replaces an older one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub name: String,
    /// The creator's endpoint id. Only the creator changes the group.
    pub creator: String,
    /// Every member's endpoint id, the creator's included, sorted.
    pub members: Vec<String>,
    /// The members' names as the creator knows them, so members who are not paired with
    /// each other still see names.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    pub version: u64,
    pub signature: String,
}

#[derive(Serialize)]
struct SignedGroup<'a> {
    kind: &'static str,
    id: &'a str,
    name: &'a str,
    creator: &'a str,
    members: &'a [String],
    names: &'a BTreeMap<String, String>,
    version: u64,
}

impl Group {
    /// Makes (or, with a higher `version`, changes) a group and signs it.
    /// `members` are (endpoint id, name) pairs; the creator is added if missing.
    pub fn new(
        key: &SecretKey,
        id: String,
        name: String,
        members: impl IntoIterator<Item = (String, String)>,
        version: u64,
    ) -> Self {
        let creator = key.public().to_string();
        let mut names = BTreeMap::new();
        for (id, name) in members {
            names.insert(id.to_lowercase(), name);
        }
        names.entry(creator.clone()).or_default();
        let members: Vec<String> = names.keys().cloned().collect();
        names.retain(|_, name| !name.is_empty());
        let mut group = Self {
            id,
            name,
            creator,
            members,
            names,
            version,
            signature: String::new(),
        };
        group.signature = HEXLOWER.encode(&key.sign(&group.signed_bytes()).to_bytes());
        group
    }

    fn signed_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&SignedGroup {
            kind: "deskbeam-chat-group-v1",
            id: &self.id,
            name: &self.name,
            creator: &self.creator,
            members: &self.members,
            names: &self.names,
            version: self.version,
        })
        .expect("a group serializes")
    }

    pub fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.id.is_empty(), "the group has no id");
        anyhow::ensure!(
            self.members.iter().any(|m| m == &self.creator),
            "the creator is not a member"
        );
        verify(&self.creator, &self.signed_bytes(), &self.signature)
    }

    pub fn has_member(&self, endpoint_id: &str) -> bool {
        self.members
            .iter()
            .any(|m| m.eq_ignore_ascii_case(endpoint_id))
    }

    pub fn conversation(&self) -> ConversationId {
        ConversationId::Group(self.id.clone())
    }
}

fn verify(author_hex: &str, bytes: &[u8], signature_hex: &str) -> anyhow::Result<()> {
    let key_bytes: [u8; 32] = HEXLOWER
        .decode(author_hex.as_bytes())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("not a device id"))?;
    let key = PublicKey::from_bytes(&key_bytes)?;
    let sig_bytes: [u8; Signature::LENGTH] = HEXLOWER
        .decode(signature_hex.as_bytes())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("not a signature"))?;
    key.verify(bytes, &Signature::from_bytes(&sig_bytes))
        .map_err(|_| anyhow::anyhow!("the signature does not match"))
}

/// What one side of a chat session sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Frame {
    /// Messages and group lists for the other side. Answered with [`Frame::Ack`].
    #[serde(rename_all = "camelCase")]
    Deliver {
        groups: Vec<Group>,
        messages: Vec<Message>,
    },
    /// The ids of the delivered messages the receiver now holds.
    #[serde(rename_all = "camelCase")]
    Ack { ids: Vec<String> },
    /// "These are the group's messages I have; send me the rest." Answered with
    /// [`Frame::Deliver`].
    #[serde(rename_all = "camelCase")]
    Sync { group: String, have: Vec<String> },
    /// The sender is writing in this conversation now. Not stored, not answered; shown for
    /// a few seconds. Builds from before it end the session on it, which is harmless.
    #[serde(rename_all = "camelCase")]
    Typing { conversation: ConversationId },
    /// The session is over.
    Done,
}

pub async fn write_frame(w: &mut (impl AsyncWrite + Unpin), frame: &Frame) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    anyhow::ensure!(bytes.len() <= MAX_FRAME, "frame too large");
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Frame> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    anyhow::ensure!(len <= MAX_FRAME, "frame too large");
    let mut bytes = vec![0u8; len];
    r.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SecretKey {
        SecretKey::generate()
    }

    #[test]
    fn direct_conversations_are_named_the_same_from_both_ends() {
        let (a, b) = ("bb", "AA");
        assert_eq!(ConversationId::direct(a, b), ConversationId::direct(b, a));
        let id = ConversationId::direct(a, b);
        assert_eq!(id.to_string(), "dm:aa:bb");
        assert_eq!(id.other_than("aa"), Some("bb"));
        assert_eq!("dm:bb:aa".parse::<ConversationId>().unwrap(), id);
        assert_eq!(
            "group:x".parse::<ConversationId>().unwrap(),
            ConversationId::Group("x".into())
        );
        assert!("nope".parse::<ConversationId>().is_err());
    }

    #[test]
    fn edits_deletions_and_replies_are_signed_in_a_third_format() {
        let k = key();
        let conv = ConversationId::direct(&k.public().to_string(), "ff");
        let first = Message::new(&k, conv.clone(), 1, 1_000, "hi".into());
        // A plain text message is still signed as before (older builds read it).
        assert!(
            String::from_utf8(first.signed_bytes())
                .unwrap()
                .contains("message-v1")
        );

        let reply = Message::write(
            &k,
            conv.clone(),
            2,
            2_000,
            Draft {
                body: "yes".into(),
                reply_to: Some(first.id.clone()),
                ..Default::default()
            },
        );
        reply.verify().unwrap();
        assert!(
            String::from_utf8(reply.signed_bytes())
                .unwrap()
                .contains("message-v3")
        );
        let mut moved = reply.clone();
        moved.reply_to = Some("other".into());
        assert!(moved.verify().is_err(), "what it answers is signed");

        let edit = Message::write(
            &k,
            conv.clone(),
            3,
            3_000,
            Draft {
                body: "hello".into(),
                edits: Some(first.id.clone()),
                ..Default::default()
            },
        );
        edit.verify().unwrap();
        assert_eq!(edit.changes(), Some(first.id.as_str()));
        let gone = Message::write(
            &k,
            conv.clone(),
            4,
            4_000,
            Draft {
                deletes: Some(first.id.clone()),
                ..Default::default()
            },
        );
        gone.verify().unwrap();

        // A deletion with text, or one that also edits, is refused.
        let wordy = Message::write(
            &k,
            conv.clone(),
            5,
            5_000,
            Draft {
                body: "x".into(),
                deletes: Some(first.id.clone()),
                ..Default::default()
            },
        );
        assert!(wordy.verify().is_err());
        let both = Message::write(
            &k,
            conv,
            6,
            6_000,
            Draft {
                edits: Some(first.id.clone()),
                deletes: Some(first.id.clone()),
                ..Default::default()
            },
        );
        assert!(both.verify().is_err());
    }

    #[test]
    fn a_signed_message_verifies_and_a_changed_one_does_not() {
        let k = key();
        let conv = ConversationId::direct(&k.public().to_string(), "ff");
        let message = Message::new(&k, conv, 3, 1_000, "hi".into());
        message.verify().unwrap();

        let mut changed = message.clone();
        changed.body = "bye".into();
        assert!(changed.verify().is_err());

        let mut stolen = message.clone();
        stolen.author = key().public().to_string();
        assert!(stolen.verify().is_err());

        let json = serde_json::to_string(&message).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        back.verify().unwrap();
    }

    fn photo() -> Attachment {
        Attachment {
            hash: "ab".repeat(32),
            name: "photo.jpg".into(),
            size: 2_000_000,
            mime: "image/jpeg".into(),
            image: Some(ImageSize {
                width: 4000,
                height: 3000,
            }),
            preview: Some("aGk=".into()),
        }
    }

    #[test]
    fn text_messages_are_signed_as_before() {
        // A text message's JSON has no attachments field, and its signed bytes are the first
        // format's: what builds from before attachments wrote and check.
        let k = key();
        let conv = ConversationId::direct(&k.public().to_string(), "ff");
        let message = Message::new(&k, conv, 1, 5, "hi".into());
        let json = serde_json::to_string(&message).unwrap();
        assert!(!json.contains("attachments"));
        let signed = String::from_utf8(message.signed_bytes()).unwrap();
        assert!(signed.contains("deskbeam-chat-message-v1"));
        assert!(!signed.contains("attachments"));
        // A message stored before attachments existed still reads and verifies.
        let old: Message = serde_json::from_str(&json).unwrap();
        old.verify().unwrap();
    }

    #[test]
    fn attachments_are_signed_and_checked() {
        let k = key();
        let conv = ConversationId::direct(&k.public().to_string(), "ff");
        let message = Message::with_attachments(&k, conv, 2, 5, String::new(), vec![photo()]);
        message.verify().unwrap();
        let back: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        back.verify().unwrap();
        assert_eq!(back.attachments, [photo()]);

        // Changing, adding or dropping an attachment breaks the signature.
        let mut renamed = message.clone();
        renamed.attachments[0].name = "other.jpg".into();
        assert!(renamed.verify().is_err());
        let mut stripped = message.clone();
        stripped.attachments.clear();
        assert!(
            stripped.verify().is_err(),
            "can't be passed off as text alone"
        );

        // Bad descriptions are refused even when signed.
        for bad in [
            Attachment {
                hash: "zz".into(),
                ..photo()
            },
            Attachment {
                name: "../etc/passwd".into(),
                ..photo()
            },
            Attachment {
                name: String::new(),
                ..photo()
            },
            Attachment {
                preview: Some("x".repeat(MAX_PREVIEW + 1)),
                ..photo()
            },
        ] {
            let conv = ConversationId::direct(&k.public().to_string(), "ff");
            let m = Message::with_attachments(&k, conv, 3, 5, String::new(), vec![bad]);
            assert!(m.verify().is_err());
        }
        let conv = ConversationId::direct(&k.public().to_string(), "ff");
        let many = vec![photo(); MAX_ATTACHMENTS + 1];
        assert!(
            Message::with_attachments(&k, conv, 4, 5, String::new(), many)
                .verify()
                .is_err()
        );
    }

    #[test]
    fn groups_are_signed_by_their_creator() {
        let creator = key();
        let other = key().public().to_string();
        let group = Group::new(
            &creator,
            new_id(),
            "Team".into(),
            [(other.clone(), "Other".to_string())],
            1,
        );
        group.verify().unwrap();
        assert!(group.has_member(&other));
        assert!(group.has_member(&creator.public().to_string()));
        assert_eq!(group.names.get(&other).map(String::as_str), Some("Other"));

        let mut renamed = group.clone();
        renamed.names.insert(other.clone(), "Mallory".into());
        assert!(renamed.verify().is_err(), "names are signed too");

        let mut grown = group.clone();
        grown.members.push(key().public().to_string());
        assert!(grown.verify().is_err(), "a member list nobody signed");
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let k = key();
        let frame = Frame::Deliver {
            groups: vec![],
            messages: vec![Message::new(
                &k,
                ConversationId::Group("g".into()),
                1,
                2,
                "x".into(),
            )],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).await.unwrap();
        write_frame(&mut buf, &Frame::Done).await.unwrap();
        let mut r = buf.as_slice();
        assert_eq!(read_frame(&mut r).await.unwrap(), frame);
        assert_eq!(read_frame(&mut r).await.unwrap(), Frame::Done);
    }
}
