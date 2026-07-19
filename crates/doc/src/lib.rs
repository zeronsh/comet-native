//! comet-doc — session & workspace Loro doc schemas and the typed mirror layer.
//!
//! Port of comet's `packages/session-doc`. The schema SHAPE (container names, part maps with
//! LoroText bodies, command entries) is kept identical to the TS implementation so the edge's
//! tail materializer and any TS peer remain compatible.
//!
//! Load-bearing invariant (measured in comet, `oplog-shape.test.ts`): message parts are a
//! LoroList of part maps whose text bodies live in **LoroText** — streaming appends RLE-merge at
//! ~1.03x oplog overhead, whereas rewriting whole part values costs ~125x.

pub mod commands;
pub mod schema;
pub mod constants;
pub mod parts;

pub use commands::*;
pub use schema::*;
pub use constants::*;
pub use parts::*;
