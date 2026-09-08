package sh.zeron.android.data

/**
 * Harness + model catalogs — a Kotlin port of the iOS HarnessCatalog
 * (apps/ios/Zeron/Models/HarnessCatalog.swift), which itself mirrors the
 * curated static catalogs in crates/harness (claude/catalog.rs,
 * codex/catalog.rs). The desktop overlays these on runtime discovery; the
 * phone uses them directly for the picker (or the device's live
 * ListHarnesses/ListModels catalog when reachable).
 *
 * Only the `default_enabled()` pair (claude-code, codex) is offered by the
 * static fallback: the ACP agents (grok/hermes/pi) appear only through a
 * device's live ListHarnesses catalog — opt-in per device on the desktop.
 *
 * Defaults mirror pickers.rs: first catalog row, reasoning xhigh where the
 * ladder has it, else high.
 */
data class HarnessInfo(val id: String, val label: String)

data class ModelInfo(
    val id: String,
    val label: String,
    val description: String? = null,
    /** Unified reasoning ladder, lowercase wire values. Empty = no efforts. */
    val reasoningLevels: List<String> = emptyList(),
)

object HarnessCatalog {
    val harnesses: List<HarnessInfo> = listOf(
        HarnessInfo(id = "claude-code", label = "Claude Code"),
        HarnessInfo(id = "codex", label = "Codex"),
    )

    /**
     * A picked harness/model/reasoning the composer currently uses. [model] is
     * nullable: a session config may record no model (the host then falls
     * back to the harness's default), and the composer must not invent one
     * for it. [reasoning] follows the same rule — null sends the harness
     * default.
     */
    data class Selection(
        val harness: String,
        val model: String?,
        val reasoning: String? = null,
    )

    /** Display names for every harness id the fleet can produce. */
    private val knownLabels: Map<String, String> = mapOf(
        "claude-code" to "Claude Code",
        "codex" to "Codex",
        "grok" to "Grok",
        "hermes" to "Hermes",
        "pi" to "Pi",
        "cursor" to "Cursor",
        "opencode" to "OpenCode",
        "mock" to "Mock",
    )

    private val fullLadder = listOf("low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink")
    private val claudeXhighLadder = listOf("low", "medium", "high", "xhigh", "max", "ultrathink")
    private val codexUltraLadder = listOf("low", "medium", "high", "xhigh", "max", "ultra")
    private val codexMaxLadder = listOf("low", "medium", "high", "xhigh", "max")
    private val codexXhighLadder = listOf("low", "medium", "high", "xhigh")

    fun harnessLabel(id: String): String = knownLabels[id] ?: id

    fun models(harness: String): List<ModelInfo> = when (harness) {
        "grok" -> listOf(
            ModelInfo("grok-4.5", "Grok 4.5", "xAI's coding model — 500k context",
                reasoningLevels = listOf("low", "medium", "high")),
        )
        "hermes" -> listOf(
            ModelInfo("hermes-4-405b", "Hermes 4 405B",
                "Nous Research's hybrid-reasoning flagship"),
            ModelInfo("hermes-4-70b", "Hermes 4 70B",
                "Faster Hermes 4 — same post-training, 70B"),
        )
        "pi" -> listOf(
            ModelInfo("default", "pi default",
                "Runs the model configured in pi (`pi` settings)",
                reasoningLevels = listOf("minimal", "low", "medium", "high", "xhigh", "max")),
        )
        "opencode" -> listOf(
            ModelInfo("opencode/big-pickle", "Big Pickle",
                "OpenCode Zen's flagship coding model"),
            ModelInfo("opencode/mimo-v2.5-free", "MiMo V2.5 Free",
                "Free tier on OpenCode Zen"),
            ModelInfo("opencode/hy3-free", "Hy3 Free", "Free tier on OpenCode Zen",
                reasoningLevels = listOf("low", "medium", "high")),
        )
        "codex" -> listOf(
            ModelInfo("gpt-5.6-sol", "GPT-5.6-Sol", "Frontier reasoning flagship",
                reasoningLevels = codexUltraLadder),
            ModelInfo("gpt-5.6-terra", "GPT-5.6-Terra", "Deep multi-step agentic work",
                reasoningLevels = codexUltraLadder),
            ModelInfo("gpt-5.6-luna", "GPT-5.6-Luna", "Fast frontier model",
                reasoningLevels = codexMaxLadder),
            ModelInfo("gpt-5.5", "GPT-5.5", "Previous generation flagship",
                reasoningLevels = codexXhighLadder),
            ModelInfo("gpt-5.4", "GPT-5.4", "Reliable general coding",
                reasoningLevels = codexXhighLadder),
            ModelInfo("gpt-5.4-mini", "GPT-5.4-Mini", "Small, fast and capable",
                reasoningLevels = codexXhighLadder),
            ModelInfo("gpt-5.3-codex-spark", "GPT-5.3-Codex-Spark", "Ultra-fast lightweight coding",
                reasoningLevels = codexXhighLadder),
        )
        else -> listOf( // claude-code (mock shares it)
            ModelInfo("claude-fable-5", "Fable 5", "Most intelligent model for building agents",
                reasoningLevels = fullLadder),
            ModelInfo("claude-opus-5", "Opus 5", "Powerful model for complex work",
                reasoningLevels = fullLadder),
            ModelInfo("claude-opus-4-8", "Opus 4.8", "Previous generation Opus",
                reasoningLevels = fullLadder),
            ModelInfo("claude-opus-4-7", "Opus 4.7", "Older generation Opus",
                reasoningLevels = claudeXhighLadder),
            ModelInfo("claude-sonnet-5", "Sonnet 5", "Balanced speed and intelligence",
                reasoningLevels = claudeXhighLadder),
            ModelInfo("claude-haiku-4-5", "Haiku 4.5", "Fastest model for everyday tasks"),
        )
    }

    fun defaultSelection(): Selection = Selection(
        harness = harnesses.first().id,
        model = models(harnesses.first().id).first().id,
        reasoning = defaultReasoning(models(harnesses.first().id).first()),
    )

    fun modelLabel(harness: String, modelId: String?): String {
        if (modelId == null) return models(harness).first().label
        return models(harness).firstOrNull { it.id == modelId }?.label ?: modelId
    }

    /** pickers.rs:126 — X-High when the ladder has it, else High. */
    fun defaultReasoning(model: ModelInfo): String? {
        if (model.reasoningLevels.isEmpty()) return null
        return if ("xhigh" in model.reasoningLevels) "xhigh" else "high"
    }

    fun reasoningLabel(level: String): String = when (level) {
        "minimal" -> "Minimal"
        "low" -> "Low"
        "medium" -> "Medium"
        "high" -> "High"
        "xhigh" -> "X-High"
        "max" -> "Max"
        "ultra" -> "Ultra"
        "ultracode" -> "Ultracode"
        "ultrathink" -> "Ultrathink"
        else -> level.replaceFirstChar { it.uppercase() }
    }

    /** One-line hints for the effort ladder (TraitPickerSheet.effortHint). */
    fun reasoningHint(level: String): String? = when (level) {
        "minimal" -> "Quickest, lightest touch"
        "low" -> "Fastest responses"
        "medium" -> "Balanced speed and depth"
        "high" -> "Thorough reasoning"
        "xhigh" -> "Extended reasoning"
        "max" -> "Maximum reasoning budget"
        "ultra" -> "Highest Codex tier"
        "ultracode" -> "X-High plus the ultracode setting"
        "ultrathink" -> "Deep-thinking prompt mode"
        else -> null
    }

    /** A model's effort ladder (live catalog or static fallback). */
    fun reasoningLevels(harness: String, modelId: String?): List<String> {
        val model = models(harness).firstOrNull { it.id == modelId } ?: return emptyList()
        return model.reasoningLevels
    }
}
