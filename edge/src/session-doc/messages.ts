/**
 * Session-doc message entries (design §1.1, §6) and the segment split cap
 * (§1.2 safeguard 1): oversized message entries split into continuation
 * entries so no single Loro op/poke a client must apply exceeds
 * {@link MSG_INLINE_MAX}. Free, invisible — renderers stitch continuations
 * back onto their parent by `continuationOf`.
 */
import type { UserInputQuestion } from "./control-types";
import { MSG_INLINE_MAX } from "./constants";
import type { RenderToolCall, SessionMessagePart } from "./render-parts";

export type SessionMessageRole = "user" | "assistant" | "system";

/**
 * The doc-resident part shape — one flat map per part inside a message's
 * `parts` LoroList. Text parts store their text in a LoroText container so
 * streaming is an RLE-mergeable append (measured 1.03x oplog overhead vs 125x
 * for whole-value rewrites — see oplog-shape.test.ts; this is the schema
 * correction anticipated by design open item §10.4). All other fields are
 * written once, so plain values are fine.
 */
export interface DocMessagePart {
  readonly id: string;
  readonly kind: "text" | "tool" | "input" | "error";
  /** kind === "text" — LoroText in the doc, string in mirror state. */
  readonly text?: string;
  /** kind === "tool" — render-only (§1.2). */
  readonly call?: RenderToolCall;
  readonly isError?: boolean;
  /** kind === "input" — the part id doubles as the requestId. */
  readonly questions?: ReadonlyArray<UserInputQuestion>;
  readonly resolved?: boolean;
  /** kind === "error". */
  readonly message?: string;
}

/** App-layer parts → doc parts. Input parts key on their requestId. */
export const toDocParts = (
  parts: ReadonlyArray<SessionMessagePart>
): ReadonlyArray<DocMessagePart> =>
  parts.map((p): DocMessagePart => {
    switch (p.kind) {
      case "text":
        return { id: p.id, kind: "text", text: p.text };
      case "tool":
        return {
          id: p.id,
          kind: "tool",
          call: p.call,
          ...(typeof p.isError === "boolean" ? { isError: p.isError } : {})
        };
      case "input":
        return {
          id: p.requestId,
          kind: "input",
          questions: p.questions,
          ...(typeof p.resolved === "boolean" ? { resolved: p.resolved } : {})
        };
      case "error":
        return { id: p.id, kind: "error", message: p.message };
    }
  });

/** Doc parts → app-layer parts (render path). Malformed entries degrade to
 * empty-text parts rather than throwing mid-render. */
export const fromDocParts = (
  parts: ReadonlyArray<DocMessagePart>
): ReadonlyArray<SessionMessagePart> =>
  parts.map((p): SessionMessagePart => {
    switch (p.kind) {
      case "tool":
        return p.call
          ? {
              kind: "tool",
              id: p.id,
              call: p.call as never,
              ...(typeof p.isError === "boolean" ? { isError: p.isError } : {})
            }
          : { kind: "text", id: p.id, text: "" };
      case "input":
        return {
          kind: "input",
          requestId: p.id,
          questions: (p.questions ?? []) as never,
          ...(typeof p.resolved === "boolean" ? { resolved: p.resolved } : {})
        };
      case "error":
        return { kind: "error", id: p.id, message: p.message ?? "" };
      default:
        return { kind: "text", id: p.id, text: p.text ?? "" };
    }
  });

/** Streaming lifecycle (§6): the stream IS the transcript being written.
 * Host recovery marks abandoned streams `aborted`, preserving a crashed
 * turn's partial output on every device. User/system entries are always
 * `complete`. */
export type SessionMessageStatus = "streaming" | "complete" | "aborted";

export interface SessionTokenUsage {
  readonly inputTokens: number;
  readonly outputTokens: number;
  readonly reasoningTokens?: number;
  readonly cachedInputTokens?: number;
}

/** One entry in the doc's `messages` list. Writer discipline (§1.1): any peer
 * inserts its own entries; the host is the sole writer of assistant/system
 * entries and of edits to any entry. */
export interface SessionMessageEntry {
  /** Client-minted UUID (same as today) — the idempotence key that makes
   * stale-peer re-submission (§3.1) and duplicate delivery no-ops. */
  readonly id: string;
  readonly role: SessionMessageRole;
  readonly parts: ReadonlyArray<DocMessagePart>;
  readonly tokens?: SessionTokenUsage;
  readonly createdAt: number;
  readonly deviceId: string;
  readonly status?: SessionMessageStatus;
  /** Set on continuation entries produced by the segment split cap; points at
   * the root entry's id. Renderers concatenate parts in list order. */
  readonly continuationOf?: string;
}

const encoder = new TextEncoder();
const partBytes = (part: DocMessagePart): number =>
  encoder.encode(JSON.stringify(part)).length;

/** Deterministic continuation id: stable across re-splits of the same entry,
 * so re-submission after a stale-peer resync stays idempotent. */
export const continuationId = (rootId: string, index: number): string => `${rootId}#c${index}`;

/**
 * Split an entry whose encoded parts exceed {@link MSG_INLINE_MAX} into a root
 * entry plus continuation entries, splitting only at part boundaries except
 * when a single part is itself oversized (then its text is chunked). Entries
 * already under the cap return as a single-element array — the common case
 * costs one size check.
 */
export const splitMessageEntry = (
  entry: SessionMessageEntry,
  maxBytes: number = MSG_INLINE_MAX
): ReadonlyArray<SessionMessageEntry> => {
  const sizes = entry.parts.map(partBytes);
  const total = sizes.reduce((a, b) => a + b, 0);
  if (total <= maxBytes) return [entry];

  // Explode oversized text parts into chunks first; non-text parts are never
  // near the cap under the render-only policy but are kept whole regardless.
  const atoms: DocMessagePart[] = [];
  entry.parts.forEach((part, i) => {
    if (part.kind === "text" && part.text !== undefined && sizes[i]! > maxBytes) {
      const text = part.text;
      // Chunk by code points to avoid splitting surrogate pairs. Budget in
      // UTF-16 units approximated from the byte cap; JSON escaping overhead is
      // covered by the 4x safety divisor.
      const chunkLen = Math.max(1, Math.floor(maxBytes / 4));
      let n = 0;
      for (let off = 0; off < text.length; off += chunkLen, n++) {
        atoms.push({ kind: "text", id: `${part.id}#t${n}`, text: text.slice(off, off + chunkLen) });
      }
    } else {
      atoms.push(part);
    }
  });

  const groups: DocMessagePart[][] = [];
  let current: DocMessagePart[] = [];
  let currentBytes = 0;
  for (const atom of atoms) {
    const bytes = partBytes(atom);
    if (current.length > 0 && currentBytes + bytes > maxBytes) {
      groups.push(current);
      current = [];
      currentBytes = 0;
    }
    current.push(atom);
    currentBytes += bytes;
  }
  if (current.length > 0) groups.push(current);

  return groups.map((parts, i) =>
    i === 0
      ? { ...entry, parts }
      : {
          ...entry,
          id: continuationId(entry.id, i),
          parts,
          continuationOf: entry.id,
          // Continuations carry no duplicate token accounting.
          tokens: undefined
        }
  );
};

/** Stitch continuation entries back onto their roots (render-time inverse of
 * {@link splitMessageEntry}); preserves list order otherwise. */
export const joinContinuations = (
  entries: ReadonlyArray<SessionMessageEntry>
): ReadonlyArray<SessionMessageEntry> => {
  if (!entries.some((e) => e.continuationOf)) return entries;
  const rootIndex = new Map<string, number>();
  const order: SessionMessageEntry[] = [];
  for (const entry of entries) {
    if (entry.continuationOf) {
      const at = rootIndex.get(entry.continuationOf);
      if (at !== undefined) {
        const root = order[at]!;
        order[at] = { ...root, parts: [...root.parts, ...entry.parts] };
        continue;
      }
      // Orphan continuation (root trimmed or not yet synced): surface as-is
      // rather than dropping content.
      order.push(entry);
      continue;
    }
    rootIndex.set(entry.id, order.length);
    order.push(entry);
  }
  return order;
};
