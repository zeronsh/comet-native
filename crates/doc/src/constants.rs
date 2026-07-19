//! Constants carried over from comet `packages/session-doc/src/constants.ts`.
//! Per the original design these are starting points — re-measure with real heavy sessions.

/// Max bytes for a single message entry before continuation splitting.
pub const MSG_INLINE_MAX: usize = 256 * 1024;
/// History retention window for shallow-snapshot trimming (days).
pub const RETAIN_DAYS: u32 = 30;
/// Session DO folds its update log into the snapshot at this size (lossless).
pub const COMPACT_LOG_BYTES: usize = 8 * 1024 * 1024;
/// Soft ceiling for total doc size before aggressive trim.
pub const SOFT_CEILING_BYTES: usize = 25 * 1024 * 1024;
/// Host commits streamed assistant segments into the doc at this cadence (ms).
pub const STREAM_COMMIT_MS: u64 = 120;
/// Session DO batches update-log flushes at this cadence (ms).
pub const DO_FLUSH_MS: u64 = 5_000;
/// Byte budget for the in-memory doc LRU on device backends.
pub const DOC_LRU_BYTE_BUDGET: usize = 80 * 1024 * 1024;
/// Number of trailing messages materialized into the tail sidecar.
pub const TAIL_MESSAGE_COUNT: usize = 64;
/// Terminal output batching cadence (ms).
pub const TERMINAL_OUTPUT_BATCH_MS: u64 = 12;
/// Default TTL for durable commands.
pub const COMMAND_DEFAULT_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// Current session doc schema version (`meta.schemaVersion`).
pub const SESSION_SCHEMA_VERSION: u32 = 1;
