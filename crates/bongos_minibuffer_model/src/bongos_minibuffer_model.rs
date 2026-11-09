//! The model of the bongos minibuffer: the pieces of completing read that need no UI. It holds the
//! candidate texts and the narrowing engine (space-separated fuzzy terms, all required, in any
//! order, with a stack of refinements and a cancel flag), the common prefix and match count text
//! of the input row, the per-prompt history ring, the yes/no answer parsers and the Emacs notation
//! of key sequences. It has no UI dependency.

mod answer;
mod candidate;
mod engine;
mod history;
mod prefix;

pub use answer::{parse_strict, parse_yes_no};
pub use candidate::{Candidate, Texts, common_prefix, count_text, texts};
pub use engine::{Cancel, Narrower};
pub use history::{HISTORY_CAPACITY, HistoryRing};
pub use prefix::{Keystroke, emacs_notation};

#[cfg(test)]
mod answer_tests;
#[cfg(test)]
mod candidate_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod history_tests;
#[cfg(test)]
mod perf_tests;
#[cfg(test)]
mod prefix_tests;
