# Markdown file preview

Files offers a native preview for `.md` and `.markdown` documents. It renders the current buffer, including unsaved edits, using Zeron's Markdown typography and existing toolbar controls. The preview does not change autosave settings.

The document is presented in a centered responsive column capped at 900px for readable line lengths. Code, tables and media share that column; images and Mermaid diagrams are centered at their natural size when narrower. On small panels the column fills the available width with Zeron's standard gutters.

Task lists use the existing GPUI base checkbox with Zeron theme colors and icons. Toggling a task changes only its marker in the current editor buffer, preserving formatting and the normal change event, autosave and editor undo/redo history. Source byte ranges distinguish duplicate and nested tasks. An outdated preview cannot edit a newer buffer, and checkboxes are disabled without an editable document. Chat retains its existing task rendering.

Workspace-relative images are loaded from the device owning the checkout. HTTP(S) images remain links. HTML and MDX are not executed. Images and diagrams open in the existing centered lightbox; wheel zoom and pan are not included. Mermaid in chat is not enabled by this change.

Mermaid diagrams use the same fence frame, header metrics, border, background and code actions as ordinary fenced code blocks. Switching between diagram and source replaces the body inside that single frame.

## Mermaid engine decision

The embedded engine is `mermaid-rs-renderer` **0.3.1**, MIT licensed, with default CLI/PNG features disabled. Its interface is isolated in `markdown/mermaid.rs`. Generated SVG is consumed by GPUI's existing SVG renderer.

The six fixtures under `scripts/fixtures/markdown-preview/` were rendered with this version and compared visually against official Mermaid **11.12.0**, with `securityLevel: strict` and `htmlLabels: false`, in headless Chrome. Flowcharts with subgraphs, sequence alternatives, classes and cardinalities, state transitions, ER attributes and Gantt dates retained their content. The corpus includes Spanish labels, long labels and explicit colors. Tests also cover `<br/>` labels and malformed source.

The native layout is not pixel-identical to Mermaid.js. Sequence actors use rectangular boxes in the evaluated output; edge routing, spacing, state-loop labels and default styling differ. This corpus establishes support for these examples, not full Mermaid syntax parity. Unsupported or invalid input remains accessible as source with a diagnostic. Browser rendering was used only for development comparison and is not a product dependency.

The adapter uses Zeron's resolved theme/font. SVG preparation loads the same bundled Geist and Geist Mono faces as the interface and uses bundled Geist as a fallback for unavailable families, including virtual system font names. This prevents unresolved fonts from silently removing labels during text-to-path conversion. Unit tests rasterize the corpus through `gpui::SvgRenderer` in light and dark themes with both bundled families, including the prepared image path used by Files, and check deterministic output. Separate regression tests verify that text produces visible pixels with Geist Mono and unavailable font names. To retain SVG and prepared PNG artifacts while running that test, set `ZERON_MERMAID_ARTIFACTS` to a local output directory.

## Limits and lifecycle

Preview parsing is debounced by 120ms and limited to the first 2 MiB of Markdown, with a visible truncation notice. Text rows are virtualized and their derived render caches retain only the previous viewport. A document admits at most 32 distinct images and 32 distinct Mermaid sources, with a combined 64 MiB media retention budget including estimated decoded texture memory. Excess media displays a limit message; its Markdown/source remains available. Image reads have a 30-second deadline and at most three concurrent jobs. Mermaid layouts are serialized.

Source revisions reject obsolete async results. Image watcher events invalidate both completed and pending loads; changing theme regenerates diagrams. Switching back to Code releases the preview's derived state while preserving the editor and preview scroll position. Closing a preview schedules image asset and atlas eviction. Image/diagram completion remeasures rows with an absolute scroll anchor.

Image reads are limited to 8 MiB, with 384 KiB binary chunks and content-hash validation between chunks. Raster images are decoded with 4096px dimension and 64 MiB allocation limits, then flattened to a static PNG, including the first frame of animations. Workspace SVG previews are limited to 2048px per side; they are parsed and reserialized with embedded/external image resolution disabled. Unsupported SVG content may therefore be omitted.

Mermaid source is limited to 16 KiB, 256 lines and 2048 lexical segments; generated SVG is limited to 2 MiB. The native engine has no cooperative cancellation or hard execution deadline. Its CPU work is serialized across previews and runs off the UI thread; obsolete results must be rejected by their owning view. These limits bound admitted work but do not constitute a strict wall-clock guarantee.

Manual validation on macOS and a second physical remote device must be recorded separately from headless Linux tests. Automated tests cannot establish platform-specific focus, GPU rendering or real network behavior by themselves.

## Implementation validation

The implementation was checked on Linux with the following commands:

| Command | Result |
| --- | --- |
| `cargo test --release --locked -p zeron-ui --lib -- --test-threads=1` | 757 passed |
| `cargo test --release --locked -p zeron-engine --lib` | 161 passed |
| `cargo test --release --locked -p zeron-proto -p zeron-rpc` | 47 passed, 1 previously ignored |
| `cargo test --release --locked -p zeron-engine --test workspace_files` | 3 passed |
| `cargo test --release --locked -p zeron-engine --test device_routing workspace_file_surface_proxies_over_the_relay` | 1 passed |
| `cargo check --locked -p zeron` | Passed |

The UI tests cover unsaved content without extra saves, independent selection surfaces, pointer opening and Escape dismissal of the lightbox, obsolete work, media limits and image invalidation. The relay test runs two engines through a test relay and checks remote image reads and checkout identity. The Mermaid corpus produces twenty-four SVG and PNG pairs across light/dark themes and Geist/Geist Mono. Prepared ER (light) and sequence (light/dark) PNGs with Geist Mono were inspected visually after fixing font resolution. Regression tests first reproduced zero visible text pixels with Geist Mono and an unavailable family, then passed after the fix.

Formatting checks pass for all changed Rust files, and `git diff --check` passes. Repository-wide `cargo fmt --all -- --check` reports existing differences in unrelated files; those files were left unchanged. Full native application review on Linux/macOS, HiDPI interaction and a physical remote connection remain manual acceptance checks.
