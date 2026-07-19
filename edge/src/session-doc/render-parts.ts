/**
 * Render-only tool-part policy (design §1.2): tool parts stored in a session
 * doc carry exactly what the UI displays, nothing more. The full tool input
 * exists only in the host's local run journal; expanded tool cards on other
 * devices show the display line, never the underlying code.
 *
 * Keep: tool tag, id, status, and the rendered display data — Exec `command`,
 * Read/Write/Edit/ApplyPatch `path`(s), Search/Glob patterns, WebFetch `url`,
 * WebSearch `query`, Todo items (plan text), Mcp `server`/`tool`.
 * Drop: non-rendered args — WriteFile `content`, EditFile `oldString`/
 * `newString`, WebFetch `prompt`, Mcp/Unknown `input`.
 *
 * This generalizes two existing precedents: tool *outputs* are already
 * discarded at the event layer (harness ToolResult carries only `isError`),
 * and ApplyPatch already stores path-only summaries.
 */
import type { MessagePart, ToolCall } from "./control-types";

/** A tool call reduced to its render surface. Structurally a subset of
 * {@link ToolCall}; every variant here is assignable-from its wire twin after
 * {@link sanitizeToolCall}. */
export type RenderToolCall =
  | { readonly _tag: "Exec"; readonly command: string; readonly background?: boolean }
  | { readonly _tag: "ReadFile"; readonly path: string }
  | { readonly _tag: "WriteFile"; readonly path: string }
  | { readonly _tag: "EditFile"; readonly path: string }
  | {
      readonly _tag: "ApplyPatch";
      readonly changes: ReadonlyArray<{
        readonly path: string;
        readonly kind: "add" | "delete" | "update";
      }>;
    }
  | { readonly _tag: "Search"; readonly pattern: string; readonly path?: string }
  | { readonly _tag: "Glob"; readonly pattern: string; readonly path?: string }
  | { readonly _tag: "WebFetch"; readonly url: string }
  | { readonly _tag: "WebSearch"; readonly query: string }
  | {
      readonly _tag: "Todo";
      readonly items: ReadonlyArray<{ readonly text: string; readonly done: boolean }>;
    }
  | { readonly _tag: "Mcp"; readonly server?: string; readonly tool: string }
  | { readonly _tag: "Unknown"; readonly name: string };

/** A message part as stored in the session doc: identical to the app-layer
 * {@link MessagePart} except tool calls are render-only. */
export type SessionMessagePart =
  | Exclude<MessagePart, { kind: "tool" }>
  | {
      readonly kind: "tool";
      readonly id: string;
      readonly call: RenderToolCall;
      readonly isError?: boolean;
    };

/** Reduce a wire tool call to its render surface. Idempotent — feeding an
 * already-sanitized call back through is a no-op, which is what makes the
 * one-time migration and live path share this function. Unrecognized tags
 * (from a future harness) pass through untouched rather than being dropped —
 * better to store an unknown shape than to erase a rendered call. */
export const sanitizeToolCall = (call: ToolCall | RenderToolCall): RenderToolCall => {
  switch (call._tag) {
    case "Exec": {
      const { command, background } = call as { command: string; background?: boolean };
      return background === undefined
        ? { _tag: "Exec", command }
        : { _tag: "Exec", command, background };
    }
    case "ReadFile":
      return { _tag: "ReadFile", path: (call as { path: string }).path };
    case "WriteFile":
      return { _tag: "WriteFile", path: (call as { path: string }).path };
    case "EditFile":
      return { _tag: "EditFile", path: (call as { path: string }).path };
    case "ApplyPatch":
      return {
        _tag: "ApplyPatch",
        changes: (call as Extract<RenderToolCall, { _tag: "ApplyPatch" }>).changes.map((c) => ({
          path: c.path,
          kind: c.kind
        }))
      };
    case "Search": {
      const { pattern, path } = call as { pattern: string; path?: string };
      return path === undefined ? { _tag: "Search", pattern } : { _tag: "Search", pattern, path };
    }
    case "Glob": {
      const { pattern, path } = call as { pattern: string; path?: string };
      return path === undefined ? { _tag: "Glob", pattern } : { _tag: "Glob", pattern, path };
    }
    case "WebFetch":
      return { _tag: "WebFetch", url: (call as { url: string }).url };
    case "WebSearch":
      return { _tag: "WebSearch", query: (call as { query: string }).query };
    case "Todo":
      return {
        _tag: "Todo",
        items: (call as Extract<RenderToolCall, { _tag: "Todo" }>).items.map((i) => ({
          text: i.text,
          done: i.done
        }))
      };
    case "Mcp": {
      const { server, tool } = call as { server?: string; tool: string };
      return server === undefined ? { _tag: "Mcp", tool } : { _tag: "Mcp", server, tool };
    }
    case "Unknown":
      return { _tag: "Unknown", name: (call as { name: string }).name };
    default:
      // Future tool tag this build doesn't know: keep it verbatim rather than
      // lose the call. Size discipline for new tags belongs to the harness
      // that introduces them.
      return call as RenderToolCall;
  }
};

/** Apply the render-only policy across a parts array (used on the live stream
 * path from the first token, and retroactively by the migration backfill —
 * the 7.3MB-message era shrinks to render size). */
export const toRenderParts = (
  parts: ReadonlyArray<MessagePart | SessionMessagePart>
): ReadonlyArray<SessionMessagePart> =>
  parts.map((p) =>
    p.kind === "tool"
      ? {
          kind: "tool",
          id: p.id,
          call: sanitizeToolCall(p.call),
          ...(typeof p.isError === "boolean" ? { isError: p.isError } : {})
        }
      : p
  );
