//! Pure model for query replace: parsing and expanding replacement
//! templates, and carrying the case of a match over to its replacement.

pub mod case;
pub mod template;

#[cfg(test)]
mod case_tests;
#[cfg(test)]
mod template_tests;
