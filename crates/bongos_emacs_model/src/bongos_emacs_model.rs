//! Pure text engine for the Emacs commands: word motion, kill and case
//! conversion over a plain text source, producing edit plans. No editor types.

mod case;
pub mod comment;
pub mod goto;
mod kill;
mod lines;
pub mod recenter;
mod space;
mod text;
mod transpose;
mod words;
mod zap;

#[cfg(test)]
mod case_tests;
#[cfg(test)]
mod comment_tests;
#[cfg(test)]
mod goto_tests;
#[cfg(test)]
mod kill_tests;
#[cfg(test)]
mod lines_tests;
#[cfg(test)]
mod recenter_tests;
#[cfg(test)]
mod space_tests;
#[cfg(test)]
mod transpose_tests;
#[cfg(test)]
mod words_tests;
#[cfg(test)]
mod zap_tests;

pub use case::{
    capitalize_previous_word, capitalize_word, downcase_previous_word, downcase_word,
    upcase_previous_word, upcase_word,
};
pub use kill::{copy_region_or_line, kill_line, kill_region_or_line};
pub use lines::{copy_from_above, duplicate_line, whole_line_span};
pub use space::{delete_blank_lines, delete_horizontal_space, just_one_space};
pub use text::{
    Caret, KilledPiece, Place, Plan, Target, TextSource, apply, build_plan, merge_targets,
};
pub use transpose::{transpose_chars, transpose_lines, transpose_words, transpose_words_reverse};
pub use words::{
    backward_kill_word, backward_word_carets, backward_word_target, forward_word_carets,
    forward_word_target, kill_word,
};
pub use zap::{zap_back_to_char, zap_back_up_to_char, zap_to_char, zap_up_to_char};
