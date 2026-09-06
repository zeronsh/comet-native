// Debug-only measurements used by hosted simulator regression tests.
import SwiftUI

#if DEBUG
@MainActor
enum TranscriptLayoutProbe {
    static var enabled = false
    static var tails: [String: CGRect] = [:]
    static var viewports: [String: CGRect] = [:]
}
#endif

struct TranscriptTailProbe: ViewModifier {
    let rowID: String
    let isTail: Bool
    let chatId: String
    func body(content: Content) -> some View {
        #if DEBUG
        if TranscriptLayoutProbe.enabled, isTail {
            content.onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: {
                TranscriptLayoutProbe.tails[chatId + "|" + rowID] = $0
            }
        } else { content }
        #else
        content
        #endif
    }
}

struct TranscriptViewportProbe: ViewModifier {
    let chatId: String
    func body(content: Content) -> some View {
        #if DEBUG
        if TranscriptLayoutProbe.enabled {
            content.onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: {
                TranscriptLayoutProbe.viewports[chatId] = $0
            }
        } else { content }
        #else
        content
        #endif
    }
}
