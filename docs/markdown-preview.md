# Markdown file preview

Files offers a native preview for `.md` and `.markdown` documents. It renders the current buffer, including unsaved edits, using Zeron's Markdown typography and existing toolbar controls. The preview does not change autosave settings.

Workspace-relative images are loaded from the device owning the checkout. HTTP(S) images remain links. HTML and MDX are not executed. Images and diagrams open in the existing centered lightbox; wheel zoom and pan are not included. Mermaid in chat is not enabled by this change.

## Mermaid engine decision

The embedded engine is `mermaid-rs-renderer` **0.3.1**, MIT licensed, with default CLI/PNG features disabled. Its interface is isolated in `markdown/mermaid.rs`. Generated SVG is consumed by GPUI's existing SVG renderer.

The six fixtures under `scripts/fixtures/markdown-preview/` were rendered with this version and compared visually against official Mermaid **11.12.0**, with `securityLevel: strict` and `htmlLabels: false`, in headless Chrome. Flowcharts with subgraphs, sequence alternatives, classes and cardinalities, state transitions, ER attributes and Gantt dates retained their content. The corpus includes Spanish labels, long labels and explicit colors. Tests also cover `<br/>` labels and malformed source.

The native layout is not pixel-identical to Mermaid.js. Sequence actors use rectangular boxes in the evaluated output; edge routing, spacing, state-loop labels and default styling differ. This corpus establishes support for these examples, not full Mermaid syntax parity. Unsupported or invalid input remains accessible as source with a diagnostic. Browser rendering was used only for development comparison and is not a product dependency.

The adapter uses Zeron's resolved theme/font. Unit tests rasterize the corpus through `gpui::SvgRenderer` in light and dark themes and check deterministic output. To retain SVG artifacts while running that test, set `ZERON_MERMAID_ARTIFACTS` to a local output directory.

## Limits and lifecycle

Image reads are limited to 8 MiB, with 384 KiB binary chunks and content-hash validation between chunks. Raster images are decoded with 4096px dimension and 64 MiB allocation limits, then flattened to a static PNG, including the first frame of animations. Workspace SVG previews are limited to 2048px per side; they are parsed and reserialized with embedded/external image resolution disabled. Unsupported SVG content may therefore be omitted.

Mermaid source is limited to 16 KiB, 256 lines and 2048 lexical segments; generated SVG is limited to 2 MiB. The native engine has no cooperative cancellation or hard execution deadline. Its CPU work is serialized across previews and runs off the UI thread; obsolete results must be rejected by their owning view. These limits bound admitted work but do not constitute a strict wall-clock guarantee.

Manual validation on macOS and a second physical remote device must be recorded separately from headless Linux tests. Automated tests cannot establish platform-specific focus, GPU rendering or real network behavior by themselves.
