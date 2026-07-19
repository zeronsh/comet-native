/**
 * Vendored from comet's `@comet/control` (packages/control/src/wire.ts +
 * parts.ts): the plain-type equivalents of the effect/Schema wire types that
 * the session-doc modules reference. Type-only — no runtime behavior — so the
 * edge package stays dependency-light (no effect).
 */

export type ToolCall =
  | { readonly _tag: "Exec"; readonly command: string; readonly background?: boolean }
  | { readonly _tag: "ReadFile"; readonly path: string }
  | { readonly _tag: "WriteFile"; readonly path: string; readonly content: string }
  | {
      readonly _tag: "EditFile";
      readonly path: string;
      readonly oldString: string;
      readonly newString: string;
      readonly replaceAll: boolean;
    }
  | {
      readonly _tag: "ApplyPatch";
      readonly changes: ReadonlyArray<{
        readonly path: string;
        readonly kind: "add" | "delete" | "update";
      }>;
    }
  | { readonly _tag: "Search"; readonly pattern: string; readonly path?: string }
  | { readonly _tag: "Glob"; readonly pattern: string; readonly path?: string }
  | { readonly _tag: "WebFetch"; readonly url: string; readonly prompt: string }
  | { readonly _tag: "WebSearch"; readonly query: string }
  | {
      readonly _tag: "Todo";
      readonly items: ReadonlyArray<{ readonly text: string; readonly done: boolean }>;
    }
  | { readonly _tag: "Mcp"; readonly server?: string; readonly tool: string; readonly input: unknown }
  | { readonly _tag: "Unknown"; readonly name: string; readonly input: unknown };

/** A question the agent poses to the user mid-run (mirrors the harness type). */
export interface UserInputQuestion {
  readonly id: string;
  readonly header: string;
  readonly question: string;
  readonly options: ReadonlyArray<{ readonly label: string; readonly description?: string }>;
  readonly multiSelect?: boolean;
}

/** Message parts — the structured view of an assistant turn (parts.ts). */
export type MessagePart =
  | {
      readonly kind: "text";
      /** Stable identity for rendering (text blocks have no natural id). */
      readonly id: string;
      readonly text: string;
    }
  | {
      readonly kind: "tool";
      readonly id: string;
      readonly call: ToolCall;
      /** Undefined while running; false on success; true on failure. */
      readonly isError?: boolean;
    }
  | {
      /** An interactive question the agent asked. Unresolved ⇒ awaiting the user. */
      readonly kind: "input";
      readonly requestId: string;
      readonly questions: ReadonlyArray<UserInputQuestion>;
      readonly resolved?: boolean;
    }
  | {
      /** The run ended abnormally (harness failure, stall, interrupt). */
      readonly kind: "error";
      readonly id: string;
      readonly message: string;
    };
