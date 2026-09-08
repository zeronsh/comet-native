package sh.zeron.android.data

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import sh.zeron.android.AppContextHolder
import sh.zeron.android.loro.LoroDoc
import java.io.File
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * On-device Loro doc persistence — the iOS DocDisk / engine DocsStore, in file
 * form: one chat2 snapshot per doc under files. The snapshot loads BEFORE the
 * room join, so the UI renders instantly from local state (offline included)
 * and the join's backfill is incremental instead of a full snapshot.
 *
 * chat2 lineage: `c2_<id>.loro` = 8-byte magic + UInt64 LE room cursor +
 * snapshot, written atomically in ONE file so doc content and cursor can never
 * diverge (a restored doc that disagreed with its own cursor was the root of
 * the redownload-forever class).
 */
object DocDisk {
    private const val CHAT2_MAGIC = "C2SNAP01"

    /** Test seam — JVM unit tests point this at a temp dir. */
    @Volatile
    var testDirectory: File? = null

    private fun directory(): File {
        val base = testDirectory ?: File(AppContextHolder.context?.filesDir ?: File("."), "ZeronDocs")
        base.mkdirs()
        return base
    }

    private fun chat2File(id: String): File {
        val safe = id.replace("/", "_")
        return File(directory(), "c2_$safe.loro")
    }

    /**
     * Import the chat2 snapshot; returns its cursor, or null when absent or
     * unreadable (the caller starts fresh at cursor 0 — the room re-serves).
     */
    suspend fun loadChat2(doc: LoroDoc, id: String): Long? {
        val data = runCatching { chat2File(id).readBytes() }.getOrNull() ?: return null
        if (data.size < 16) return null
        val magic = CHAT2_MAGIC.toByteArray()
        if (!data.copyOfRange(0, 8).contentEquals(magic)) return null
        val cursor = ByteBuffer.wrap(data, 8, 8).order(ByteOrder.LITTLE_ENDIAN).long
        if (data.size > 16) {
            val imported = runCatching {
                doc.importBytes(data.copyOfRange(16, data.size))
            }.isSuccess
            if (!imported) return null
        }
        return cursor
    }

    /** Atomically persist the chat2 doc snapshot + its room cursor. */
    suspend fun saveChat2(doc: LoroDoc, id: String, cursor: Long) {
        val snapshot = runCatching { doc.exportSnapshot() }.getOrNull() ?: return
        val cursorBytes = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN)
            .putLong(cursor).array()
        val data = CHAT2_MAGIC.toByteArray() + cursorBytes + snapshot
        val target = chat2File(id)
        val tmp = File(target.parentFile, "${target.name}.tmp")
        runCatching {
            tmp.writeBytes(data)
            if (!tmp.renameTo(target)) {
                target.delete()
                tmp.renameTo(target)
            }
        }
    }

    /** The snapshot file's mtime — a cheap change signal for badge re-scans. */
    fun snapshotStamp(id: String): Long? =
        chat2File(id).takeIf { it.isFile() }?.lastModified()

    /** The chat2 snapshot's Loro bytes (after the magic + cursor header). */
    fun readChat2Snapshot(id: String): ByteArray? {
        val data = runCatching { chat2File(id).readBytes() }.getOrNull() ?: return null
        if (data.size < 16) return null
        if (!data.copyOfRange(0, 8).contentEquals(CHAT2_MAGIC.toByteArray())) return null
        return data.copyOfRange(16, data.size)
    }

    /** Sign-out hygiene: local doc state belongs to the signed-in identity. */
    fun wipeAll() {
        runCatching { directory().deleteRecursively() }
    }
}

/**
 * Debounced snapshot persistence (iOS DocSaver): poke on every change; `save`
 * runs ~1.5s after the last poke, and [flush] forces it (backgrounding, store
 * teardown).
 */
class DocSaver(
    private val scope: CoroutineScope,
    private val save: suspend () -> Unit,
) {
    private var generation = 0
    private var dirty = false
    private var job: Job? = null

    fun poke() {
        dirty = true
        generation += 1
        val expected = generation
        job?.cancel()
        job = scope.launch {
            delay(1_500)
            if (this@DocSaver.generation == expected) flush()
        }
    }

    suspend fun flush() {
        if (!dirty) return
        dirty = false
        runCatching { save() }
    }
}
