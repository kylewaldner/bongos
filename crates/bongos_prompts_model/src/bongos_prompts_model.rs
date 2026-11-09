//! The file model of the bongos prompt editor: the marker line that separates prompts in a
//! `prompts.txt` file (parse and render, both the default and a configured syntax), the blocks a
//! file splits into and the text a block sends, an incremental index of the marker rows with the
//! queries navigation needs, and the edit plans that stamp a block as sent or add a block. Plans
//! only ever touch marker rows, so body text is never rewritten. It has no UI dependency.

mod index;
mod marker;
mod parse;
mod plan;

pub use index::{
    BlockInfo, IndexChange, IndexStats, IndexedMarker, PromptIndex, RowEdit, merge_row_edits,
};
pub use marker::{DEFAULT_CLOSE, DEFAULT_OPEN, MarkerLine, MarkerSyntax, format_stamp};
pub use parse::{Block, parse, send_text, split_rows};
pub use plan::{PlanEdit, insert_marker_above, mark_sent, new_block_after};

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod marker_tests;
#[cfg(test)]
mod parse_tests;
#[cfg(test)]
mod plan_tests;
