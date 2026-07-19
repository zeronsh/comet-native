//! comet-sync — Loro room client (loro-protocol over WebSocket against the TS edge),
//! ephemeral presence, and the local `DocsStore` (SQLite snapshots + processed-command ledger).
//!
//! M1 deliverables:
//! - `RoomClient`: join `wss://…/session/{chatId}/ws?token=`, VV backfill, fragment reassembly,
//!   reconnect with backoff, local-update push, remote-update import.
//! - `DocsStore`: snapshot persistence (doc IS the outbox — commands + user entries flush
//!   immediately), processed-command ledger with mark-BEFORE-execute semantics.

pub struct DocsStore;

impl DocsStore {
    // TODO(M1): open(path), load_snapshot(chat_id), save_snapshot(chat_id, bytes),
    // is_processed(command_id), mark_processed(command_id).
}
