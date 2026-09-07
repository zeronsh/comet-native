//! HTTP client for the edge vault control plane (`edge/src/vault-room.ts`).
//! Every request carries the profile bearer; every response is treated as
//! untrusted bytes until the service verifies it against local pins.

use serde::Deserialize;

use crate::EngineError;
use crate::doc_host::EdgeConfig;

use super::store::Hex;

#[derive(Clone)]
pub struct VaultClient {
    http: reqwest::Client,
    edge: EdgeConfig,
    org_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub vault_id: Hex,
    pub generation: Hex,
    pub head_seq: i64,
    pub head_hash: Hex,
    pub genesis_hash: Hex,
    pub active_epoch: String,
    pub profile_hash: Hex,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MembershipPage {
    pub records: Vec<String>,
    #[serde(default)]
    pub truncated: bool,
    pub head_seq: i64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostOutcome {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub head_seq: Option<i64>,
    #[serde(default)]
    pub head_hash: Option<Hex>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectKeyEntry {
    pub epoch: String,
    pub record: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentInfo {
    pub request_id: Hex,
    pub device_id: Hex,
    pub signing_key: Hex,
    pub encryption_key: Hex,
    pub created_at: i64,
    pub expires_at: i64,
    pub status: String,
    #[serde(default)]
    pub membership_seq: Option<i64>,
}

/// Outcome of publishing an object key: the bytes every writer must use.
pub struct ObjectKeyPublished {
    pub record: Vec<u8>,
    pub adopted_existing: bool,
}

impl VaultClient {
    pub fn new(http: reqwest::Client, edge: EdgeConfig, org_id: impl Into<String>) -> Self {
        Self {
            http,
            edge,
            org_id: org_id.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/vault/{}{}",
            self.edge.url.trim_end_matches('/'),
            self.org_id,
            path
        )
    }

    async fn bearer(&self) -> Result<String, EngineError> {
        self.edge
            .bearer()
            .await
            .ok_or_else(|| EngineError::Other("not signed in".into()))
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, EngineError> {
        request
            .send()
            .await
            .map_err(|err| EngineError::Other(format!("the edge is unreachable: {err}")))
    }

    async fn json<T: serde::de::DeserializeOwned>(
        res: reqwest::Response,
    ) -> Result<T, EngineError> {
        res.json::<T>()
            .await
            .map_err(|err| EngineError::Other(format!("malformed vault response: {err}")))
    }

    /// `None` when no vault exists for this profile.
    pub async fn descriptor(&self) -> Result<Option<Descriptor>, EngineError> {
        let res = self
            .send(
                self.http
                    .get(self.url(""))
                    .bearer_auth(self.bearer().await?),
            )
            .await?;
        match res.status().as_u16() {
            404 => Ok(None),
            _ if res.status().is_success() => Ok(Some(Self::json(res).await?)),
            code => Err(EngineError::Other(format!("vault descriptor http {code}"))),
        }
    }

    pub async fn membership_after(&self, after: i64) -> Result<MembershipPage, EngineError> {
        let res = self
            .send(
                self.http
                    .get(self.url("/membership"))
                    .query(&[("after", after.to_string())])
                    .bearer_auth(self.bearer().await?),
            )
            .await?;
        if !res.status().is_success() {
            return Err(EngineError::Other(format!(
                "vault membership http {}",
                res.status().as_u16()
            )));
        }
        Self::json(res).await
    }

    /// Append a signed policy record. `Ok(Err(outcome))` is a server-side
    /// refusal (stale parent, invalid record) the caller may recover from.
    pub async fn post_membership(
        &self,
        record: Vec<u8>,
    ) -> Result<Result<PostOutcome, PostOutcome>, EngineError> {
        let res = self
            .send(
                self.http
                    .post(self.url("/membership"))
                    .bearer_auth(self.bearer().await?)
                    .header("content-type", "application/octet-stream")
                    .body(record),
            )
            .await?;
        let success = res.status().is_success();
        let outcome: PostOutcome = Self::json(res).await?;
        Ok(if success && outcome.ok {
            Ok(outcome)
        } else {
            Err(outcome)
        })
    }

    pub async fn get_envelope(&self, recipient: &[u8; 16]) -> Result<Option<Vec<u8>>, EngineError> {
        let path = format!("/envelopes/{}", Hex::of(recipient).0);
        let res = self
            .send(
                self.http
                    .get(self.url(&path))
                    .bearer_auth(self.bearer().await?),
            )
            .await?;
        match res.status().as_u16() {
            404 => Ok(None),
            _ if res.status().is_success() => Ok(Some(
                res.bytes()
                    .await
                    .map_err(|err| EngineError::Other(err.to_string()))?
                    .to_vec(),
            )),
            code => Err(EngineError::Other(format!("vault envelope http {code}"))),
        }
    }

    pub async fn put_envelope(
        &self,
        recipient: &[u8; 16],
        record: Vec<u8>,
    ) -> Result<(), EngineError> {
        let path = format!("/envelopes/{}", Hex::of(recipient).0);
        let res = self
            .send(
                self.http
                    .put(self.url(&path))
                    .bearer_auth(self.bearer().await?)
                    .header("content-type", "application/octet-stream")
                    .body(record),
            )
            .await?;
        if !res.status().is_success() {
            let code = res.status().as_u16();
            let body: PostOutcome = Self::json(res).await.unwrap_or(PostOutcome {
                ok: false,
                error: None,
                head_seq: None,
                head_hash: None,
            });
            return Err(EngineError::Other(format!(
                "vault envelope rejected ({code}: {})",
                body.error.unwrap_or_default()
            )));
        }
        Ok(())
    }

    pub async fn object_keys(&self, object: &[u8; 16]) -> Result<Vec<ObjectKeyEntry>, EngineError> {
        #[derive(Deserialize)]
        struct Body {
            keys: Vec<ObjectKeyEntry>,
        }
        let path = format!("/objects/{}/keys", Hex::of(object).0);
        let res = self
            .send(
                self.http
                    .get(self.url(&path))
                    .bearer_auth(self.bearer().await?),
            )
            .await?;
        if !res.status().is_success() {
            return Err(EngineError::Other(format!(
                "vault object keys http {}",
                res.status().as_u16()
            )));
        }
        Ok(Self::json::<Body>(res).await?.keys)
    }

    /// First writer wins: a 409 returns the record already stored for this
    /// object/epoch, which the caller must adopt in place of its own.
    pub async fn put_object_key(
        &self,
        object: &[u8; 16],
        record: Vec<u8>,
    ) -> Result<ObjectKeyPublished, EngineError> {
        #[derive(Deserialize)]
        struct Body {
            #[serde(default)]
            conflict: bool,
            #[serde(default)]
            record: Option<String>,
            #[serde(default)]
            error: Option<String>,
        }
        let path = format!("/objects/{}/keys", Hex::of(object).0);
        let res = self
            .send(
                self.http
                    .put(self.url(&path))
                    .bearer_auth(self.bearer().await?)
                    .header("content-type", "application/octet-stream")
                    .body(record.clone()),
            )
            .await?;
        let status = res.status().as_u16();
        let body: Body = Self::json(res).await?;
        if status == 409 && body.conflict {
            let stored = body
                .record
                .as_deref()
                .and_then(decode_base64)
                .ok_or_else(|| EngineError::Other("vault key conflict without record".into()))?;
            return Ok(ObjectKeyPublished {
                record: stored,
                adopted_existing: true,
            });
        }
        if !(200..300).contains(&status) {
            return Err(EngineError::Other(format!(
                "vault object key rejected ({status}: {})",
                body.error.unwrap_or_default()
            )));
        }
        Ok(ObjectKeyPublished {
            record,
            adopted_existing: false,
        })
    }

    pub async fn create_enrollment(
        &self,
        request: &zeron_crypto::policy::EnrollmentRequest,
        proof: &[u8; 64],
    ) -> Result<EnrollmentInfo, EngineError> {
        let body = serde_json::json!({
            "requestId": Hex::of(&request.request_id).0,
            "deviceId": Hex::of(&request.device_id).0,
            "signingKey": Hex::of(&request.signing_key).0,
            "encryptionKey": Hex::of(&request.encryption_key).0,
            "proof": Hex::of(proof).0,
        });
        let res = self
            .send(
                self.http
                    .post(self.url("/enroll"))
                    .bearer_auth(self.bearer().await?)
                    .json(&body),
            )
            .await?;
        if !res.status().is_success() {
            let code = res.status().as_u16();
            let body: PostOutcome = Self::json(res).await.unwrap_or(PostOutcome {
                ok: false,
                error: None,
                head_seq: None,
                head_hash: None,
            });
            return Err(EngineError::Other(format!(
                "enrollment refused ({code}: {})",
                body.error.unwrap_or_default()
            )));
        }
        Self::json(res).await
    }

    pub async fn list_enrollments(&self) -> Result<Vec<EnrollmentInfo>, EngineError> {
        #[derive(Deserialize)]
        struct Body {
            requests: Vec<EnrollmentInfo>,
        }
        let res = self
            .send(
                self.http
                    .get(self.url("/enroll"))
                    .bearer_auth(self.bearer().await?),
            )
            .await?;
        if res.status().as_u16() == 404 {
            return Ok(Vec::new());
        }
        if !res.status().is_success() {
            return Err(EngineError::Other(format!(
                "enrollment list http {}",
                res.status().as_u16()
            )));
        }
        Ok(Self::json::<Body>(res).await?.requests)
    }

    pub async fn enrollment(
        &self,
        request_id: &[u8; 16],
    ) -> Result<Option<EnrollmentInfo>, EngineError> {
        let path = format!("/enroll/{}", Hex::of(request_id).0);
        let res = self
            .send(
                self.http
                    .get(self.url(&path))
                    .bearer_auth(self.bearer().await?),
            )
            .await?;
        match res.status().as_u16() {
            404 => Ok(None),
            _ if res.status().is_success() => Ok(Some(Self::json(res).await?)),
            code => Err(EngineError::Other(format!("enrollment status http {code}"))),
        }
    }

    pub async fn approve_enrollment(
        &self,
        request_id: &[u8; 16],
        membership_seq: i64,
    ) -> Result<(), EngineError> {
        let path = format!("/enroll/{}/approve", Hex::of(request_id).0);
        let res = self
            .send(
                self.http
                    .post(self.url(&path))
                    .bearer_auth(self.bearer().await?)
                    .json(&serde_json::json!({ "membershipSeq": membership_seq })),
            )
            .await?;
        if !res.status().is_success() {
            return Err(EngineError::Other(format!(
                "enrollment approve http {}",
                res.status().as_u16()
            )));
        }
        Ok(())
    }

    pub async fn reject_enrollment(&self, request_id: &[u8; 16]) -> Result<(), EngineError> {
        let path = format!("/enroll/{}/reject", Hex::of(request_id).0);
        let res = self
            .send(
                self.http
                    .post(self.url(&path))
                    .bearer_auth(self.bearer().await?),
            )
            .await?;
        if !res.status().is_success() && res.status().as_u16() != 404 {
            return Err(EngineError::Other(format!(
                "enrollment reject http {}",
                res.status().as_u16()
            )));
        }
        Ok(())
    }
}

pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

pub fn encode_base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
