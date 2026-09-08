package sh.zeron.android.data

import org.json.JSONArray
import org.json.JSONObject

data class ChatRow(
    val id: String,
    val title: String?,
    val archived: Boolean,
    val spaceId: String?,
    /** The host device that runs this chat's sessions. */
    val deviceId: String? = null,
    /** Session config (harness/model the host runs with), when the row carries one. */
    val config: ChatConfig? = null,
    /** The base ref the session is pinned to (composer BranchContextChip). */
    val branch: String? = null,
)

/** Chat config — LWW `config` field on the registry chat row (iOS ChatConfig). */
data class ChatConfig(
    val harness: String?,
    val model: String?,
    val reasoning: String? = null,
) {
    companion object {
        fun parse(json: JSONObject?): ChatConfig? {
            if (json == null || json.length() == 0) return null
            val harness = json.optString("harness").takeIf { it.isNotEmpty() } ?: return null
            return ChatConfig(
                harness,
                json.optString("model").takeIf { it.isNotEmpty() },
                json.optString("reasoning").takeIf { it.isNotEmpty() },
            )
        }
    }
}

data class SpaceRow(
    val id: String,
    val path: String,
    val deviceId: String?,
    /** True when the host detected git in the space folder (shows ref/checkout pickers). */
    val gitDetected: Boolean = false,
)

/**
 * A host device row — `devices` kind (iOS DeviceRow). [version] feeds the
 * feature gates (`deviceVersionAtLeast`); unknown/unstamped reads "too old".
 */
data class DeviceRow(
    val id: String,
    val name: String,
    val platform: String = "",
    val version: String? = null,
)

/** Registry row — wire shape `{kind,id,seq,deleted,delHlc?,fields,clocks}`. */
data class RegistryRow(
    val kind: String,
    val id: String,
    val deleted: Boolean,
    val fields: JSONObject,
    /** Per-field LWW clocks (`{field: hlc}`) — needed to merge our own writes. */
    val clocks: JSONObject = JSONObject(),
    val delHlc: String? = null,
) {
    fun field(name: String): String? = fields.optString(name).takeIf { it.isNotEmpty() && !fields.isNull(name) }
    fun fieldLong(name: String): Long? = if (fields.has(name) && !fields.isNull(name)) fields.optLong(name) else null

    companion object {
        fun parse(o: JSONObject): RegistryRow = RegistryRow(
            kind = o.getString("kind"),
            id = o.getString("id"),
            deleted = o.optBoolean("deleted", false),
            fields = o.optJSONObject("fields") ?: JSONObject(),
            clocks = o.optJSONObject("clocks") ?: JSONObject(),
            delHlc = o.optString("delHlc").takeIf { it.isNotEmpty() && !o.isNull("delHlc") },
        )
    }
}

/**
 * One registry mutation, wire shape `{kind,id,op,set?,hlc}` (docs/registry-sync.md).
 * Android only emits `update` ops (it never invents rows), but the shape is the
 * full one so re-pushes stay idempotent under the server's `>` compare.
 */
data class RegistryOp(
    val kind: String,
    val id: String,
    val op: String,
    val set: JSONObject?,
    val hlc: String,
) {
    fun toJson(): JSONObject = JSONObject()
        .put("kind", kind)
        .put("id", id)
        .put("op", op)
        .put("hlc", hlc)
        .apply { set?.let { put("set", it) } }
}

/**
 * Hybrid-logical clock — `{ms:013}-{counter:06}-{device}` (docs/registry-sync.md).
 * Fixed-width zero padding makes lexicographic order = (ms, counter, device)
 * order; the device suffix makes it total — two writers can never tie.
 */
class HlcClock {
    private var lastMs: Long = 0
    private var counter: Long = 0

    fun next(nowMs: Long, device: String): String {
        if (nowMs > lastMs) {
            lastMs = nowMs
            counter = 0
        } else {
            counter += 1
            if (counter > 999_999) { lastMs += 1; counter = 0 }
        }
        return "${lastMs.toString().padStart(13, '0')}-${counter.toString().padStart(6, '0')}-$device"
    }
}

/**
 * Client-side registry table: authoritative rows (server truth) + a pending-op
 * queue (offline writes, replayed as an overlay for reads — the iOS
 * RegistryCore shape). Rows merge under per-field LWW by HLC string compare.
 */
class RegistryDoc {
    private val rows = mutableMapOf<String, RegistryRow>() // key = kind:id
    private val pending = mutableListOf<PendingBatch>()

    data class PendingBatch(val batch: String, val ops: List<RegistryOp>, var inFlight: Boolean = false)

    fun applyState(full: Boolean, rowsIn: List<RegistryRow>, cursor: Long) {
        if (full) rows.clear()
        rowsIn.forEach { rows["${it.kind}:${it.id}"] = it }
    }

    /** Live rows of `kind`, pending ops replayed on top. Includes rows a local
     *  pending upsert created (they don't exist authoritatively yet). */
    fun overlayRows(kind: String): List<RegistryRow> =
        keysFor(kind).mapNotNull { overlayRow(kind, it) }.sortedBy { it.id }

    fun overlayRow(kind: String, id: String): RegistryRow? {
        val authoritative = rows["$kind:$id"]
        val base = authoritative ?: pendingUpsertRow(kind, id) ?: return null
        return replay(base).takeIf { !it.deleted }
    }

    fun clear() {
        rows.clear()
        pending.clear()
    }

    /** Row ids that exist authoritatively or were created by a pending upsert. */
    private fun keysFor(kind: String): Set<String> {
        val keys = rows.values.filter { it.kind == kind }.map { it.id }.toMutableSet()
        for (batch in pending) for (op in batch.ops) {
            if (op.kind == kind && op.op == "upsert") keys += op.id
        }
        return keys
    }

    /** The latest pending upsert for a local-only row — its base for the overlay. */
    private fun pendingUpsertRow(kind: String, id: String): RegistryRow? {
        var base: RegistryRow? = null
        for (batch in pending) for (op in batch.ops) {
            if (op.kind == kind && op.id == id && op.op == "upsert") {
                val fields = op.set ?: JSONObject()
                val clocks = JSONObject()
                val it = fields.keys()
                while (it.hasNext()) {
                    val key = it.next()
                    clocks.put(key, op.hlc)
                }
                base = RegistryRow(kind, id, deleted = false, fields = fields, clocks = clocks)
            }
        }
        return base
    }

    /** Batches not yet in flight, marked in-flight (the caller owns the socket send). */
    fun takePushable(): List<PendingBatch> =
        pending.filter { !it.inFlight }.map { it.apply { inFlight = true } }

    /** Connection dropped: everything unacked becomes pushable again (re-push is idempotent). */
    fun markDisconnected() {
        pending.forEach { it.inFlight = false }
    }

    fun retire(batch: String) {
        pending.removeAll { it.batch == batch }
    }

    /**
     * Local LWW write. `update` never creates or revives rows (the caller
     * guards existence); `upsert` creates. The op joins the pending queue and
     * its fields apply as an overlay immediately.
     */
    fun write(kind: String, id: String, op: String, set: JSONObject, hlc: String) {
        pending += PendingBatch(batch = "b-$hlc", ops = listOf(RegistryOp(kind, id, op, set, hlc)))
    }

    /** Replay pending ops over an authoritative row; a field applies iff its HLC wins. */
    private fun replay(row: RegistryRow): RegistryRow {
        var fields = row.fields
        var clocks = row.clocks
        var touched = false
        for (batch in pending) {
            for (op in batch.ops) {
                if (op.kind != row.kind || op.id != row.id) continue
                if (op.op != "update" && op.op != "upsert") continue
                val set = op.set ?: continue
                val keys = set.keys()
                while (keys.hasNext()) {
                    val key = keys.next()
                    val stored = clocks.optString(key)
                    if (stored.isEmpty() || op.hlc > stored) {
                        if (!touched) {
                            fields = JSONObject(fields.toString())
                            clocks = JSONObject(clocks.toString())
                            touched = true
                        }
                        if (set.isNull(key)) fields.remove(key) else fields.put(key, set.get(key))
                        clocks.put(key, op.hlc)
                    }
                }
            }
        }
        return if (touched) row.copy(fields = fields, clocks = clocks) else row
    }
}

/** Project registry rows into the workspace/domain models the UI reads. */
class RegistryAdapter(private val doc: RegistryDoc) {
    fun chats(): List<ChatRow> = doc.overlayRows("chats").map { row ->
        ChatRow(
            id = row.id,
            title = row.field("title") ?: row.field("id"),
            archived = row.field("archived")?.toBooleanStrictOrNull() ?: false,
            spaceId = row.field("spaceId"),
            deviceId = row.field("deviceId"),
            config = ChatConfig.parse(row.fields.optJSONObject("config")),
            branch = row.field("branch"),
        )
    }.sortedWith(compareByDescending<ChatRow> { it.archived }.thenBy { it.title })

    fun spaces(): List<SpaceRow> = doc.overlayRows("spaces").map { row ->
        SpaceRow(
            id = row.id,
            path = row.field("path") ?: row.id,
            deviceId = row.field("deviceId"),
            gitDetected = row.field("gitDetected")?.toBooleanStrictOrNull() ?: false,
        )
    }

    fun devices(): List<DeviceRow> = doc.overlayRows("devices").map { row ->
        DeviceRow(
            id = row.id,
            name = row.field("name") ?: row.id,
            platform = row.field("platform") ?: "",
            version = row.field("version"),
        )
    }
}
