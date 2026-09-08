//! Our own markdown stack — pulldown-cmark parse into a [`parser::BlockTree`],
//! block-level incremental reparse for streaming, gpui rendering, and a
//! Tree-sitter paint-only syntax highlighting. No zed GPL crates.
//!
//! Design (docs/research/mugen-pretext.md §2):
//! - the parse is block-granular and append-incremental: streaming reparses only
//!   from the last stable top-level block boundary;
//! - highlighting is **pure paint** — token colors on identical mono runs, so
//!   layout never depends on it;
//! - streaming fade-in is a per-appended-chunk opacity veil over the text runs
//!   ([`veil`]) — paint-only, never measured, layout commits instantly;
//! - hanging inline markers (`**bold`, `[link](url…`) are auto-closed in the
//!   streaming *display* parse only ([`mend`]), so closing markers never
//!   reflow already-painted text; the canonical parse settles honestly on
//!   completion.

pub mod mend;
pub mod parser;
pub mod render;
pub mod selection;
pub mod veil;

pub use parser::{Block, BlockTree, IncrementalParser, InlineRun, InlineStyle, parse_full};

pub mod mermaid;
