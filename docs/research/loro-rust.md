# Loro Rust Ecosystem (as of 2026-07-19)

## Versions
- Rust crate `loro` = 1.13.7 (2026-07-15); npm `loro-crdt` = 1.13.7 — lockstep releases from one
  monorepo; SAME binary format, stabilized at 1.0 ("no breaking changes"). Rust <-> JS interop
  guaranteed: bytes export/import cleanly both directions.

## Rust API surface (docs.rs/loro)
- LoroDoc: new(), get_text/get_list/get_map/get_movable_list/get_tree/get_counter, export(ExportMode),
  import(bytes), fork(), fork_at(), oplog_vv(), state_frontiers(), get_deep_value(), checkout(&frontiers),
  checkout_to_latest().
- Containers: LoroText (insert/delete/apply_delta/mark), LoroList, LoroMap, LoroMovableList (mov/set),
  LoroTree, LoroCounter.
- Subscriptions: doc.subscribe(container_id, cb), subscribe_root(cb), subscribe_local_update(cb)
  (binary updates for network sync). Subscription guard; drop = unsubscribe.
- ExportMode: Snapshot, Updates{from: vv}, UpdatesInRange, ShallowSnapshot(frontiers),
  StateOnly(Option<frontiers>), SnapshotAt{version}. Helpers: ExportMode::snapshot(), updates(&vv), etc.
- UndoManager: undo/redo, group_start/end, set_merge_interval, exclude-origin prefixes; undoes only
  local ops from the bound peer (correct collaborative semantics).
- EphemeralStore (loro::awareness::EphemeralStore): timestamp-LWW presence store, per-key timeout
  (JS default 30s), encode/encode_all/apply, local/remote/timeout subscription triggers. In BOTH
  Rust and JS. (Comet's %EPH presence maps to this.)
- Serde: LoroValue impls Serialize/Deserialize, From<serde_json::Value>, ToJson trait.

## Mirror layer
- loro-mirror is TypeScript-ONLY (schema DSL, Mirror, setState diff, subscribe w/ local/remote source).
  No official Rust port.
- Closest Rust equivalent: `lorosurgeon` 0.2.1 (2026-07-04, MIT, third-party; "autosurgeon for Loro"):
  #[derive(Hydrate)] (doc -> structs), #[derive(Reconcile)] (structs -> minimal ops, Myers/LCS for
  Vec, #[loro(movable)] + #[key] -> LoroMovableList, #[loro(text)] char-level LoroText diff),
  DocSync (to_doc/from_doc, #[loro(root)]), VersionGuard for stale heads.
  GAP: no incremental event-driven mirror — either re-hydrate subtrees on subscribe events or
  hand-write: subscribe_root -> walk event diffs (map/list/text deltas) -> patch cached Rust state.
- Plan: build a small `comet-mirror` crate: typed schema structs + incremental diff application over
  doc.subscribe events + lorosurgeon-style reconcile for writes (evaluate lorosurgeon as a dep vs
  hand-rolling; our schema is small and known).

## wasm32 / Workers
- loro compiles to wasm32-unknown-unknown (getrandom "js" feature wired since PR #681); works in
  Cloudflare Workers (issue #440 fixed 0.16.11+). wasm ~3.1MB raw / 1.03MB gzip.

## Compaction / history trim
- ShallowSnapshot(frontiers): keeps state + history since frontiers; typically 70-90% smaller.
  StateOnly even smaller. Pattern: full Snapshot -> R2 cold storage, ShallowSnapshot for active use.
- Sync caveat: peers can only sync if their version is after the shallow start — the session DO is
  the natural sequencer for compaction (comet already does daily frontier checkpoints >= RETAIN_DAYS).
- Redaction: loro::json::redact(json, version_range) — export_json_updates -> redact -> fresh doc.

## 2026 changes relevant to chat transcripts
- 1.13.7: snapshot import ~2x faster; local text edit ~35% faster; checkout-hang fix.
- 1.13.x: lazy snapshot loading (containers decode lazily — memory-friendly for big docs);
  O(n^2) UTF-16 position-validation fixes; deadlock fixes.
- 1.13.0: ensure_mergeable_{map,list,text,...} — concurrently-created child containers at same key
  converge (useful for per-message metadata).
- 1.12.0: atomic update imports w/ rollback.
- Watch: #940 (storage-backed LoroDoc, open), #1040 (shallow-snapshot import w/ concurrent ops can
  stall pending — compact at quiesced points; comet's daily-checkpoint scheme should respect this).
