// Harness + model catalogs — ports of crates/harness's curated static
// catalogs (claude/catalog.rs, codex/catalog.rs). The desktop overlays these
// on runtime discovery; the phone uses them directly for the pickers.
// Defaults mirror pickers.rs: first catalog row, reasoning xhigh where the
// ladder has it, else high.

import Foundation

struct HarnessInfo: Identifiable, Hashable {
    let id: String
    let label: String
}

struct ModelInfo: Identifiable, Hashable {
    let id: String
    let label: String
    let description: String?
    /// Unified reasoning ladder, lowercase wire values. Empty = no efforts.
    let reasoningLevels: [String]
}

enum HarnessCatalog {
    static let harnesses: [HarnessInfo] = [
        HarnessInfo(id: "claude-code", label: "Claude Code"),
        HarnessInfo(id: "codex", label: "Codex"),
    ]

    private static let fullLadder = ["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"]
    private static let claudeXhighLadder = ["low", "medium", "high", "xhigh", "max", "ultrathink"]
    private static let codexUltraLadder = ["low", "medium", "high", "xhigh", "max", "ultra"]
    private static let codexMaxLadder = ["low", "medium", "high", "xhigh", "max"]
    private static let codexXhighLadder = ["low", "medium", "high", "xhigh"]

    static func models(for harness: String) -> [ModelInfo] {
        switch harness {
        case "codex":
            return [
                ModelInfo(id: "gpt-5.6-sol", label: "GPT-5.6-Sol",
                          description: "Frontier reasoning flagship", reasoningLevels: codexUltraLadder),
                ModelInfo(id: "gpt-5.6-terra", label: "GPT-5.6-Terra",
                          description: "Deep multi-step agentic work", reasoningLevels: codexUltraLadder),
                ModelInfo(id: "gpt-5.6-luna", label: "GPT-5.6-Luna",
                          description: "Fast frontier model", reasoningLevels: codexMaxLadder),
                ModelInfo(id: "gpt-5.5", label: "GPT-5.5",
                          description: "Previous generation flagship", reasoningLevels: codexXhighLadder),
                ModelInfo(id: "gpt-5.4", label: "GPT-5.4",
                          description: "Reliable general coding", reasoningLevels: codexXhighLadder),
                ModelInfo(id: "gpt-5.4-mini", label: "GPT-5.4-Mini",
                          description: "Small, fast and capable", reasoningLevels: codexXhighLadder),
                ModelInfo(id: "gpt-5.3-codex-spark", label: "GPT-5.3-Codex-Spark",
                          description: "Ultra-fast lightweight coding", reasoningLevels: codexXhighLadder),
            ]
        default:  // claude-code (mock shares it)
            return [
                ModelInfo(id: "claude-fable-5", label: "Fable 5",
                          description: "Most intelligent model for building agents", reasoningLevels: fullLadder),
                ModelInfo(id: "claude-opus-4-8", label: "Opus 4.8",
                          description: "Powerful model for complex work", reasoningLevels: fullLadder),
                ModelInfo(id: "claude-opus-4-7", label: "Opus 4.7",
                          description: "Previous generation Opus", reasoningLevels: claudeXhighLadder),
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

    /// pickers.rs:126 — X-High when the ladder has it, else High.
    static func defaultReasoning(for model: ModelInfo) -> String? {
        if model.reasoningLevels.isEmpty { return nil }
        return model.reasoningLevels.contains("xhigh") ? "xhigh" : "high"
    }

    static func reasoningLabel(_ level: String) -> String {
        switch level {
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
