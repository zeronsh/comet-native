// Transcript load benchmark — launch with `-bench`. Builds a synthetic session
// doc the size of a long agent transcript and times the three stages that run
// between "cached bytes on disk" and "rows on screen":
//
//   decode   SessionStore.decodeEntries — getDeepValue + whole-doc walk
//   cold     row build with empty caches — what EVERY rebuild used to cost
//   warm     row build with the completed-parse memo primed — cost per doc update
//   cached   TranscriptBuilderCache at an unchanged revision — cost per scroll frame
//
// cold is the pre-change number for both warm and cached, so the same run gives
// the before/after. Results append to Documents/bench.log for simctl to read.

import Foundation
import Loro

@MainActor
enum BenchRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("bench.log")
    }

    static func log(_ line: String) {
        print("BENCH: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile()
            handle.write(Data("\(line)\n".utf8))
            try? handle.close()
        } else {
            try? Data("\(line)\n".utf8).write(to: logURL)
        }
    }

    static func run() async {
        try? FileManager.default.removeItem(at: logURL)
        for turns in [50, 200, 500] {
            measure(turns: turns)
        }
        log("done")
    }

    // MARK: Measurement

    private static func measure(turns: Int) {
        let doc = buildDoc(turns: turns)
        let snapshot = (try? doc.export(mode: .snapshot)) ?? Data()
        let bytes = snapshot.count

        // Stage 0 — the disk hydration import (DocDisk.load). STILL on the main
        // thread: it has to finish before the room join for the backfill to be
        // incremental, so it was left alone. Measured to size that trade.
        let importMs = best(3) {
            _ = try? LoroDoc().importWith(bytes: snapshot, origin: "disk")
        }

        // Stage 1 — projection. Unchanged in cost; the fix moved it off the
        // main thread, so this is the main-thread stall that used to happen.
        var entries: [MessageEntry] = []
        let decode = time { entries = SessionStore.decodeEntries(from: doc)?.entries ?? [] }

        // Stage 2 — cold row build (empty caches). The OLD per-rebuild cost.
        var rowCount = 0
        let cold = best(3) {
            var parsers: [String: IncrementalMarkdownParser] = [:]
            var memo: [String: CompletedParse] = [:]
            let rows = TranscriptRowBuilder.rows(entries: entries, pendingSends: [],
                                                 parsers: &parsers, completed: &memo)
            rowCount = rows.count
        }

        // Stage 3 — warm rebuild: memo primed, as after any doc update.
        var parsers: [String: IncrementalMarkdownParser] = [:]
        var memo: [String: CompletedParse] = [:]
        _ = TranscriptRowBuilder.rows(entries: entries, pendingSends: [],
                                      parsers: &parsers, completed: &memo)
        let warm = best(5) {
            _ = TranscriptRowBuilder.rows(entries: entries, pendingSends: [],
                                          parsers: &parsers, completed: &memo)
        }

        // Stage 4 — revision-gated cache: the scroll-frame path.
        let cache = TranscriptBuilderCache()
        _ = cache.rows(revision: 1, entries: entries, pendingSends: [])
        let cached = best(5) {
            _ = cache.rows(revision: 1, entries: entries, pendingSends: [])
        }

        log("--- \(turns) turns · \(entries.count) entries · \(rowCount) rows · \(bytes / 1024) KB snapshot")
        log(String(format: "disk import (ON MAIN)   %8.2f ms", importMs))
        log(String(format: "decode (now off-main)   %8.2f ms", decode))
        log(String(format: "row build cold  [was]   %8.2f ms", cold))
        log(String(format: "row build warm  [now]   %8.2f ms   %.0fx", warm, cold / max(warm, 0.0001)))
        log(String(format: "scroll frame    [now]   %8.4f ms   %.0fx", cached, cold / max(cached, 0.0001)))
    }

    private static func time(_ body: () -> Void) -> Double {
        let t0 = CFAbsoluteTimeGetCurrent()
        body()
        return (CFAbsoluteTimeGetCurrent() - t0) * 1000
    }

    /// Best-of-n: the floor is the honest number for cache behaviour (noise
    /// only ever adds).
    private static func best(_ n: Int, _ body: () -> Void) -> Double {
        var lowest = Double.greatestFiniteMagnitude
        for _ in 0..<n { lowest = min(lowest, time(body)) }
        return lowest
    }

    /// A big synthetic transcript for the `-big` demo route — stresses the
    /// scroll-settle path with far more lazy rows than the demo dataset has.
    static func syntheticEntries(turns: Int) -> [MessageEntry] {
        SessionStore.decodeEntries(from: buildDoc(turns: turns))?.entries ?? []
    }

    // MARK: Synthetic doc (schema.rs shape — see SessionStore.entryFrom)

    private static func buildDoc(turns: Int) -> LoroDoc {
        let doc = LoroDoc()
        let messages = doc.getList(id: "messages")
        for i in 0..<turns {
            let user = try! messages.pushContainer(child: LoroMap())
            try! user.insert(key: "id", v: "u\(i)")
            try! user.insert(key: "role", v: "user")
            try! user.insert(key: "createdAt", v: Int64(i * 1000))
            try! user.insert(key: "deviceId", v: "bench")
            try! user.insert(key: "status", v: "complete")
            let uparts = try! user.insertContainer(key: "parts", child: LoroList())
            try! addText(to: uparts, id: "t0", text: "Turn \(i): the ref dropdown still hangs on open — dig into it.")

            let bot = try! messages.pushContainer(child: LoroMap())
            try! bot.insert(key: "id", v: "a\(i)")
            try! bot.insert(key: "role", v: "assistant")
            try! bot.insert(key: "createdAt", v: Int64(i * 1000 + 1))
            try! bot.insert(key: "deviceId", v: "dev-mac")
            try! bot.insert(key: "status", v: "complete")
            let aparts = try! bot.insertContainer(key: "parts", child: LoroList())
            try! addText(to: aparts, id: "t0", text: prose(i))
            for t in 0..<4 {
                try! addTool(to: aparts, id: "k\(i).\(t)", index: i * 4 + t)
            }
            try! addText(to: aparts, id: "t1", text: closing(i))
        }
        return doc
    }

    private static func addText(to parts: LoroList, id: String, text: String) throws {
        let p = try parts.pushContainer(child: LoroMap())
        try p.insert(key: "id", v: id)
        try p.insert(key: "kind", v: "text")
        try p.insert(key: "text", v: text)
    }

    private static func addTool(to parts: LoroList, id: String, index: Int) throws {
        let p = try parts.pushContainer(child: LoroMap())
        try p.insert(key: "id", v: id)
        try p.insert(key: "kind", v: "tool")
        try p.insert(key: "isError", v: index % 17 == 0)
        let call = try p.insertContainer(key: "call", child: LoroMap())
        switch index % 4 {
        case 0:
            try call.insert(key: "kind", v: "exec")
            try call.insert(key: "command", v: "rg -n 'refDropdown' src/components --glob '!*.test.ts'")
        case 1:
            try call.insert(key: "kind", v: "readFile")
            try call.insert(key: "path", v: "src/components/refs/RefDropdown.tsx")
        case 2:
            try call.insert(key: "kind", v: "editFile")
            try call.insert(key: "path", v: "src/components/refs/useRefIndex.ts")
        default:
            try call.insert(key: "kind", v: "search")
            try call.insert(key: "pattern", v: "loadRefs\\(")
        }
    }

    /// A realistic assistant block: headings, prose, a list, a table, code.
    private static func prose(_ i: Int) -> String {
        """
        ## Pass \(i): where the dropdown stalls

        The dropdown's open handler awaits `loadRefs()` **before** it paints, so
        the menu can't render until the full ref index resolves. On a repo with
        many refs that's a visible hang, and it is paid again on every open
        because the result is never memoized between mounts.

        Three things stack up here:

        1. `loadRefs()` walks every ref and builds a fresh array each call
        2. The handler `await`s it inline instead of rendering an empty menu
        3. `useRefIndex` has no cache, so remount re-does the whole walk

        | Stage | Cost | Cached |
        | --- | --- | --- |
        | `loadRefs` | O(refs) | no |
        | `useRefIndex` | O(refs) | no |
        | paint | O(visible) | n/a |

        > The fix is to paint first and fill in — the index can arrive late.

        ```ts
        const refs = useRefIndex()          // memoized, suspense-free
        useEffect(() => { void warmRefIndex() }, [])
        return <Menu items={refs ?? []} loading={refs == null} />
        ```
        """
    }

    private static func closing(_ i: Int) -> String {
        """
        Landed the pass-\(i) change behind `refIndexCache`. Open latency drops to
        a paint, and the index warms in the background on first hover.
        """
    }
}
