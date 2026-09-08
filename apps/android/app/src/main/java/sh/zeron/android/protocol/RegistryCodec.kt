package sh.zeron.android.protocol

import org.json.JSONObject

sealed class RegistryFrame {
    data class Hello(val cursor: Long?, val device: String) : RegistryFrame()
    /** ops is the JSON array `[Op]` itself — the wire shape is an array, not a string. */
    data class Push(val batch: String, val ops: org.json.JSONArray) : RegistryFrame()
    data class Presence(val at: Long) : RegistryFrame()
    object Probe : RegistryFrame()
    data class State(
        val seq: Long,
        val full: Boolean,
        val gcFloor: Long,
        val rows: String,
        /** deviceId → last presence beat ms (drives deviceOnline). */
        val presence: Map<String, Long> = emptyMap(),
    ) : RegistryFrame()
    data class Rows(val seq: Long, val rows: String) : RegistryFrame()
    data class Ack(val batch: String, val seq: Long, val applied: Long) : RegistryFrame()
    data class PresenceBeat(val device: String, val at: Long) : RegistryFrame()
    object ProbeOk : RegistryFrame()
    data class Error(val code: String, val message: String) : RegistryFrame()
}

/**
 * Registry wire codec — JSON text frames (docs/registry-sync.md).
 * Rows arrive as a JSON array; they are kept as raw text here so the doc layer
 * owns all row semantics.
 */
object RegistryCodec {
    fun encode(frame: RegistryFrame): String = when (frame) {
        is RegistryFrame.Hello -> JSONObject().apply {
            put("t", "hello")
            put("device", frame.device)
            put("cursor", frame.cursor ?: JSONObject.NULL)
        }.toString()
        is RegistryFrame.Push -> JSONObject().apply {
            put("t", "push"); put("batch", frame.batch); put("ops", frame.ops)
        }.toString()
        is RegistryFrame.Presence -> JSONObject().apply {
            put("t", "presence"); put("at", frame.at)
        }.toString()
        is RegistryFrame.Probe -> JSONObject().put("t", "probe").toString()
        else -> "{}"
    }

    fun decode(json: String): RegistryFrame? = try {
        val o = JSONObject(json)
        when (o.optString("t")) {
            // rows/presence are containers — keep them as text/typed values and
            // let the doc layer merge; optJSONArray keeps a missing key benign.
            "state" -> {
                val beats = o.optJSONObject("presence")?.let { p ->
                    val keys = p.keys()
                    buildMap {
                        while (keys.hasNext()) {
                            val device = keys.next()
                            put(device, p.optLong(device, 0))
                        }
                    }
                } ?: emptyMap()
                RegistryFrame.State(
                    seq = o.optLong("seq", 0),
                    full = o.optBoolean("full", true),
                    gcFloor = o.optLong("gcFloor", 0),
                    rows = (o.optJSONArray("rows") ?: org.json.JSONArray()).toString(),
                    presence = beats,
                )
            }
            "rows" -> RegistryFrame.Rows(
                seq = o.optLong("seq", 0),
                rows = (o.optJSONArray("rows") ?: org.json.JSONArray()).toString(),
            )
            "ack" -> RegistryFrame.Ack(o.optString("batch"), o.optLong("seq", 0), o.optLong("applied", 0))
            "presence" -> RegistryFrame.PresenceBeat(o.optString("device"), o.optLong("at", 0))
            "probe-ok" -> RegistryFrame.ProbeOk
            "error" -> RegistryFrame.Error(o.optString("code", "unknown"), o.optString("message", ""))
            else -> null
        }
    } catch (_: Exception) { null }
}
