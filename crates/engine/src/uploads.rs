//! Uploads — attachment staging + the content-addressed edge mirror
//! (feature-inventory §3.7 "Uploads"; port of comet's `uploads.ts`).
//!
//! The UI streams a file as base64 chunks (~60KB, sized for the relay when the
//! target device is remote); chunks stage on disk under `{data_dir}/uploads/tmp/
//! {uploadId}/{seq}.b64` (surviving an engine restart mid-upload, unlike comet's
//! in-memory buffers), and `commit` assembles them into
//! `{data_dir}/uploads/{id8}-{name}` and returns the absolute path, which the
//! composer appends to the prompt so the agent can read the file from disk.
//!
//! On commit the assembled bytes are also mirrored to the edge, best-effort:
//! `PUT {edge}/attachments/{sha256}` (bearer auth, content-addressed R2 —
//! `edge/src/index.ts`). A device that doesn't hold the file locally can fall
//! back to `GET {edge}/attachments/{sha256}` with the same bearer; native keeps
//! reads local-first (`read_chunk` proxies through the owning device), so the
//! GET fallback is the disaster path, not the hot path.
//!
//! `read_chunk` serves transcript images back in 45KB base64 chunks. Path jail:
//! only files under the uploads dir or a workspace-known chat cwd are readable
//! (the RPC layer supplies the cwd roots) — and only supported image types, as
//! in comet.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::EngineError;
use crate::doc_host::EdgeConfig;
use crate::repos::hex;

/// A pending upload must finish within this window (covers slow mesh links).
const STAGING_TTL: Duration = Duration::from_secs(10 * 60);
/// Hard cap on an assembled file (matches the edge's 32MB attachment cap).
const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Multiple of 3 so independent base64 chunks concatenate losslessly.
const READ_CHUNK_BYTES: u64 = 45_000;

/// `ReadAttachmentChunk` reply.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentChunk {
    pub name: String,
    pub mime_type: String,
    /// Base64 of this chunk's byte range.
    pub data: String,
    pub next_offset: u64,
    pub done: bool,
}

struct UploadsInner {
    /// Durable home for committed attachments (`{data_dir}/uploads`).
    dir: PathBuf,
    /// Chunk staging (`{data_dir}/uploads/tmp/{uploadId}/`).
    tmp: PathBuf,
    edge: Option<EdgeConfig>,
    http: reqwest::Client,
}

#[derive(Clone)]
pub struct Uploads {
    inner: Arc<UploadsInner>,
}

impl Uploads {
    pub fn new(data_dir: &Path, edge: Option<EdgeConfig>) -> Self {
        let dir = data_dir.join("uploads");
        Self {
            inner: Arc::new(UploadsInner {
                tmp: dir.join("tmp"),
                dir,
                edge,
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new()),
            }),
        }
    }

    /// The durable uploads dir (a path-jail root).
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Stage one base64 chunk. Positional (`seq`) writes are IDEMPOTENT: a client
    /// retrying a chunk whose ack was lost overwrites the same slot instead of
    /// double-appending. Callers without `seq` get append-only behavior.
    pub fn append(&self, upload_id: &str, data: &str, seq: Option<u64>) -> Result<(), EngineError> {
        let dir = self.staging_dir(upload_id)?;
        self.sweep();
        std::fs::create_dir_all(&dir)?;
        let at = match seq {
            Some(seq) => seq,
            None => next_free_seq(&dir)?,
        };
        if at > 1_000_000 {
            return Err(EngineError::Other("Invalid chunk index".into()));
        }
        // Base64 inflates by ~4/3; bound the staged payload against the file cap.
        let staged: u64 = chunk_files(&dir)?
            .iter()
            .filter(|(seq, _)| *seq != at)
            .map(|(_, path)| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0))
            .sum();
        if (staged + data.len() as u64) * 3 / 4 > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::write(dir.join(format!("{at:06}.b64")), data)?;
        Ok(())
    }

    /// Assemble the staged chunks into a durable file and return its absolute
    /// path. Also mirrors the bytes to the edge (content-addressed), best-effort.
    pub fn commit(&self, upload_id: &str, file_name: &str) -> Result<String, EngineError> {
        let dir = self.staging_dir(upload_id)?;
        let mut parts = chunk_files(&dir)?;
        if parts.is_empty() {
            return Err(EngineError::Other("Unknown or expired upload".into()));
        }
        parts.sort_by_key(|(seq, _)| *seq);
        // Positional appends may leave holes if a chunk never arrived — joining
        // around them would silently corrupt the file.
        let mut joined = String::new();
        for (i, (seq, path)) in parts.iter().enumerate() {
            if *seq != i as u64 {
                return Err(EngineError::Other("Upload is missing a chunk".into()));
            }
            joined.push_str(std::fs::read_to_string(path)?.trim());
        }
        let bytes = BASE64
            .decode(joined.as_bytes())
            .map_err(|e| EngineError::Other(format!("upload is not valid base64: {e}")))?;
        if bytes.len() as u64 > MAX_BYTES {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(EngineError::Other("Upload too large".into()));
        }
        std::fs::create_dir_all(&self.inner.dir)?;
        let name = sanitize(file_name);
        let id8: String = upload_id.chars().take(8).collect();
        let path = self.inner.dir.join(format!("{id8}-{name}"));
        std::fs::write(&path, &bytes)?;
        let _ = std::fs::remove_dir_all(&dir);
        self.mirror_to_edge(&path, bytes);
        Ok(path.to_string_lossy().to_string())
    }

    /// Read one 45KB chunk of an attachment. `extra_roots` are the workspace's
    /// known chat cwds — together with the uploads dir they form the path jail.
    pub fn read_chunk(
        &self,
        path: &str,
        offset: u64,
        extra_roots: &[PathBuf],
    ) -> Result<AttachmentChunk, EngineError> {
        use std::io::{Read, Seek};
        let file = self.inspect(path, extra_roots)?;
        let size = file.size;
        let start = offset.min(size);
        let next_offset = (start + READ_CHUNK_BYTES).min(size);
        // Read ONLY this chunk's byte range — never the whole file per chunk.
        let mut buf = vec![0u8; (next_offset - start) as usize];
        let mut handle = std::fs::File::open(&file.resolved)?;
        handle.seek(std::io::SeekFrom::Start(start))?;
        let mut read = 0usize;
        while read < buf.len() {
            let n = handle.read(&mut buf[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.truncate(read);
        Ok(AttachmentChunk {
            name: file.name,
            mime_type: file.mime_type,
            data: BASE64.encode(&buf),
            next_offset,
            done: next_offset >= size,
        })
    }

    // ── internals ───────────────────────────────────────────────────────────

    fn staging_dir(&self, upload_id: &str) -> Result<PathBuf, EngineError> {
        // The id becomes a directory name — jail it to a safe charset.
        let ok = !upload_id.is_empty()
            && upload_id.len() <= 64
            && upload_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
        if !ok {
            return Err(EngineError::Other("Invalid upload id".into()));
        }
        Ok(self.inner.tmp.join(upload_id))
    }

    /// Reclaim staging dirs whose newest chunk is older than the TTL (an upload
    /// abandoned mid-stream must not hold up to 32MB forever).
    fn sweep(&self) {
        let Ok(entries) = std::fs::read_dir(&self.inner.tmp) else { return };
        for entry in entries.flatten() {
            let newest = std::fs::read_dir(entry.path())
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|f| f.metadata().ok()?.modified().ok())
                .max();
            let expired = match newest {
                Some(at) => at.elapsed().map(|age| age > STAGING_TTL).unwrap_or(false),
                None => true, // empty dir — reclaim
            };
            if expired {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn inspect(&self, path: &str, extra_roots: &[PathBuf]) -> Result<InspectedFile, EngineError> {
        let outside = || EngineError::Other("Attachment is outside the upload cache".into());
        // Canonicalize BOTH sides so `..` segments and symlinks can't escape.
        let resolved = std::fs::canonicalize(path).map_err(|_| outside())?;
        let allowed = std::iter::once(&self.inner.dir)
            .chain(extra_roots.iter())
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| resolved.starts_with(&root) && resolved != root);
        if !allowed {
            return Err(outside());
        }
        let meta = std::fs::metadata(&resolved)?;
        if !meta.is_file() {
            return Err(EngineError::Other("Attachment is not a file".into()));
        }
        if meta.len() > MAX_BYTES {
            return Err(EngineError::Other("Attachment is too large".into()));
        }
        let mime_type = mime_by_ext(&resolved)
            .ok_or_else(|| EngineError::Other("Attachment is not a supported image".into()))?;
        Ok(InspectedFile {
            name: resolved
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "attachment".into()),
            mime_type: mime_type.to_string(),
            size: meta.len(),
            resolved,
        })
    }

    /// Best-effort content-addressed mirror (`PUT /attachments/{sha256}`, bearer
    /// auth). Failures only log — local commit already succeeded.
    fn mirror_to_edge(&self, path: &Path, bytes: Vec<u8>) {
        let Some(edge) = self.inner.edge.clone() else { return };
        let sha = hex(&Sha256::digest(&bytes));
        let mime = mime_by_ext(path).unwrap_or("application/octet-stream").to_string();
        let url = format!("{}/attachments/{sha}", edge.url.trim_end_matches('/'));
        let http = self.inner.http.clone();
        tokio::spawn(async move {
            let sent = http
                .put(&url)
                .bearer_auth(&edge.token)
                .header("content-type", mime)
                .body(bytes)
                .send()
                .await;
            match sent {
                Ok(res) if res.status().is_success() => {
                    tracing::debug!(sha = %sha, "attachment mirrored to edge");
                }
                Ok(res) => {
                    tracing::warn!(sha = %sha, status = %res.status(), "edge attachment mirror rejected");
                }
                Err(err) => {
                    tracing::warn!(sha = %sha, error = %err, "edge attachment mirror failed");
                }
            }
        });
    }
}

struct InspectedFile {
    resolved: PathBuf,
    name: String,
    mime_type: String,
    size: u64,
}

fn chunk_files(dir: &Path) -> Result<Vec<(u64, PathBuf)>, EngineError> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(Vec::new()) };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let seq = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(seq) = seq
            && path.extension().and_then(|e| e.to_str()) == Some("b64")
        {
            files.push((seq, path));
        }
    }
    Ok(files)
}

fn next_free_seq(dir: &Path) -> Result<u64, EngineError> {
    Ok(chunk_files(dir)?.iter().map(|(seq, _)| seq + 1).max().unwrap_or(0))
}

fn sanitize(file_name: &str) -> String {
    let base = Path::new(file_name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    let tail: String = cleaned
        .chars()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.is_empty() { "upload".into() } else { tail }
}

fn mime_by_ext(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "svg" => Some("image/svg+xml"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        "avif" => Some("image/avif"),
        "heic" => Some("image/heic"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_names() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("my photo (1).png"), "my_photo__1_.png");
        assert_eq!(sanitize(""), "upload");
    }
}
