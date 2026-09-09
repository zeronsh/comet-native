use std::sync::Arc;

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::sync::mpsc;
use zeron_proto::{
    ListWorkspaceDirectoryRequest, ReadWorkspaceFileRequest, SearchWorkspaceFilesRequest,
    WatchWorkspaceFilesRequest, WorkspaceDirectoryPage, WorkspaceFileSearchMatch,
    WorkspaceFileText, WorkspaceTarget, WriteWorkspaceFileOutcome, WriteWorkspaceFileRequest,
};
use zeron_rpc::{RpcError, methods};

use crate::state::{AppState, EngineHandle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesRequestContext {
    pub target: WorkspaceTarget,
    pub target_device_id: Option<String>,
    pub cwd: String,
    pub checkout_id: Option<String>,
}

impl FilesRequestContext {
    pub fn for_chat(state: &AppState, chat_id: &str) -> Option<Self> {
        let chat = state.chats.iter().find(|chat| chat.id == chat_id)?;
        let cwd = chat.cwd.clone()?;
        let target_device_id = (state.local_device_id.as_deref() != Some(&chat.device_id))
            .then(|| chat.device_id.clone());
        Some(Self {
            target: WorkspaceTarget {
                chat_id: Some(chat.id.clone()),
                space_id: None,
                checkout_path: None,
            },
            target_device_id,
            cwd,
            checkout_id: chat.checkout_id.clone(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FilesClientError {
    #[error("workspace request could not be encoded: {0}")]
    Encode(String),
    #[error("workspace response was invalid: {0}")]
    Decode(String),
    #[error("workspace connection unavailable: {0}")]
    Transport(String),
    #[error("workspace request failed: {0}")]
    Request(String),
}

impl FilesClientError {
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

impl From<RpcError> for FilesClientError {
    fn from(error: RpcError) -> Self {
        match error {
            RpcError::Transport(message) => Self::Transport(message),
            RpcError::Closed => Self::Transport("connection closed".into()),
            other => Self::Request(other.to_string()),
        }
    }
}

#[derive(Clone)]
pub struct WorkspaceFilesClient {
    transport: Arc<dyn WorkspaceFilesTransport>,
    context: FilesRequestContext,
}

#[async_trait]
trait WorkspaceFilesTransport: Send + Sync {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>;
    async fn subscribe(
        &self,
        method: &str,
        params: Value,
    ) -> Result<mpsc::Receiver<Value>, RpcError>;
}

struct EngineFilesTransport(EngineHandle);

#[async_trait]
impl WorkspaceFilesTransport for EngineFilesTransport {
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.0.client().call(method, params).await
    }

    async fn subscribe(
        &self,
        method: &str,
        params: Value,
    ) -> Result<mpsc::Receiver<Value>, RpcError> {
        self.0.client().subscribe(method, params).await
    }
}

impl WorkspaceFilesClient {
    pub fn new(engine: EngineHandle, context: FilesRequestContext) -> Self {
        Self {
            transport: Arc::new(EngineFilesTransport(engine)),
            context,
        }
    }

    #[cfg(test)]
    fn with_transport(
        transport: Arc<dyn WorkspaceFilesTransport>,
        context: FilesRequestContext,
    ) -> Self {
        Self { transport, context }
    }

    pub async fn list_directory(
        &self,
        request: ListWorkspaceDirectoryRequest,
    ) -> Result<WorkspaceDirectoryPage, FilesClientError> {
        self.call(methods::LIST_WORKSPACE_DIRECTORY, &request).await
    }

    pub async fn search(
        &self,
        request: SearchWorkspaceFilesRequest,
    ) -> Result<Vec<WorkspaceFileSearchMatch>, FilesClientError> {
        self.call(methods::SEARCH_WORKSPACE_FILES, &request).await
    }

    pub async fn read_file(
        &self,
        request: ReadWorkspaceFileRequest,
    ) -> Result<WorkspaceFileText, FilesClientError> {
        self.call(methods::READ_WORKSPACE_FILE, &request).await
    }

    pub async fn read_image(
        &self,
        path: String,
        checkout_id: String,
    ) -> Result<(String, Vec<u8>), FilesClientError> {
        use base64::Engine as _;
        use zeron_proto::{MAX_WORKSPACE_IMAGE_BYTES, WORKSPACE_IMAGE_CHUNK_BYTES};
        let mut request = zeron_proto::ReadWorkspaceImageRequest {
            target: self.context.target.clone(),
            path,
            expected_checkout_id: checkout_id,
            offset: 0,
            expected_content_hash: None,
        };
        let mut bytes = Vec::new();
        let mut mime = None;
        let mut size = None;
        for _ in 0..=MAX_WORKSPACE_IMAGE_BYTES / WORKSPACE_IMAGE_CHUNK_BYTES {
            let chunk: zeron_proto::WorkspaceImageChunk =
                self.call(methods::READ_WORKSPACE_IMAGE, &request).await?;
            if chunk.checkout_id != request.expected_checkout_id
                || chunk.size > MAX_WORKSPACE_IMAGE_BYTES
                || chunk.data.len() > WORKSPACE_IMAGE_CHUNK_BYTES.div_ceil(3) * 4
                || request
                    .expected_content_hash
                    .as_ref()
                    .is_some_and(|hash| hash != &chunk.content_hash)
                || mime.as_ref().is_some_and(|m| m != &chunk.mime_type)
                || size.is_some_and(|s| s != chunk.size)
            {
                return Err(FilesClientError::Decode(
                    "Image identity or size changed".into(),
                ));
            }
            let part = base64::engine::general_purpose::STANDARD
                .decode(&chunk.data)
                .map_err(|e| FilesClientError::Decode(e.to_string()))?;
            if part.is_empty()
                || request.offset.checked_add(part.len()) != Some(chunk.next_offset)
                || chunk.next_offset > chunk.size
                || chunk.done != (chunk.next_offset == chunk.size)
            {
                return Err(FilesClientError::Decode(
                    "Invalid image chunk offset".into(),
                ));
            }
            bytes.extend(part);
            if chunk.done {
                return Ok((chunk.mime_type, bytes));
            }
            request.offset = chunk.next_offset;
            request.expected_content_hash = Some(chunk.content_hash);
            mime = Some(chunk.mime_type);
            size = Some(chunk.size);
        }
        Err(FilesClientError::Decode(
            "Image chunk limit exceeded".into(),
        ))
    }

    pub async fn write_file(
        &self,
        request: WriteWorkspaceFileRequest,
    ) -> Result<WriteWorkspaceFileOutcome, FilesClientError> {
        self.call(methods::WRITE_WORKSPACE_FILE, &request).await
    }

    pub async fn watch(&self) -> Result<mpsc::Receiver<serde_json::Value>, FilesClientError> {
        let request = WatchWorkspaceFilesRequest {
            target: self.context.target.clone(),
        };
        let params = request_params(&request, self.context.target_device_id.as_deref())?;
        self.transport
            .subscribe(methods::WATCH_WORKSPACE_FILES, params)
            .await
            .map_err(Into::into)
    }

    async fn call<Request, Response>(
        &self,
        method: &str,
        request: &Request,
    ) -> Result<Response, FilesClientError>
    where
        Request: Serialize,
        Response: DeserializeOwned,
    {
        let params = request_params(request, self.context.target_device_id.as_deref())?;
        let response = self.transport.call(method, params).await?;
        serde_json::from_value(response)
            .map_err(|error| FilesClientError::Decode(error.to_string()))
    }
}

pub fn request_params<Request: Serialize>(
    request: &Request,
    target_device_id: Option<&str>,
) -> Result<Value, FilesClientError> {
    let mut value = serde_json::to_value(request)
        .map_err(|error| FilesClientError::Encode(error.to_string()))?;
    if let Some(target_device_id) = target_device_id {
        let object = value.as_object_mut().ok_or_else(|| {
            FilesClientError::Encode("workspace request must serialize as an object".into())
        })?;
        object.insert(
            "targetDeviceId".into(),
            Value::String(target_device_id.into()),
        );
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use super::*;

    #[derive(Default)]
    struct DeterministicTransport {
        responses: HashMap<String, Value>,
        calls: Mutex<Vec<(String, Value)>>,
        watch_values: Vec<Value>,
    }

    #[async_trait]
    impl WorkspaceFilesTransport for DeterministicTransport {
        async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            self.calls.lock().unwrap().push((method.into(), params));
            Ok(self.responses.get(method).cloned().unwrap())
        }

        async fn subscribe(
            &self,
            method: &str,
            params: Value,
        ) -> Result<mpsc::Receiver<Value>, RpcError> {
            self.calls.lock().unwrap().push((method.into(), params));
            let (sender, receiver) = mpsc::channel(self.watch_values.len().max(1));
            for value in &self.watch_values {
                sender.try_send(value.clone()).unwrap();
            }
            Ok(receiver)
        }
    }

    fn target() -> WorkspaceTarget {
        WorkspaceTarget {
            chat_id: Some("chat-1".into()),
            space_id: None,
            checkout_path: None,
        }
    }

    #[test]
    fn local_params_preserve_the_typed_workspace_shape() {
        let params = request_params(
            &ListWorkspaceDirectoryRequest {
                target: target(),
                directory: "src".into(),
                include_ignored: false,
                cursor: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(params["chatId"], "chat-1");
        assert_eq!(params["directory"], "src");
        assert!(params.get("targetDeviceId").is_none());
    }

    #[test]
    fn remote_params_add_only_the_relay_target() {
        let params = request_params(
            &ReadWorkspaceFileRequest {
                target: target(),
                path: "src/lib.rs".into(),
            },
            Some("device-b"),
        )
        .unwrap();
        assert_eq!(params["chatId"], "chat-1");
        assert_eq!(params["path"], "src/lib.rs");
        assert_eq!(params["targetDeviceId"], "device-b");
        assert!(params.get("spaceId").is_none());
        assert!(params.get("checkoutPath").is_none());
    }

    #[test]
    fn protocol_responses_decode_into_the_final_backend_types() {
        let page: WorkspaceDirectoryPage = serde_json::from_value(serde_json::json!({
            "directory": "",
            "entries": [],
            "nextCursor": null,
            "truncated": false
        }))
        .unwrap();
        let search: Vec<WorkspaceFileSearchMatch> = serde_json::from_value(serde_json::json!([{
            "path": "src/lib.rs",
            "name": "lib.rs",
            "kind": "file",
            "score": 42
        }]))
        .unwrap();
        let text: WorkspaceFileText = serde_json::from_value(serde_json::json!({
            "path": "src/lib.rs",
            "text": "fn main() {}",
            "contentHash": "abc",
            "checkoutId": "checkout-1",
            "size": 12,
            "modifiedAt": null,
            "encoding": "utf8",
            "lineEnding": "lf",
            "readOnlyReason": null,
            "truncated": false
        }))
        .unwrap();
        assert!(page.entries.is_empty());
        assert_eq!(search[0].path, "src/lib.rs");
        assert_eq!(text.content_hash.as_deref(), Some("abc"));
    }

    #[test]
    fn malformed_response_is_reported_as_decode_error() {
        let result = serde_json::from_value::<WorkspaceDirectoryPage>(serde_json::json!({
            "directory": "",
            "entries": "not-an-array"
        }))
        .map_err(|error| FilesClientError::Decode(error.to_string()));
        assert!(matches!(result, Err(FilesClientError::Decode(_))));
    }

    #[tokio::test]
    async fn deterministic_transport_covers_list_search_read_and_watch_routing() {
        let transport = Arc::new(DeterministicTransport {
            responses: HashMap::from([
                (
                    methods::LIST_WORKSPACE_DIRECTORY.into(),
                    serde_json::json!({
                        "directory": "",
                        "entries": [],
                        "nextCursor": null,
                        "truncated": false
                    }),
                ),
                (
                    methods::SEARCH_WORKSPACE_FILES.into(),
                    serde_json::json!([{
                        "path": "src/lib.rs",
                        "name": "lib.rs",
                        "kind": "file",
                        "score": 100
                    }]),
                ),
                (
                    methods::READ_WORKSPACE_FILE.into(),
                    serde_json::json!({
                        "path": "src/lib.rs",
                        "text": "fn lib() {}",
                        "contentHash": "hash",
                        "checkoutId": "checkout-1",
                        "size": 11,
                        "modifiedAt": null,
                        "encoding": "utf8",
                        "lineEnding": "lf",
                        "readOnlyReason": null,
                        "truncated": false
                    }),
                ),
                (
                    methods::WRITE_WORKSPACE_FILE.into(),
                    serde_json::json!({
                        "status": "written",
                        "file": {
                            "path": "src/lib.rs",
                            "contentHash": "hash-2",
                            "size": 12,
                            "modifiedAt": null
                        }
                    }),
                ),
            ]),
            watch_values: vec![serde_json::json!({ "sequence": 1, "changes": [] })],
            ..Default::default()
        });
        let context = FilesRequestContext {
            target: target(),
            target_device_id: Some("remote-device".into()),
            cwd: "/workspace".into(),
            checkout_id: Some("checkout-1".into()),
        };
        let client = WorkspaceFilesClient::with_transport(transport.clone(), context);

        let page = client
            .list_directory(ListWorkspaceDirectoryRequest {
                target: target(),
                directory: String::new(),
                include_ignored: false,
                cursor: None,
            })
            .await
            .unwrap();
        let search = client
            .search(SearchWorkspaceFilesRequest {
                target: target(),
                query: "lib".into(),
                include_ignored: false,
                limit: Some(20),
            })
            .await
            .unwrap();
        let file = client
            .read_file(ReadWorkspaceFileRequest {
                target: target(),
                path: "src/lib.rs".into(),
            })
            .await
            .unwrap();
        let outcome = client
            .write_file(WriteWorkspaceFileRequest {
                expected_checkout_id: "checkout-1".into(),
                target: target(),
                path: "src/lib.rs".into(),
                text: "fn main() {}".into(),
                expected_content_hash: "hash".into(),
                encoding: zeron_proto::WorkspaceWritableEncoding::Utf8,
                line_ending: zeron_proto::WorkspaceWritableLineEnding::Lf,
            })
            .await
            .unwrap();
        let mut watch = client.watch().await.unwrap();

        assert!(page.entries.is_empty());
        assert_eq!(search[0].path, "src/lib.rs");
        assert_eq!(file.text.as_deref(), Some("fn lib() {}"));
        assert!(matches!(
            outcome,
            WriteWorkspaceFileOutcome::Written { file } if file.content_hash == "hash-2"
        ));
        assert_eq!(watch.recv().await.unwrap()["sequence"], 1);
        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        assert!(calls.iter().all(|(_, params)| {
            params["chatId"] == "chat-1" && params["targetDeviceId"] == "remote-device"
        }));
    }

    #[tokio::test]
    async fn write_decodes_conflicts_without_changing_request_shape() {
        let transport = Arc::new(DeterministicTransport {
            responses: HashMap::from([(
                methods::WRITE_WORKSPACE_FILE.into(),
                serde_json::json!({
                    "status": "conflict",
                    "reason": "changed",
                    "currentContentHash": "disk-hash",
                    "currentModifiedAt": null
                }),
            )]),
            ..Default::default()
        });
        let context = FilesRequestContext {
            target: target(),
            target_device_id: None,
            cwd: "/workspace".into(),
            checkout_id: Some("checkout-1".into()),
        };
        let client = WorkspaceFilesClient::with_transport(transport.clone(), context);
        let outcome = client
            .write_file(WriteWorkspaceFileRequest {
                expected_checkout_id: "checkout-1".into(),
                target: target(),
                path: "src/lib.rs".into(),
                text: "changed".into(),
                expected_content_hash: "hash".into(),
                encoding: zeron_proto::WorkspaceWritableEncoding::Utf8,
                line_ending: zeron_proto::WorkspaceWritableLineEnding::Lf,
            })
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            WriteWorkspaceFileOutcome::Conflict {
                current_content_hash: Some(hash),
                ..
            } if hash == "disk-hash"
        ));
        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls[0].1["expectedContentHash"], "hash");
        assert_eq!(calls[0].1["encoding"], "utf8");
        assert_eq!(calls[0].1["lineEnding"], "lf");
        assert!(calls[0].1.get("targetDeviceId").is_none());
    }
    struct ImageTransport {
        responses: Mutex<std::collections::VecDeque<Result<Value, RpcError>>>,
        calls: Mutex<Vec<Value>>,
    }
    #[async_trait]
    impl WorkspaceFilesTransport for ImageTransport {
        async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            assert_eq!(method, methods::READ_WORKSPACE_IMAGE);
            self.calls.lock().unwrap().push(params);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra request")
        }
        async fn subscribe(&self, _: &str, _: Value) -> Result<mpsc::Receiver<Value>, RpcError> {
            unreachable!()
        }
    }
    fn image_client(
        responses: Vec<Result<Value, RpcError>>,
    ) -> (WorkspaceFilesClient, Arc<ImageTransport>) {
        let transport = Arc::new(ImageTransport {
            responses: Mutex::new(responses.into()),
            calls: Mutex::new(Vec::new()),
        });
        let client = WorkspaceFilesClient {
            transport: transport.clone(),
            context: FilesRequestContext {
                target: target(),
                target_device_id: Some("remote".into()),
                cwd: "/remote/checkout".into(),
                checkout_id: Some("checkout".into()),
            },
        };
        (client, transport)
    }
    fn image_chunk(data: &[u8], end: usize, done: bool) -> Value {
        use base64::Engine as _;
        serde_json::to_value(zeron_proto::WorkspaceImageChunk {
            checkout_id: "checkout".into(),
            content_hash: "hash".into(),
            mime_type: "image/png".into(),
            data: base64::engine::general_purpose::STANDARD.encode(data),
            next_offset: end,
            size: 4,
            done,
        })
        .unwrap()
    }
    #[tokio::test]
    async fn remote_images_keep_checkout_hash_and_offset_across_chunks() {
        let (client, transport) = image_client(vec![
            Ok(image_chunk(b"ab", 2, false)),
            Ok(image_chunk(b"cd", 4, true)),
        ]);
        assert_eq!(
            client
                .read_image("docs/image.png".into(), "checkout".into())
                .await
                .unwrap(),
            ("image/png".into(), b"abcd".to_vec())
        );
        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["targetDeviceId"], "remote");
        assert_eq!(calls[0]["path"], "docs/image.png");
        assert_eq!(calls[0]["expectedCheckoutId"], "checkout");
        assert_eq!(calls[1]["expectedContentHash"], "hash");
        assert_eq!(calls[1]["offset"], 2);
    }
    #[tokio::test]
    async fn image_reads_reject_inconsistent_or_unsupported_responses() {
        for field in ["checkoutId", "contentHash", "mimeType"] {
            let mut chunk = image_chunk(b"cd", 4, true);
            chunk[field] = "changed".into();
            let (client, _) = image_client(vec![Ok(image_chunk(b"ab", 2, false)), Ok(chunk)]);
            assert!(
                client
                    .read_image("image.png".into(), "checkout".into())
                    .await
                    .is_err()
            );
        }
        for chunk in [
            image_chunk(b"ab", 0, false),
            image_chunk(b"ab", 2, true),
            image_chunk(b"", 0, false),
        ] {
            let (client, _) = image_client(vec![Ok(chunk)]);
            assert!(
                client
                    .read_image("image.png".into(), "checkout".into())
                    .await
                    .is_err()
            );
        }
        let (client, _) = image_client(vec![Err(RpcError::UnknownMethod(
            methods::READ_WORKSPACE_IMAGE.into(),
        ))]);
        assert!(
            client
                .read_image("image.png".into(), "checkout".into())
                .await
                .is_err()
        );
    }
}
