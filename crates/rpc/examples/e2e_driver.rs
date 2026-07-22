//! Two-device e2e smoke driver (`scripts/e2e-smoke.sh` runs it).
//!
//! Connects to two running headless engines over their localhost IPC ports and proves
//! the cross-device command plane end to end against a real edge:
//!
//! 1. `LocalDevice` on both — distinct device ids;
//! 2. `Mutate createChat` on A (A hosts the chat, mock harness config);
//! 3. waits until the chat row syncs A → edge → B (`WatchChats` on B);
//! 4. `QueueCommand` a Run on **B** — the doc path: B commits the command into its
//!    replica, nudges A's device room, A's host executor drains and runs it;
//! 5. waits on **B** until the assistant entry lands `complete` with the mock
//!    transcript (A → edge → B), and the session-status row round-trips.
//!
//! Prints `PASS`/`FAIL` lines; exits nonzero on failure.

use std::time::{Duration, Instant};

use comet_rpc::{RpcClient, connect_ws, methods};

const STEP_TIMEOUT: Duration = Duration::from_secs(90);
const MOCK_TEXT: &str = "Mock harness reporting in.";

fn fail(message: &str) -> ! {
    eprintln!("FAIL: {message}");
    std::process::exit(1);
}

fn pass(message: &str) {
    println!("PASS: {message}");
}

async fn device_id(client: &RpcClient, label: &str) -> String {
    match client
        .call(methods::LOCAL_DEVICE, serde_json::json!({}))
        .await
    {
        Ok(value) => value
            .get("deviceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| fail(&format!("{label}: LocalDevice reply missing deviceId"))),
        Err(err) => fail(&format!("{label}: LocalDevice call failed: {err}")),
    }
}

/// Subscribe to a watch-stream and poll items until `predicate` returns `Some`.
async fn wait_stream<T>(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
    what: &str,
    predicate: impl Fn(&serde_json::Value) -> Option<T>,
) -> T {
    let mut rx = match client.subscribe(method, params).await {
        Ok(rx) => rx,
        Err(err) => fail(&format!("{what}: subscribe {method} failed: {err}")),
    };
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            fail(&format!(
                "{what}: timed out after {}s",
                STEP_TIMEOUT.as_secs()
            ));
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(item)) => {
                if let Some(found) = predicate(&item) {
                    return found;
                }
            }
            Ok(None) => fail(&format!("{what}: stream ended early")),
            Err(_) => fail(&format!(
                "{what}: timed out after {}s",
                STEP_TIMEOUT.as_secs()
            )),
        }
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let a_port: u16 = args
        .next()
        .unwrap_or_else(|| "27801".into())
        .parse()
        .expect("A port");
    let b_port: u16 = args
        .next()
        .unwrap_or_else(|| "27802".into())
        .parse()
        .expect("B port");

    let a = connect_ws(&format!("ws://127.0.0.1:{a_port}"))
        .await
        .unwrap_or_else(|err| fail(&format!("connect device A ipc :{a_port}: {err}")));
    let b = connect_ws(&format!("ws://127.0.0.1:{b_port}"))
        .await
        .unwrap_or_else(|err| fail(&format!("connect device B ipc :{b_port}: {err}")));

    // 1. Distinct identities.
    let a_dev = device_id(&a, "A").await;
    let b_dev = device_id(&b, "B").await;
    if a_dev == b_dev {
        fail("A and B report the same deviceId — data dirs are shared");
    }
    pass(&format!("devices distinct (A={a_dev} B={b_dev})"));

    // 2. Create a space + chat on A, hosted by A, with the mock harness (the
    //    space fixes device + cwd — the chat row derives both from it).
    let space_id = uuid::Uuid::new_v4().to_string();
    a.call(
        methods::MUTATE,
        serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": a_dev,
            "path": "/tmp",
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("createSpace on A: {err}")));
    let chat_id = uuid::Uuid::new_v4().to_string();
    a.call(
        methods::MUTATE,
        serde_json::json!({
            "op": "createChat",
            "chatId": chat_id,
            "spaceId": space_id,
            "config": {
                "harness": "mock",
                "model": null,
                "reasoning": null,
                "sandbox": "workspace-write",
            },
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("createChat on A: {err}")));
    pass(&format!("space + chat created on A ({chat_id})"));

    // 2b. Space row syncs A → edge → B (WatchSpaces).
    let space_device = wait_stream(
        &b,
        methods::WATCH_SPACES,
        serde_json::json!({}),
        "space row visible on B",
        |item| {
            item.as_array()?
                .iter()
                .find(|space| space.get("id").and_then(|v| v.as_str()) == Some(space_id.as_str()))
                .and_then(|space| space.get("deviceId")?.as_str().map(str::to_string))
        },
    )
    .await;
    if space_device != a_dev {
        fail(&format!(
            "space synced to B but owned by {space_device}, expected {a_dev}"
        ));
    }
    pass("space row synced A -> edge -> B (owner = A)");

    // 3. Workspace sync A → edge → B: the chat row appears in B's WatchChats.
    let hosted_by = wait_stream(
        &b,
        methods::WATCH_CHATS,
        serde_json::json!({}),
        "chat row visible on B",
        |item| {
            item.as_array()?
                .iter()
                .find(|chat| chat.get("id").and_then(|v| v.as_str()) == Some(chat_id.as_str()))
                .and_then(|chat| chat.get("deviceId")?.as_str().map(str::to_string))
        },
    )
    .await;
    if hosted_by != a_dev {
        fail(&format!(
            "chat synced to B but hosted by {hosted_by}, expected {a_dev}"
        ));
    }
    pass("chat row synced A -> edge -> B (host = A)");

    // 4. Queue the run from B THROUGH THE DOC (no targetDeviceId): B commits the
    //    command into its replica; the nudge + room sync hand it to A's executor.
    let message_id = uuid::Uuid::new_v4().to_string();
    b.call(
        methods::QUEUE_COMMAND,
        serde_json::json!({
            "chatId": chat_id,
            "command": {
                "kind": "run",
                "messageId": message_id,
                "request": {
                    "prompt": "e2e smoke: report in",
                    "model": null,
                    "reasoning": null,
                    "cwd": "/tmp",
                    "sandbox": "workspace-write",
                    "autoApprove": true,
                    "resume": null,
                },
            },
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("QueueCommand on B: {err}")));
    pass("run command queued on B via the doc command queue");

    // 5a. Assistant entry executed by A arrives back on B, complete, with the mock text.
    let (by_device, text) = wait_stream(
        &b,
        methods::WATCH_DOC_MESSAGES,
        serde_json::json!({ "chatId": chat_id }),
        "assistant transcript on B",
        |item| {
            let entry = item.as_array()?.iter().find(|entry| {
                entry.get("role").and_then(|v| v.as_str()) == Some("assistant")
                    && entry.get("status").and_then(|v| v.as_str()) == Some("complete")
            })?;
            let device = entry.get("deviceId")?.as_str()?.to_string();
            let text = entry.to_string();
            Some((device, text))
        },
    )
    .await;
    if by_device != a_dev {
        fail(&format!(
            "assistant entry written by {by_device}, expected host {a_dev}"
        ));
    }
    if !text.contains(MOCK_TEXT) {
        fail(&format!(
            "assistant entry lacks the mock transcript: {text}"
        ));
    }
    pass("assistant entry (host-executed, status=complete) synced back to B");

    // 5b. Session-status row round-trips: B sees A's session for this chat go idle.
    wait_stream(
        &b,
        methods::WATCH_SESSIONS,
        serde_json::json!({}),
        "session status on B",
        |item| {
            item.as_array()?.iter().find(|session| {
                session.get("chatId").and_then(|v| v.as_str()) == Some(chat_id.as_str())
                    && session.get("deviceId").and_then(|v| v.as_str()) == Some(a_dev.as_str())
                    && session.get("status").and_then(|v| v.as_str()) == Some("idle")
            })?;
            Some(())
        },
    )
    .await;
    pass("session status (A working -> idle) round-tripped to B");

    println!("PASS: two-device e2e smoke complete");
}
