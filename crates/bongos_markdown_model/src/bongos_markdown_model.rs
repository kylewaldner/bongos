//! A pure model of the Markdown structure of a text: which byte ranges form
//! which element, and which byte ranges are markup that may be concealed.

mod conceal;
mod index;
mod parse;
mod reveal;

#[cfg(test)]
mod tests;

pub use conceal::{Conceal, ConcealKind, diff, plan};
pub use index::{CodeFences, Element, ElementKind, MarkdownIndex, TableRow};
pub use parse::parse;
pub use reveal::{RevealSelection, RevealSet, revealed};
