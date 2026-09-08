package sh.zeron.android.data

import android.graphics.Bitmap
import android.graphics.BitmapFactory

/**
 * Decoded transcript attachment thumbnails keyed by (deviceId, path) — the
 * Android port of iOS AttachmentImageCache. Bytes come from the owning
 * device's relay (ReadAttachmentChunk loop, driven by AppViewModel); entries
 * sit in loading/loaded/error states with a bounded encoded-byte LRU. Failed
 * loads retry on the 2s→15s ladder.
 */
object AttachmentImageCache {
    sealed class Snapshot {
        object Loading : Snapshot()
        data class Loaded(val name: String, val bitmap: Bitmap) : Snapshot()
        object Error : Snapshot()
    }

    private data class Key(val deviceId: String, val path: String)

    private sealed class Entry {
        data class Loading(val attempts: Int) : Entry()
        data class Loaded(val name: String, val bitmap: Bitmap, val bytes: Int, val lastUsed: Long) : Entry()
        data class Error(val attempts: Int, val at: Long) : Entry()
    }

    private const val BUDGET_BYTES = 64 * 1024 * 1024
    private const val MAX_READ_CHUNKS = 1_000

    private val entries = LinkedHashMap<Key, Entry>(32, 0.75f, true)
    private var tick = 0L
    private var loadedBytes = 0

    @Synchronized
    fun snapshot(deviceId: String, path: String): Snapshot = when (val e = entries[Key(deviceId, path)]) {
        is Entry.Loaded -> Snapshot.Loaded(e.name, e.bitmap)
        is Entry.Error -> {
            if (System.currentTimeMillis() - e.at < retryDelay(e.attempts)) Snapshot.Error
            else Snapshot.Loading // ladder elapsed — the next load owns it
        }
        is Entry.Loading, null -> Snapshot.Loading
    }

    @Synchronized
    fun isLoadedOrLoading(deviceId: String, path: String): Boolean =
        entries[Key(deviceId, path)] is Entry.Loaded || entries[Key(deviceId, path)] is Entry.Loading

    @Synchronized
    fun markLoading(deviceId: String, path: String, attempts: Int = 0) {
        entries[Key(deviceId, path)] = Entry.Loading(attempts)
    }

    @Synchronized
    fun markError(deviceId: String, path: String, attempts: Int) {
        entries[Key(deviceId, path)] = Entry.Error(attempts, System.currentTimeMillis())
    }

    @Synchronized
    fun seed(deviceId: String, path: String, name: String, bytes: ByteArray) {
        val bitmap = BitmapFactory.decodeByteArray(bytes, 0, bytes.size) ?: return
        store(Key(deviceId, path), name, bitmap, bytes.size)
    }

    @Synchronized
    fun store(deviceId: String, path: String, name: String, bitmap: Bitmap, bytes: Int) {
        store(Key(deviceId, path), name, bitmap, bytes)
    }

    private fun store(key: Key, name: String, bitmap: Bitmap, bytes: Int) {
        tick += 1
        (entries[key] as? Entry.Loaded)?.let { loadedBytes -= it.bytes }
        entries[key] = Entry.Loaded(name, bitmap, bytes, tick)
        loadedBytes += bytes
        while (loadedBytes > BUDGET_BYTES) {
            val oldest = entries.entries
                .filter { it.key != key && it.value is Entry.Loaded }
                .minByOrNull { (it.value as Entry.Loaded).lastUsed }
                ?: break
            loadedBytes -= (oldest.value as Entry.Loaded).bytes
            entries.remove(oldest.key)
        }
    }

    private fun retryDelay(attempts: Int): Long {
        val exp = (attempts - 1).coerceIn(0, 3)
        return minOf(2L shl exp, 15L) * 1000
    }

    @Synchronized
    fun clear() {
        entries.clear()
        loadedBytes = 0
    }
}
