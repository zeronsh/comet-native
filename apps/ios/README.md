# Zeron for iOS

A native SwiftUI viewport onto the zeron mesh. The phone is a **peer
device**: it joins the same Loro CRDT rooms as every other device (workspace
doc + per-chat session docs over the edge's Durable Objects), renders the
mirrors, and drives remote engines through the durable command queue. No
engine runs on the phone.

## Build & run

Requires Xcode 26+ (iOS 26 SDK — Liquid Glass APIs).

```sh
cd apps/ios
xcodebuild -project Zeron.xcodeproj -scheme Zeron \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build
```

Or open `Zeron.xcodeproj` in Xcode and run. Dependencies (SPM, resolved
automatically): [loro-swift 1.13.x](https://github.com/loro-dev/loro-swift)
(matches the engine's loro 1.13), [swift-markdown](https://github.com/swiftlang/swift-markdown)
(cmark-gfm: tables/strikethrough/tasklists — the same feature set as the
desktop's pulldown-cmark config).

## TestFlight release

Run the **TestFlight** workflow from GitHub Actions on `main`. It compiles the
iOS app and tests, selects the next numeric build number from App Store
Connect, archives with automatic signing, uploads an internal-only TestFlight
build, and waits for Apple processing to report `VALID`.

The workflow uses the `AC_API_KEY_P8`, `AC_API_KEY_ID`, and
`AC_API_ISSUER_ID` repository secrets.

### Connecting

- **WorkOS**: enter the edge URL, open the sign-in page on any device, paste
  the code it shows (`/auth/exchange`), pick an org (`/auth/refresh` re-scopes
  the token with the `org_id` claim).
- **Dev**: against an `AUTH_MODE=dev` edge (e.g. `wrangler dev`), enter a user
  id + org id; the bearer is `userId@orgId`.
- **Demo mode**: fully offline dataset with a scripted streaming reply —
  explore the UI with no infrastructure. Launch args for screenshot rigs:
  `-demo [-route chat:<id>|space:<id>] [-stream]`.

## Architecture

```
Sync/
  LoroProtocol.swift    loro-protocol 0.3 wire codec (byte-compatible port of
                        the crate's encoding.rs: magic/varBytes/type/payload)
  RoomClient.swift      room.rs port: join with oplog VV, snapshot backfill,
                        resubmit-from-server-VV, DocUpdate+Ack, fragments,
                        %EPH presence sub-room, ping/pong lease, backoff
  WorkspaceStore.swift  ws3/{org}/{user} mirror: devices/spaces/chats/sessions
                        rows, presence heartbeats, viewer-side writes
                        (createChat, archive, lastSeenAt, own device row)
  SessionStore.swift    session doc mirror: entries/parts (continuations
                        joined), command ledger appends (rule 1), host nudge
Markdown/
  MarkdownModel.swift   block model + incremental tail re-parser (re-parse
                        from the 2nd-to-last top-level block; link-defs force
                        full parses) — parser.rs port
  Highlight.swift       line tokenizer with carry state, paint-only
  MarkdownBlockView.swift  mobile metrics: body 17/26, headings 23/31…17/26,
                        code 14/21 (Dynamic Type scaled line rows), violet inline code,
                        accent blockquotes, hairline tables
Transcript/
  TranscriptRows.swift  rows_for_entry port: block-granularity rows, stable
                        ids ({msg}#{part}.{block}, {msg}#g{n}), fingerprint
                        versions, consecutive-tool grouping
  NativeTranscriptTable.swift  UIKit row reuse, gesture anchoring and
                        animated local-send runway retained across navigation
  TranscriptView.swift  SwiftUI message content, user-message folding, follow
                        (70pt re-engage, 140pt jump), tool activity rail
  Veil.swift            paint-only streaming fade (EMA-tracked duration,
                        1−(1−p)^1.6 curve)
Composer/               glass pill, Send→Steer→Stop morph, QuestionPanel
                        (paged, numbered options, 220ms auto-advance)
Theme/                  theme.rs port: oklch→sRGB converter, exact palette,
                        Geist/Geist Mono, motion timings + flavour words
```

### Parity notes (desktop ⇄ mobile translations)

| Desktop | iOS |
| --- | --- |
| Sidebar: Spaces + attention-sorted Sessions | Home screen sections (same sort ranks: awaiting > errored > working > completed > idle) |
| Horizontal session tabs per space | Space detail: vertical session list (creation order) |
| Tab close = archive | Swipe-to-archive |
| Archived shelf under the sidebar list (open by default, Show-more paging, hover-swap Unarchive) | Same shelf under Home/space lists; unarchive is swipe-to-unarchive |
| Status word in the row corner (muted dots; Done keeps its pop; spinner rides bottom-right) | Same, same colors |
| Composer `white_alpha(0.03)` pill + hairline | Liquid Glass pill (`glassEffect`) + hairline |
| Harness brand SVG marks (icons.rs) | Same path data via a native SVG path parser (`BrandMarks.swift`) |
| Harness/model picker popover + curated catalogs | Brand-mark cards + catalog menu + reasoning-ladder chips (`HarnessCatalog.swift`, ported from crates/harness) |
| Add-space palette (device + folder browser) | New-space sheet: device tabs + remote folder browser (ListFolders over the device-room relay, git repos badged) |
| ControlRpc over device-room relay | `DeviceRelayClient` — binary `uleb128(len)+header+payload` frames, `{"s","k","to","from"}` header, ndjson ControlRpc; used for ListFolders + direct-to-host `Mutate {createSpace}` (local doc-write fallback when the host is offline) |
| Hover timestamps / copy | Context menus |
| Long user messages: Show more / Show less | Five-line preview, 44pt disclosure target, expansion retained per session |
| gpui `list()` sum-tree virtualization | Native table row reuse with SwiftUI content, stable row ids and version fingerprints |
| Stick-to-bottom spring, wheel-up breaks pin | Gesture-owned follow, composer-sized viewport with glass underlap, retained local prompt runway |

Status colors, font families, veil timing, command-ledger shapes, and wire
protocol follow desktop. Text sizes and touch targets are adapted for phones;
see [mobile polish and simulator coverage](../../docs/mobile-polish.md).

### Writer discipline (what the phone writes)

- Workspace doc: its own device row, chat creates (host = the space's owning
  device), `archived`/`title`/`lastSeenAt` LWW sets, presence heartbeats.
- Session docs: append commands (`run`/`steer`/`interrupt`/`respondInput`)
  with client-minted message ids for optimistic echo; add and reorder shared
  queue rows. Queue edits and removals require host acknowledgement. The host
  writes all transcript entries and command outcomes.
- After queuing a command it POSTs `/device/{host}/nudge` so a cold host
  opens the doc and drains — delivery stays durable in the doc regardless.

### Shared message queue

Messages submitted during an active turn use the host's shared queue when it
advertises support. The composer toolbar's **Queue / Steer** menu persists the
phone's preference; **Queue** remains the default. Steer allows automatic
mid-turn delivery when the provider supports it. Attachments wait for a turn
that can accept files.

Rows offer **Steer** for a host-advertised mid-turn provider and text-only
messages, or **Send now** (interrupt) otherwise. Unknown provider capabilities
leave delivery actions unavailable. Edits require a host lease; removal waits
for the host's acknowledgement and keeps the row inert while pending. Failed
or unconfirmed actions show an error above the composer and trigger sync repair.
Deleting an actively edited row discards it without first releasing its lease.

With an external keyboard, **Command+Return** submits the draft (including
attachment-only drafts), saves an active queue edit, or activates the first
queued row when the composer is empty. It never skips a blocked head or stops
the agent merely because the draft is empty.
