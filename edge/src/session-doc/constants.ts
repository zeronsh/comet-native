/**
 * Tunables for the Loro session-doc pipeline (design: loro-sessions §1.2, §3,
 * §5, §6, §10.3). Every constant here is a starting point to be re-measured
 * against a real converted heavy session; keep them in one place so the
 * measurement pass has a single file to touch.
 */

/** Segment split cap: a single `messages` entry never exceeds this many bytes
 * of JSON-encoded parts. Oversized turns split into continuation entries —
 * not a size reducer, a spike smoother (bounds the largest single op/poke a
 * client must apply). */
export const MSG_INLINE_MAX = 256 * 1024;

/** Shallow-compaction retention: the DO re-exports a shallow snapshot at the
 * `now − RETAIN_DAYS` frontier when the update log grows past
 * {@link COMPACT_LOG_BYTES}. Trimmed op history is discarded permanently;
 * state is fully preserved. A peer offline longer than this re-syncs fresh and
 * re-submits its unacked entries at the app layer (idempotent by entry id). */
export const RETAIN_DAYS = 30;

/** Update-log size that triggers a compaction pass in the session DO. */
export const COMPACT_LOG_BYTES = 8 * 1024 * 1024;

/** Soft ceiling: past this much doc state the UI nudges toward a fresh
 * session. No enforcement machinery — a product stance, not a limit. */
export const SOFT_CEILING_BYTES = 25 * 1024 * 1024;

/** Host stream batching: token/part deltas are committed to the doc on this
 * cadence while a run is streaming (single-peer appends RLE-merge in Loro). */
export const STREAM_COMMIT_MS = 120;

/** DO durability batching during active streams: buffered updates are flushed
 * to SQLite on this cadence. A crash losing the buffer is healed by normal
 * CRDT resync from the host on reconnect. */
export const DO_FLUSH_MS = 5_000;

/** Mobile doc LRU budget — bytes of resident doc *state*, not doc count.
 * Eviction drops the Mirror + doc; state stays on disk. */
export const DOC_LRU_BYTE_BUDGET = 80 * 1024 * 1024;

/** How many trailing messages the DO materializes into its `tail` slot for the
 * L2 instant-open path (mirrors today's RECENT_MESSAGES_LIMIT). */
export const TAIL_MESSAGE_COUNT = 64;

/** Default TTL for durable commands that should not run on a host that wakes
 * up much later (staleness guard §7). Composers may override per command. */
export const COMMAND_DEFAULT_TTL_MS = 24 * 60 * 60 * 1000;

/** Current session-doc schema version, written to `meta.schemaVersion`. */
export const SESSION_SCHEMA_VERSION = 1;

/** Terminal output batching over the device DO (§8). */
export const TERMINAL_OUTPUT_BATCH_MS = 12;
