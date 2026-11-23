//! A problem found while checking an environment or a definition, with a way to fix it.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// The requirement that is not met, in words.
    pub requirement: String,
    pub message: String,
    pub hint: Option<String>,
}
