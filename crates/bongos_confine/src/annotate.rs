//! Naming the rule after a refused write in a job's output.
//!
//! Agent steps and task checks run programs bongos cannot see into. A write outside the
//! confinement fails with `EROFS` on Linux and `EPERM` on macOS, and most programs print the
//! standard message for it with the path. The runner passes each output line here and writes the
//! returned annotation after the line. This is best effort: a program that prints nothing leaves
//! no annotation, though its write is still refused. A refusal under a protected location names
//! that location's rule instead.

use std::path::{Component, Path, PathBuf};

use crate::{
    RULE,
    protected::{self, ProtectedKind, ProtectedLocation},
};

/// The messages the OS gives for a write the sandbox refused.
const REFUSAL_MESSAGES: [&str; 2] = ["Read-only file system", "Operation not permitted"];

/// The messages for a refused write under a protected location.
const PROTECTED_MESSAGES: [&str; 3] = [
    "Read-only file system",
    "Operation not permitted",
    "Permission denied",
];

/// The messages for a refused connection to the engine host: a hidden hosts directory looks
/// missing, so a missing path or a refused connection counts too.
const HOST_SOCKET_MESSAGES: [&str; 5] = [
    "Read-only file system",
    "Operation not permitted",
    "Permission denied",
    "No such file or directory",
    "Connection refused",
];

/// The annotation `bongos: <RULE>: <path>` for an output line that reports a refused write to a
/// path outside `roots`, or `None` when the line reports no refusal, names no absolute path or
/// names a path inside a writable root. A refusal under one of this process's protected
/// locations ([`protected::current`]) is annotated as in [`refusal_with_protected`].
pub fn refusal(line: &str, roots: &[PathBuf]) -> Option<String> {
    refusal_with_protected(line, roots, &protected::current())
}

/// [`refusal`] against the given protected locations: a refused write under one of them is
/// annotated `bongos: <the kind's rule>: <path>`.
pub fn refusal_with_protected(
    line: &str,
    roots: &[PathBuf],
    protected: &[ProtectedLocation],
) -> Option<String> {
    protected_refusal(line, protected).or_else(|| outside_roots(line, roots))
}

fn protected_refusal(line: &str, protected: &[ProtectedLocation]) -> Option<String> {
    if protected.is_empty() {
        return None;
    }
    let message_at = HOST_SOCKET_MESSAGES
        .iter()
        .filter_map(|message| line.find(message))
        .min()?;
    let path = quoted_path(line).or_else(|| bare_path(&line[..message_at]))?;
    let path = normalize(Path::new(path));
    let location = protected.iter().find(|location| {
        protected::forms(&location.path)
            .iter()
            .any(|form| path.starts_with(form))
    })?;
    let messages: &[&str] = match location.kind {
        ProtectedKind::HostSocket => &HOST_SOCKET_MESSAGES,
        _ => &PROTECTED_MESSAGES,
    };
    if !messages.iter().any(|message| line.contains(message)) {
        return None;
    }
    Some(format!("bongos: {}: {}", location.kind.rule(), path.display()))
}

fn outside_roots(line: &str, roots: &[PathBuf]) -> Option<String> {
    let message_at = REFUSAL_MESSAGES
        .iter()
        .filter_map(|message| line.find(message))
        .min()?;
    let path = quoted_path(line).or_else(|| bare_path(&line[..message_at]))?;
    let path = normalize(Path::new(path));
    if roots.iter().any(|root| path.starts_with(root)) {
        return None;
    }
    Some(format!("bongos: {RULE}: {}", path.display()))
}

/// The first quoted absolute path in the line (`'/a'`, `"/a"`, `‘/a’` or `` `/a' ``).
fn quoted_path(line: &str) -> Option<&str> {
    let mut rest = line;
    while let Some((open_at, open)) = rest
        .char_indices()
        .find(|(_, c)| matches!(c, '\'' | '"' | '‘' | '`'))
    {
        let after = &rest[open_at + open.len_utf8()..];
        let Some(close) = after.find(|c| match open {
            '‘' => c == '’',
            '`' => c == '\'' || c == '`',
            _ => c == open,
        }) else {
            break;
        };
        let quoted = &after[..close];
        if quoted.starts_with('/') {
            return Some(quoted);
        }
        rest = &after[close..];
        rest = &rest[rest.chars().next().map_or(0, char::len_utf8)..];
    }
    None
}

/// The last absolute path before the message, as in `sh: /a/b: Read-only file system`.
fn bare_path(prefix: &str) -> Option<&str> {
    prefix
        .split_whitespace()
        .map(|word| {
            word.trim_end_matches(':')
                .trim_matches(|c| matches!(c, '\'' | '"' | '‘' | '’' | '`'))
        })
        .rfind(|word| word.starts_with('/'))
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}
