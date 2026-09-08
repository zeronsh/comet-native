package sh.zeron.android.protocol

import org.json.JSONObject

/**
 * chat2 wire codec — binary frames `[type u8][headerLen u32 LE][header JSON][payload]`
 * (crates/sync/src/chat_frames.rs / edge/src/chat-frames.ts). Payloads are opaque
 * bytes: Loro updates, checkpoint frontiers, presence ephemera.
 */
object Chat2Codec {
    const val HELLO: Byte = 0x01
    const val STATE: Byte = 0x02
    const val ROWS_REQ: Byte = 0x03
    const val ROW: Byte = 0x04
    const val ROWS_DONE: Byte = 0x05
    const val PUSH: Byte = 0x06
    const val ACK: Byte = 0x07
    const val PRESENCE: Byte = 0x08
    const val PROBE: Byte = 0x09
    const val PROBE_OK: Byte = 0x0a
    const val ERROR: Byte = 0x0b

    const val MAX_HEADER_BYTES = 4096

    data class Frame(val kind: Byte, val header: JSONObject, val payload: ByteArray)

    fun encode(kind: Byte, header: JSONObject, payload: ByteArray = ByteArray(0)): ByteArray {
        val h = header.toString().toByteArray(Charsets.UTF_8)
        val out = ByteArray(5 + h.size + payload.size)
        out[0] = kind
        out[1] = (h.size and 0xFF).toByte()
        out[2] = ((h.size shr 8) and 0xFF).toByte()
        out[3] = ((h.size shr 16) and 0xFF).toByte()
        out[4] = ((h.size shr 24) and 0xFF).toByte()
        h.copyInto(out, 5)
        payload.copyInto(out, 5 + h.size)
        return out
    }

    /** null = malformed. Unknown type bytes are tolerated (future server frames). */
    fun decode(bytes: ByteArray): Frame? {
        if (bytes.size < 5) return null
        val headerLen = (bytes[1].toInt() and 0xFF) or
            ((bytes[2].toInt() and 0xFF) shl 8) or
            ((bytes[3].toInt() and 0xFF) shl 16) or
            ((bytes[4].toInt() and 0xFF) shl 24)
        if (headerLen < 0 || headerLen > MAX_HEADER_BYTES || 5 + headerLen > bytes.size) return null
        val header = try {
            JSONObject(String(bytes, 5, headerLen, Charsets.UTF_8))
        } catch (_: Exception) { return null }
        return Frame(bytes[0], header, bytes.copyOfRange(5 + headerLen, bytes.size))
    }

    fun hello(cursor: Long, device: String): ByteArray =
        encode(HELLO, JSONObject().put("cursor", cursor).put("device", device))

    fun rowsReq(after: Long, excludeOwn: Boolean): ByteArray =
        encode(ROWS_REQ, JSONObject().put("after", after).put("excludeOwn", excludeOwn))

    fun push(batchId: String, update: ByteArray): ByteArray =
        encode(PUSH, JSONObject().put("batchId", batchId), update)
}
