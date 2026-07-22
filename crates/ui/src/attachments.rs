//! Attachments (feature-inventory §1.7/§1.8): the composer's staged images,
//! the chunked upload to the chat's host device, the plain-text attachment-ref
//! transport that rides the prompt, the transcript read-back cache, and the
//! full-size preview lightbox.
//!
//! Ports of comet's `composer/use-attachments.ts` (staging/upload),
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
use gpui::{
    AnyElement, BackgroundExecutor, Image, ImageFormat, ObjectFit, SharedString, Size,
    StyledImage as _, div, img, prelude::*, px,
};

use crate::state::EngineHandle;
use crate::theme::white_alpha;
use comet_rpc::methods;

/// use-attachments.ts `MAX_ATTACHMENT_BYTES`.
pub const MAX_ATTACHMENT_BYTES: u64 = 24 * 1024 * 1024;
/// Base64 chars per `UploadChunk` (comet state.ts `UPLOAD_CHUNK` — sized for
/// the relay when the target device is remote).
pub const UPLOAD_CHUNK_B64_CHARS: usize = 60_000;
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
/// `(body_end, refs_start)` byte offsets — the tolerant equivalent of comet's
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
async fn call_with_timeout(
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

/// Chunked upload: base64 the bytes, `UploadChunk{uploadId,seq,data}` per 60KB
/// slice (positional `seq` makes the cheap retry idempotent), then
/// `UploadCommit{uploadId,fileName}` → the durable absolute path on the target
/// device. Errors return the raw cause (the composer shows friendly copy).
pub async fn upload_attachment(
    engine: &EngineHandle,
    executor: &BackgroundExecutor,
    target_device_id: Option<&str>,
    attachment: &StagedAttachment,
) -> Result<String, String> {
    let b64 = BASE64.encode(attachment.bytes());
    let upload_id = uuid::Uuid::new_v4().to_string();
    let mut start = 0usize;
    let mut seq = 0u64;
    loop {
        let end = (start + UPLOAD_CHUNK_B64_CHARS).min(b64.len());
        let params = with_target(
            serde_json::json!({ "uploadId": upload_id, "seq": seq, "data": &b64[start..end] }),
            target_device_id,
        );
        let timeout = if seq == 0 {
            FIRST_CHUNK_TIMEOUT
        } else {
            CHUNK_TIMEOUT
        };
        // One transient blip must not abort a ~400-chunk upload; `seq` slots
        // are idempotent engine-side, so a blind re-send is safe (timeouts
        // retry too, like the original's per-chunk `withTimeout` + retry ×2).
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
                    tracing::debug!(error = %err, seq, "upload chunk retry");
                }
                Err(err) => return Err(err),
            }
        }
        start = end;
        seq += 1;
        if start >= b64.len() {
            break;
        }
    }
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
}

/// A transcript image read back from the owning device.
pub struct LoadedAttachmentImage {
    pub name: String,
    pub image: Arc<Image>,
}

/// `ReadAttachmentChunk` loop: 45KB base64 chunks until `done` (bounded, with
/// the same stuck-offset guard as comet's `readAttachmentImage`).
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
    Loading { attempts: u32 },
    Loaded(CachedAttachmentImage),
    Error { attempts: u32, at: Instant },
}

fn retry_delay(attempts: u32) -> Duration {
    Duration::from_millis((2_000u64 << attempts.min(3)).min(15_000))
}

fn cache() -> &'static Mutex<HashMap<(String, String), CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(device_id: &str, path: &str) -> (String, String) {
    (device_id.to_string(), path.to_string())
}

pub fn attachment_snapshot(device_id: &str, path: &str) -> AttachmentSnapshot {
    match cache().lock().unwrap().get(&key(device_id, path)) {
        Some(CacheEntry::Loaded(image)) => AttachmentSnapshot::Loaded(image.clone()),
        Some(CacheEntry::Error { attempts, at }) => AttachmentSnapshot::Error {
            retry_in: retry_delay(attempts.saturating_sub(1)).saturating_sub(at.elapsed()),
        },
        _ => AttachmentSnapshot::Loading,
    }
}

/// Claim the load for a source: `true` ⇒ the caller should start fetching now
/// (the entry is marked Loading so concurrent renders don't double-fetch).
/// Errored sources hand out a retry only after their backoff has elapsed.
pub fn begin_load(device_id: &str, path: &str) -> bool {
    let mut cache = cache().lock().unwrap();
    let entry = cache.entry(key(device_id, path));
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
    cache().lock().unwrap().insert(
        key(device_id, path),
        CacheEntry::Loaded(CachedAttachmentImage { name, image }),
    );
}

pub fn store_error(device_id: &str, path: &str) {
    let mut cache = cache().lock().unwrap();
    let attempts = match cache.get(&key(device_id, path)) {
        Some(CacheEntry::Loading { attempts }) => attempts + 1,
        Some(CacheEntry::Error { attempts, .. }) => *attempts,
        _ => 1,
    };
    cache.insert(
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
/// original's `cursor-zoom-out` figure).
pub fn lightbox(
    viewport: Size<gpui::Pixels>,
    preview: &PreviewImage,
    on_close: impl Fn(&mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    let max_h = px(f32::from(viewport.height) * 0.85);
    let max_w = px(f32::from(viewport.width) * 0.9);
    gpui::deferred(
        gpui::anchored()
            .position(gpui::point(px(0.0), px(0.0)))
            .child(
                div()
                    .id("attachment-lightbox")
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    .bg(gpui::hsla(0.0, 0.0, 0.0, 0.7))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(12.0))
                    .cursor_pointer()
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
                            .text_size(px(11.0))
                            .text_color(white_alpha(0.45))
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
}
