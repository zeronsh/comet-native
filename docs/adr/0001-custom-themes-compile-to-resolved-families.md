# ADR 0001: Compile custom theme sources into resolved families

- Status: Accepted
- Date: 2026-08-22

## Context

Zeron supports native built-ins and needs to accept VS Code-compatible files and extension packages. Source formats contain workbench keys, TextMate scopes, semantic-token selectors, includes, and package metadata that should not leak into runtime components. Linked sources may also become temporarily missing or invalid.

## Decision

Every custom source compiles into the same source-neutral `ThemeFamily` and `ThemeVariant` model used by built-ins. Durable sources are represented as `ImportedSnapshot`, `LinkedFile`, `LinkedPackage`, or `EditableFile`. Editable files contain native resolved-family JSON produced by duplicating an installed family.

Imported snapshots persist their compiled family. Linked and editable sources persist both their location and their last successfully compiled family. A failed reload records a quiet warning and continues using the last known good family; it never replaces working runtime data with an invalid result. Library mutations reach the runtime registry only after their durable state has been written successfully.

Compilation detects package variants and light/dark appearance, maps workbench, syntax, semantic-token, terminal, and accent roles, validates the resulting Zeron palette, and emits a complete per-variant import report. Runtime UI resolves only compiled Zeron roles and never reads VS Code tokens.

Compilation also hardens shared Zeron foreground and interaction roles against
their resolved solid surfaces. Repairs prefer semantically related source
tokens before minimally adjusting a color and are recorded in the report.
Structural validation failures block installation; contrast findings are
diagnostic because runtime resolution repeats essential text safeguards and
forced frost derives safe tint density from the composited background.

Theme-default accents remain part of the compiled variant. Zeron accent presets override interaction roles only and do not alter syntax, terminal ANSI, diff, warning, error, or success roles.

## Consequences

- Built-in, imported, and linked families share one runtime registry and rendering path.
- Source-specific complexity stays in import adapters and can grow without changing components.
- Linked themes remain usable during transient source failures.
- Persistence is larger than storing links alone because it includes last-known-good compiled data and reports.
- Source changes are not reflected until an explicit or future automatic reload succeeds.
