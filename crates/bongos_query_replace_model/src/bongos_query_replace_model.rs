//! Pure model for query replace: replacement templates, case transfer, the
//! session state machine, its texts, and a simulator that applies a whole
//! session to a string.

pub mod case;
pub mod session;
pub mod simulate;
pub mod template;
pub mod text;

#[cfg(test)]
mod case_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod simulate_tests;
#[cfg(test)]
mod template_tests;
#[cfg(test)]
mod text_tests;
