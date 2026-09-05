use zeron_crypto::content::{
    self, ContentKey, ContentPurpose, DeviceSigner, KeyScope, SealedContent,
};
use zeron_crypto::record::{RecordBinding, RecordKind};
use zeron_sync::{DocsStore, MAX_ENCRYPTED_OUTBOX_BYTES, StoreError};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn binding(epoch: u64) -> RecordBinding {
    RecordBinding {
        kind: RecordKind::Content,
        vault_id: [1; 16],
        generation: [2; 16],
        epoch,
        object_id: [3; 16],
        author_id: [4; 16],
        membership_hash: [5; 32],
    }
}

fn seal(
    epoch: u64,
    plaintext: &[u8],
) -> Result<(SealedContent, ContentKey, DeviceSigner), content::ContentError> {
    let context = binding(epoch);
    let key = ContentKey::generate(KeyScope::from(&context))?;
    let signer = DeviceSigner::from_seed([4; 16], &[8; 32])?;
    let sealed = content::seal(
        &context,
        ContentPurpose::ChatUpdate,
        &key,
        &signer,
        plaintext,
        1024,
    )?;
    Ok((sealed, key, signer))
}

#[test]
fn non_chat_and_oversized_records_cannot_enter_the_chat_outbox() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = DocsStore::open(directory.path())?;
    let (_, key, signer) = seal(1, b"command")?;
    let blob = content::seal(&binding(1), ContentPurpose::Blob, &key, &signer, b"blob", 4)?;
    assert!(matches!(
        store.persist_encrypted_batch(
            "chat-1",
            b"snapshot",
            0,
            2,
            &blob,
            MAX_ENCRYPTED_OUTBOX_BYTES
        ),
        Err(StoreError::InvalidEncryptedBatch)
    ));
    let plaintext = vec![42; zeron_sync::chat_client::MAX_PUSH_BYTES];
    let oversized = content::seal(
        &binding(1),
        ContentPurpose::ChatUpdate,
        &key,
        &signer,
        &plaintext,
        plaintext.len(),
    )?;
    assert!(matches!(
        store.persist_encrypted_batch(
            "chat-1",
            b"snapshot",
            0,
            2,
            &oversized,
            MAX_ENCRYPTED_OUTBOX_BYTES
        ),
        Err(StoreError::EncryptedBatchTooLarge)
    ));
    assert_eq!(store.encrypted_outbox_stats()?.0, 0);
    assert!(!store.has_snapshot("chat-1")?);
    Ok(())
}

#[test]
fn full_width_key_epochs_round_trip_without_sqlite_integer_truncation() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = DocsStore::open(directory.path())?;
    let (sealed, _, signer) = seal(u64::MAX, b"command")?;
    store.persist_encrypted_batch(
        "chat-1",
        b"snapshot",
        0,
        2,
        &sealed,
        MAX_ENCRYPTED_OUTBOX_BYTES,
    )?;
    let pending = store.pending_encrypted_batches(&binding(u64::MAX), signer.public_key(), 16)?;
    assert_eq!(
        pending
            .first()
            .ok_or("missing full-width epoch batch")?
            .binding()
            .epoch,
        u64::MAX
    );
    Ok(())
}

#[test]
fn snapshot_and_ciphertext_survive_restart_and_ack() -> TestResult {
    let directory = tempfile::tempdir()?;
    let (sealed, key, signer) = seal(1, b"private command canary")?;
    {
        let store = DocsStore::open(directory.path())?;
        let receipt = store.persist_encrypted_batch(
            "chat-1",
            b"snapshot",
            7,
            2,
            &sealed,
            MAX_ENCRYPTED_OUTBOX_BYTES,
        )?;
        assert_eq!(receipt.encoded(), sealed.encoded());
        assert_eq!(receipt.revision_id(), sealed.revision_id());
        assert_eq!(
            store.load_snapshot_with_cursor("chat-1")?,
            Some((b"snapshot".to_vec(), 7, 2))
        );
    }
    let store = DocsStore::open(directory.path())?;
    let pending = store.pending_encrypted_batches(&binding(1), signer.public_key(), 16)?;
    assert_eq!(pending.len(), 1);
    let receipt = pending.first().ok_or("missing persisted batch")?;
    assert_eq!(receipt.encoded(), sealed.encoded());
    let plaintext = content::open(
        receipt.encoded(),
        &binding(1),
        ContentPurpose::ChatUpdate,
        &key,
        signer.public_key(),
        1024,
    )?;
    assert_eq!(plaintext.plaintext().as_bytes(), b"private command canary");
    assert!(store.acknowledge_encrypted_batch(receipt)?);
    assert!(!store.acknowledge_encrypted_batch(receipt)?);
    assert_eq!(store.encrypted_outbox_stats()?.0, 0);
    assert!(store.has_snapshot("chat-1")?);
    Ok(())
}

#[test]
fn duplicate_enqueue_does_not_rewrite_snapshot_or_ciphertext() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = DocsStore::open(directory.path())?;
    let (sealed, _, signer) = seal(1, b"command")?;
    store.persist_encrypted_batch(
        "chat-1",
        b"new snapshot",
        9,
        2,
        &sealed,
        MAX_ENCRYPTED_OUTBOX_BYTES,
    )?;
    store.persist_encrypted_batch(
        "chat-1",
        b"stale retry",
        0,
        2,
        &sealed,
        MAX_ENCRYPTED_OUTBOX_BYTES,
    )?;
    assert_eq!(
        store.load_snapshot_with_cursor("chat-1")?,
        Some((b"new snapshot".to_vec(), 9, 2))
    );
    assert_eq!(
        store
            .pending_encrypted_batches(&binding(1), signer.public_key(), 16)?
            .len(),
        1
    );
    assert!(matches!(
        store.persist_encrypted_batch(
            "another-doc",
            b"wrong",
            0,
            2,
            &sealed,
            MAX_ENCRYPTED_OUTBOX_BYTES
        ),
        Err(StoreError::EncryptedBatchConflict)
    ));
    assert!(!store.has_snapshot("another-doc")?);
    Ok(())
}

#[test]
fn outbox_failure_rolls_back_the_snapshot() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = DocsStore::open(directory.path())?;
    store.save_snapshot_with_cursor("chat-1", b"old snapshot", 3, 2)?;
    let connection = rusqlite::Connection::open(directory.path().join("docs.sqlite3"))?;
    connection.execute_batch("CREATE TRIGGER fail_outbox BEFORE INSERT ON encrypted_outbox BEGIN SELECT RAISE(ABORT, 'test failure'); END;")?;
    let (sealed, _, _) = seal(1, b"command")?;
    assert!(
        store
            .persist_encrypted_batch(
                "chat-1",
                b"new snapshot",
                4,
                2,
                &sealed,
                MAX_ENCRYPTED_OUTBOX_BYTES
            )
            .is_err()
    );
    assert_eq!(
        store.load_snapshot_with_cursor("chat-1")?,
        Some((b"old snapshot".to_vec(), 3, 2))
    );
    assert_eq!(store.encrypted_outbox_stats()?.0, 0);
    Ok(())
}

#[test]
fn quotas_and_cursor_regression_preserve_last_good_state() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = DocsStore::open(directory.path())?;
    store.save_snapshot_with_cursor("chat-1", b"old", 5, 2)?;
    let (sealed, _, _) = seal(1, b"command")?;
    assert!(matches!(
        store.persist_encrypted_batch("chat-1", b"new", 5, 2, &sealed, 0),
        Err(StoreError::EncryptedOutboxFull)
    ));
    assert!(matches!(
        store.persist_encrypted_batch(
            "chat-1",
            b"old cursor",
            4,
            2,
            &sealed,
            MAX_ENCRYPTED_OUTBOX_BYTES
        ),
        Err(StoreError::CursorRegression)
    ));
    assert!(matches!(
        store.persist_encrypted_batch(
            "chat-1",
            b"overflow",
            u64::MAX,
            2,
            &sealed,
            MAX_ENCRYPTED_OUTBOX_BYTES
        ),
        Err(StoreError::InvalidCursor)
    ));
    assert_eq!(
        store.load_snapshot_with_cursor("chat-1")?,
        Some((b"old".to_vec(), 5, 2))
    );
    assert_eq!(store.encrypted_outbox_stats()?.0, 0);
    Ok(())
}

#[test]
fn replay_filters_policy_and_keeps_old_epoch_work() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = DocsStore::open(directory.path())?;
    let (first, _, signer) = seal(1, b"first")?;
    let (second, _, _) = seal(1, b"second")?;
    store.persist_encrypted_batch("chat-1", b"one", 0, 2, &first, MAX_ENCRYPTED_OUTBOX_BYTES)?;
    store.persist_encrypted_batch("chat-1", b"two", 0, 2, &second, MAX_ENCRYPTED_OUTBOX_BYTES)?;
    let pending = store.pending_encrypted_batches(&binding(1), signer.public_key(), 1)?;
    assert_eq!(
        pending.first().ok_or("missing first batch")?.revision_id(),
        first.revision_id()
    );
    assert!(
        store
            .pending_encrypted_batches(&binding(2), signer.public_key(), 16)?
            .is_empty()
    );
    let mut changed_policy = binding(1);
    changed_policy.membership_hash = [9; 32];
    assert!(
        store
            .pending_encrypted_batches(&changed_policy, signer.public_key(), 16)?
            .is_empty()
    );
    assert_eq!(store.encrypted_outbox_stats()?.0, 2);
    Ok(())
}

#[test]
fn concurrent_connections_cannot_overfill_the_outbox() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = DocsStore::open(directory.path())?;
    let first_store = DocsStore::open(directory.path())?;
    let second_store = DocsStore::open(directory.path())?;
    let (first, _, _) = seal(1, b"a")?;
    let (second, _, _) = seal(1, b"b")?;
    assert_eq!(first.encoded().len(), second.encoded().len());
    let quota = first.encoded().len();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut writers = Vec::new();
    for (connection, sealed) in [(first_store, first), (second_store, second)] {
        let barrier = barrier.clone();
        writers.push(std::thread::spawn(move || {
            barrier.wait();
            match connection.persist_encrypted_batch("chat-1", b"snapshot", 0, 2, &sealed, quota) {
                Ok(_) => Ok(true),
                Err(StoreError::EncryptedOutboxFull) => Ok(false),
                Err(error) => Err(error),
            }
        }));
    }
    let mut committed = 0;
    for writer in writers {
        if writer.join().map_err(|_| "outbox writer panicked")?? {
            committed += 1;
        }
    }
    assert_eq!(committed, 1);
    assert_eq!(store.encrypted_outbox_stats()?, (1, quota));
    Ok(())
}

#[test]
fn corrupted_stored_ciphertext_is_not_replayed_or_deleted() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = DocsStore::open(directory.path())?;
    let (sealed, _, signer) = seal(1, b"command")?;
    let receipt = store.persist_encrypted_batch(
        "chat-1",
        b"snapshot",
        0,
        2,
        &sealed,
        MAX_ENCRYPTED_OUTBOX_BYTES,
    )?;
    let mut damaged = sealed.encoded().to_vec();
    *damaged.last_mut().ok_or("empty encrypted record")? ^= 1;
    let connection = rusqlite::Connection::open(directory.path().join("docs.sqlite3"))?;
    connection.execute(
        "UPDATE encrypted_outbox SET record = ?1 WHERE batch_id = ?2",
        rusqlite::params![damaged, sealed.revision_id().as_slice()],
    )?;
    assert!(matches!(
        store.pending_encrypted_batches(&binding(1), signer.public_key(), 16),
        Err(StoreError::InvalidEncryptedBatch)
    ));
    assert!(!store.acknowledge_encrypted_batch(&receipt)?);
    assert_eq!(store.encrypted_outbox_stats()?.0, 1);
    Ok(())
}
