use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeron_crypto::{SecretBytes, keyring::Keyring, record::UnverifiedRecord};
use zeron_engine::doc_host::EdgeConfig;
use zeron_engine::vault::client::{VaultClient, encode_base64};
use zeron_engine::vault::store::{LocalVaultState, VaultStoreError};
use zeron_engine::vault::{
    ProtectionKeyProvider, ProtectionMode, VaultPhase, VaultService, VaultStore,
};

struct Protection;

impl ProtectionKeyProvider for Protection {
    fn mode(&self) -> ProtectionMode {
        ProtectionMode::Memory
    }
    fn load_or_create(&self, _: &str) -> Result<SecretBytes, VaultStoreError> {
        Ok(SecretBytes::from_slice(&[42; 32]))
    }
}

fn store(path: &Path) -> VaultStore {
    VaultStore::new(path, "org/user", Box::new(Protection))
}

struct ServerState {
    path: PathBuf,
    records: Vec<Vec<u8>>,
    attempts: Vec<Vec<u8>>,
    journal_observed: Vec<bool>,
    envelope_attempts: Vec<(String, Vec<u8>)>,
    accept_membership: bool,
    accept_envelopes: bool,
    lose_reply: bool,
}

impl ServerState {
    fn response(&mut self, method: &str, path: &str, body: &[u8]) -> (u16, serde_json::Value) {
        if method == "POST" && path == "/vault/org/membership" {
            let local = store(&self.path).load().unwrap();
            let epoch = UnverifiedRecord::parse(body, 65536)
                .unwrap()
                .untrusted_binding()
                .epoch;
            let held = local
                .keyring
                .as_ref()
                .and_then(|key| key.bytes())
                .and_then(|bytes| Keyring::decode(&bytes).ok())
                .is_some_and(|keyring| keyring.epoch_key(epoch).is_some());
            self.journal_observed.push(
                local.pending_membership.as_deref() == Some(encode_base64(body).as_str()) && held,
            );
            self.attempts.push(body.to_vec());
            if !self.accept_membership {
                return (
                    503,
                    serde_json::json!({"ok": false, "error": "unavailable"}),
                );
            }
            if self.records.last().is_some_and(|last| last == body) {
                return (
                    409,
                    serde_json::json!({"ok": false, "error": "stale_parent"}),
                );
            }
            self.records.push(body.to_vec());
            if self.lose_reply {
                self.lose_reply = false;
                return (503, serde_json::json!({"ok": false, "error": "lost_ack"}));
            }
            return (200, serde_json::json!({"ok": true}));
        }
        if method == "GET" && path.starts_with("/vault/org/membership?") {
            let after: i64 = path.split("after=").nth(1).unwrap().parse().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .skip((after + 1) as usize)
                .map(|r| encode_base64(r))
                .collect();
            return (
                200,
                serde_json::json!({"records": records, "headSeq": self.records.len() as i64 - 1, "truncated": false}),
            );
        }
        if method == "PUT" && path.starts_with("/vault/org/envelopes/") {
            self.envelope_attempts
                .push((path.to_string(), body.to_vec()));
            return if self.accept_envelopes {
                (200, serde_json::json!({"ok": true}))
            } else {
                (
                    503,
                    serde_json::json!({"ok": false, "error": "unavailable"}),
                )
            };
        }
        (404, serde_json::json!({"error": "not_found"}))
    }
}

struct Server {
    url: String,
    state: Arc<Mutex<ServerState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(path: &Path) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(ServerState {
            path: path.to_path_buf(),
            records: Vec::new(),
            attempts: Vec::new(),
            journal_observed: Vec::new(),
            envelope_attempts: Vec::new(),
            accept_membership: true,
            accept_envelopes: true,
            lose_reply: false,
        }));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 4096];
                let header_end = loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    assert!(bytes.len() <= 128 * 1024);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                let mut request = headers.lines().next().unwrap().split_whitespace();
                let method = request.next().unwrap();
                let path = request.next().unwrap();
                let length = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                    .unwrap_or(0);
                assert!(length <= 128 * 1024);
                while bytes.len() < header_end + length {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                }
                let (status, body) = shared.lock().unwrap().response(
                    method,
                    path,
                    &bytes[header_end..header_end + length],
                );
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self { url, state, task }
    }

    fn vault(&self, path: &Path) -> VaultService {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let client = VaultClient::new(
            http,
            EdgeConfig::with_static_token(&self.url, "test"),
            "org",
        );
        VaultService::open(store(path), Some(client), "org", "user")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn setup_restarts_with_identical_membership_and_requires_kit_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path()).await;
    server.state.lock().unwrap().accept_membership = false;
    let vault = server.vault(dir.path());
    assert!(vault.setup().await.is_err());
    assert!(vault.is_enrolled());
    assert!(!vault.is_ready());
    let staged = store(dir.path()).load().unwrap();
    assert!(staged.pending_membership.is_some());
    assert!(staged.setup_recovery.is_some());
    drop(vault);
    server.state.lock().unwrap().accept_membership = true;
    let vault = server.vault(dir.path());
    let kit = vault.setup().await.unwrap();
    assert_eq!(
        vault.status().phase,
        VaultPhase::RecoveryConfirmationRequired
    );
    assert!(!vault.is_ready());
    assert!(vault.seal_material([9; 16]).await.is_err());
    assert_eq!(vault.setup().await.unwrap().kit, kit.kit);
    {
        let state = server.state.lock().unwrap();
        assert_eq!(state.attempts[0], state.attempts[1]);
        assert!(state.journal_observed.iter().all(|seen| *seen));
    }
    vault.confirm_recovery_kit().await.unwrap();
    assert!(vault.is_ready());
    let saved = store(dir.path()).load().unwrap();
    assert!(saved.setup_recovery.is_none());
    assert!(saved.pending_membership.is_none());
    assert!(saved.owed_envelopes.is_empty());
    assert!(vault.setup().await.is_err());
}

#[tokio::test]
async fn accepted_membership_and_failed_envelopes_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path()).await;
    {
        let mut state = server.state.lock().unwrap();
        state.lose_reply = true;
        state.accept_envelopes = false;
    }
    let vault = server.vault(dir.path());
    assert!(vault.setup().await.is_err());
    let before: LocalVaultState = store(dir.path()).load().unwrap();
    assert!(before.pending_membership.is_none());
    assert_eq!(before.owed_envelopes.len(), 2);
    assert!(!vault.is_ready());
    drop(vault);
    server.state.lock().unwrap().accept_envelopes = true;
    let vault = server.vault(dir.path());
    let kit = vault.setup().await.unwrap();
    assert!(!kit.kit.is_empty());
    assert_eq!(server.state.lock().unwrap().records.len(), 1);
    assert_eq!(store(dir.path()).load().unwrap().keyring, before.keyring);
    vault.confirm_recovery_kit().await.unwrap();
    assert!(server.vault(dir.path()).is_ready());
    let state = server.state.lock().unwrap();
    assert_eq!(state.envelope_attempts.len(), 4);
    assert_eq!(state.envelope_attempts[0], state.envelope_attempts[2]);
    assert_eq!(state.envelope_attempts[1], state.envelope_attempts[3]);
}

#[tokio::test]
async fn rotation_key_is_durable_before_publication_and_survives_restart() {
    use zeron_crypto::{
        content::DeviceSigner,
        hpke::HpkePrivateKey,
        policy::{self, DeviceEntry, DeviceStatus, MembershipState, Operation},
    };
    use zeron_engine::vault::store::Hex;
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path()).await;
    let vault = server.vault(dir.path());
    vault.setup().await.unwrap();
    vault.confirm_recovery_kit().await.unwrap();
    let local = store(dir.path()).load().unwrap();
    let pin = local.vault.unwrap();
    let identity = local.device.unwrap();
    let genesis = server.state.lock().unwrap().records[0].clone();
    let head = MembershipState::from_genesis(
        &genesis,
        &pin.vault_id.decode().unwrap(),
        &pin.generation.decode().unwrap(),
        &policy::profile_hash("org", "user"),
    )
    .unwrap();
    let signer = DeviceSigner::from_seed(
        identity.device_id.decode().unwrap(),
        &identity.signing_seed.bytes().unwrap(),
    )
    .unwrap();
    let peer_id = [9; 16];
    let peer = DeviceSigner::from_seed(peer_id, &[7; 32]).unwrap();
    let encryption = HpkePrivateKey::from_bytes(&[8; 32]).unwrap();
    let mut payload = head.next_payload(Operation::AddDevice);
    payload.devices.push(DeviceEntry {
        device_id: peer_id,
        signing_key: peer.public_key().try_into().unwrap(),
        encryption_key: *encryption.public_key().as_bytes(),
        status: DeviceStatus::Active,
    });
    let binding = policy::policy_binding(
        *head.vault_id(),
        *head.generation(),
        payload.epoch,
        *signer.author_id(),
        *head.hash(),
    );
    let added = policy::encode_policy(&binding, &payload, &signer).unwrap();
    server.state.lock().unwrap().records.push(added);
    vault.refresh().await.unwrap();
    server.state.lock().unwrap().accept_membership = false;
    assert!(vault.revoke(&Hex::of(&peer_id).0).await.is_err());
    let staged = store(dir.path()).load().unwrap();
    assert!(staged.pending_membership.is_some());
    let ring = Keyring::decode(&staged.keyring.unwrap().bytes().unwrap()).unwrap();
    let key = ring.epoch_key(2).unwrap().to_vec();
    assert!(!vault.is_ready());
    drop(vault);
    server.state.lock().unwrap().accept_membership = true;
    let vault = server.vault(dir.path());
    vault.refresh().await.unwrap();
    assert!(vault.is_ready());
    assert_eq!(vault.status().epoch, Some(2));
    let saved = store(dir.path()).load().unwrap();
    assert!(saved.pending_membership.is_none());
    let ring = Keyring::decode(&saved.keyring.unwrap().bytes().unwrap()).unwrap();
    assert_eq!(ring.epoch_key(2).unwrap().as_slice(), key.as_slice());
    let state = server.state.lock().unwrap();
    assert!(state.journal_observed.iter().all(|seen| *seen));
    assert_eq!(state.attempts[1], state.attempts[2]);
}
