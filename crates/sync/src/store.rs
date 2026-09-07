//! `DocsStore` — local SQLite persistence for doc snapshots and the
//! processed-command ledger (ARCHITECTURE §2 command plane: entries are marked
//! processed BEFORE execution so a crash can never double-execute a command).

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use zeron_crypto::content::{ContentPurpose, SealedContent};
use zeron_crypto::record::{RecordBinding, RecordKind, UnverifiedRecord};

pub const MAX_ENCRYPTED_OUTBOX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENCRYPTED_RECORD_BYTES: usize = crate::chat_client::MAX_PUSH_BYTES;

/// Errors surfaced by [`DocsStore`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encrypted batch conflicts with an existing immutable record")]
    EncryptedBatchConflict,
    #[error("encrypted outbox capacity reached")]
    EncryptedOutboxFull,
    #[error("encrypted record exceeds the chat transport limit")]
    EncryptedBatchTooLarge,
    #[error("invalid encrypted outbox limit")]
    InvalidOutboxLimit,
    #[error("stored encrypted batch could not be verified")]
    InvalidEncryptedBatch,
    #[error("snapshot cursor exceeds the supported storage range")]
    InvalidCursor,
    #[error("encrypted snapshot cursor would regress")]
    CursorRegression,
}

/// Ordered, append-only migrations. Each entry runs once inside a transaction;
/// `schema_migrations` records what has been applied.
const MIGRATIONS: &[&str] = &[
    // v1 — snapshots + processed-command ledger
    "CREATE TABLE snapshots (
        doc_id   TEXT PRIMARY KEY,
        bytes    BLOB NOT NULL,
        saved_at INTEGER NOT NULL
     ) STRICT;
     CREATE TABLE processed_commands (
        command_id   TEXT PRIMARY KEY,
        processed_at INTEGER NOT NULL
     ) STRICT;",
    // v2 — chat2 room cursor + doc epoch (docs/chat2-sync.md C2). The cursor
    // is persisted in the SAME transaction as the snapshot bytes, so content
    // and cursor cannot diverge (restored backups / copied devices simply
    // redownload from their honest cursor). `epoch` marks rebuild lineage
    // (M1/M3): 2 = thin chat2 rebuild; NULL/0 = pre-migration s2 doc.
    "ALTER TABLE snapshots ADD COLUMN cursor INTEGER;
     ALTER TABLE snapshots ADD COLUMN epoch INTEGER;",
    "CREATE TABLE encrypted_outbox (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
        batch_id BLOB NOT NULL UNIQUE CHECK(length(batch_id) = 16),
        doc_id TEXT NOT NULL CHECK(length(doc_id) BETWEEN 1 AND 256),
        vault_id BLOB NOT NULL CHECK(length(vault_id) = 16),
        generation BLOB NOT NULL CHECK(length(generation) = 16),
        key_epoch BLOB NOT NULL CHECK(length(key_epoch) = 8),
        object_id BLOB NOT NULL CHECK(length(object_id) = 16),
        author_id BLOB NOT NULL CHECK(length(author_id) = 16),
        membership_hash BLOB NOT NULL CHECK(length(membership_hash) = 32),
        record BLOB NOT NULL CHECK(length(record) BETWEEN 1 AND 1044480),
        queued_at INTEGER NOT NULL
     ) STRICT;
     CREATE INDEX encrypted_outbox_scope ON encrypted_outbox
        (vault_id, generation, key_epoch, object_id, author_id, membership_hash, sequence);",
];

#[derive(Clone)]
pub struct PendingEncryptedBatch {
    sequence: i64,
    doc_id: String,
    binding: RecordBinding,
    revision_id: [u8; 16],
    encoded: Vec<u8>,
}

impl PendingEncryptedBatch {
    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }
    pub fn binding(&self) -> &RecordBinding {
        &self.binding
    }
    pub fn revision_id(&self) -> &[u8; 16] {
        &self.revision_id
    }
    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }
}

impl std::fmt::Debug for PendingEncryptedBatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PendingEncryptedBatch([REDACTED])")
    }
}

/// SQLite-backed store under a data directory (`{data_dir}/docs.sqlite3`).
///
/// Holds warm-open doc snapshots (the DO room is authoritative; these make
/// cold starts instant and offline restarts possible) and the command ledger
/// that gives command execution mark-BEFORE-execute idempotence.
pub struct DocsStore {
    conn: Mutex<Connection>,
}

impl DocsStore {
    /// Open (creating directory, database, and schema as needed).
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir)?;
        let mut conn = Connection::open(data_dir.join("docs.sqlite3"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Latest saved snapshot for `doc_id`, if any.
    pub fn load_snapshot(&self, doc_id: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let bytes = self
            .conn()
            .query_row(
                "SELECT bytes FROM snapshots WHERE doc_id = ?1",
                params![doc_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(bytes)
    }

    /// Save (upsert) the snapshot for `doc_id`.
    pub fn save_snapshot(&self, doc_id: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO snapshots (doc_id, bytes, saved_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(doc_id) DO UPDATE SET bytes = excluded.bytes, saved_at = excluded.saved_at",
            params![doc_id, bytes, now_ms()],
        )?;
        Ok(())
    }

    /// Save the snapshot together with its chat2 room cursor and doc epoch —
    /// ONE transaction, so bytes and cursor can never disagree (the C2 rule;
    /// a divergent pair is exactly the restored-backup redownload bug).
    pub fn save_snapshot_with_cursor(
        &self,
        doc_id: &str,
        bytes: &[u8],
        cursor: u64,
        epoch: u32,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO snapshots (doc_id, bytes, saved_at, cursor, epoch) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(doc_id) DO UPDATE SET bytes = excluded.bytes, saved_at = excluded.saved_at,
                 cursor = excluded.cursor, epoch = excluded.epoch",
            params![doc_id, bytes, now_ms(), cursor as i64, epoch as i64],
        )?;
        Ok(())
    }

    /// Snapshot + chat2 cursor + epoch. Pre-migration rows (or rows written
    /// by [`Self::save_snapshot`]) read back as `(bytes, 0, 0)`.
    pub fn load_snapshot_with_cursor(
        &self,
        doc_id: &str,
    ) -> Result<Option<(Vec<u8>, u64, u32)>, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT bytes, cursor, epoch FROM snapshots WHERE doc_id = ?1",
                params![doc_id],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64,
                        row.get::<_, Option<i64>>(2)?.unwrap_or(0) as u32,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Delete the snapshot row for `doc_id` (destructive schema breaks: the
    /// legacy `workspace` row is dropped on open). Missing rows are a no-op.
    pub fn delete_snapshot(&self, doc_id: &str) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM snapshots WHERE doc_id = ?1", params![doc_id])?;
        Ok(())
    }

    /// Whether a snapshot row exists for `doc_id` — presence only, no blob read.
    pub fn has_snapshot(&self, doc_id: &str) -> Result<bool, StoreError> {
        let hit = self
            .conn()
            .query_row(
                "SELECT 1 FROM snapshots WHERE doc_id = ?1",
                params![doc_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// The full command ledger — profile-import reads the source's claims so
    /// imported pending commands can never re-execute under the new profile.
    pub fn processed_commands(&self) -> Result<Vec<(String, i64)>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT command_id, processed_at FROM processed_commands")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Merge foreign ledger claims (profile import). Existing claims win;
    /// returns how many rows were newly inserted.
    pub fn import_processed_commands(&self, rows: &[(String, i64)]) -> Result<usize, StoreError> {
        let mut inserted = 0;
        let conn = self.conn();
        for (command_id, processed_at) in rows {
            inserted += conn.execute(
                "INSERT OR IGNORE INTO processed_commands (command_id, processed_at) VALUES (?1, ?2)",
                params![command_id, processed_at],
            )?;
        }
        Ok(inserted)
    }

    /// Whether `command_id` has already been claimed for execution.
    pub fn is_processed(&self, command_id: &str) -> Result<bool, StoreError> {
        let hit = self
            .conn()
            .query_row(
                "SELECT 1 FROM processed_commands WHERE command_id = ?1",
                params![command_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Claim `command_id` for execution — call BEFORE executing (ledger rule:
    /// a crash mid-execution must never re-run the command). Returns `true`
    /// if this call claimed it, `false` if it was already processed.
    pub fn mark_processed(&self, command_id: &str) -> Result<bool, StoreError> {
        let changed = self.conn().execute(
            "INSERT OR IGNORE INTO processed_commands (command_id, processed_at) VALUES (?1, ?2)",
            params![command_id, now_ms()],
        )?;
        Ok(changed > 0)
    }

    pub fn persist_encrypted_batch(
        &self,
        doc_id: &str,
        snapshot: &[u8],
        cursor: u64,
        document_epoch: u32,
        sealed: &SealedContent,
        max_pending_bytes: usize,
    ) -> Result<PendingEncryptedBatch, StoreError> {
        if max_pending_bytes > MAX_ENCRYPTED_OUTBOX_BYTES {
            return Err(StoreError::InvalidOutboxLimit);
        }
        if doc_id.is_empty() || doc_id.len() > 256 || sealed.purpose() != ContentPurpose::ChatUpdate
        {
            return Err(StoreError::InvalidEncryptedBatch);
        }
        if sealed.encoded().len() > MAX_ENCRYPTED_RECORD_BYTES {
            return Err(StoreError::EncryptedBatchTooLarge);
        }
        let cursor = i64::try_from(cursor).map_err(|_| StoreError::InvalidCursor)?;
        let binding = sealed.binding();
        let mut connection = self.conn();
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(i64, String, Vec<u8>)> = transaction
            .query_row(
                "SELECT sequence, doc_id, record FROM encrypted_outbox WHERE batch_id = ?1",
                params![sealed.revision_id().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((sequence, existing_doc_id, encoded)) = existing {
            if existing_doc_id != doc_id || encoded != sealed.encoded() {
                return Err(StoreError::EncryptedBatchConflict);
            }
            transaction.commit()?;
            return Ok(PendingEncryptedBatch {
                sequence,
                doc_id: existing_doc_id,
                binding: *binding,
                revision_id: *sealed.revision_id(),
                encoded,
            });
        }
        let stored_cursor: Option<Option<i64>> = transaction
            .query_row(
                "SELECT cursor FROM snapshots WHERE doc_id = ?1",
                params![doc_id],
                |row| row.get(0),
            )
            .optional()?;
        if stored_cursor
            .flatten()
            .is_some_and(|stored| stored < 0 || cursor < stored)
        {
            return Err(StoreError::CursorRegression);
        }
        let queued_bytes: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(length(record)), 0) FROM encrypted_outbox",
            [],
            |row| row.get(0),
        )?;
        let queued_bytes =
            usize::try_from(queued_bytes).map_err(|_| StoreError::InvalidEncryptedBatch)?;
        if queued_bytes
            .checked_add(sealed.encoded().len())
            .is_none_or(|total| total > max_pending_bytes)
        {
            return Err(StoreError::EncryptedOutboxFull);
        }
        transaction.execute(
            "INSERT INTO snapshots (doc_id, bytes, saved_at, cursor, epoch) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(doc_id) DO UPDATE SET bytes = excluded.bytes, saved_at = excluded.saved_at,
                 cursor = excluded.cursor, epoch = excluded.epoch",
            params![doc_id, snapshot, now_ms(), cursor, i64::from(document_epoch)],
        )?;
        transaction.execute(
            "INSERT INTO encrypted_outbox
             (batch_id, doc_id, vault_id, generation, key_epoch, object_id, author_id, membership_hash, record, queued_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![sealed.revision_id().as_slice(), doc_id, binding.vault_id.as_slice(), binding.generation.as_slice(),
                binding.epoch.to_be_bytes().as_slice(), binding.object_id.as_slice(), binding.author_id.as_slice(),
                binding.membership_hash.as_slice(), sealed.encoded(), now_ms()],
        )?;
        let sequence = transaction.last_insert_rowid();
        transaction.commit()?;
        Ok(PendingEncryptedBatch {
            sequence,
            doc_id: doc_id.to_owned(),
            binding: *binding,
            revision_id: *sealed.revision_id(),
            encoded: sealed.encoded().to_vec(),
        })
    }

    pub fn pending_encrypted_batches(
        &self,
        binding: &RecordBinding,
        trusted_public_key: &[u8],
        max_batches: usize,
    ) -> Result<Vec<PendingEncryptedBatch>, StoreError> {
        if max_batches > 128 {
            return Err(StoreError::InvalidOutboxLimit);
        }
        if binding.kind != RecordKind::Content {
            return Err(StoreError::InvalidEncryptedBatch);
        }
        let connection = self.conn();
        let mut statement = connection.prepare(
            "SELECT sequence, doc_id, batch_id, length(record), record FROM encrypted_outbox
             WHERE vault_id = ?1 AND generation = ?2 AND key_epoch = ?3 AND object_id = ?4
                AND author_id = ?5 AND membership_hash = ?6 ORDER BY sequence LIMIT ?7",
        )?;
        let mut rows = statement.query(params![
            binding.vault_id.as_slice(),
            binding.generation.as_slice(),
            binding.epoch.to_be_bytes().as_slice(),
            binding.object_id.as_slice(),
            binding.author_id.as_slice(),
            binding.membership_hash.as_slice(),
            max_batches as i64
        ])?;
        let mut pending = Vec::new();
        let mut loaded_bytes = 0usize;
        while let Some(row) = rows.next()? {
            let length = usize::try_from(row.get::<_, i64>(3)?)
                .map_err(|_| StoreError::InvalidEncryptedBatch)?;
            loaded_bytes = loaded_bytes
                .checked_add(length)
                .ok_or(StoreError::InvalidEncryptedBatch)?;
            if length > MAX_ENCRYPTED_RECORD_BYTES || loaded_bytes > MAX_ENCRYPTED_OUTBOX_BYTES {
                return Err(StoreError::InvalidEncryptedBatch);
            }
            let sequence = row.get(0)?;
            let doc_id: String = row.get(1)?;
            let revision_bytes: Vec<u8> = row.get(2)?;
            let revision_id: [u8; 16] = revision_bytes
                .as_slice()
                .try_into()
                .map_err(|_| StoreError::InvalidEncryptedBatch)?;
            let encoded: Vec<u8> = row.get(4)?;
            if doc_id.is_empty() || doc_id.len() > 256 || encoded.len() != length {
                return Err(StoreError::InvalidEncryptedBatch);
            }
            let parsed = UnverifiedRecord::parse(&encoded, MAX_ENCRYPTED_RECORD_BYTES)
                .map_err(|_| StoreError::InvalidEncryptedBatch)?;
            if parsed.untrusted_revision_id() != &revision_id {
                return Err(StoreError::InvalidEncryptedBatch);
            }
            parsed
                .verify(binding, trusted_public_key)
                .map_err(|_| StoreError::InvalidEncryptedBatch)?;
            pending.push(PendingEncryptedBatch {
                sequence,
                doc_id,
                binding: *binding,
                revision_id,
                encoded,
            });
        }
        Ok(pending)
    }

    pub fn acknowledge_encrypted_batch(
        &self,
        batch: &PendingEncryptedBatch,
    ) -> Result<bool, StoreError> {
        let connection = self.conn();
        connection.pragma_update(None, "synchronous", "FULL")?;
        let removed = connection.execute(
            "DELETE FROM encrypted_outbox WHERE sequence = ?1 AND batch_id = ?2 AND record = ?3",
            params![
                batch.sequence,
                batch.revision_id.as_slice(),
                batch.encoded.as_slice()
            ],
        )?;
        Ok(removed == 1)
    }

    pub fn encrypted_outbox_stats(&self) -> Result<(usize, usize), StoreError> {
        let (count, bytes): (i64, i64) = self.conn().query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(record)), 0) FROM encrypted_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((
            usize::try_from(count).map_err(|_| StoreError::InvalidEncryptedBatch)?,
            usize::try_from(bytes).map_err(|_| StoreError::InvalidEncryptedBatch)?,
        ))
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked mid-query; the
        // connection itself is still usable.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
         ) STRICT",
    )?;
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, now_ms()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrip_and_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();

        assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
        store.save_snapshot("chat-1", b"v1").unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v1"[..])
        );
        store.save_snapshot("chat-1", b"v2-longer-bytes").unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v2-longer-bytes"[..])
        );
        // Distinct docs do not collide.
        store.save_snapshot("chat-2", b"other").unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v2-longer-bytes"[..])
        );
    }

    #[test]
    fn cursor_rides_the_snapshot_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();

        // Plain saves (pre-chat2 path) read back with cursor/epoch 0.
        store.save_snapshot("chat-1", b"v1").unwrap();
        assert_eq!(
            store.load_snapshot_with_cursor("chat-1").unwrap(),
            Some((b"v1".to_vec(), 0, 0))
        );
        // Cursor and bytes land together; a re-save moves both.
        store
            .save_snapshot_with_cursor("chat-1", b"v2", 41, 2)
            .unwrap();
        assert_eq!(
            store.load_snapshot_with_cursor("chat-1").unwrap(),
            Some((b"v2".to_vec(), 41, 2))
        );
        // Plain load still works for cursor-written rows.
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v2"[..])
        );
        // A plain save (legacy caller) clears nothing — cursor persists…
        store.save_snapshot("chat-1", b"v3").unwrap();
        let (bytes, cursor, epoch) = store.load_snapshot_with_cursor("chat-1").unwrap().unwrap();
        assert_eq!((bytes.as_slice(), cursor, epoch), (&b"v3"[..], 41, 2));
    }

    #[test]
    fn processed_ledger_claims_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();

        assert!(!store.is_processed("cmd-1").unwrap());
        assert!(store.mark_processed("cmd-1").unwrap(), "first mark claims");
        assert!(store.is_processed("cmd-1").unwrap());
        assert!(
            !store.mark_processed("cmd-1").unwrap(),
            "second mark must not re-claim"
        );
    }

    #[test]
    fn reopen_preserves_data_and_migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = DocsStore::open(dir.path()).unwrap();
            store.save_snapshot("chat-1", b"persisted").unwrap();
            store.mark_processed("cmd-1").unwrap();
        }
        let store = DocsStore::open(dir.path()).unwrap(); // re-runs migrate()
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"persisted"[..])
        );
        assert!(store.is_processed("cmd-1").unwrap());
        assert!(!store.mark_processed("cmd-1").unwrap());
    }
}
