/**
 * Tail materialization — vendored verbatim from comet's
 * packages/session-doc/src/schema.ts (`readMessageEntries` /
 * `materializeTail`), minus the loro-mirror schema those functions do not
 * depend on. The DO reads the doc's plain JSON shape directly; no Mirror.
 */
import { LoroDoc } from "loro-crdt";
import { SESSION_SCHEMA_VERSION, TAIL_MESSAGE_COUNT } from "./constants";
import { joinContinuations, type SessionMessageEntry } from "./messages";
import type { SessionTail } from "./sidecar";

/** Read the doc's message entries without a Mirror (used by the DO for tail
 * materialization). `doc.toJSON()` yields the plain state shape for
 * lists-of-maps. */
export const readMessageEntries = (doc: LoroDoc): ReadonlyArray<SessionMessageEntry> => {
  const json = doc.toJSON() as { messages?: SessionMessageEntry[] };
  return json.messages ?? [];
};

/** Materialize the DO's `tail` slot (§5 L2): last-N messages with
 * continuations joined, plus enough meta for the client to render instantly
 * and know how much history the full sync will bring. */
export const materializeTail = (
  doc: LoroDoc,
  now: number,
  tailCount: number = TAIL_MESSAGE_COUNT
): SessionTail => {
  const json = doc.toJSON() as {
    meta?: { chatId?: string; schemaVersion?: number };
    messages?: SessionMessageEntry[];
  };
  const all = joinContinuations(json.messages ?? []);
  return {
    chatId: json.meta?.chatId ?? "",
    schemaVersion: json.meta?.schemaVersion ?? SESSION_SCHEMA_VERSION,
    messages: all.slice(-tailCount),
    totalMessages: all.length,
    updatedAt: now
  };
};
