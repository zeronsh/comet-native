//! Registry field encryption (RFC 0001 §9): each field VALUE becomes a
//! content record (purpose RegistryField) whose authenticated plaintext also
//! names the row kind, row id, field, and original clock, so the server can
//! keep merging by row/field/HLC while a relay can neither read a value nor
//! move one between slots. Every field of the profile's registry shares one
//! object (`registry`) so the control plane holds one key per epoch, not
//! one per field.
//!
//! Wire value: `{"e1": "<base64 sealed record>"}`. A plaintext value where a
//! sealed one is required is REJECTED (never displayed): an encrypted profile
//! reads only its encrypted registry generation.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use zeron_crypto::content::{self, ContentPurpose};
use zeron_crypto::record::UnverifiedRecord;
use zeron_sync::{FieldOpenFailure, RegistryCodec};

use super::{ChatKeyMaterial, OpenFailure, VaultService, object_id_for};
use crate::EngineError;

/// Registry field plaintext cap: the edge's 16 KiB per-op budget minus the
/// record overhead and base64 expansion, spread over a row's fields.
const MAX_FIELD_PLAINTEXT: usize = 8 * 1024;
const WIRE_KEY: &str = "e1";

#[derive(Serialize, Deserialize)]
struct FieldPlaintext<'a> {
    kind: &'a str,
    id: &'a str,
    field: &'a str,
    hlc: &'a str,
    value: serde_json::Value,
}

pub struct VaultRegistryCodec {
    vault: VaultService,
    object_id: [u8; 16],
    material: Mutex<Option<Arc<ChatKeyMaterial>>>,
}

impl VaultRegistryCodec {
    pub fn new(vault: VaultService, user_id: &str) -> Self {
        Self {
            vault,
            object_id: object_id_for("registry", user_id),
            material: Mutex::new(None),
        }
    }

    pub fn vault(&self) -> &VaultService {
        &self.vault
    }

    /// Obtain (or refresh after an epoch change) the sealing material. The
    /// object key becomes durable on the control plane before this returns.
    pub async fn prepare(&self) -> Result<(), EngineError> {
        let material = self.vault.seal_material(self.object_id).await?;
        *lock(&self.material) = Some(Arc::new(material));
        Ok(())
    }

    /// Drop cached material (vault left Ready): sealing pauses until the
    /// next successful `prepare`.
    pub fn clear(&self) {
        *lock(&self.material) = None;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl RegistryCodec for VaultRegistryCodec {
    fn seal_field(
        &self,
        kind: &str,
        id: &str,
        field: &str,
        hlc: &str,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let material = lock(&self.material)
            .clone()
            .ok_or_else(|| "vault keys not ready".to_string())?;
        // The sealing binding must still be the head: a stale head would
        // author under a superseded revision (readers accept it as history,
        // but a rotation must move new writes forward).
        if self.vault.current_content_binding(self.object_id) != Some(material.binding) {
            return Err("vault epoch changed; re-preparing".into());
        }
        let plaintext = serde_json::to_vec(&FieldPlaintext {
            kind,
            id,
            field,
            hlc,
            value: value.clone(),
        })
        .map_err(|e| e.to_string())?;
        if plaintext.len() > MAX_FIELD_PLAINTEXT {
            return Err(format!("registry field {field} exceeds the sealed budget"));
        }
        let sealed = content::seal(
            &material.binding,
            ContentPurpose::RegistryField,
            &material.key,
            &material.signer,
            &plaintext,
            MAX_FIELD_PLAINTEXT,
        )
        .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ WIRE_KEY: super::client::encode_base64(sealed.encoded()) }))
    }

    fn open_field(
        &self,
        kind: &str,
        id: &str,
        field: &str,
        hlc: &str,
        wire: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, FieldOpenFailure> {
        let Some(encoded) = wire
            .get(WIRE_KEY)
            .and_then(|v| v.as_str())
            .and_then(super::client::decode_base64)
        else {
            return Err(FieldOpenFailure::Rejected);
        };
        let parsed = UnverifiedRecord::parse(&encoded, MAX_FIELD_PLAINTEXT + 144)
            .map_err(|_| FieldOpenFailure::Rejected)?;
        let binding = *parsed.untrusted_binding();
        let context = self
            .vault
            .open_material_cached(self.object_id, &binding)
            .map_err(|failure| match failure {
                OpenFailure::Unavailable | OpenFailure::KeyUnavailable => {
                    self.vault.spawn_key_refresh(self.object_id);
                    FieldOpenFailure::KeyUnavailable
                }
                OpenFailure::NotAuthorized => FieldOpenFailure::Rejected,
            })?;
        let opened = content::open(
            &encoded,
            &context.binding,
            ContentPurpose::RegistryField,
            &context.key,
            &context.author_public_key,
            MAX_FIELD_PLAINTEXT,
        )
        .map_err(|_| FieldOpenFailure::Rejected)?;
        let plaintext: FieldPlaintext = serde_json::from_slice(opened.plaintext().as_bytes())
            .map_err(|_| FieldOpenFailure::Rejected)?;
        // The authenticated slot must be the slot the server filed it under.
        if plaintext.kind != kind
            || plaintext.id != id
            || plaintext.field != field
            || plaintext.hlc != hlc
        {
            return Err(FieldOpenFailure::Rejected);
        }
        Ok(if plaintext.value.is_null() {
            None
        } else {
            Some(plaintext.value)
        })
    }
}
