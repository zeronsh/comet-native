# Mugen + Pretext: Techniques for a Rust/gpui Transcript Reimplementation

(Report from exploration of comet's node_modules + app wiring. NOTE: "pretext" is NOT markdown —
it's the text-measurement/line-break kernel. Markdown is @wingleeio/mugen-markdown.)

## 0. Package map
| Package | Role |
|---|---|
| @chenglou/pretext (0.0.8) | Pure-TS multiline text measurement & layout kernel (segmentation, line-break, bidi, font-table advance widths). No DOM. |
| @wingleeio/pretext-core (0.1.4) | Same kernel ported to C++/JSI for RN; synchronous native measure. |
| @wingleeio/mugen (0.8.0) | Virtualized React list with ANALYTICALLY-computed row heights (via pretext), stick-to-bottom, tweens. Used in packages/ui (desktop). |
| @wingleeio/mugen-native (0.10.1) | RN variant (recycling pool). |
| @wingleeio/mugen-markdown (0.7.0) | Measurable markdown -> mugen primitives (incremark parse, RichText, CodeBlock). |

Core invariant: ONE description of a row feeds both measurement and render, so they can't desync.
Heights are computed analytically, never read back from layout — never-mounted rows have exact offsets.

## 1. Virtualization (mugen)
- 1a. Analytic height walker: interprets row primitive tree (VStack/HStack/Text/Escape/Collapse), threads width top-down, sums child heights + chrome, calls pretext only at Text leaves. Same tree renders. Spacing/sizing are typed primitive props, not CSS.
- 1b. Fenwick (binary-indexed) tree of offsets: offsetOf(i), indexAt(y); invalidate(key) re-measures one row, patches index O(log n), re-anchors scroll. Renders visible slice + overscan (desktop 320px, mobile 1800px w/ recycling pool).
- 1c. Lazy measurement for cold open: lazyMeasure {head:8, tail:30}; others get running-average estimates; refineEstimates(budgetMs) in idle time nearest-to-bottom-first; ensureMeasured(key) before paint. Estimate->exact deltas above viewport flow through the scroll-anchor channel so content never shifts.
- 1d. Per-instance heightMemo keyed by row key + item identity: an append is O(rows) map lookups + one real walk for the new row. THE central streaming trick.
- 1e. Stick-to-bottom: velocity-based spring + feed-forward term (smoothed target growth px/frame) to track streaming growth. initialScroll:'bottom'. Interrupt detection from USER INPUT (wheel deltaY<0, touch drag up), not scrollbar position — expectedTop/lastScrollTop distinguish controller frames from user frames. Re-engage within STICK_THRESHOLD_PX (70) of bottom.
- 1f. Scroll retention: captureScrollAnchor/takeScrollAnchorDelta shift scroll by delta px on prepends/above-fold height changes. Desktop coalesces width-driven re-pins (40ms settle timer); composer height change corrects on next frame.
- 1g. Height changes while streaming: growing text re-measures that row via memo+invalidate; Collapse (tool folds) tweens committed height between 0 and natural height, but content changing while open SNAPS (only `open` toggles animate) so it composes with the stick spring.
- 1h. Stable keys drive identity, slot state, offset patching. Per-row slot state persists while unmounted (off-screen expansion state stays real; height stays exact).
- 1i. AnimationClock: ONE frame loop per list advancing all tweens; animates the COMMITTED height — each frame the tween value both re-measures the row AND styles the paint, so painted and computed layout agree at every intermediate frame. Runs only while tweens active. Honors prefers-reduced-motion (snap).
- 1j. Escape hatch: Escape (fixed declared height, children unwalked) and renderMeasure(height) (mounted row reports true height through the estimate->anchor channel).

## 2. Markdown (@wingleeio/mugen-markdown)
- 2a. Parser: incremark (@incremark/core) -> mdast, memoized by (source, options). Growing the same source appends only new text to a retained parser (O(delta)); unchanged source served from AST cache. Pure & synchronous -> safe inside measure walk. GFM default.
- 2b. AST -> mugen primitives; identical tree in measure walk and render. Block nodes overridable.
- 2c. RichText primitive: mixed-font inline runs measured as one wrapping flow via pretext rich-inline layout; height = lines x lineHeight. InlineFormat threads nested marks (bold in heading -> one bold run at heading size). Inline boxes (advance+content), noLigatures for code spans, links, decorations. Inline styling via theme (numbers), not components.
- 2d. CodeBlock: code doesn't wrap -> height = lineCount x lineHeight + padding (+ fixed header), width-independent. Syntax highlight is PURE PAINT: plain text renders immediately (layout source of truth); tokenize off critical path in time-sliced chunks; paint colors onto canvas tiles over the text; flip text transparent same frame. Streaming appends re-tokenize only the tail. Tiles allocate near viewport only.
- 2e. Streaming fade-in: layout commits instantly (heights exact); a canvas veil over new characters dissolves — fade is purely cosmetic paint, out of flow, never measured. prefers-reduced-motion honored.
- 2f. Theme = concrete numbers for everything affecting height (fonts, line-heights, paddings, gaps). "Numbers drive measured height; colours are paint."

## 3. Session-doc parts -> rendered blocks
- 3a. foldEventIntoParts (packages/control/src/parts.ts:132): folds AgentEvents into MessagePart[] — SessionStarted/Steered reset (replay-safe); TextDelta appends to trailing text part; ToolCall append/refresh-by-id (idempotent); ToolResult sets isError; Error/Done(error) -> error parts. Kinds: text|tool|input|error.
- 3b. parseParts (parts.ts:205): persisted body JSON -> MessagePart[]; called only when messages change, NEVER per token.
- 3c. Desktop: one row = one message; groupRowParts folds consecutive tools into collapsible group. Mobile: one row = one BLOCK — splitTextBlocks slices text at incremark's top-level AST position.offset boundaries so each slice re-parses to exactly that block; guards (definitions/footnotes stay whole; join mismatch -> unsplit). Gaps: turn 14 / block 8 / md blockGap 14.
- 3d. Live turn <-> persisted handoff: live rows get ids matching eventual persisted ids (`${msgId}#${blockId}`) so row identities are reused on persist -> no flicker. Live turn stays UNSPLIT (boundaries shift while streaming); re-splits on persist. `__live__#` rows excluded from all caches.

## 4. Smoothness tricks
- heightMemo (per-instance, identity-guarded) — collapses per-token full re-walk to one row.
- messageRowsCache keyed id+content.length+deviceIds — persisted messages are append-only so rows built once (fixes "streaming stutter" of rebuilding all rows per sync tick).
- Split cache keyed part-id+text.length.
- Persistent height cache (mobile): SQLite, keys `${width}:${rowKey}`, 800ms debounced writes, VERSION-salted table (bump on any measurement-affecting change), boot warmer.
- Paint cache for laid-out lines, keyed by compact dual 32-bit hash + length; streaming rows skipped.
- rAF batching: window repaint once per frame; selector-based scroll subscriptions (re-render only when selected slice changes, e.g. distanceFromBottom > 320 for scroll-to-bottom button).
- Effects run for every row on- or off-screen so off-screen heights stay exact.
- Font-settle invalidation: on font load, clear text caches, re-measure.
- pretext prepare()/layout() split: prepare = one-time (normalize, segment, measure segments -> handle); layout = pure arithmetic on resize. Memoize prepare per (font, options, text).
- Controlled viewport dimensions passed in (no blank first frame waiting on layout).

## Constants to port
DEFAULT_TWEEN_MS = 200; STICK_THRESHOLD_PX = 70; overscan desktop 320 / mobile 1800; height-cache debounce 800ms; width-settle 40ms; scroll-to-bottom button threshold 320px; gaps turn 14 / block 8 / mdBlock 14; lazyMeasure head 8 / tail 30.

## gpui mapping notes (added by orchestrator)
- gpui's `list()` already measures rendered elements + sum_tree offsets — closest analog to mugen. But mugen's analytic-height + estimate-refinement + anchor-absorption model is richer; we can either lean on gpui ListState (measure-based, in-process = cheap in Rust) or port analytic heights for exact cold-open offsets.
- Stick-to-bottom spring w/ input-based interrupt must be hand-built on ListState (is_following_tail + scroll handlers + wheel events).
- CodeBlock "highlight is pure paint" maps naturally: gpui text runs with colors don't change layout if font metrics identical — highlighting in gpui never affects layout anyway (monospace, same font). Time-sliced tokenization via background_spawn.
- Streaming fade-in: paint-layer overlay (opacity animation on newly appended runs) via with_animation.
- Block-granularity rows + stable row ids (`msgId#blockId`) and live-turn-unsplit rules port directly.
