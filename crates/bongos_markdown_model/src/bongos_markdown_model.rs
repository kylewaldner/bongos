//! A pure model of the Markdown structure of a text: which byte ranges form
//! which element, and which byte ranges are markup that may be concealed.

pub mod blocks;
mod conceal;
mod edits;
mod index;
mod parse;
mod reveal;
mod table;

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
pub use table::{CellDirection, table_cell};
