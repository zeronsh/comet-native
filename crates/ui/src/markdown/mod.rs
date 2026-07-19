//! Our own markdown stack — pulldown-cmark parse into a [`parser::BlockTree`],
//! block-level incremental reparse for streaming, gpui rendering, and a
//! lightweight paint-only syntax highlighter. No zed GPL crates.
//!
//! Design (docs/research/mugen-pretext.md §2):
//! - the parse is block-granular and append-incremental: streaming reparses only
//!   from the last stable top-level block boundary;
//! - highlighting is **pure paint** — token colors on identical mono runs, so
//!   layout never depends on it;
//! - streaming fade-in is an opacity animation keyed by stable block identity —
//!   paint-only, never measured.

pub mod highlight;
pub mod parser;
pub mod render;

pub use parser::{Block, BlockTree, IncrementalParser, InlineRun, InlineStyle, parse_full};
