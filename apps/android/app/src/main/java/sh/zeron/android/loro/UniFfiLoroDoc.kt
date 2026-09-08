package sh.zeron.android.loro

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONObject
import sh.zeron.android.core.AppError

class UniFfiLoroDoc(private val handle: Any? = null) : LoroDoc {
    override suspend fun importBytes(bytes: ByteArray) = withContext(Dispatchers.IO) {
        if (bytes.isEmpty()) throw AppError.Loro("empty bytes")
        // Real impl delegates to zeron_loro_android UniFFI: doc_import_bytes(handle, bytes)
        // Errors map to AppError.Loro without leaking payload.
    }
    override suspend fun exportSnapshot(): ByteArray = withContext(Dispatchers.IO) {
        // Real: doc_export_snapshot(handle)
        ByteArray(0)
    }
    override suspend fun getDeepValueJson(): String = withContext(Dispatchers.IO) { "{}" }

    override suspend fun appendCommand(kind: String, payloadJson: String, issuedBy: String): Map<String, Any> =
        withContext(Dispatchers.IO) {
            // Real: append to `commands` LoroList; returns client-minted entry id.
            mapOf("id" to java.util.UUID.randomUUID().toString().lowercase(), "kind" to kind)
        }

    override suspend fun closeDoc() = withContext(Dispatchers.IO) { }
    override fun close() {}
}

open class FakeLoroDoc(var json: String = "{}") : LoroDoc {
    var closed = false
    override suspend fun importBytes(bytes: ByteArray) { check(!closed) }
    override suspend fun exportSnapshot(): ByteArray = json.toByteArray()
    override suspend fun getDeepValueJson(): String = json
    override suspend fun appendCommand(kind: String, payloadJson: String, issuedBy: String): Map<String, Any> {
        check(!closed)
        return mapOf("id" to java.util.UUID.randomUUID().toString().lowercase(), "kind" to kind)
    }
    override suspend fun closeDoc() { closed = true }
    override fun close() { closed = true }
}
