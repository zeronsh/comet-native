//! End-to-end vault control-plane exercise against a REAL edge worker
//! (`wrangler dev --var AUTH_MODE:dev`): two devices set up / pair / seal /
//! open / revoke / rotate, and a third recovers with the kit. The edge holds
//! only ciphertext and public records throughout; every trust decision is
//! made client-side against local pins.
//!
//! Run with:
//!   (cd edge && npx wrangler dev --port 27640 --var AUTH_MODE:dev --local)
//!   ZERON_VAULT_EDGE_URL=http://127.0.0.1:27640 cargo test -p zeron-engine --test vault_e2e
//!
//! Without the env var the test is skipped (no network in unit CI).

use std::sync::Arc;

use zeron_crypto::content::{self, ContentPurpose};
use zeron_crypto::record::UnverifiedRecord;
use zeron_engine::doc_host::EdgeConfig;
use zeron_engine::vault::client::VaultClient;
use zeron_engine::vault::{MemoryProtection, VaultPhase, VaultService, VaultStore, object_id_for};

fn edge_url() -> Option<String> {
    std::env::var("ZERON_VAULT_EDGE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
}

fn device(dir: &std::path::Path, edge: &str, org: &str, user: &str) -> VaultService {
    let bearer = format!("{user}@{org}");
    let config = EdgeConfig::with_static_token(edge, bearer);
    let client = VaultClient::new(reqwest::Client::new(), config, org);
    let store = VaultStore::new(
        dir,
        format!("{org}/{user}"),
        Box::new(MemoryProtection::new()),
    );
    VaultService::open(store, Some(client), org, user)
}

fn fresh_profile() -> (String, String) {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    (
        format!("org-{}", &nonce[..8]),
        format!("user-{}", &nonce[8..16]),
    )
}

#[tokio::test]
async fn two_devices_pair_seal_open_revoke_and_recover() {
    let Some(edge) = edge_url() else {
        eprintln!("ZERON_VAULT_EDGE_URL unset; skipping live vault e2e");
        return;
    };
    let (org, user) = fresh_profile();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();

    // ── A: nothing exists yet, set up ────────────────────────────────────
    let a = device(dir_a.path(), &edge, &org, &user);
    let status = a.refresh().await.unwrap();
    assert_eq!(
        status.phase,
        VaultPhase::NotEnrolled {
            remote_vault: false
        }
    );
    let kit = a.setup().await.unwrap();
    assert!(a.is_ready(), "{:?}", a.status().phase);
    assert_eq!(kit.kit.split('-').count(), 11);
    assert!(a.setup().await.is_err(), "second setup is refused");

    // ── B: pairs through the untrusted relay with a comparison code ─────
    let b = device(dir_b.path(), &edge, &org, &user);
    let status = b.refresh().await.unwrap();
    assert_eq!(status.phase, VaultPhase::NotEnrolled { remote_vault: true });
    assert!(
        b.setup().await.is_err(),
        "cannot create a second vault over an existing one"
    );
    let (request_id, code_on_b) = b.request_enrollment().await.unwrap();
    assert!(matches!(b.status().phase, VaultPhase::Pending { .. }));
    let pending = a.pending_requests().await.unwrap();
    let request = pending
        .iter()
        .find(|r| r.request_id == request_id)
        .expect("A sees B's request");
    assert_eq!(
        request.pairing_code, code_on_b,
        "both sides derive the same code"
    );
    // A wrong code (a relay that swapped keys) is refused.
    assert!(a.approve(&request_id, "0000-0000").await.is_err());
    a.approve(&request_id, &code_on_b).await.unwrap();
    // B learns of the approval on refresh and becomes Ready.
    let status = b.refresh().await.unwrap();
    assert_eq!(status.phase, VaultPhase::Ready, "{status:?}");
    assert_eq!(status.devices.len(), 2);

    // ── A seals, B opens (object key published through the control plane)
    let object = object_id_for("chat", "chat-e2e");
    let material = a.seal_material(object).await.unwrap();
    let sealed = content::seal(
        &material.binding,
        ContentPurpose::ChatUpdate,
        &material.key,
        &material.signer,
        b"private canary from A",
        1024,
    )
    .unwrap();
    let untrusted = *UnverifiedRecord::parse(sealed.encoded(), 2048)
        .unwrap()
        .untrusted_binding();
    let context = b.open_material(object, &untrusted).await.unwrap();
    let opened = content::open(
        sealed.encoded(),
        &context.binding,
        ContentPurpose::ChatUpdate,
        &context.key,
        &context.author_public_key,
        1024,
    )
    .unwrap();
    assert_eq!(opened.plaintext().as_bytes(), b"private canary from A");
    // Both writers converge on ONE key per object/epoch (first writer wins).
    let material_b = b.seal_material(object).await.unwrap();
    assert_eq!(material_b.key.identifier(), material.key.identifier());

    // ── A revokes B: fresh epoch; B is out, A still seals under epoch 2 ──
    let b_id = b.status().device_id.clone().unwrap();
    a.revoke(&b_id).await.unwrap();
    let status = a.status();
    assert_eq!(status.epoch, Some(2));
    let status = b.refresh().await.unwrap();
    assert_eq!(status.phase, VaultPhase::Revoked, "{status:?}");
    let material2 = a.seal_material(object).await.unwrap();
    assert_eq!(material2.binding.epoch, 2);
    assert_ne!(
        material2.key.identifier(),
        material.key.identifier(),
        "new epoch, new object key"
    );
    let sealed2 = content::seal(
        &material2.binding,
        ContentPurpose::ChatUpdate,
        &material2.key,
        &material2.signer,
        b"after revocation",
        1024,
    )
    .unwrap();
    let untrusted2 = *UnverifiedRecord::parse(sealed2.encoded(), 2048)
        .unwrap()
        .untrusted_binding();
    // B (revoked) cannot obtain epoch-2 material; its historical epoch-1
    // material still opens the earlier record (accepted history).
    assert!(b.open_material(object, &untrusted2).await.is_err());
    assert!(b.open_material(object, &untrusted).await.is_ok());

    // ── C recovers with the kit (no existing device involved) ───────────
    let c = device(dir_c.path(), &edge, &org, &user);
    assert!(
        c.recover(
            "AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA-AAAAA",
            None
        )
        .await
        .is_err()
    );
    let genesis = kit.recovery_file["genesisHash"].as_str().map(|h| {
        zeron_engine::vault::store::Hex(h.to_string())
            .decode::<32>()
            .unwrap()
    });
    c.recover(&kit.kit, genesis).await.unwrap();
    assert!(c.is_ready(), "{:?}", c.status().phase);
    assert_eq!(c.status().epoch, Some(3), "recovery is a fresh epoch");
    // C holds history: opens A's epoch-1 and epoch-2 records.
    let context = c.open_material(object, &untrusted2).await.unwrap();
    let opened = content::open(
        sealed2.encoded(),
        &context.binding,
        ContentPurpose::ChatUpdate,
        &context.key,
        &context.author_public_key,
        1024,
    )
    .unwrap();
    assert_eq!(opened.plaintext().as_bytes(), b"after revocation");
    assert!(c.open_material(object, &untrusted).await.is_ok());
    // A catches up to epoch 3 through the recovery envelope C published.
    let status = a.refresh().await.unwrap();
    assert_eq!(status.phase, VaultPhase::Ready, "{status:?}");
    assert_eq!(status.epoch, Some(3));
    let material3 = a.seal_material(object).await.unwrap();
    assert_eq!(material3.binding.epoch, 3);

    // ── Persistence: reopening C's store restores trust without the network
    let c_again = device(dir_c.path(), &edge, &org, &user);
    let _ = c_again; // fresh MemoryProtection cannot open the file → Locked, never plaintext
    let locked = VaultService::open(
        VaultStore::new(
            dir_c.path(),
            format!("{org}/{user}"),
            Box::new(MemoryProtection::new()),
        ),
        None,
        &org,
        &user,
    );
    assert!(matches!(
        locked.status().phase,
        VaultPhase::Unavailable { .. } | VaultPhase::Locked { .. }
    ));
    drop(Arc::new(()));
}

/// Registry field envelopes (RFC 0001 §9) through the live control plane:
/// one object key per epoch for the whole registry, values bound to their
/// row/field/clock, deletion markers, and key-unavailable withholding.
#[tokio::test]
async fn registry_fields_seal_open_and_bind_their_slot() {
    use zeron_engine::vault::VaultRegistryCodec;
    use zeron_sync::{FieldOpenFailure, RegistryCodec};

    let Some(edge) = edge_url() else {
        eprintln!("ZERON_VAULT_EDGE_URL unset; skipping live vault e2e");
        return;
    };
    let (org, user) = fresh_profile();
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = device(dir_a.path(), &edge, &org, &user);
    a.refresh().await.unwrap();
    let _kit = a.setup().await.unwrap();
    let b = device(dir_b.path(), &edge, &org, &user);
    b.refresh().await.unwrap();
    let (request_id, code) = b.request_enrollment().await.unwrap();
    a.approve(&request_id, &code).await.unwrap();
    b.refresh().await.unwrap();
    assert!(b.is_ready());

    let codec_a = VaultRegistryCodec::new(a.clone(), &user);
    let codec_b = VaultRegistryCodec::new(b.clone(), &user);
    // Sealing before `prepare` is refused (no half-sealed batches).
    assert!(
        codec_a
            .seal_field(
                "chats",
                "c1",
                "title",
                "1-000000-a",
                &serde_json::json!("t")
            )
            .is_err()
    );
    codec_a.prepare().await.unwrap();
    let title = codec_a
        .seal_field(
            "chats",
            "c1",
            "title",
            "0000000000001-000000-a",
            &serde_json::json!("secret title"),
        )
        .unwrap();
    let deleted = codec_a
        .seal_field(
            "chats",
            "c1",
            "branch",
            "0000000000002-000000-a",
            &serde_json::Value::Null,
        )
        .unwrap();
    let wire = serde_json::to_string(&title).unwrap();
    assert!(
        !wire.contains("secret title"),
        "plaintext on the wire: {wire}"
    );
    assert!(title.get("e1").is_some());

    // B opens both after fetching the object key from the control plane.
    let first = codec_b.open_field("chats", "c1", "title", "0000000000001-000000-a", &title);
    if first == Err(FieldOpenFailure::KeyUnavailable) {
        // The synchronous path spawned a key fetch; wait for it.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if codec_b
                .open_field("chats", "c1", "title", "0000000000001-000000-a", &title)
                .is_ok()
            {
                break;
            }
        }
    }
    assert_eq!(
        codec_b.open_field("chats", "c1", "title", "0000000000001-000000-a", &title),
        Ok(Some(serde_json::json!("secret title")))
    );
    assert_eq!(
        codec_b.open_field("chats", "c1", "branch", "0000000000002-000000-a", &deleted),
        Ok(None),
        "an authenticated deletion marker opens as absence"
    );
    // Moved between fields, rows, or clocks: rejected, never displayed.
    assert_eq!(
        codec_b.open_field("chats", "c1", "cwd", "0000000000001-000000-a", &title),
        Err(FieldOpenFailure::Rejected)
    );
    assert_eq!(
        codec_b.open_field("chats", "c2", "title", "0000000000001-000000-a", &title),
        Err(FieldOpenFailure::Rejected)
    );
    assert_eq!(
        codec_b.open_field("chats", "c1", "title", "0000000000009-000000-a", &title),
        Err(FieldOpenFailure::Rejected)
    );
    // Plaintext where ciphertext is required is rejected too.
    assert_eq!(
        codec_b.open_field(
            "chats",
            "c1",
            "title",
            "0000000000001-000000-a",
            &serde_json::json!("plain")
        ),
        Err(FieldOpenFailure::Rejected)
    );
}
