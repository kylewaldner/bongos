//! Edits to a process environment, applied in order to a base environment.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One change to an environment. `Set` replaces a variable, `Unset` removes it and `PrependPath`
/// puts directories before the current `PATH`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvEdit {
    Set { name: String, value: String },
    Unset { name: String },
    PrependPath { dirs: Vec<PathBuf> },
}
