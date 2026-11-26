//! A pure model of the Markdown structure of a text: which byte ranges form
//! which element, and which byte ranges are markup that may be concealed.

mod index;
mod parse;

#[cfg(test)]
mod tests;

pub use index::{CodeFences, Element, ElementKind, MarkdownIndex, TableRow};
pub use parse::parse;
