pub mod pattern;
pub mod spec;
pub mod state;
pub mod text;
pub mod window;

#[cfg(test)]
mod pattern_tests;
#[cfg(test)]
mod spec_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod text_tests;
#[cfg(test)]
mod window_tests;

pub use pattern::{Compiled, compile, escape};
pub use spec::{CaseMode, Direction, SearchSpec};
pub use state::{AbortOutcome, Isearch, Request, Step};
pub use text::{COUNT_CAP, counter, from_input, prompt, to_input};
pub use window::{overlapped, plan};
