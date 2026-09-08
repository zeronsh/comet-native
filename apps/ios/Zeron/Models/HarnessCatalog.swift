// Harness + model catalogs — ports of crates/harness's curated static
// catalogs (claude/catalog.rs, codex/catalog.rs). Desktop discovers at runtime;
// the phone mirrors its run device's live catalog and falls back to these.
// Defaults mirror pickers.rs: first catalog row, reasoning high where the
// ladder has it, else medium/first.

import Foundation

struct HarnessInfo: Identifiable, Hashable {
    let id: String
    let label: String
}

struct ModelOptionChoiceInfo: Identifiable, Hashable {
    let id: String
    let label: String
}

struct ModelOptionInfo: Identifiable, Hashable {
    let id: String
    let label: String
    let choices: [ModelOptionChoiceInfo]
    let defaultChoice: String
}

struct ModelInfo: Identifiable, Hashable {
    let id: String
    let label: String
    let description: String?
    /// Unified reasoning ladder, lowercase wire values. Empty = no efforts.
    let reasoningLevels: [String]
    /// Harness-specific traits such as Codex Standard/Fast.
    let options: [ModelOptionInfo]

    init(id: String, label: String, description: String?, reasoningLevels: [String],
         options: [ModelOptionInfo] = []) {
        self.id = id
        self.label = label
        self.description = description
        self.reasoningLevels = reasoningLevels
        self.options = options
    }
}

enum HarnessCatalog {
    /// Static fallback = the engine's `default_enabled()` pair. The ACP
    /// agents (grok/hermes/pi) appear only through a device's live
    /// `ListHarnesses` catalog — they're opt-in per device via
    /// Settings → Agents on the desktop.
    static let harnesses: [HarnessInfo] = [
        HarnessInfo(id: "claude-code", label: "Claude Code"),
        HarnessInfo(id: "codex", label: "Codex"),
    ]

    /// Display names for every harness id the fleet can produce, including
    /// ones this device's static list doesn't offer (acp/mod.rs specs).
    static let knownLabels: [String: String] = [
        "claude-code": "Claude Code",
        "codex": "Codex",
        "devin": "Devin",
        "grok": "Grok",
        "hermes": "Hermes",
        "pi": "Pi",
        "cursor": "Cursor",
        "opencode": "OpenCode",
        "mock": "Mock",
    ]

    static func label(for harness: String) -> String {
        knownLabels[harness] ?? harness
    }

    private static let fullLadder = ["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"]
    private static let claudeXhighLadder = ["low", "medium", "high", "xhigh", "max", "ultrathink"]
    private static let codexUltraLadder = ["low", "medium", "high", "xhigh", "max", "ultra"]
    private static let codexMaxLadder = ["low", "medium", "high", "xhigh", "max"]
    private static let codexXhighLadder = ["low", "medium", "high", "xhigh"]
    private static let codexServiceTier = [
        ModelOptionInfo(id: "serviceTier", label: "Service Tier", choices: [
            ModelOptionChoiceInfo(id: "default", label: "Standard"),
            ModelOptionChoiceInfo(id: "fast", label: "Fast"),
        ], defaultChoice: "default"),
    ]

    static func models(for harness: String) -> [ModelInfo] {
        switch harness {
        case "grok":
            return [
                ModelInfo(id: "grok-4.5", label: "Grok 4.5",
                          description: "xAI's coding model — 500k context",
                          reasoningLevels: ["low", "medium", "high"]),
            ]
        case "devin":
            return [
                ModelInfo(id: "swe-1-7-medium", label: "SWE-1.7 Medium",
                          description: "Devin's default coding model", reasoningLevels: []),
                ModelInfo(id: "claude-fable-5-1-high", label: "Claude Fable 5.1 High",
                          description: "Anthropic's frontier model through Devin", reasoningLevels: []),
                ModelInfo(id: "adaptive", label: "Adaptive",
                          description: "Devin picks the model per request", reasoningLevels: []),
            ]
        case "hermes":
            return [
                ModelInfo(id: "hermes-4-405b", label: "Hermes 4 405B",
                          description: "Nous Research's hybrid-reasoning flagship", reasoningLevels: []),
                ModelInfo(id: "hermes-4-70b", label: "Hermes 4 70B",
                          description: "Faster Hermes 4 — same post-training, 70B", reasoningLevels: []),
            ]
        case "pi":
            return [
                ModelInfo(id: "default", label: "pi default",
                          description: "Runs the model configured in pi (`pi` settings)",
                          reasoningLevels: ["minimal", "low", "medium", "high", "xhigh", "max"]),
            ]
        case "opencode":
            // Static fallback only — a reachable host answers `listModels`
            // with its live discovery (connected providers). The anonymous
            // OpenCode Zen tier is always available, so these always run.
            return [
                ModelInfo(id: "opencode/big-pickle", label: "Big Pickle",
                          description: "OpenCode Zen's flagship coding model", reasoningLevels: []),
                ModelInfo(id: "opencode/mimo-v2.5-free", label: "MiMo V2.5 Free",
                          description: "Free tier on OpenCode Zen", reasoningLevels: []),
                ModelInfo(id: "opencode/hy3-free", label: "Hy3 Free",
                          description: "Free tier on OpenCode Zen",
                          reasoningLevels: ["low", "medium", "high"]),
            ]
        case "codex":
            return [
                ModelInfo(id: "gpt-6-astra", label: "GPT-6-Astra",
                          description: "Our most capable model for complex, demanding work.",
                          reasoningLevels: codexUltraLadder, options: codexServiceTier),
                ModelInfo(id: "gpt-5.6-sol", label: "GPT-5.6-Sol",
                          description: "Frontier reasoning flagship", reasoningLevels: codexUltraLadder,
                          options: codexServiceTier),
                ModelInfo(id: "gpt-5.6-terra", label: "GPT-5.6-Terra",
                          description: "Deep multi-step agentic work", reasoningLevels: codexUltraLadder,
                          options: codexServiceTier),
                ModelInfo(id: "gpt-5.6-luna", label: "GPT-5.6-Luna",
                          description: "Fast frontier model", reasoningLevels: codexMaxLadder,
                          options: codexServiceTier),
                ModelInfo(id: "gpt-daybreak-blue-latest", label: "Daybreak Blue",
                          description: "Frontier model for defensive cybersecurity work",
                          reasoningLevels: codexUltraLadder),
                ModelInfo(id: "gpt-5.5", label: "GPT-5.5",
                          description: "Previous generation flagship", reasoningLevels: codexXhighLadder,
                          options: codexServiceTier),
                ModelInfo(id: "gpt-5.4", label: "GPT-5.4",
                          description: "Reliable general coding", reasoningLevels: codexXhighLadder,
                          options: codexServiceTier),
                ModelInfo(id: "gpt-5.4-mini", label: "GPT-5.4-Mini",
                          description: "Small, fast and capable", reasoningLevels: codexXhighLadder,
                          options: codexServiceTier),
                ModelInfo(id: "gpt-5.3-codex-spark", label: "GPT-5.3-Codex-Spark",
                          description: "Ultra-fast lightweight coding", reasoningLevels: codexXhighLadder,
                          options: codexServiceTier),
            ]
        default:  // claude-code (mock shares it)
            return [
                ModelInfo(id: "claude-fable-5", label: "Fable 5",
                          description: "Most intelligent model for building agents", reasoningLevels: fullLadder),
                ModelInfo(id: "claude-opus-5", label: "Opus 5",
                          description: "Powerful model for complex work", reasoningLevels: fullLadder),
                ModelInfo(id: "claude-opus-4-8", label: "Opus 4.8",
                          description: "Previous generation Opus", reasoningLevels: fullLadder),
                ModelInfo(id: "claude-opus-4-7", label: "Opus 4.7",
                          description: "Older generation Opus", reasoningLevels: claudeXhighLadder),
                ModelInfo(id: "claude-sonnet-5", label: "Sonnet 5",
                          description: "Balanced speed and intelligence", reasoningLevels: claudeXhighLadder),
                ModelInfo(id: "claude-haiku-4-5", label: "Haiku 4.5",
                          description: "Fastest model for everyday tasks", reasoningLevels: []),
            ]
        }
    }

    static func defaultModel(for harness: String) -> ModelInfo {
        models(for: harness)[0]
    }

    /// pickers.rs — High when available, then Medium, then the first level.
    static func defaultReasoning(for model: ModelInfo) -> String? {
        if model.reasoningLevels.isEmpty { return nil }
        if model.reasoningLevels.contains("high") { return "high" }
        if model.reasoningLevels.contains("medium") { return "medium" }
        return model.reasoningLevels.first
    }

    static func selectedChoice(for option: ModelOptionInfo,
                               selectedId: String?) -> ModelOptionChoiceInfo {
        option.choices.first { $0.id == selectedId }
            ?? option.choices.first { $0.id == option.defaultChoice }
            ?? option.choices.first
            ?? ModelOptionChoiceInfo(id: option.defaultChoice, label: option.defaultChoice)
    }

    static func reasoningLabel(_ level: String) -> String {
        switch level {
        case "minimal": return "Minimal"
        case "low": return "Low"
        case "medium": return "Medium"
        case "high": return "High"
        case "xhigh": return "X-High"
        case "max": return "Max"
        case "ultra": return "Ultra"
        case "ultracode": return "Ultracode"
        case "ultrathink": return "Ultrathink"
        default: return level.capitalized
        }
    }

    static func modelLabel(harness: String, modelId: String?) -> String {
        guard let modelId else { return defaultModel(for: harness).label }
        return models(for: harness).first { $0.id == modelId }?.label ?? modelId
    }
}
