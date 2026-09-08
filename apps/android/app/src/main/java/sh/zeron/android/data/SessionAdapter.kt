package sh.zeron.android.data

import org.json.JSONArray
import org.json.JSONObject
import sh.zeron.android.loro.LoroDoc
import java.util.UUID

sealed class Part {
    data class Text(val id: String, val text: String) : Part()
    data class Reasoning(val id: String, val text: String) : Part()
    data class Tool(val id: String, val call: String, val isError: Boolean, val output: String?) : Part()
    data class Input(val id: String, val questions: List<InputQuestion>, val resolved: Boolean) : Part()
    data class Error(val id: String, val message: String) : Part()
}

/**
 * One agent question (proto `UserInputQuestion`): a header, the question text,
 * plain option labels, and a multi-select flag.
 */
data class InputQuestion(
    val id: String,
    val header: String,
    val question: String,
    val options: List<String>,
    val multiSelect: Boolean,
)

/** One answered question (proto `UserInputAnswer`). */
data class InputAnswer(
    val questionId: String,
    val labels: List<String>,
)

/**
 * Entry lifecycle (schema.rs `MessageStatus`). `Streaming` is the host saying
 * "this turn is still being written" — the only honest signal the viewer has
 * that the model has not finished.
 */
enum class MessageStatus {
    Streaming, Complete, Aborted;

    companion object {
        fun parse(raw: String?): MessageStatus? = when (raw?.lowercase()) {
            "streaming" -> Streaming
            "complete" -> Complete
            "aborted" -> Aborted
            else -> null
        }
    }
}

/** Message author (schema.rs `MessageRole`). */
enum class MessageRole {
    User, Assistant, System;

    companion object {
        fun parse(raw: String?): MessageRole = when (raw?.lowercase()) {
            "user" -> User
            "system" -> System
            else -> Assistant
        }
    }
}

/**
 * One authored turn. Parts stay grouped under their message so the transcript
 * can attribute them — rendering a flat part list made every row look the same
 * regardless of who produced it.
 */
data class TranscriptMessage(
    val id: String,
    val role: MessageRole,
    val parts: List<Part>,
    val status: MessageStatus? = null,
)

/**
 * @param working the turn is still being written. Only the LAST doc entry
 *   counts — an older `streaming` entry belongs to a run that died, and the
 *   host stamps those `aborted` on recovery, so treating them as live would
 *   spin forever. It is carried rather than derived from [messages] because a
 *   just-opened entry has no parts yet and never reaches the list.
 */
data class Transcript(
    val messages: List<TranscriptMessage>,
    val working: Boolean = false,
) {
    /** Flattened view, for counts and assertions that don't care about grouping. */
    val parts: List<Part> get() = messages.flatMap { it.parts }

    val isEmpty: Boolean get() = messages.isEmpty()

    /**
     * The unresolved input request to surface in the question panel (iOS
     * SessionStore.openInputRequest): the newest input part that is not yet
     * resolved and actually has answerable questions. An empty question list
     * must not take the composer's place — the user would have no way to type.
     */
    val openInputRequest: Part.Input? get() {
        for (message in messages.asReversed()) {
            for (part in message.parts.asReversed()) {
                if (part is Part.Input && !part.resolved && part.questions.isNotEmpty()) return part
            }
        }
        return null
    }
}

class SessionAdapter(private val doc: LoroDoc) {
    /**
     * Parse the session doc's `messages`/`parts` container (schema.rs) into
     * viewer-safe domain parts. Text lives in LoroText, so `getDeepValue()`
     * returns them as flattened strings. Continuation parts (continuationOf)
     * are appended to their message so streaming never duplicates text.
     */
    suspend fun transcript(): Transcript {
        val json = doc.getDeepValueJson()
        if (json.isBlank() || json == "{}" || json == "null") return Transcript(emptyList())
        val out = mutableListOf<TranscriptMessage>()
        var lastStatus: MessageStatus? = null
        try {
            val root = JSONObject(json)
            val messages = root.optJSONArray("messages") ?: JSONArray()
            for (i in 0 until messages.length()) {
                val msg = messages.getJSONObject(i)
                val msgId = msg.optString("id", "$i")
                val role = MessageRole.parse(msg.optString("role").takeIf { it.isNotEmpty() })
                val msgParts = msg.optJSONArray("parts") ?: JSONArray()
                val parts = mutableListOf<Part>()
                for (j in 0 until msgParts.length()) {
                    val p = msgParts.getJSONObject(j)
                    val kind = p.optString("kind")
                    val partId = p.optString("id", "$msgId.$j")
                    val text = p.optString("text").takeIf { it.isNotEmpty() }
                    when (kind) {
                        "text" -> text?.let { parts += Part.Text(partId, it) }
                        "reasoning" -> text?.let { parts += Part.Reasoning(partId, it) }
                        "tool" -> {
                            val call = p.optJSONObject("call")?.toString() ?: ""
                            parts += Part.Tool(
                                partId,
                                p.optString("subagent_tail", call),
                                p.optBoolean("isError", false),
                                p.optString("output").ifBlank { null },
                            )
                        }
                        "input" -> parts += Part.Input(partId, parseInputQuestions(p.optJSONArray("questions")), p.optBoolean("resolved", false))
                        "error" -> parts += Part.Error(partId, p.optString("message", "Error"))
                        else -> text?.let { parts += Part.Text(partId, it) }
                    }
                }
                val status = MessageStatus.parse(msg.optString("status").takeIf { it.isNotEmpty() })
                lastStatus = status
                if (parts.isNotEmpty()) out += TranscriptMessage(msgId, role, parts, status)
            }
        } catch (e: Exception) {
            // Malformed doc: surface what we can, never crash the viewer.
            if (out.isEmpty()) return Transcript(emptyList())
        }
        return Transcript(out, working = lastStatus == MessageStatus.Streaming)
    }

    /**
     * Wire questions (`proto::agent::UserInputQuestion`, camelCase): a list of
     * {id, header, question, options, multiSelect} objects. Unknown entries
     * are skipped rather than dropped wholesale — one malformed question must
     * not blank the panel.
     */
    private fun parseInputQuestions(arr: JSONArray?): List<InputQuestion> {
        if (arr == null) return emptyList()
        val out = mutableListOf<InputQuestion>()
        for (i in 0 until arr.length()) {
            val o = arr.optJSONObject(i) ?: continue
            val id = o.optString("id")
            if (id.isEmpty()) continue
            val options = o.optJSONArray("options")?.let { opts ->
                (0 until opts.length()).mapNotNull { opts.optString(it).takeIf { s -> s.isNotEmpty() } }
            } ?: emptyList()
            out += InputQuestion(
                id = id,
                header = o.optString("header"),
                question = o.optString("question", "Question"),
                options = options,
                multiSelect = o.optBoolean("multiSelect", false),
            )
        }
        return out
    }

    /// Durable command-ledger append (viewer-only write allowed by writer discipline).
    suspend fun queueCommand(kind: String, payload: String): String {
        val cmd = doc.appendCommand(kind, payload, "android")
        return cmd.getValue("id") as? String ?: UUID.randomUUID().toString().lowercase()
    }
}
