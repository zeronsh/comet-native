# Syntax highlighting

Desktop syntax highlighting lives in the pure `zeron-syntax` crate. It detects languages, runs pinned Tree-sitter grammars and queries, and returns sorted, non-overlapping UTF-8 byte spans relative to each source line. The UI resolves those neutral `HighlightKind` values through `Theme::syntax`; parser code never depends on GPUI or colors.

Markdown fences and tool diffs parse complete documents on GPUI's background executor. Changes first parses separate old/new hunk excerpts, then lazily asks the checkout host for checksum-bound complete sources. Deleted lines use the old document; added and context lines prefer the new document. A stale checksum or any visible-line mismatch discards the full result atomically.

## Query composition and ownership

Derived grammars must explicitly compose every compatible base and extension query in base-to-extension order. JavaScript uses the JavaScript query, JSX adds the JSX extension, TypeScript adds the TypeScript extension to JavaScript, and TSX combines all three. Query extensions must not be treated as standalone highlight definitions.

Kotlin uses `crates/syntax/queries/kotlin/highlights.scm`, a project-owned query written for the AST of the pinned `tree-sitter-kotlin-ng` version. Changes to that grammar require compiling the query and rerunning the token-to-`HighlightKind` quality fixtures before updating the pin.

Injection registries are closed per parent language. Containerfile/Dockerfile accepts only the bundled Bash, JSON, YAML, and TOML children advertised by its upstream injection query; unsupported names such as XML and `comment` remain parent-highlighted or plain. Markdown and HTML keep their own independent child registries.

## Adding a grammar

1. Review the parser, generated sources, and queries' licenses. Pin an exact crate version in `crates/syntax/Cargo.toml` and add it to `THIRD_PARTY_NOTICES.md`.
2. Add aliases, extensions, exact filenames, and any unambiguous shebang to the central registry in `crates/syntax/src/lib.rs`.
3. Add its `HighlightConfiguration` using official compatible queries or a clearly documented project-owned query. Compose inherited queries explicitly, map new capture vocabulary to an existing `HighlightKind`, and never expose capture names to the theme.
4. Add a minimal distinctive fixture to the ABI/query-load table and token-to-role fixtures for visual quality. If the language supports injections, register only known child parsers and keep unknown injected languages plain.
5. Run `cargo test -p zeron-syntax`, UI Markdown/Changes tests, the ignored diagnostic benchmark when parser cost changes, and the workspace checks.

Do not add language-specific parsing to a renderer. Unknown languages, binaries, oversized sources, incompatible queries, and parse failures must remain plain. Highlighting changes foreground color only—never font, weight, style, wrapping, height, or scroll geometry.
