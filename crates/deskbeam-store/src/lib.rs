//! DeskBeam's persistent data (plan §3.1, principle 4).
//!
//! Only the engine writes here. Clients read through the engine and never open this
//! database themselves.
//!
//! It holds per-device permissions, settings, and the messaging data (plan §5): messages,
//! the outbox of messages still to deliver, delivery receipts, groups and how far each
//! conversation has been read. Messages and groups are stored as the JSON the engine hands
//! in, next to the columns that are queried. The paired-device records themselves live in
//! DashBeam's `paired-devices.json`, owned by the forked `deskbeam-net` crate.

use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
};

use deskbeam_model::Permissions;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored value is not valid: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("the store's lock was poisoned by a panic")]
    Poisoned,
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A message as the store keeps it: the queried columns and the engine's JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMessage {
    pub id: String,
    pub conversation: String,
    pub author: String,
    pub lamport: u64,
    pub json: String,
    /// For an edit or a deletion: the message it changes. Such rows are not shown as
    /// messages, but travel and sync like them.
    pub target: Option<String>,
}

/// One conversation's summary, for the list of chats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRow {
    pub conversation: String,
    /// The newest message's JSON, if there is one.
    pub last: Option<String>,
    /// Messages from others after the read marker.
    pub unread: usize,
}

/// Everything the engine persists.
pub trait Store: Send + Sync {
    /// The device's permissions, or the defaults when nothing was stored for it.
    fn permissions(&self, endpoint_id: &str) -> Result<Permissions>;
    fn set_permissions(&self, endpoint_id: &str, permissions: &Permissions) -> Result<()>;
    /// Removes the permissions and the name stored for a device, after it is unpaired, and the
    /// messages still waiting for it: an unpaired device is no longer sent to.
    fn forget_device(&self, endpoint_id: &str) -> Result<()>;
    /// The name this device gave a paired device, if it gave one.
    fn device_name(&self, endpoint_id: &str) -> Result<Option<String>>;
    /// Names a paired device on this device only; `None` goes back to the device's own name.
    fn set_device_name(&self, endpoint_id: &str, name: Option<&str>) -> Result<()>;

    fn setting(&self, key: &str) -> Result<Option<String>>;
    fn set_setting(&self, key: &str, value: &str) -> Result<()>;

    /// Keeps a message. Returns false when it was already there (same id).
    fn insert_message(&self, message: &StoredMessage) -> Result<bool>;
    fn message(&self, id: &str) -> Result<Option<StoredMessage>>;
    /// The newest `limit` messages of a conversation, oldest first, edits and deletions
    /// included (what syncing passes on).
    fn messages(&self, conversation: &str, limit: usize) -> Result<Vec<StoredMessage>>;
    /// The newest `limit` messages to show, oldest first: without edits and deletions.
    fn shown_messages(&self, conversation: &str, limit: usize) -> Result<Vec<StoredMessage>>;
    /// The edits and deletions of these messages, oldest first.
    fn changes_to(&self, ids: &[String]) -> Result<Vec<StoredMessage>>;
    /// Messages and edits whose text contains `text` (any case), newest first; in one
    /// conversation, or in all.
    fn search(
        &self,
        text: &str,
        conversation: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>>;
    /// The ids of a conversation's newest `limit` messages.
    fn message_ids(&self, conversation: &str, limit: usize) -> Result<Vec<String>>;
    /// The highest Lamport time of any stored message, for the clock to start from.
    fn max_lamport(&self) -> Result<u64>;

    /// Marks a message as still to deliver to `recipient`.
    fn add_outbox(&self, message_id: &str, recipient: &str) -> Result<()>;
    /// The messages still to deliver to `recipient`, oldest first.
    fn outbox_for(&self, recipient: &str) -> Result<Vec<StoredMessage>>;
    /// Every device something is still waiting for.
    fn outbox_recipients(&self) -> Result<Vec<String>>;
    /// `recipient` has the message: off the outbox, and counted as delivered.
    fn mark_delivered(&self, message_id: &str, recipient: &str, at_ms: u64) -> Result<()>;
    /// (delivered, still waiting) for one message.
    fn delivery(&self, message_id: &str) -> Result<(usize, usize)>;

    /// Keeps a group's JSON if this version is newer than the one held. Returns whether it
    /// was kept.
    fn put_group(&self, id: &str, version: u64, json: &str) -> Result<bool>;
    fn group(&self, id: &str) -> Result<Option<String>>;
    fn groups(&self) -> Result<Vec<String>>;

    /// Every conversation with messages or a group, newest activity first. `me` decides
    /// which messages count as unread.
    fn conversations(&self, me: &str) -> Result<Vec<ConversationRow>>;
    /// Marks a conversation read up to and including Lamport time `lamport`.
    fn set_read(&self, conversation: &str, lamport: u64) -> Result<()>;

    /// Notes that message `message_id`, in `conversation`, has the file `hash` attached.
    fn add_attachment(&self, hash: &str, message_id: &str, conversation: &str) -> Result<()>;
    /// The conversations with a message that has `hash` attached, to decide who may fetch it.
    fn attachment_conversations(&self, hash: &str) -> Result<Vec<String>>;
    /// Files attached only to messages others wrote (none by `me`): what "Free up space" may
    /// remove, since others still have them.
    fn received_attachments(&self, me: &str) -> Result<Vec<String>>;
}

/// Schema migrations, applied in order. `PRAGMA user_version` counts how many have run.
const MIGRATIONS: &[&str] = &[
    // 1: permissions and settings
    "CREATE TABLE device_permissions (
        endpoint_id TEXT PRIMARY KEY NOT NULL,
        permissions TEXT NOT NULL
     );
     CREATE TABLE settings (
        key TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL
     );",
    // 2: messaging
    "CREATE TABLE chat_messages (
        id TEXT PRIMARY KEY NOT NULL,
        conversation TEXT NOT NULL,
        author TEXT NOT NULL,
        lamport INTEGER NOT NULL,
        json TEXT NOT NULL
     );
     CREATE INDEX chat_messages_by_conversation ON chat_messages (conversation, lamport, author, id);
     CREATE TABLE chat_outbox (
        message_id TEXT NOT NULL REFERENCES chat_messages (id) ON DELETE CASCADE,
        recipient TEXT NOT NULL,
        PRIMARY KEY (message_id, recipient)
     );
     CREATE INDEX chat_outbox_by_recipient ON chat_outbox (recipient);
     CREATE TABLE chat_delivered (
        message_id TEXT NOT NULL REFERENCES chat_messages (id) ON DELETE CASCADE,
        recipient TEXT NOT NULL,
        at INTEGER NOT NULL,
        PRIMARY KEY (message_id, recipient)
     );
     CREATE TABLE chat_groups (
        id TEXT PRIMARY KEY NOT NULL,
        version INTEGER NOT NULL,
        json TEXT NOT NULL
     );
     CREATE TABLE chat_read (
        conversation TEXT PRIMARY KEY NOT NULL,
        lamport INTEGER NOT NULL
     );",
    // 3: names given to paired devices on this device
    "CREATE TABLE device_names (
        endpoint_id TEXT PRIMARY KEY NOT NULL,
        name TEXT NOT NULL
     );",
    // 4: files attached to messages (the files are in the attachments blob store)
    "CREATE TABLE chat_attachments (
        hash TEXT NOT NULL,
        message_id TEXT NOT NULL REFERENCES chat_messages (id) ON DELETE CASCADE,
        conversation TEXT NOT NULL,
        PRIMARY KEY (hash, message_id)
     );
     CREATE INDEX chat_attachments_by_message ON chat_attachments (message_id);",
    // 5: edits and deletions name the message they change
    "ALTER TABLE chat_messages ADD COLUMN target TEXT;
     CREATE INDEX chat_messages_by_target ON chat_messages (target);",
];

/// The SQLite store, one file in the data directory.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    pub const FILE_NAME: &'static str = "deskbeam.sqlite";

    /// Opens (or creates) `deskbeam.sqlite` in `data_dir` and brings its schema up to date.
    pub fn open_in(data_dir: &Path) -> Result<Self> {
        Self::open(&data_dir.join(Self::FILE_NAME))
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    /// A store that lives only as long as it does, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| StoreError::Poisoned)
    }

    /// How many migrations this database has had.
    pub fn schema_version(&self) -> Result<u32> {
        Ok(self
            .conn()?
            .pragma_query_value(None, "user_version", |r| r.get(0))?)
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let done: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(done as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as u32 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

fn message_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        id: r.get(0)?,
        conversation: r.get(1)?,
        author: r.get(2)?,
        lamport: r.get::<_, i64>(3)? as u64,
        json: r.get(4)?,
        target: r.get(5)?,
    })
}

const MESSAGE_COLUMNS: &str = "id, conversation, author, lamport, json, target";

impl Store for SqliteStore {
    fn permissions(&self, endpoint_id: &str) -> Result<Permissions> {
        let raw: Option<String> = self
            .conn()?
            .query_row(
                "SELECT permissions FROM device_permissions WHERE endpoint_id = ?1",
                [endpoint_id],
                |r| r.get(0),
            )
            .optional()?;
        match raw {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(Permissions::default()),
        }
    }

    fn set_permissions(&self, endpoint_id: &str, permissions: &Permissions) -> Result<()> {
        let json = serde_json::to_string(permissions)?;
        self.conn()?.execute(
            "INSERT INTO device_permissions (endpoint_id, permissions) VALUES (?1, ?2)
             ON CONFLICT(endpoint_id) DO UPDATE SET permissions = excluded.permissions",
            params![endpoint_id, json],
        )?;
        Ok(())
    }

    fn forget_device(&self, endpoint_id: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "DELETE FROM device_permissions WHERE endpoint_id = ?1",
            [endpoint_id],
        )?;
        conn.execute(
            "DELETE FROM device_names WHERE endpoint_id = ?1",
            [endpoint_id],
        )?;
        conn.execute(
            "DELETE FROM chat_outbox WHERE recipient = lower(?1)",
            [endpoint_id],
        )?;
        Ok(())
    }

    fn device_name(&self, endpoint_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn()?
            .query_row(
                "SELECT name FROM device_names WHERE endpoint_id = ?1",
                [endpoint_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn set_device_name(&self, endpoint_id: &str, name: Option<&str>) -> Result<()> {
        let conn = self.conn()?;
        match name {
            Some(name) => conn.execute(
                "INSERT INTO device_names (endpoint_id, name) VALUES (?1, ?2)
                 ON CONFLICT(endpoint_id) DO UPDATE SET name = excluded.name",
                params![endpoint_id, name],
            )?,
            None => conn.execute(
                "DELETE FROM device_names WHERE endpoint_id = ?1",
                [endpoint_id],
            )?,
        };
        Ok(())
    }

    fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()?
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn insert_message(&self, m: &StoredMessage) -> Result<bool> {
        let inserted = self.conn()?.execute(
            "INSERT OR IGNORE INTO chat_messages (id, conversation, author, lamport, json, target)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                m.id,
                m.conversation,
                m.author,
                m.lamport as i64,
                m.json,
                m.target
            ],
        )?;
        Ok(inserted == 1)
    }

    fn message(&self, id: &str) -> Result<Option<StoredMessage>> {
        Ok(self
            .conn()?
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM chat_messages WHERE id = ?1"),
                [id],
                message_row,
            )
            .optional()?)
    }

    fn messages(&self, conversation: &str, limit: usize) -> Result<Vec<StoredMessage>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM chat_messages WHERE conversation = ?1
             ORDER BY lamport DESC, author DESC, id DESC LIMIT ?2"
        ))?;
        let mut rows = statement
            .query_map(params![conversation, limit as i64], message_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    fn shown_messages(&self, conversation: &str, limit: usize) -> Result<Vec<StoredMessage>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM chat_messages WHERE conversation = ?1 AND target IS NULL
             ORDER BY lamport DESC, author DESC, id DESC LIMIT ?2"
        ))?;
        let mut rows = statement
            .query_map(params![conversation, limit as i64], message_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    fn changes_to(&self, ids: &[String]) -> Result<Vec<StoredMessage>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn()?;
        let marks = vec!["?"; ids.len()].join(",");
        let mut statement = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM chat_messages WHERE target IN ({marks})
             ORDER BY lamport, author, id"
        ))?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(ids.iter()), message_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn search(
        &self,
        text: &str,
        conversation: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredMessage>> {
        let conn = self.conn()?;
        let pattern = format!(
            "%{}%",
            text.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut statement = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM chat_messages
             WHERE json_extract(json, '$.body') LIKE ?1 ESCAPE '\\'
               AND (?2 IS NULL OR conversation = ?2)
             ORDER BY lamport DESC, author DESC, id DESC LIMIT ?3"
        ))?;
        let rows = statement
            .query_map(params![pattern, conversation, limit as i64], message_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn message_ids(&self, conversation: &str, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(
            "SELECT id FROM chat_messages WHERE conversation = ?1
             ORDER BY lamport DESC LIMIT ?2",
        )?;
        let ids = statement
            .query_map(params![conversation, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    fn max_lamport(&self) -> Result<u64> {
        let max: Option<i64> =
            self.conn()?
                .query_row("SELECT MAX(lamport) FROM chat_messages", [], |r| r.get(0))?;
        Ok(max.unwrap_or(0) as u64)
    }

    fn add_outbox(&self, message_id: &str, recipient: &str) -> Result<()> {
        self.conn()?.execute(
            "INSERT OR IGNORE INTO chat_outbox (message_id, recipient) VALUES (?1, ?2)",
            params![message_id, recipient],
        )?;
        Ok(())
    }

    fn outbox_for(&self, recipient: &str) -> Result<Vec<StoredMessage>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(
            "SELECT m.id, m.conversation, m.author, m.lamport, m.json, m.target
             FROM chat_outbox o JOIN chat_messages m ON m.id = o.message_id
             WHERE o.recipient = ?1 ORDER BY m.lamport, m.author, m.id",
        )?;
        let rows = statement
            .query_map([recipient], message_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn outbox_recipients(&self) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare("SELECT DISTINCT recipient FROM chat_outbox")?;
        let rows = statement
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn mark_delivered(&self, message_id: &str, recipient: &str, at_ms: u64) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let removed = tx.execute(
            "DELETE FROM chat_outbox WHERE message_id = ?1 AND recipient = ?2",
            params![message_id, recipient],
        )?;
        if removed == 1 {
            tx.execute(
                "INSERT OR IGNORE INTO chat_delivered (message_id, recipient, at) VALUES (?1, ?2, ?3)",
                params![message_id, recipient, at_ms as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn delivery(&self, message_id: &str) -> Result<(usize, usize)> {
        let conn = self.conn()?;
        let delivered: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chat_delivered WHERE message_id = ?1",
            [message_id],
            |r| r.get(0),
        )?;
        let waiting: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chat_outbox WHERE message_id = ?1",
            [message_id],
            |r| r.get(0),
        )?;
        Ok((delivered as usize, waiting as usize))
    }

    fn put_group(&self, id: &str, version: u64, json: &str) -> Result<bool> {
        let changed = self.conn()?.execute(
            "INSERT INTO chat_groups (id, version, json) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET version = excluded.version, json = excluded.json
             WHERE excluded.version > chat_groups.version",
            params![id, version as i64, json],
        )?;
        Ok(changed == 1)
    }

    fn group(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn()?
            .query_row("SELECT json FROM chat_groups WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?)
    }

    fn groups(&self) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare("SELECT json FROM chat_groups ORDER BY id")?;
        let rows = statement
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn conversations(&self, me: &str) -> Result<Vec<ConversationRow>> {
        let conn = self.conn()?;
        // Every conversation with a message, plus groups that have none yet.
        let mut statement = conn.prepare(
            "WITH convs AS (
                SELECT conversation FROM chat_messages
                UNION SELECT 'group:' || id FROM chat_groups
             )
             SELECT c.conversation,
                (SELECT json FROM chat_messages m WHERE m.conversation = c.conversation
                    AND m.target IS NULL
                    ORDER BY lamport DESC, author DESC, id DESC LIMIT 1),
                (SELECT COUNT(*) FROM chat_messages m WHERE m.conversation = c.conversation
                    AND m.target IS NULL AND m.author != ?1
                    AND m.lamport > COALESCE((SELECT lamport FROM chat_read r
                        WHERE r.conversation = c.conversation), -1)),
                (SELECT MAX(lamport) FROM chat_messages m WHERE m.conversation = c.conversation
                    AND m.target IS NULL)
             FROM convs c
             ORDER BY 4 DESC NULLS LAST, c.conversation",
        )?;
        let rows = statement
            .query_map([me], |r| {
                Ok(ConversationRow {
                    conversation: r.get(0)?,
                    last: r.get(1)?,
                    unread: r.get::<_, i64>(2)? as usize,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn set_read(&self, conversation: &str, lamport: u64) -> Result<()> {
        self.conn()?.execute(
            "INSERT INTO chat_read (conversation, lamport) VALUES (?1, ?2)
             ON CONFLICT(conversation) DO UPDATE SET lamport = MAX(lamport, excluded.lamport)",
            params![conversation, lamport as i64],
        )?;
        Ok(())
    }

    fn add_attachment(&self, hash: &str, message_id: &str, conversation: &str) -> Result<()> {
        self.conn()?.execute(
            "INSERT OR IGNORE INTO chat_attachments (hash, message_id, conversation)
             VALUES (?1, ?2, ?3)",
            params![hash, message_id, conversation],
        )?;
        Ok(())
    }

    fn received_attachments(&self, me: &str) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(
            "SELECT a.hash FROM chat_attachments a JOIN chat_messages m ON m.id = a.message_id
             GROUP BY a.hash HAVING SUM(m.author = ?1) = 0",
        )?;
        let hashes = statement
            .query_map([me], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(hashes)
    }

    fn attachment_conversations(&self, hash: &str) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut statement = conn.prepare(
            "SELECT DISTINCT conversation FROM chat_attachments WHERE hash = ?1 ORDER BY conversation",
        )?;
        let rows = statement.query_map([hash], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, conversation: &str, author: &str, lamport: u64) -> StoredMessage {
        StoredMessage {
            id: id.into(),
            conversation: conversation.into(),
            author: author.into(),
            lamport,
            json: format!("{{\"id\":\"{id}\",\"body\":\"text of {id}\"}}"),
            target: None,
        }
    }

    #[test]
    fn edits_and_deletions_travel_but_are_not_shown_or_counted() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .insert_message(&message("m1", "c", "them", 1))
            .unwrap();
        store
            .insert_message(&message("m2", "c", "them", 2))
            .unwrap();
        let mut edit = message("e1", "c", "them", 3);
        edit.target = Some("m1".into());
        edit.json = r#"{"id":"e1","body":"a 100% new_text"}"#.into();
        store.insert_message(&edit).unwrap();

        let shown: Vec<String> = store
            .shown_messages("c", 10)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(shown, ["m1", "m2"]);
        assert_eq!(
            store.messages("c", 10).unwrap().len(),
            3,
            "syncing passes edits on"
        );
        let changes = store.changes_to(&["m1".into(), "m2".into()]).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].target.as_deref(), Some("m1"));

        let row = &store.conversations("me").unwrap()[0];
        assert_eq!(row.unread, 2, "an edit is not a new unread message");
        assert!(row.last.as_deref().unwrap().contains("\"m2\""));

        // Search finds bodies and edits, any case, and takes % and _ literally.
        let found: Vec<String> = store
            .search("TEXT OF", None, 10)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(found, ["m2", "m1"]);
        assert_eq!(store.search("100%", Some("c"), 10).unwrap()[0].id, "e1");
        assert!(store.search("100%", Some("other"), 10).unwrap().is_empty());
        assert!(
            store.search("1_0", None, 10).unwrap().is_empty(),
            "_ is not a wildcard"
        );
    }

    #[test]
    fn permissions_and_settings() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(store.permissions("a").unwrap(), Permissions::default());

        let mut p = Permissions {
            remote_desktop: true,
            ..Default::default()
        };
        store.set_permissions("a", &p).unwrap();
        assert!(store.permissions("a").unwrap().remote_desktop);
        assert!(!store.permissions("b").unwrap().remote_desktop);

        p.chat = false;
        store.set_permissions("a", &p).unwrap();
        store.forget_device("a").unwrap();
        assert_eq!(store.permissions("a").unwrap(), Permissions::default());

        assert_eq!(store.setting("relay").unwrap(), None);
        store.set_setting("relay", "default").unwrap();
        store.set_setting("relay", "custom").unwrap();
        assert_eq!(store.setting("relay").unwrap().as_deref(), Some("custom"));
    }

    #[test]
    fn device_names_are_kept_until_cleared_or_forgotten() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(store.device_name("a").unwrap(), None);
        store.set_device_name("a", Some("Office")).unwrap();
        store.set_device_name("a", Some("Den")).unwrap();
        assert_eq!(store.device_name("a").unwrap().as_deref(), Some("Den"));
        assert_eq!(store.device_name("b").unwrap(), None);
        store.set_device_name("a", None).unwrap();
        assert_eq!(store.device_name("a").unwrap(), None);
        store.set_device_name("a", Some("Den")).unwrap();
        store.forget_device("a").unwrap();
        assert_eq!(store.device_name("a").unwrap(), None);
    }

    #[test]
    fn sqlite_persists_and_migrates_once() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SqliteStore::open_in(dir.path()).unwrap();
            assert_eq!(store.schema_version().unwrap() as usize, MIGRATIONS.len());
            store.set_setting("k", "v").unwrap();
        }
        let store = SqliteStore::open_in(dir.path()).unwrap();
        assert_eq!(store.schema_version().unwrap() as usize, MIGRATIONS.len());
        assert_eq!(store.setting("k").unwrap().as_deref(), Some("v"));
    }

    #[test]
    fn attachments_name_the_conversations_they_are_in() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .insert_message(&message("m1", "dm:a:b", "a", 1))
            .unwrap();
        store
            .insert_message(&message("m2", "group:g", "a", 2))
            .unwrap();
        store.add_attachment("h1", "m1", "dm:a:b").unwrap();
        store.add_attachment("h1", "m1", "dm:a:b").unwrap();
        store.add_attachment("h1", "m2", "group:g").unwrap();
        store.add_attachment("h2", "m2", "group:g").unwrap();
        assert_eq!(
            store.attachment_conversations("h1").unwrap(),
            ["dm:a:b", "group:g"]
        );
        assert_eq!(store.attachment_conversations("h2").unwrap(), ["group:g"]);
        assert!(store.attachment_conversations("h3").unwrap().is_empty());
    }

    #[test]
    fn messages_are_kept_once_and_in_order() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.insert_message(&message("m2", "c", "bob", 2)).unwrap());
        assert!(store.insert_message(&message("m1", "c", "ann", 1)).unwrap());
        assert!(!store.insert_message(&message("m1", "c", "ann", 1)).unwrap());
        assert!(
            store
                .insert_message(&message("x", "other", "ann", 9))
                .unwrap()
        );

        let ids: Vec<_> = store
            .messages("c", 10)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["m1", "m2"]);
        assert_eq!(store.messages("c", 1).unwrap()[0].id, "m2", "the newest");
        assert_eq!(store.max_lamport().unwrap(), 9);
        assert_eq!(store.message("m1").unwrap().unwrap().author, "ann");
    }

    #[test]
    fn outbox_and_delivery() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.insert_message(&message("m1", "g", "me", 1)).unwrap();
        store.insert_message(&message("m2", "g", "me", 2)).unwrap();
        for recipient in ["bob", "cat"] {
            store.add_outbox("m1", recipient).unwrap();
        }
        store.add_outbox("m2", "bob").unwrap();
        // Unpairing drops what waits for that device only.
        store.add_outbox("m2", "gone").unwrap();
        store.forget_device("GONE").unwrap();
        assert!(store.outbox_for("gone").unwrap().is_empty());
        let mut recipients = store.outbox_recipients().unwrap();
        recipients.sort();
        assert_eq!(recipients, ["bob", "cat"]);
        assert_eq!(store.outbox_for("bob").unwrap().len(), 2);
        assert_eq!(store.delivery("m1").unwrap(), (0, 2));

        store.mark_delivered("m1", "bob", 5).unwrap();
        store.mark_delivered("m1", "bob", 6).unwrap();
        assert_eq!(store.delivery("m1").unwrap(), (1, 1));
        assert_eq!(store.outbox_for("bob").unwrap()[0].id, "m2");
    }

    #[test]
    fn groups_keep_the_newest_version() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.put_group("g", 1, "v1").unwrap());
        assert!(!store.put_group("g", 1, "v1 again").unwrap());
        assert!(store.put_group("g", 2, "v2").unwrap());
        assert!(!store.put_group("g", 1, "old").unwrap());
        assert_eq!(store.group("g").unwrap().as_deref(), Some("v2"));
        assert_eq!(store.groups().unwrap(), ["v2"]);
    }

    #[test]
    fn conversations_count_what_others_wrote_after_reading() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.put_group("empty", 1, "{}").unwrap();
        store
            .insert_message(&message("a", "dm:x", "bob", 1))
            .unwrap();
        store
            .insert_message(&message("b", "dm:x", "me", 2))
            .unwrap();
        store
            .insert_message(&message("c", "dm:x", "bob", 3))
            .unwrap();
        store
            .insert_message(&message("d", "dm:y", "cat", 7))
            .unwrap();

        let list = store.conversations("me").unwrap();
        let names: Vec<_> = list.iter().map(|c| c.conversation.as_str()).collect();
        assert_eq!(names, ["dm:y", "dm:x", "group:empty"], "newest first");
        assert_eq!(list[1].unread, 2);
        assert!(list[1].last.as_deref().unwrap().contains("\"c\""));
        assert_eq!(list[2].last, None);

        store.set_read("dm:x", 1).unwrap();
        assert_eq!(store.conversations("me").unwrap()[1].unread, 1);
        store.set_read("dm:x", 3).unwrap();
        store.set_read("dm:x", 2).unwrap();
        assert_eq!(
            store.conversations("me").unwrap()[1].unread,
            0,
            "never moves back"
        );
    }
}
