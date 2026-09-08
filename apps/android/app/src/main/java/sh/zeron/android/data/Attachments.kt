package sh.zeron.android.data

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import org.json.JSONObject
import sh.zeron.android.sync.DeviceRelayClient
import java.io.ByteArrayOutputStream
import java.util.UUID
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.withTimeoutOrNull
import java.util.Base64

/**
 * Attachments — the Android port of apps/ios/Zeron/Composer/Attachments.swift
 * (crates/ui/src/attachments.rs): composer staging, the chunked upload to the
 * chat's host device, and the plain-text attachment-ref transport that rides
 * the prompt.
 *
 * The transport is TEXT: local paths appended to the prompt under an
 * "Attached images (local files — open them to view):" trailer — that's what
 * persists in the doc, what the agent reads, and what every client parses
 * back out to render thumbnails. RunRequest.attachments additionally carries
 * the paths so a harness can inline the bytes.
 */

/** The body used for image-only sends (message-attachments.ts). */
const val ATTACHMENT_ONLY_TEXT = "See the attached image(s)."

/** use-attachments.ts `withAttachments`: plain local paths appended to text. */
fun withAttachments(text: String, paths: List<String>): String {
    if (paths.isEmpty()) return text
    val body = if (text.isEmpty()) ATTACHMENT_ONLY_TEXT else text
    val refs = paths.joinToString("\n") { "- $it" }
    return "$body\n\nAttached images (local files — open them to view):\n$refs"
}

/** An attachment ref parsed back out of a user message's text. */
data class UserImageAttachment(val id: String, val path: String, val name: String)

data class ParsedUserMessage(val text: String, val attachments: List<UserImageAttachment>)

private fun nameFromPath(path: String): String {
    val name = path.substringAfterLast('/')
    return name.ifEmpty { "image" }
}

/**
 * message-attachments.ts `parseUserMessageImages`: split the visible prompt
 * from its attachment-ref trailer (case-insensitive marker, `- path` lines).
 */
fun parseUserMessageImages(content: String): ParsedUserMessage {
    val lines = content.split("\n")
    var markerIx: Int? = null
    for (ix in lines.indices) {
        if (ix == 0) continue
        val line = lines[ix].trim()
        if (lines[ix - 1].trim().isEmpty() &&
            line.lowercase().startsWith("attached images (local files") &&
            line.endsWith("):")
        ) {
            markerIx = ix
            break
        }
    }
    val marker = markerIx ?: return ParsedUserMessage(content, emptyList())
    val attachments = lines.drop(marker + 1).mapNotNull { raw ->
        val trimmed = raw.trim()
        if (!trimmed.startsWith("- ")) return@mapNotNull null
        val path = trimmed.removePrefix("- ").trim()
        path.takeIf { it.isNotEmpty() }
    }.mapIndexed { ix, path ->
        UserImageAttachment(id = "$ix:$path", path = path, name = nameFromPath(path))
    }
    if (attachments.isEmpty()) return ParsedUserMessage(content, emptyList())
    val body = lines.take(marker - 1).joinToString("\n").trim()
    return ParsedUserMessage(
        text = if (body == ATTACHMENT_ONLY_TEXT) "" else body,
        attachments = attachments,
    )
}

// MARK: - Staging

/** use-attachments.ts MAX_ATTACHMENT_BYTES. */
const val MAX_ATTACHMENT_BYTES = 24 * 1024 * 1024

/** Base64 chars per UploadChunk (attachments.rs UPLOAD_CHUNK_B64_CHARS). */
const val UPLOAD_CHUNK_B64_CHARS = 680_000
/** Chunks in flight at once (attachments.rs UPLOAD_CONCURRENCY). */
const val UPLOAD_CONCURRENCY = 3

/** attachments.rs attachment_deadline. */
fun attachmentDeadlineSeconds(chunkCount: Int): Long = minOf(120L + 15L * chunkCount, 900L)

/**
 * An image staged in the composer, before upload. Bytes are what uploads; the
 * decoded bitmap feeds thumbnails and the post-send cache seed.
 */
data class StagedAttachment(val id: String, val name: String, val data: ByteArray, val bitmap: Bitmap) {
    override fun equals(other: Any?): Boolean = other is StagedAttachment && other.id == id
    override fun hashCode(): Int = id.hashCode()
}

/**
 * Stage picked photo bytes (iOS StagedAttachment.stage): keep formats the
 * engine's read-back jail serves (png/jpg/gif/webp) as-is; transcode
 * everything else (HEIC camera shots, mainly) to JPEG. Rejects > 24 MB.
 */
fun stageAttachment(data: ByteArray): StagedAttachment? {
    var bytes = data
    var ext = sniffExtension(data)
    if (ext == null) {
        val bitmap = BitmapFactory.decodeByteArray(data, 0, data.size) ?: return null
        val out = ByteArrayOutputStream()
        if (!bitmap.compress(Bitmap.CompressFormat.JPEG, 90, out)) return null
        bytes = out.toByteArray()
        ext = "jpg"
    }
    if (bytes.size > MAX_ATTACHMENT_BYTES) return null
    val bitmap = BitmapFactory.decodeByteArray(bytes, 0, bytes.size) ?: return null
    val id = UUID.randomUUID().toString().lowercase()
    return StagedAttachment(
        id = id,
        name = "photo-${id.take(8)}.$ext",
        data = bytes,
        bitmap = bitmap,
    )
}

/** Magic-byte sniff for the formats both ends support. */
private fun sniffExtension(data: ByteArray): String? {
    if (data.size < 12) return null
    val b = data
    if (b[0] == 0x89.toByte() && b[1] == 'P'.code.toByte() && b[2] == 'N'.code.toByte() && b[3] == 'G'.code.toByte()) return "png"
    if (b[0] == 0xFF.toByte() && b[1] == 0xD8.toByte() && b[2] == 0xFF.toByte()) return "jpg"
    if (b[0] == 'G'.code.toByte() && b[1] == 'I'.code.toByte() && b[2] == 'F'.code.toByte() && b[3] == '8'.code.toByte()) return "gif"
    if (b[0] == 'R'.code.toByte() && b[1] == 'I'.code.toByte() && b[2] == 'F'.code.toByte() && b[3] == 'F'.code.toByte() &&
        b[8] == 'W'.code.toByte() && b[9] == 'E'.code.toByte() && b[10] == 'B'.code.toByte() && b[11] == 'P'.code.toByte()
    ) return "webp"
    return null
}

/** One queued-flow attachment: client-minted uploadId + the bytes the escort pushes. */
data class AttachmentTransfer(val uploadId: String, val name: String, val data: ByteArray)

/**
 * Chunked upload straight to the host device's relay room: base64 slices as
 * `UploadChunk {uploadId, seq, data}` — positional `seq` makes retries
 * idempotent — 3 in flight at once, then `UploadCommit {uploadId, fileName}`
 * → the durable absolute path on the host. The whole upload races a
 * chunk-count-scaled deadline; per-chunk retries stagger by seq.
 *
 * The caller mints `uploadId`: on the queued flow the id is the persisted
 * `pending://` ref's identity, and retries re-commit the same file. Progress
 * reports committed binary bytes, clamped at 0.99 — the commit owns the last
 * point. (iOS uploadAttachmentChunked port; this repo's hosts are all ≥ 0.2.12
 * so the legacy host-staged path exists for gates only.)
 */
suspend fun uploadAttachmentChunked(
    relay: DeviceRelayClient,
    name: String,
    data: ByteArray,
    uploadId: String? = null,
    progress: ((Double) -> Unit)? = null,
): Result<String> {
    val id = uploadId ?: UUID.randomUUID().toString().lowercase()
    // Slice the BINARY at a % 3 == 0 boundary (b64 chars / 4 * 3): each
    // slice's independent base64 then concatenates to the whole file's.
    val chunkBytes = UPLOAD_CHUNK_B64_CHARS / 4 * 3
    val ranges = mutableListOf<Pair<Int, Int>>()
    var offset = 0
    repeat(if (data.isEmpty()) 1 else ((data.size + chunkBytes - 1) / chunkBytes).coerceAtLeast(1)) {
        val end = minOf(offset + chunkBytes, data.size)
        ranges += offset to end
        offset = end
    }

    var done = 0
    val total = maxOf(data.size, 1)
    suspend fun pushChunk(seq: Int, range: Pair<Int, Int>): Result<Unit> {
        val slice = Base64.getEncoder().encodeToString(data.copyOfRange(range.first, range.second))
        val timeout = if (seq < UPLOAD_CONCURRENCY) 90L else 30L
        var attempt = 0
        while (true) {
            val result = relay.call(
                "UploadChunk",
                JSONObject().put("uploadId", id).put("seq", seq).put("data", slice),
                timeoutSeconds = timeout,
            )
            if (result.isSuccess) break
            attempt += 1
            if (attempt >= 3) return Result.failure(result.exceptionOrNull() ?: Exception("upload failed"))
            delay(50L * attempt * (seq + 1))
        }
        done += range.second - range.first
        progress?.invoke(minOf(done.toDouble() / total, 0.99))
        return Result.success(Unit)
    }

    return coroutineScope {
        val deadline = attachmentDeadlineSeconds(ranges.size)
        val upload = async {
            val window = mutableListOf<kotlinx.coroutines.Deferred<Result<Unit>>>()
            var next = 0
            while (next < ranges.size) {
                while (window.size < UPLOAD_CONCURRENCY && next < ranges.size) {
                    val seq = next
                    window += async { pushChunk(seq, ranges[seq]) }
                    next += 1
                }
                val first = window.removeAt(0).await()
                if (first.isFailure) return@async Result.failure(first.exceptionOrNull()!!)
            }
            // Commit (outlasts the engine's assemble + best-effort mirror).
            relay.call(
                "UploadCommit",
                JSONObject().put("uploadId", id).put("fileName", name),
                timeoutSeconds = 150L,
            ).map { ok -> JSONObject(ok).optString("path") }
        }
        // The whole upload races the deadline; timeout cancels the window.
        withTimeoutOrNull(deadline * 1000) { upload.await() }
            ?: Result.failure(Exception("upload exceeded the ${deadline}s deadline"))
    }
}
