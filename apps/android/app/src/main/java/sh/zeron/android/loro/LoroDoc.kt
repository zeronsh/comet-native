package sh.zeron.android.loro

interface LoroDoc : AutoCloseable {
    suspend fun importBytes(bytes: ByteArray)
    suspend fun exportSnapshot(): ByteArray
    suspend fun getDeepValueJson(): String
    suspend fun appendCommand(kind: String, payloadJson: String, issuedBy: String): Map<String, Any>
    suspend fun closeDoc()
}