/**
 * Non-CRDT sidecar payloads served by the session DO (design §5 L2, §6.1).
 * These are plain JSON shapes shared by the DO (producer), the Worker routes
 * (`GET /tail/{chatId}`, `GET /diff/{chatId}`), and every client.
 */
import type { SessionMessageEntry } from "./messages";

/** Materialized last-N-messages JSON kept in the DO's `tail` slot, refreshed
 * on segment commit. Powers instant-open: a device with no local doc copy
 * paints this in ~50–150ms while the full doc syncs behind it (same message
 * ids → the mirror replaces the tail render without flicker). */
export interface SessionTail {
  readonly chatId: string;
  readonly schemaVersion: number;
  /** Last {@link TAIL_MESSAGE_COUNT} entries, continuations already joined. */
  readonly messages: ReadonlyArray<SessionMessageEntry>;
  /** Total entries in the doc, so the client can show "N earlier messages". */
  readonly totalMessages: number;
  readonly updatedAt: number;
}

/** File summary mirrored from today's checkout-diff pipeline. */
export interface DiffFileSummary {
  readonly path: string;
  readonly oldPath?: string;
  readonly status: "added" | "modified" | "deleted" | "renamed" | "copied" | "unmerged";
  readonly additions: number;
  readonly deletions: number;
  readonly binary: boolean;
}

/** Latest-only working-tree diff, host-published to the DO's `diff` slot and
 * overwritten on each publish (§6.1). Zero history by construction; clients
 * persist the last-received copy per chat and stale-mark it offline. */
export interface DiffSidecar {
  readonly chatId: string;
  readonly deviceId: string;
  readonly checkoutPath: string;
  readonly branch?: string;
  readonly headSha?: string;
  /** Unified patch, bounded (same 3MiB cap as today). */
  readonly patch: string;
  readonly files: ReadonlyArray<DiffFileSummary>;
  readonly additions: number;
  readonly deletions: number;
  readonly truncated: boolean;
  readonly publishedAt: number;
}

/** Low-churn summary optionally denormalized into `meta` for inline badges. */
export interface DiffSummary {
  readonly fileCount: number;
  readonly additions: number;
  readonly deletions: number;
  readonly publishedAt: number;
}
