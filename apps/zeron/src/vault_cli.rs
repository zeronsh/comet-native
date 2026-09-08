//! `zeron vault …` — the headless surface for encrypted sync (RFC 0001 §4.5):
//! status, setup with a recovery kit, pairing by comparison code, approving
//! and removing devices, and recovery. Every command talks to the RUNNING
//! engine over localhost IPC, so the keys stay inside the engine process;
//! nothing here prints or accepts private key material, only the recovery
//! kit the user is asked to save.

use std::io::{IsTerminal, Read, Write};

use serde_json::Value;
use zeron_rpc::methods;

async fn client(ipc_port: u16) -> anyhow::Result<zeron_rpc::RpcClient> {
    zeron_rpc::connect_ws(&format!("ws://127.0.0.1:{ipc_port}"))
        .await
        .map_err(|e| {
            anyhow::anyhow!("no engine listening on 127.0.0.1:{ipc_port} ({e}) — is zeron running?")
        })
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn print_status(status: &Value) {
    let phase = field(status, "phase");
    let line = match phase {
        "ready" => "encrypted (this device is approved)".to_string(),
        "notEnrolled" => {
            if status.get("remoteVault").and_then(Value::as_bool) == Some(true) {
                "not approved — run `zeron vault pair` on this device, then approve it elsewhere, or `zeron vault recover`".into()
            } else {
                "not set up — run `zeron vault setup`".into()
            }
        }
        "pending" => format!(
            "waiting for approval — compare code {} on an approved device",
            field(status, "pairingCode")
        ),
        "locked" => format!("locked — {}", field(status, "reason")),
        "keyUpdateRequired" => "waiting for encryption keys from another device".into(),
        "verificationFailed" => format!("sync paused — {}", field(status, "reason")),
        "revoked" => "removed from the vault".into(),
        "unavailable" => format!("not available — {}", field(status, "reason")),
        other => other.to_string(),
    };
    println!("Encryption: {line}");
    if let Some(fingerprint) = status.get("genesisHash").and_then(Value::as_str) {
        println!("Vault fingerprint: {fingerprint}");
    }
    if let Some(epoch) = status.get("epoch").and_then(Value::as_u64) {
        println!("Key epoch:  {epoch}");
    }
    println!(
        "Key store:  {}",
        match field(status, "protection") {
            "keychain" => "macOS Keychain",
            "systemdCredential" => "systemd credential (unattended)",
            "keyFile" => "ZERON_VAULT_KEY_FILE (unattended)",
            _ => "none",
        }
    );
    if let Some(devices) = status.get("devices").and_then(Value::as_array)
        && !devices.is_empty()
    {
        println!("Devices:");
        for device in devices {
            println!(
                "  {} {}{}",
                field(device, "deviceId"),
                field(device, "status"),
                if device.get("thisDevice").and_then(Value::as_bool) == Some(true) {
                    " (this device)"
                } else {
                    ""
                }
            );
        }
    }
}

pub async fn status(ipc_port: u16) -> anyhow::Result<()> {
    let client = client(ipc_port).await?;
    let status = client
        .call(methods::VAULT_REFRESH, serde_json::json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("VaultRefresh failed: {e}"))?;
    print_status(&status);
    Ok(())
}

pub async fn confirm_recovery(ipc_port: u16) -> anyhow::Result<()> {
    client(ipc_port)
        .await?
        .call(methods::VAULT_CONFIRM_RECOVERY, serde_json::json!({}))
        .await?;
    println!("Recovery kit confirmed. The vault is ready.");
    Ok(())
}

pub async fn setup(ipc_port: u16) -> anyhow::Result<()> {
    let client = client(ipc_port).await?;
    let kit = client
        .call(methods::VAULT_SETUP, serde_json::json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("setup failed: {e}"))?;
    println!(
        "Vault prepared. Encrypted writes remain paused until you save and confirm the recovery kit.\n"
    );
    println!("Recovery key (save it in a password manager now):\n");
    println!("    {}\n", field(&kit, "kit"));
    println!("Recovery file (save alongside the key):\n");
    println!(
        "{}\n",
        serde_json::to_string_pretty(kit.get("recoveryFile").unwrap_or(&Value::Null))?
    );
    println!(
        "If you lose every approved device and your recovery key, we cannot recover your \
         encrypted data. Resetting your account password will not restore access.\n\n\
         After saving both, run `zeron vault confirm-recovery`."
    );
    Ok(())
}

/// Request approval and wait (polling) until an approved device decides.
pub async fn pair(ipc_port: u16) -> anyhow::Result<()> {
    let client = client(ipc_port).await?;
    let request = client
        .call(methods::VAULT_REQUEST_ENROLLMENT, serde_json::json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("enrollment request failed: {e}"))?;
    println!(
        "Approve this device from an approved device (Settings → Encryption or `zeron vault approve`).\n\
         Comparison code: {}\n\
         Approve only if the other device shows exactly this code. Waiting…",
        field(&request, "pairingCode")
    );
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let status = client
            .call(methods::VAULT_REFRESH, serde_json::json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("VaultRefresh failed: {e}"))?;
        match field(&status, "phase") {
            "pending" => continue,
            "ready" => {
                println!("Approved. Encrypted sync is active on this device.");
                return Ok(());
            }
            other => {
                anyhow::bail!("the request ended without approval (state: {other})");
            }
        }
    }
}

pub async fn requests(ipc_port: u16) -> anyhow::Result<()> {
    let client = client(ipc_port).await?;
    let list = client
        .call(methods::VAULT_PENDING_REQUESTS, serde_json::json!({}))
        .await
        .map_err(|e| anyhow::anyhow!("VaultPendingRequests failed: {e}"))?;
    let requests = list
        .get("requests")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if requests.is_empty() {
        println!("No devices are waiting for approval.");
        return Ok(());
    }
    for request in requests {
        println!(
            "{}  device {}  code {}",
            field(&request, "requestId"),
            field(&request, "deviceId"),
            field(&request, "pairingCode")
        );
    }
    println!("\nApprove with: zeron vault approve <requestId> <code-shown-on-the-new-device>");
    Ok(())
}

pub async fn approve(ipc_port: u16, request_id: &str, code: &str) -> anyhow::Result<()> {
    let client = client(ipc_port).await?;
    println!(
        "This grants the device full access to every synced session, file and workspace detail, \
         and the ability to approve or remove other devices."
    );
    client
        .call(
            methods::VAULT_APPROVE,
            serde_json::json!({ "requestId": request_id, "code": code }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("approval failed: {e}"))?;
    println!("Approved.");
    Ok(())
}

pub async fn reject(ipc_port: u16, request_id: &str) -> anyhow::Result<()> {
    let client = client(ipc_port).await?;
    client
        .call(
            methods::VAULT_REJECT,
            serde_json::json!({ "requestId": request_id }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("reject failed: {e}"))?;
    println!("Rejected.");
    Ok(())
}

pub async fn revoke(ipc_port: u16, device_id: &str) -> anyhow::Result<()> {
    let client = client(ipc_port).await?;
    client
        .call(
            methods::VAULT_REVOKE,
            serde_json::json!({ "deviceId": device_id }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("revoke failed: {e}"))?;
    println!(
        "Removed. This stops future sync access after the change takes effect. It cannot erase \
         information the device already downloaded."
    );
    Ok(())
}

/// Recover with the kit: read it from the argument, or from stdin (never a
/// command-line secret in shell history when stdin is available).
pub async fn recover(ipc_port: u16, kit: Option<String>) -> anyhow::Result<()> {
    let kit = match kit {
        Some(kit) => kit,
        None => {
            if std::io::stdin().is_terminal() {
                print!("Recovery key: ");
                std::io::stdout().flush()?;
            }
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            text
        }
    };
    let kit = kit.trim().to_string();
    if kit.is_empty() {
        anyhow::bail!("no recovery key given");
    }
    let client = client(ipc_port).await?;
    client
        .call(methods::VAULT_RECOVER, serde_json::json!({ "kit": kit }))
        .await
        .map_err(|e| anyhow::anyhow!("recovery failed: {e}"))?;
    println!("Recovered. This device is approved under a fresh key epoch.");
    Ok(())
}
