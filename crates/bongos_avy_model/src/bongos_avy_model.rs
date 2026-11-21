pub mod labels;
pub mod matching;
pub mod rank;

#[cfg(test)]
mod labels_tests;
#[cfg(test)]
mod matching_tests;
#[cfg(test)]
mod rank_tests;

pub use labels::{HOME_ROW, Resolved, labels, resolve};
pub use matching::{char_matches, word_starts};
pub use rank::{ScreenPoint, rank};
