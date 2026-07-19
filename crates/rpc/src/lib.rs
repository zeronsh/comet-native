//! comet-rpc — the typed control plane (UiRpc / ControlRpc) over WebSocket + in-memory
//! transports, and the device-room virtual-socket relay ({s,k,to,from} frames).
//!
//! Framing: ndjson envelopes `{id, method, params}` / `{id, ok|err}` / `{id, item}` (streams),
//! matching the shape of comet's Effect RPC without the Effect runtime.

/// RPC method names — single source of truth for both ends.
/// Full surface: docs/research/feature-inventory.md §2.
pub mod methods {
    pub const LIST_HARNESSES: &str = "ListHarnesses";
    pub const LIST_MODELS: &str = "ListModels";
    pub const QUEUE_COMMAND: &str = "QueueCommand";
    pub const WATCH_DOC_MESSAGES: &str = "WatchDocMessages";
    pub const WATCH_CHATS: &str = "WatchChats";
    pub const WATCH_DEVICES: &str = "WatchDevices";
    pub const WATCH_SESSIONS: &str = "WatchSessions";
    pub const AUTH_STATUS: &str = "AuthStatus";
    // TODO(M2+): repos/folders/worktrees, uploads/attachments, terminals, agent accounts,
    // auth mutations, Mutate.
}
