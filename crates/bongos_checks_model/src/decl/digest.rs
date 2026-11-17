//! The digest of a check: SHA-256 over the canonical JSON of the resolved declaration and the
//! bytes of every definition file it names.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::CheckDecl;

/// The hex SHA-256 of a check and the files it names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Digest(String);

impl Digest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    #[error("check {check}: {path} is not among the given files")]
    MissingFile { check: String, path: String },
    #[error("check {check}: the declaration cannot be written as JSON: {reason}")]
    Unserializable { check: String, reason: String },
}

/// The digest of `decl` over `files`, the definition files (path relative to `definitions/`, as
/// written in the declaration, and their bytes), read from the pin. Changing the declaration
/// (a default written out counts as unchanged) or the bytes of `run`, `scope.script`,
/// `parts.plan` or any `pinned` file changes it. A `command` binary is not hashed.
pub fn digest(decl: &CheckDecl, files: &BTreeMap<String, Vec<u8>>) -> Result<Digest, DigestError> {
    let value =
        serde_json::to_value(decl.resolved()).map_err(|error| DigestError::Unserializable {
            check: decl.name.clone(),
            reason: error.to_string(),
        })?;
    let json = canonical(value).to_string();
    let mut hasher = Sha256::new();
    hasher.update(b"bongos-check-digest-1\n");
    hasher.update((json.len() as u64).to_le_bytes());
    hasher.update(json.as_bytes());
    let paths: BTreeSet<&str> = decl.named_files().into_iter().collect();
    for path in paths {
        let bytes = files.get(path).ok_or_else(|| DigestError::MissingFile {
            check: decl.name.clone(),
            path: path.to_string(),
        })?;
        hasher.update((path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    Ok(Digest(format!("{:x}", hasher.finalize())))
}

/// The same JSON with every object's keys sorted, at any depth (`preserve_order` keeps insertion
/// order, which would make the digest depend on how a table was written).
fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}
