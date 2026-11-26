//! A pure model of the Markdown structure of a text: which byte ranges form
//! which element, and which byte ranges are markup that may be concealed.

pub mod blocks;
mod conceal;
mod edits;
mod index;
mod parse;
mod reveal;
mod source;
mod table;

#[cfg(test)]
mod perf_tests;
#[cfg(test)]
mod tests;

pub use blocks::{
    Block, BlockId, BlockNodeKind, BlockSelection, BlockTree, Direction, MoveEdit, NoNeighbor,
    move_block,
};
pub use conceal::{Conceal, ConcealKind, diff, plan};
pub use edits::{BlockKind, Edit, LABELS, insert_block, toggle_checkbox};
pub use index::{CodeFences, Element, ElementKind, MarkdownIndex, TableRow};
pub use parse::parse;
pub use reveal::{RevealSelection, RevealSet, revealed};
pub use source::{TextSlices, TextSource};
pub use table::{CellDirection, has_leading_pipe, row_pipes, table_cell};

/// The default of `markdown.live_view_max_file_size`: the largest buffer, in bytes, that starts in
/// the live view (design D8).
pub const LIVE_VIEW_MAX_FILE_SIZE: u64 = 5_000_000;

/// Whether a buffer of `len` bytes is over the live view size `limit` (0 means no limit), so it
/// starts in source mode.
pub fn over_live_view_limit(len: usize, limit: u64) -> bool {
    limit != 0 && len as u64 > limit
}
