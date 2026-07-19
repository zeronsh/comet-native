//! comet-sync — Loro room client (loro-protocol over WebSocket against the TS edge),
//! ephemeral presence, and the local `DocsStore` (SQLite snapshots + processed-command ledger).
//!
//! - [`RoomClient`]: joins a SessionRoom DO room (`wss://…/session/{chatId}/ws?token=`),
//!   backfills via version-vector diff, pushes local commits, imports remote updates,
//!   reassembles/produces fragments, relays `%EPH` presence, and reconnects with
//!   exponential backoff. Wire format is the official `loro-protocol` crate — byte-identical
//!   to the npm package the edge imports.
//! - [`DocsStore`]: snapshot persistence (the doc IS the outbox — commands + user entries
//!   flush immediately) and the processed-command ledger with mark-BEFORE-execute semantics.

mod room;
mod store;

pub use room::{RoomClient, RoomEvent, SyncError};
pub use store::{DocsStore, StoreError};
