//! Attachments (feature-inventory §1.7/§1.8): the composer's staged images,
//! the chunked upload to the chat's host device, the plain-text attachment-ref
//! transport that rides the prompt, the transcript read-back cache, and the
//! full-size preview lightbox.
//!
//! Ports of zeron's `composer/use-attachments.ts` (staging/upload),
//! `control/message-attachments.ts` (the `withAttachments` /
//! `parseUserMessageImages` text transport — attachment refs are embedded in
//! the user message's plain text, which is exactly what persists in the doc),
//! and `lib/transcript-attachment-cache.ts` (decoded-image cache keyed by
//! `(deviceId, path)`, seeded locally after a send so own bubbles never
//! round-trip).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::TryStreamExt as _;
use gpui::{
    AnyElement, BackgroundExecutor, Image, ImageFormat, ObjectFit, SharedString, Size,
    StyledImage as _, div, img, prelude::*, px,
};

use crate::state::EngineHandle;
use crate::theme::ink;
use zeron_rpc::methods;

/// use-attachments.ts `MAX_ATTACHMENT_BYTES`.
pub const MAX_ATTACHMENT_BYTES: u64 = 24 * 1024 * 1024;
/// Base64 chars per `UploadChunk`, sized against the relay's hard ceiling:
/// Cloudflare caps a WebSocket message at 1 MiB, and a chunk rides one relay
/// frame (JSON envelope + uleb header add ~150 bytes) — 680 000 chars ≈
/// 510 KB binary leaves ~35% headroom. Multiple of 4 so a slice of the
/// whole-file base64 stays independently decodable. The old 60 000 (45 KB)
/// made a 3 MB screenshot ~70 sequential round trips — each one a stall
/// opportunity on a flaky link.
pub const UPLOAD_CHUNK_B64_CHARS: usize = 680_000;
/// state.ts `MAX_ATTACHMENT_READ_CHUNKS` — bounds the read-back loop.
const MAX_READ_CHUNKS: usize = 1_000;

// ---------------------------------------------------------------------------
// Text transport (message-attachments.ts)
// ---------------------------------------------------------------------------

/// The body used for image-only sends (`use-attachments.ts`).
pub const ATTACHMENT_ONLY_TEXT: &str = "See the attached image(s).";

/// How attachments ride the prompt (use-attachments.ts `withAttachments`):
/// plain local paths appended to the text — the files are staged on the device
/// that runs the agent, so the agent can open them with its own tools; the
/// same text is what persists as the user doc entry.
pub fn with_attachments(text: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return text.to_string();
    }
    let refs: Vec<String> = paths.iter().map(|p| format!("- {p}")).collect();
    let body = if text.is_empty() {
        ATTACHMENT_ONLY_TEXT
    } else {
        text
    };
    format!(
        "{body}\n\nAttached images (local files — open them to view):\n{}",
        refs.join("\n")
    )
}

/// An attachment ref parsed back out of a user message's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserImageAttachment {
    pub id: String,
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUserMessage {
    /// The visible prompt (the refs trailer stripped; empty for image-only sends).
    pub text: String,
    pub attachments: Vec<UserImageAttachment>,
}

fn name_from_path(path: &str) -> String {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if name.is_empty() {
        "image".to_string()
    } else {
        name.to_string()
    }
}

/// Find the refs trailer: a blank line, then a line starting (case-insensitive)
/// with `Attached images (local files` and ending `):`. Returns
/// `(body_end, refs_start)` byte offsets — the tolerant equivalent of zeron's
/// `ATTACHED_IMAGES_RE`.
fn find_refs_marker(content: &str) -> Option<(usize, usize)> {
    let lower = content.to_ascii_lowercase();
    let needle = "\n\nattached images (local files";
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(needle) {
        let gap = from + rel;
        let line_start = gap + 2;
        let line_end = content[line_start..]
            .find('\n')
            .map(|p| line_start + p)
            .unwrap_or(content.len());
        let line = content[line_start..line_end].trim_end_matches('\r');
        if line.ends_with("):") {
            let refs_start = (line_end + 1).min(content.len());
            return Some((gap, refs_start));
        }
        from = line_start;
    }
    None
}

/// message-attachments.ts `parseUserMessageImages`: split the visible prompt
/// from its attachment-ref trailer.
pub fn parse_user_message_images(content: &str) -> ParsedUserMessage {
    let Some((body_end, refs_start)) = find_refs_marker(content) else {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments: Vec::new(),
        };
    };
    let body = content[..body_end].trim_end();
    let attachments: Vec<UserImageAttachment> = content[refs_start..]
        .lines()
        .filter_map(|line| {
            let path = line.trim_start().strip_prefix("- ")?.trim();
            (!path.is_empty()).then(|| path.to_string())
        })
        .enumerate()
        .map(|(index, path)| UserImageAttachment {
            id: format!("{index}:{path}"),
            name: name_from_path(&path),
            path,
        })
        .collect();
    if attachments.is_empty() {
        return ParsedUserMessage {
            text: content.to_string(),
            attachments,
        };
    }
    ParsedUserMessage {
        text: if body.trim() == ATTACHMENT_ONLY_TEXT {
            String::new()
        } else {
            body.to_string()
        },
        attachments,
    }
}

/// message-attachments.ts `userMessageRailText`: what the rail/sidebar shows
/// for a user message ("Attached image" / "N attached images" when image-only).
pub fn user_message_rail_text(content: &str) -> String {
    let parsed = parse_user_message_images(content);
    if !parsed.text.trim().is_empty() {
        return parsed.text;
    }
    match parsed.attachments.len() {
        0 => content.to_string(),
        1 => "Attached image".to_string(),
        n => format!("{n} attached images"),
    }
}

// ---------------------------------------------------------------------------
// Staging (use-attachments.ts intake)
// ---------------------------------------------------------------------------

/// An image staged in the composer, before upload. The raw bytes live inside
/// the [`Image`] (gpui decodes them at paint; the same Arc feeds thumbnails,
/// the lightbox, the upload, and the post-send cache seed).
#[derive(Clone)]
pub struct StagedAttachment {
    pub id: String,
    /// File name with a type-matching extension (use-attachments.ts
    /// `ensureExtension` — agents sniff images by extension).
    pub name: String,
    pub image: Arc<Image>,
}

impl StagedAttachment {
    pub fn bytes(&self) -> &[u8] {
        &self.image.bytes
    }
}

/// Image formats the whole pipeline supports: intersection of gpui's decoders
/// and the engine's `mime_by_ext` read-back jail.
pub fn format_by_extension(path: &Path) -> Option<ImageFormat> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "gif" => Some(ImageFormat::Gif),
        "webp" => Some(ImageFormat::Webp),
        "svg" => Some(ImageFormat::Svg),
        "bmp" => Some(ImageFormat::Bmp),
        "tif" | "tiff" => Some(ImageFormat::Tiff),
        _ => None,
    }
}

/// use-attachments.ts `ensureExtension`: pasted screenshots often arrive as a
/// bare "image" — make sure the staged name carries a type-matching extension.
pub fn ensure_extension(name: &str, format: ImageFormat) -> String {
    let has_ext = name
        .rsplit_once('.')
        .map(|(stem, ext)| {
            !stem.is_empty()
                && (2..=5).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or(false);
    if has_ext {
        name.to_string()
    } else {
        format!("{name}.{}", format.extension())
    }
}

/// Stage a file from disk (picker / drop / pasted path). `Err` carries the
/// user-facing message (mirrors the old `onError` copy).
pub fn stage_file(path: &Path) -> Result<StagedAttachment, String> {
    let display_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());
    let Some(format) = format_by_extension(path) else {
        return Err(format!("{display_name} is not a supported image."));
    };
    let meta = std::fs::metadata(path).map_err(|_| format!("{display_name} could not be read."))?;
    if meta.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!("{display_name} is too large (24 MB max)."));
    }
    let bytes = std::fs::read(path).map_err(|_| format!("{display_name} could not be read."))?;
    Ok(StagedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        name: ensure_extension(&display_name, format),
        image: Arc::new(Image::from_bytes(format, bytes)),
    })
}

/// Stage an image pasted from the clipboard.
pub fn stage_clipboard_image(image: Image) -> StagedAttachment {
    let format = image.format;
    StagedAttachment {
        id: uuid::Uuid::new_v4().to_string(),
        name: ensure_extension("image", format),
        image: Arc::new(image),
    }
}

// ---------------------------------------------------------------------------
// Upload (state.ts uploadAttachment) + read-back (state.ts readAttachmentImage)
// ---------------------------------------------------------------------------

fn with_target(mut params: serde_json::Value, target_device_id: Option<&str>) -> serde_json::Value {
    if let (Some(target), Some(map)) = (target_device_id, params.as_object_mut()) {
        map.insert("targetDeviceId".into(), target.into());
    }
    params
}

/// Per-call deadlines (desktop state.ts): a stalled-but-open relay link never
/// fails an RPC on its own, so every attachment call races a timer. The first
/// chunk gets 90s (a cold dial to a remote device), later chunks 30s; commit
/// 150s (it must outlast the engine's cross-device assemble); reads 20s.
const FIRST_CHUNK_TIMEOUT: Duration = Duration::from_secs(90);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(150);
const READ_CHUNK_TIMEOUT: Duration = Duration::from_secs(20);

/// Race an RPC against `timeout` on the gpui background executor (these
/// futures run under `cx.spawn`, so tokio's timer reactor isn't available).
pub(crate) async fn call_with_timeout(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let call = engine.client().call(method, params);
    let timer = executor.timer(timeout);
    futures::pin_mut!(call);
    match futures::future::select(call, timer).await {
        futures::future::Either::Left((result, _)) => result.map_err(|e| e.to_string()),
        futures::future::Either::Right(_) => Err(format!("{method} timed out")),
    }
}

/// Chunks in flight at once. `seq` slots are idempotent engine-side, so
/// completion order doesn't matter; a small window hides per-chunk latency
/// without flooding the relay socket.
const UPLOAD_CONCURRENCY: usize = 3;

/// Whole-attachment deadline. Per-chunk timeouts + retries bound each CALL,
/// but on a flapping link chunks that succeed on attempt 2-of-3 never trip
/// the 3-consecutive-failure abort — an upload could lawfully crawl for hours
/// reading "Sending…" (2026-08-18 user report). Scaled with size, capped:
/// past this, fail the send with the banner instead of spinning.
fn attachment_deadline(n_chunks: usize) -> Duration {
    Duration::from_secs((120 + 15 * n_chunks as u64).min(900))
}

/// The `(seq, b64 byte-range)` plan for a file's chunks. An empty file still
/// sends one empty chunk (the commit needs the uploadId staged).
fn chunk_ranges(b64_len: usize) -> Vec<(u64, std::ops::Range<usize>)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut seq = 0u64;
    loop {
        let end = (start + UPLOAD_CHUNK_B64_CHARS).min(b64_len);
        ranges.push((seq, start..end));
        start = end;
        seq += 1;
        if start >= b64_len {
            break;
        }
    }
    ranges
}

/// Chunked upload: base64 the bytes, `UploadChunk{uploadId,seq,data}` per
/// [`UPLOAD_CHUNK_B64_CHARS`] slice (positional `seq` makes the cheap retry
/// idempotent), a few chunks in flight at once, then
/// `UploadCommit{uploadId,fileName}` → the durable absolute path on the target
/// device. The caller mints `upload_id` — the queued-attachment flow derives
/// its `pending://` refs from the same identity before the bytes move.
/// `progress` (when given) accumulates uploaded BINARY bytes — the
/// composer's "Uploading… N%" reads it every paint. Errors return the raw
/// cause (the composer shows friendly copy).
pub async fn upload_attachment(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    target_device_id: Option<&str>,
    upload_id: &str,
    attachment: &StagedAttachment,
    progress: Option<Arc<std::sync::atomic::AtomicU64>>,
) -> Result<String, String> {
    let b64 = BASE64.encode(attachment.bytes());
    let ranges = chunk_ranges(b64.len());
    let deadline = executor.timer(attachment_deadline(ranges.len()));
    let upload = async {
        futures::stream::iter(ranges.iter().cloned().map(Ok::<_, String>))
            .try_for_each_concurrent(UPLOAD_CONCURRENCY, |(seq, range)| {
                let progress = progress.clone();
                let upload_id = &upload_id;
                let b64 = &b64;
                async move {
                    let params = with_target(
                        serde_json::json!({
                            "uploadId": upload_id,
                            "seq": seq,
                            "data": &b64[range.clone()],
                        }),
                        target_device_id,
                    );
                    // The first WINDOW (not just seq 0) gets the cold-dial
                    // allowance — its chunks all start before the link is warm.
                    let timeout = if seq < UPLOAD_CONCURRENCY as u64 {
                        FIRST_CHUNK_TIMEOUT
                    } else {
                        CHUNK_TIMEOUT
                    };
                    // One transient blip must not abort the upload; `seq`
                    // slots are idempotent engine-side, so a blind re-send is
                    // safe (timeouts retry too).
                    let mut attempt = 0u32;
                    loop {
                        match call_with_timeout(
                            engine,
                            executor,
                            methods::UPLOAD_CHUNK,
                            params.clone(),
                            timeout,
                        )
                        .await
                        {
                            Ok(_) => break,
                            Err(err) if attempt < 2 => {
                                attempt += 1;
                                // warn, not debug: the 2026-08-19 incident
                                // ground through silent timeout/retry cycles
                                // for minutes with a literally empty log —
                                // degraded uploads must narrate.
                                tracing::warn!(error = %err, seq, attempt, "upload chunk retry");
                                // Stagger by seq so parallel chunks that failed
                                // together don't re-collide in lockstep.
                                executor
                                    .timer(Duration::from_millis(50 * (attempt as u64) * (seq + 1)))
                                    .await;
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    if let Some(progress) = &progress {
                        // b64 → binary bytes (final chunk's padding rounds up
                        // by ≤2 bytes — irrelevant for a percentage).
                        progress.fetch_add(
                            (range.len() * 3 / 4) as u64,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    Ok(())
                }
            })
            .await?;
        let params = with_target(
            serde_json::json!({ "uploadId": upload_id, "fileName": attachment.name }),
            target_device_id,
        );
        let reply = call_with_timeout(
            engine,
            executor,
            methods::UPLOAD_COMMIT,
            params,
            COMMIT_TIMEOUT,
        )
        .await?;
        reply
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| "upload commit returned no path".to_string())
    };
    futures::pin_mut!(upload);
    match futures::future::select(upload, deadline).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => Err(format!(
            "attachment upload exceeded {}s",
            attachment_deadline(ranges.len()).as_secs()
        )),
    }
}

/// A transcript image read back from the owning device.
pub struct LoadedAttachmentImage {
    pub name: String,
    pub image: Arc<Image>,
}

/// `ReadAttachmentChunk` loop: 45KB base64 chunks until `done` (bounded, with
/// the same stuck-offset guard as zeron's `readAttachmentImage`).
pub async fn read_attachment_image(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    target_device_id: Option<&str>,
    path: &str,
) -> Option<LoadedAttachmentImage> {
    let mut name = String::new();
    let mut mime = String::new();
    let mut b64 = String::new();
    let mut offset = 0u64;
    let mut done = false;
    for _ in 0..MAX_READ_CHUNKS {
        let params = with_target(
            serde_json::json!({ "path": path, "offset": offset }),
            target_device_id,
        );
        let chunk = call_with_timeout(
            engine,
            executor,
            methods::READ_ATTACHMENT_CHUNK,
            params,
            READ_CHUNK_TIMEOUT,
        )
        .await
        .ok()?;
        name = chunk.get("name")?.as_str()?.to_string();
        mime = chunk.get("mimeType")?.as_str()?.to_string();
        b64.push_str(chunk.get("data")?.as_str()?);
        done = chunk.get("done")?.as_bool()?;
        if done {
            break;
        }
        let next = chunk.get("nextOffset")?.as_u64()?;
        if next <= offset {
            return None;
        }
        offset = next;
    }
    if !done || b64.is_empty() {
        return None;
    }
    let bytes = BASE64.decode(b64.as_bytes()).ok()?;
    let format = ImageFormat::from_mime_type(&mime).unwrap_or(ImageFormat::Png);
    Some(LoadedAttachmentImage {
        name: if name.is_empty() {
            name_from_path(path)
        } else {
            name
        },
        image: Arc::new(Image::from_bytes(format, bytes)),
    })
}

// ---------------------------------------------------------------------------
// Transcript image cache (transcript-attachment-cache.ts)
// ---------------------------------------------------------------------------

/// A decoded transcript image, ready for `img(...)`.
#[derive(Clone)]
pub struct CachedAttachmentImage {
    pub name: SharedString,
    pub image: Arc<Image>,
}

/// What a render pass sees for one `(deviceId, path)` source.
#[derive(Clone)]
pub enum AttachmentSnapshot {
    Loading,
    Loaded(CachedAttachmentImage),
    /// Load failed; `retry_in` is how long until [`begin_load`] would hand out
    /// another attempt (the exponential 2s→15s ladder from user-attachments.tsx).
    Error {
        retry_in: Duration,
    },
}

enum CacheEntry {
    Loading {
        attempts: u32,
    },
    Loaded {
        image: CachedAttachmentImage,
        bytes: usize,
        last_used: u64,
    },
    Error {
        attempts: u32,
        at: Instant,
    },
}

fn retry_delay(attempts: u32) -> Duration {
    Duration::from_millis((2_000u64 << attempts.min(3)).min(15_000))
}

/// Byte budget for retained encoded images. The decoded copies gpui holds are
/// proportional (and usually larger), so bounding the encoded side bounds both
/// — this cache previously grew for the process lifetime with no eviction.
const IMAGE_CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct ImageCache {
    map: HashMap<(String, String), CacheEntry>,
    /// Monotonic access clock for LRU ordering.
    tick: u64,
    loaded_bytes: usize,
    /// Evicted images awaiting `flush_evicted` (freeing needs `&mut App`,
    /// which eviction sites — async load completions — don't always have).
    pending_free: Vec<Arc<Image>>,
}

impl ImageCache {
    fn insert_loaded(&mut self, key: (String, String), image: CachedAttachmentImage) {
        let bytes = image.image.bytes.len();
        self.tick += 1;
        if let Some(CacheEntry::Loaded { image, bytes, .. }) = self.map.insert(
            key.clone(),
            CacheEntry::Loaded {
                image,
                bytes,
                last_used: self.tick,
            },
        ) {
            self.loaded_bytes = self.loaded_bytes.saturating_sub(bytes);
            self.pending_free.push(image.image);
        }
        self.loaded_bytes += bytes;
        let shielded = protected().lock().unwrap().clone();
        while self.loaded_bytes > IMAGE_CACHE_BUDGET_BYTES {
            let oldest = self
                .map
                .iter()
                .filter(|(k, _)| **k != key && !shielded.contains(*k))
                .filter_map(|(k, e)| match e {
                    CacheEntry::Loaded { last_used, .. } => Some((*last_used, k.clone())),
                    _ => None,
                })
                .min();
            let Some((_, evict_key)) = oldest else { break };
            if let Some(CacheEntry::Loaded { image, bytes, .. }) = self.map.remove(&evict_key) {
                self.loaded_bytes = self.loaded_bytes.saturating_sub(bytes);
                self.pending_free.push(image.image);
            }
        }
    }
}

fn cache() -> &'static Mutex<ImageCache> {
    static CACHE: OnceLock<Mutex<ImageCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ImageCache::default()))
}

/// Keys shielded from LRU eviction — the open transcript's attachments. The
/// gpui list caches rendered rows across frames, so a VISIBLE thumbnail's
/// `last_used` tick can go stale and budget pressure evicted images still on
/// screen (user report: "images unload before they are scrolled out of
/// view"). The transcript replaces this set on every row sync; other chats'
/// images stay evictable, so the budget still bounds the cache overall.
fn protected() -> &'static Mutex<std::collections::HashSet<(String, String)>> {
    static PROTECTED: OnceLock<Mutex<std::collections::HashSet<(String, String)>>> =
        OnceLock::new();
    PROTECTED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Replace the eviction shield with the given keys (see [`protected`]).
pub fn protect_attachments(keys: std::collections::HashSet<(String, String)>) {
    *protected().lock().unwrap() = keys;
}

fn key(device_id: &str, path: &str) -> (String, String) {
    (device_id.to_string(), path.to_string())
}

pub fn attachment_snapshot(device_id: &str, path: &str) -> AttachmentSnapshot {
    let mut cache = cache().lock().unwrap();
    let tick = {
        cache.tick += 1;
        cache.tick
    };
    match cache.map.get_mut(&key(device_id, path)) {
        Some(CacheEntry::Loaded {
            image, last_used, ..
        }) => {
            *last_used = tick;
            AttachmentSnapshot::Loaded(image.clone())
        }
        Some(CacheEntry::Error { attempts, at }) => AttachmentSnapshot::Error {
            retry_in: retry_delay(attempts.saturating_sub(1)).saturating_sub(at.elapsed()),
        },
        Some(CacheEntry::Loading { .. }) => AttachmentSnapshot::Loading,
        None => {
            // Queued-send alias: the host materializes `pending://{id}/{name}`
            // at `{uploads}/{id8}-{name}` and rewrites the persisted ref to
            // that ABSOLUTE path — one the sender can't know up front (it's
            // the host's disk). The id8 basename prefix IS derivable though,
            // so the send seeds the bytes under an alias and this fallback
            // resolves the rewritten ref instantly instead of blanking the
            // thumbnail into a skeleton while the bytes round-trip
            // (2026-08-19 "photo disappears after it finishes sending").
            if let Some(image) = upload_alias_id8(path)
                .and_then(|id8| match cache.map.get(&alias_key(device_id, &id8)) {
                    Some(CacheEntry::Loaded { image, .. }) => Some(image.clone()),
                    _ => None,
                })
            {
                cache.insert_loaded(key(device_id, path), image.clone());
                return AttachmentSnapshot::Loaded(image);
            }
            AttachmentSnapshot::Loading
        }
    }
}

/// The uploadId fragment a committed upload's basename starts with
/// (`{id8}-{name}` per the engine's `Uploads::pending_target`). `None` when
/// the path can't be a committed upload.
fn upload_alias_id8(path: &str) -> Option<String> {
    let base = std::path::Path::new(path).file_name()?.to_str()?;
    let (id8, _) = base.split_at_checked(8)?;
    (base.as_bytes().get(8) == Some(&b'-')
        && id8.bytes().all(|b| b.is_ascii_alphanumeric()))
    .then(|| id8.to_string())
}

fn alias_key(device_id: &str, id8: &str) -> (String, String) {
    key(device_id, &format!("upload-alias://{id8}"))
}

/// Seed the just-sent image under its upload identity so the persisted
/// message's rewritten absolute ref (host-side path) resolves from the same
/// local bytes — see the alias fallback in [`attachment_snapshot`].
pub fn seed_attachment_alias(device_id: &str, upload_id: &str, name: &str, image: Arc<Image>) {
    let id8: String = upload_id.chars().take(8).collect();
    let (device, path) = alias_key(device_id, &id8);
    store_loaded(&device, &path, name.to_string().into(), image);
}

/// Release gpui's decoded copies of evicted images: the asset-system entry
/// AND the sprite-atlas tiles (`ImageSource::evict` — `remove_asset` alone
/// left the tiles resident forever). Pass the window being updated when
/// calling from a render path, since that window is detached from
/// `App::windows` during its own update. Cheap when nothing was evicted.
pub fn flush_evicted(mut window: Option<&mut gpui::Window>, cx: &mut gpui::App) {
    let evicted = std::mem::take(&mut cache().lock().unwrap().pending_free);
    for image in evicted {
        gpui::ImageSource::Image(image).evict(window.as_deref_mut(), cx);
    }
}

/// Claim the load for a source: `true` ⇒ the caller should start fetching now
/// (the entry is marked Loading so concurrent renders don't double-fetch).
/// Errored sources hand out a retry only after their backoff has elapsed.
pub fn begin_load(device_id: &str, path: &str) -> bool {
    let mut cache = cache().lock().unwrap();
    let entry = cache.map.entry(key(device_id, path));
    match entry {
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(CacheEntry::Loading { attempts: 0 });
            true
        }
        std::collections::hash_map::Entry::Occupied(mut o) => match o.get() {
            CacheEntry::Error { attempts, at }
                if at.elapsed() >= retry_delay(attempts.saturating_sub(1)) =>
            {
                let attempts = *attempts;
                o.insert(CacheEntry::Loading { attempts });
                true
            }
            _ => false,
        },
    }
}

pub fn store_loaded(device_id: &str, path: &str, name: SharedString, image: Arc<Image>) {
    cache()
        .lock()
        .unwrap()
        .insert_loaded(key(device_id, path), CachedAttachmentImage { name, image });
}

pub fn store_error(device_id: &str, path: &str) {
    let mut cache = cache().lock().unwrap();
    let attempts = match cache.map.get(&key(device_id, path)) {
        Some(CacheEntry::Loading { attempts }) => attempts + 1,
        Some(CacheEntry::Error { attempts, .. }) => *attempts,
        _ => 1,
    };
    cache.map.insert(
        key(device_id, path),
        CacheEntry::Error {
            attempts,
            at: Instant::now(),
        },
    );
}

/// Seed the cache after a successful upload (composer send path) so the just-
/// sent bubble's thumbnails render from local bytes instead of a round-trip.
pub fn seed_attachment(device_id: &str, path: &str, name: &str, image: Arc<Image>) {
    store_loaded(device_id, path, name.to_string().into(), image);
}

// ---------------------------------------------------------------------------
// Preview lightbox (attachment-ui.tsx AttachmentPreviewDialog)
// ---------------------------------------------------------------------------

/// A full-size preview target (staged strip or transcript thumbnail).
#[derive(Clone)]
pub struct PreviewImage {
    pub name: SharedString,
    pub image: Arc<Image>,
}

/// The bare lightbox: dim scrim, the image at ≤85vh/90vw, the file name under
/// it. Any click closes (the whole dialog is the close button, as in the
/// original's `cursor-zoom-out` figure), and so does Escape — `focus` must be
/// focused by the caller when the preview opens so the key reaches us.
pub fn lightbox(
    viewport: Size<gpui::Pixels>,
    preview: &PreviewImage,
    focus: &gpui::FocusHandle,
    on_close: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let max_h = px(f32::from(viewport.height) * 0.85);
    let max_w = px(f32::from(viewport.width) * 0.9);
    let on_close = std::rc::Rc::new(on_close);
    let close_on_key = on_close.clone();
    gpui::deferred(
        gpui::anchored()
            .position(gpui::point(px(0.0), px(0.0)))
            .child(
                div()
                    .id("attachment-lightbox")
                    .occlude()
                    .track_focus(focus)
                    .w(viewport.width)
                    .h(viewport.height)
                    .bg(crate::popover::scrim_alpha(0.7))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.0))
                    .cursor_pointer()
                    .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            close_on_key(window, cx);
                        }
                    })
                    .on_click(move |_, window, cx| on_close(window, cx))
                    .child(
                        img(preview.image.clone())
                            .object_fit(ObjectFit::Contain)
                            .max_h(max_h)
                            .max_w(max_w)
                            .rounded(px(6.0))
                            .shadow_2xl(),
                    )
                    .child(
                        div()
                            .max_w(max_w)
                            .overflow_hidden()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(ink(0.45))
                            .child(preview.name.clone()),
                    ),
            ),
    )
    .priority(3)
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_attachments_round_trips_through_parse() {
        let paths = vec!["/data/uploads/ab-cat.png".to_string(), "/x/dog.jpg".into()];
        let content = with_attachments("look at these", &paths);
        let parsed = parse_user_message_images(&content);
        assert_eq!(parsed.text, "look at these");
        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].path, "/data/uploads/ab-cat.png");
        assert_eq!(parsed.attachments[0].name, "ab-cat.png");
        assert_eq!(parsed.attachments[1].name, "dog.jpg");
        assert_eq!(parsed.attachments[0].id, "0:/data/uploads/ab-cat.png");
    }

    #[test]
    fn image_only_send_hides_placeholder_body() {
        let content = with_attachments("", &["/a/b.png".to_string()]);
        assert!(content.starts_with(ATTACHMENT_ONLY_TEXT));
        let parsed = parse_user_message_images(&content);
        assert_eq!(parsed.text, "");
        assert_eq!(parsed.attachments.len(), 1);
    }

    #[test]
    fn plain_text_passes_through_unchanged() {
        assert_eq!(with_attachments("hello", &[]), "hello");
        let parsed = parse_user_message_images("hello\n\nno images here");
        assert!(parsed.attachments.is_empty());
        assert_eq!(parsed.text, "hello\n\nno images here");
    }

    #[test]
    fn marker_is_case_insensitive_and_requires_ref_lines() {
        let parsed = parse_user_message_images(
            "hi\n\nATTACHED IMAGES (local files — open them to view):\n- /p/q.png",
        );
        assert_eq!(parsed.attachments.len(), 1);
        // A trailer with no valid `- path` lines is left as plain text.
        let empty = parse_user_message_images(
            "hi\n\nAttached images (local files — open them to view):\nnothing",
        );
        assert!(empty.attachments.is_empty());
        assert!(empty.text.contains("Attached images"));
    }

    #[test]
    fn rail_text_summarizes_image_only_sends() {
        let one = with_attachments("", &["/a/b.png".to_string()]);
        assert_eq!(user_message_rail_text(&one), "Attached image");
        let two = with_attachments("", &["/a/b.png".to_string(), "/c/d.png".into()]);
        assert_eq!(user_message_rail_text(&two), "2 attached images");
        let with_text = with_attachments("fix this", &["/a/b.png".to_string()]);
        assert_eq!(user_message_rail_text(&with_text), "fix this");
        assert_eq!(user_message_rail_text("plain"), "plain");
    }

    #[test]
    fn ensure_extension_matches_browser_heuristic() {
        assert_eq!(ensure_extension("shot.png", ImageFormat::Png), "shot.png");
        assert_eq!(ensure_extension("image", ImageFormat::Png), "image.png");
        assert_eq!(
            ensure_extension("photo.j", ImageFormat::Jpeg),
            "photo.j.jpg"
        );
        assert_eq!(
            ensure_extension("archive.tar.gz", ImageFormat::Png),
            "archive.tar.gz"
        );
    }

    #[test]
    fn supported_formats_match_engine_jail() {
        for (ext, expect) in [
            ("png", Some(ImageFormat::Png)),
            ("JPG", Some(ImageFormat::Jpeg)),
            ("webp", Some(ImageFormat::Webp)),
            ("svg", Some(ImageFormat::Svg)),
            ("ico", None),
            ("txt", None),
        ] {
            assert_eq!(
                format_by_extension(Path::new(&format!("f.{ext}"))),
                expect,
                "ext {ext}"
            );
        }
    }

    #[test]
    fn retry_ladder_is_2s_doubling_capped_at_15s() {
        assert_eq!(retry_delay(0), Duration::from_millis(2_000));
        assert_eq!(retry_delay(1), Duration::from_millis(4_000));
        assert_eq!(retry_delay(2), Duration::from_millis(8_000));
        assert_eq!(retry_delay(3), Duration::from_millis(15_000));
        assert_eq!(retry_delay(9), Duration::from_millis(15_000));
    }

    #[test]
    fn upload_chunk_fits_the_relay_frame_ceiling() {
        // Cloudflare caps a WebSocket message at 1 MiB; the chunk rides one
        // relay frame with a small JSON envelope + uleb header.
        assert!(UPLOAD_CHUNK_B64_CHARS + 1_024 < 1_048_576);
        // A slice of the whole-file base64 must stay independently decodable.
        assert_eq!(UPLOAD_CHUNK_B64_CHARS % 4, 0);
    }

    #[test]
    fn chunk_ranges_cover_the_buffer_exactly() {
        // Empty file: one empty chunk (the commit needs the id staged).
        assert_eq!(chunk_ranges(0), vec![(0, 0..0)]);
        // Exact multiple: no trailing empty chunk.
        let exact = chunk_ranges(UPLOAD_CHUNK_B64_CHARS * 2);
        assert_eq!(exact.len(), 2);
        assert_eq!(exact[1], (1, UPLOAD_CHUNK_B64_CHARS..UPLOAD_CHUNK_B64_CHARS * 2));
        // Partial tail.
        let partial = chunk_ranges(UPLOAD_CHUNK_B64_CHARS + 7);
        assert_eq!(partial.len(), 2);
        assert_eq!(
            partial[1],
            (1, UPLOAD_CHUNK_B64_CHARS..UPLOAD_CHUNK_B64_CHARS + 7)
        );
        // Ranges tile the buffer: contiguous, in order, fully covering.
        let mut expected_start = 0;
        for (seq, range) in &partial {
            assert_eq!(range.start, expected_start, "seq {seq} contiguous");
            expected_start = range.end;
        }
        assert_eq!(expected_start, UPLOAD_CHUNK_B64_CHARS + 7);
    }

    #[test]
    fn attachment_deadline_scales_and_caps() {
        // A one-chunk screenshot fails within ~2 minutes, not hours.
        assert_eq!(attachment_deadline(1), Duration::from_secs(135));
        // A max-size upload is still bounded.
        assert_eq!(attachment_deadline(1_000), Duration::from_secs(900));
    }
}
