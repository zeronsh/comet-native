package sh.zeron.android.data

import sh.zeron.android.AppContextHolder
import java.io.File

/**
 * Durable staging for queued attachments (iOS UploadStash): the bytes of every
 * pending://-referenced attachment persist on device, keyed by uploadId, so
 * the delivery escort can re-derive its transfers after a retry, a relaunch,
 * or a tapped \"retry\" — the command in the doc names the ref; the stash holds
 * the bytes it points at.
 */
object UploadStash {
    const val PENDING_REF_PREFIX = "pending://"

    /** Test seam — JVM unit tests point this at a temp dir. */
    @Volatile
    var testDirectory: File? = null

    private fun directory(): File {
        val base = testDirectory ?: File(AppContextHolder.context?.filesDir ?: File("."), "zeron-uploads")
        base.mkdirs()
        return base
    }

    /** The persisted ref (uploads.rs PENDING_REF_PREFIX): the ORIGINAL file name rides it. */
    fun pendingRef(uploadId: String, name: String): String =
        "$PENDING_REF_PREFIX$uploadId/$name"

    /** Parse a pending ref back into (uploadId, fileName). */
    fun parseRef(ref: String): Pair<String, String>? {
        if (!ref.startsWith(PENDING_REF_PREFIX)) return null
        val body = ref.removePrefix(PENDING_REF_PREFIX)
        val slash = body.indexOf('/')
        if (slash <= 0 || slash == body.length - 1) return null
        val id = body.substring(0, slash)
        val name = body.substring(slash + 1)
        return if (id.isEmpty() || name.isEmpty()) null else id to name
    }

    private fun file(uploadId: String): File {
        val safe = uploadId.filter { it.isLetterOrDigit() || it == '-' }
        return File(directory(), "$safe.bin")
    }

    fun save(uploadId: String, data: ByteArray) {
        runCatching { file(uploadId).writeBytes(data) }
    }

    fun load(uploadId: String): ByteArray? = runCatching { file(uploadId).readBytes() }.getOrNull()

    fun delete(uploadId: String) {
        runCatching { file(uploadId).delete() }
    }

    /** Drop entries older than the command TTL (24h) — their commands expired. */
    fun sweep(ttlMs: Long = 24 * 60 * 60 * 1000L) {
        val cutoff = System.currentTimeMillis() - ttlMs
        directory().listFiles()?.forEach { f ->
            if (f.lastModified() < cutoff) f.delete()
        }
    }
}
