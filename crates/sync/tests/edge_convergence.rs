//! Live-edge integration test (M1 exit criterion): two Rust `RoomClient`s
//! converge through a real SessionRoom Durable Object.
//!
//! Ignored by default — requires the TS edge running (e.g. `wrangler dev` in
//! `edge/` with AUTH_MODE=dev). Run with:
//!
//! ```sh
//! COMET_EDGE_WS=ws://127.0.0.1:8787 cargo test -p comet-sync -- --ignored
//! ```
//!
//! `COMET_EDGE_TOKEN` overrides the dev bearer (defaults to `sync-it-user`;
//! both clients must share it — chat rooms are claim-on-first-join owned).

use std::time::Duration;

use comet_doc::{MessagePart, MessageRole, SessionDoc, SessionMessageEntry};
use comet_sync::RoomClient;

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("condition not reached in time");
}

fn text_message(id: &str, device: &str, text: &str) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.to_string(),
        role: MessageRole::User,
        parts: vec![MessagePart::Text {
            id: format!("{id}-p0"),
            text: text.to_string(),
        }],
        created_at: 1,
        device_id: device.to_string(),
        status: None,
        continuation_of: None,
    }
}

#[tokio::test]
#[ignore = "requires a live edge: set COMET_EDGE_WS (e.g. ws://127.0.0.1:8787)"]
async fn two_session_docs_converge_through_a_real_room() {
    let base = std::env::var("COMET_EDGE_WS")
        .expect("set COMET_EDGE_WS to the edge origin, e.g. ws://127.0.0.1:8787");
    let token = std::env::var("COMET_EDGE_TOKEN").unwrap_or_else(|_| "sync-it-user".to_string());
    let chat_id = format!("it-{}", uuid::Uuid::new_v4().simple());
    let url = format!("{base}/session/{chat_id}/ws?token={token}");

    // Host device initializes the doc and claims the room.
    let host = SessionDoc::init(&chat_id).expect("init session doc");
    let host_client = RoomClient::connect(&url, &chat_id, host.doc().clone())
        .await
        .expect("host connect");

    // Second device starts empty and backfills from the room.
    let peer_doc = loro::LoroDoc::new();
    let peer_client = RoomClient::connect(&url, &chat_id, peer_doc.clone())
        .await
        .expect("peer connect");
    let peer = SessionDoc::from_doc(peer_doc);
    wait_until(|| peer.chat_id().as_deref() == Some(chat_id.as_str())).await;

    // Host → peer.
    host.push_message(&text_message("m1", "device-host", "hello from host"))
        .expect("push m1");
    wait_until(|| {
        peer.read_entries()
            .map(|e| e.iter().any(|m| m.id == "m1"))
            .unwrap_or(false)
    })
    .await;

    // Peer → host.
    peer.push_message(&text_message("m2", "device-peer", "hello from peer"))
        .expect("push m2");
    wait_until(|| {
        host.read_entries()
            .map(|e| e.iter().any(|m| m.id == "m2"))
            .unwrap_or(false)
    })
    .await;

    // Full deep-value convergence.
    use loro::ToJson;
    wait_until(|| {
        host.doc().get_deep_value().to_json_value() == peer.doc().get_deep_value().to_json_value()
    })
    .await;

    // Presence relays through %EPH.
    host_client.ephemeral().set("device:host", "online");
    wait_until(|| peer_client.ephemeral().get("device:host") == Some("online".into())).await;

    host_client.shutdown().await.expect("host shutdown");
    peer_client.shutdown().await.expect("peer shutdown");
}
