//! zeron-sync — the edge room clients (registry rows + chat2 row protocol over
//! WebSocket against the TS edge) and the local `DocsStore` (SQLite snapshots +
//! processed-command ledger).
//!
//! - [`ChatClient`]: joins a ChatRoom DO (`wss://…/chat2/{chatId}/ws?token=`),
//!   catches up via checkpoint + row backfill, pushes local loro updates as
//!   rows, and reconnects with exponential backoff.
//! - [`RegistryClient`]: the per-profile workspace registry room (sidebar rows,
//!   presence).
//! - [`DocsStore`]: snapshot persistence (the doc IS the outbox — commands + user entries
//!   flush immediately) and the processed-command ledger with mark-BEFORE-execute semantics.

pub mod chat_client;
pub mod chat_frames;
pub mod dial;
pub mod net_path;
pub mod registry;
mod store;
mod types;
pub mod wake;

pub use chat_client::{
    ChatClient, ChatDocSink, ChatEvent, ChatStatsSnapshot, ChatTuning, CheckpointFetcher,
};
pub use registry::{
    ReconnectState, RegistryClient, RegistryEvent, RegistryTransport, RegistryTuning,
};
pub use store::{DocsStore, MAX_ENCRYPTED_OUTBOX_BYTES, PendingEncryptedBatch, StoreError};
pub use types::{RoomStatsSnapshot, StaticUrl, SyncError, UrlProvider};
