//! Pure text engine for the Emacs commands: word motion, kill and case
//! conversion over a plain text source, producing edit plans. No editor types.

mod case;
mod text;
mod words;

#[cfg(test)]
mod case_tests;
#[cfg(test)]
mod words_tests;

pub use case::{
    capitalize_previous_word, capitalize_word, downcase_previous_word, downcase_word,
    upcase_previous_word, upcase_word,
};
pub use text::{
    Caret, KilledPiece, Place, Plan, Target, TextSource, apply, build_plan, merge_targets,
};
pub use words::{
    backward_kill_word, backward_word_carets, backward_word_target, forward_word_carets,
    forward_word_target, kill_word,
};
