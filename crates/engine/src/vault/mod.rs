//! Per-profile encrypted-sync vault (RFC 0001): device identity and key
//! persistence (`store`), the edge control-plane client (`client`), and the
//! lifecycle service that verifies membership, holds keys, and hands sealing
//! / opening material to the content transports (`service`).
//!
//! Nothing here activates encrypted transport by itself: the service exposes
//! explicit states and the transports consult it before serializing content.

pub mod client;
pub mod service;
pub mod store;

pub use service::{
    ChatKeyMaterial, OpenContext, OpenFailure, RecoveryKit, VaultDevice, VaultPhase, VaultService,
    VaultStatus, object_id_for,
};
pub use store::{
    LockedProtection, MemoryProtection, ProtectionKeyProvider, ProtectionMode, VaultStore,
    VaultStoreError, platform_protection,
};
