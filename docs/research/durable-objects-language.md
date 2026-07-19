# Durable Objects: Rust (workers-rs) vs TypeScript — Decision (2026-07)

## Decision: KEEP THE DOs IN TYPESCRIPT (reuse/adapt comet's existing apps/edge).

Deciding fact: Loro's core is already Rust compiled to wasm — loro-crdt npm wraps the same Rust
engine a workers-rs build would link. Rewriting the DO layer in Rust buys ~zero performance on the
only CPU-heavy path (compaction/snapshot export) while adding real risk.

## Evidence
- workers-rs (worker 0.8.5, 2026-06-12) DOES support: DO SQLite storage, WebSocket hibernation
  (PR #436/#728), alarms, R2 incl. multipart. But README still says "expect rough edges"; RPC
  experimental; everything runs through a wasm-bindgen JS shim.
- CRITICAL open bug: workers-rs #722 — ~100MB wasm memory NOT freed per DO eviction/re-init cycle;
  fix PR #832 ("eager memory deallocation on hibernation") still open, blocked on workerd #5211.
  Exactly our failure mode: hibernating DO holding a large LoroDoc in a shared 128MB isolate.
- Limits: worker bundle 3MB free / 10MB paid (gzip); 128MB per isolate INCLUDING wasm, shared by
  co-located DOs (billed at full 128MB each); startup (global scope) limit 1s (raised from 400ms
  2025-10) — never init wasm at top level, do it lazily in the DO constructor path.
- loro-crdt 1.13.7 wasm: 3.1MB raw / ~1.03MB gzip — fits fine. Workers compatibility confirmed
  (loro issue #440 fixed in 0.16.11; use lazily-init / base64 entry if needed).
- Hibernation is language-agnostic: in-memory state resets on eviction; LoroDoc must be lazily
  rebuilt from SQLite snapshot+update log on wake in EITHER language. serializeAttachment limit now
  16,384 bytes. setTimeout/setInterval PREVENT hibernation — schedule the 5s flush only while dirty
  (or use alarms) so idle rooms hibernate.
- Pricing has no language-sensitive CPU component; dominant cost lever is maximizing hibernation.
- Ecosystem: production CRDT-DO backends (pluv, y-durableobjects, PartyKit-style) are TS + wasm cores.

## Consequences for comet-native
- apps/edge (session-room, device-room, worker front, auth, R2 attachments) carries over as the
  TS edge — port/adapt, don't rewrite. All 14 smoke assertions already exist.
- Rust backend + gpui app use the `loro` Rust crate 1.13.7; binary format identical to JS 1.13.7
  (stable since 1.0, lockstep releases) — cross-language sync is guaranteed.
- Revisit only if workers-rs lands #832/workerd #5211 AND a profiled bottleneck appears in the
  orchestration layer.

(Full citations in agent report: crates.io/worker, workers-rs README + PRs 436/495/728, issues
722/832, workerd 5211, CF DO docs on websockets/pricing/limits, changelog 2025-10-10, nickb.dev,
pluv, loro #440/#681.)

## Additional community-verdict evidence (second verification pass)
- Cloudflare blog "Making Rust Workers reliable" (2026-04): panics historically fatal/state-poisoning
  for DOs; panic=unwind only arrived in worker 0.8.0 and is not yet default — hardening in progress.
- Every notable CRDT/collab framework on DOs writes the DO layer in TS (PartyKit/PartyServer —
  now IN the Cloudflare org, y-durableobjects, Jazz, TinyBase, Liveblocks); Rust CRDTs (Loro, yrs)
  appear only as wasm consumed from JS. No published workers-rs-native CRDT DO server exists.
- HN 2025 anecdote: >10x CPU-time penalty for non-trivial Rust worker vs JS (JS-boundary
  serialization); wasm cannot shrink linear memory.
- Outgoing-WebSocket hibernation is an open workerd request (#4864) in both languages.
