package sh.zeron.android.protocol

object LoroProtocol {
    const val MAGIC: Byte = 0x6C.toByte()
    fun encode(type: Byte, payload: ByteArray): ByteArray {
        val out = ByteArray(1 + 4 + payload.size)
        out[0] = type
        val len = payload.size
        out[1] = (len and 0xFF).toByte()
        out[2] = ((len shr 8) and 0xFF).toByte()
        out[3] = ((len shr 16) and 0xFF).toByte()
        out[4] = ((len shr 24) and 0xFF).toByte()
        payload.copyInto(out, 5)
        return out
    }
    fun decode(bytes: ByteArray): Pair<Byte, ByteArray>? {
        if (bytes.size < 5) return null
        val len = (bytes[1].toInt() and 0xFF) or ((bytes[2].toInt() and 0xFF) shl 8) or ((bytes[3].toInt() and 0xFF) shl 16) or ((bytes[4].toInt() and 0xFF) shl 24)
        if (bytes.size < 5 + len) return null
        return bytes[0] to bytes.copyOfRange(5, 5 + len)
    }
}
