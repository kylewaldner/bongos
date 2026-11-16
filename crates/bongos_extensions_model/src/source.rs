//! Repository URLs, relative subdirectories and revisions for extension sources.

use std::fmt;
use std::path::{Component, Path};

/// Why a repository URL, subdirectory or revision was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceUrlError {
    #[error("the repository URL is empty")]
    Empty,
    #[error("`http://` is not allowed; use `https://`")]
    InsecureScheme,
    #[error("unsupported URL scheme in `{0}`; use https, ssh, git@host:path or file")]
    UnsupportedScheme(String),
    #[error("the repository URL `{0}` has no host")]
    EmptyHost(String),
    #[error("the repository URL `{0}` has no repository path")]
    EmptyPath(String),
    #[error("the repository URL must not contain credentials")]
    EmbeddedCredentials,
    #[error("the repository URL `{0}` is not valid")]
    Invalid(String),
    #[error("`{0}` is not a relative path without `..` components")]
    InvalidSubdirectory(String),
    #[error("`{0}` looks like a short commit hash; give the full commit (40 or 64 hex digits)")]
    ShortCommit(String),
}

/// A validated, normalized repository URL. Two URLs that differ only in host case, a trailing
/// `.git` or a trailing `/` compare equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SourceUrl {
    normalized: String,
}

impl SourceUrl {
    /// Accepts `https://host/path`, `ssh://...`, scp-like `user@host:path` and `file://` URLs.
    pub fn parse(input: &str) -> Result<SourceUrl, SourceUrlError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(SourceUrlError::Empty);
        }
        if input.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(SourceUrlError::Invalid(input.to_string()));
        }
        let lower_prefix = |prefix: &str| {
            input
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        };
        if lower_prefix("http://") {
            return Err(SourceUrlError::InsecureScheme);
        }
        if lower_prefix("https://") {
            return Self::parse_network(input, "https", &input["https://".len()..]);
        }
        if lower_prefix("ssh://") {
            return Self::parse_network(input, "ssh", &input["ssh://".len()..]);
        }
        if lower_prefix("file://") {
            let path = &input["file://".len()..];
            let path = strip_repository_suffixes(path);
            if !path.starts_with('/') || path == "/" {
                return Err(SourceUrlError::EmptyPath(input.to_string()));
            }
            return Ok(SourceUrl {
                normalized: format!("file://{path}"),
            });
        }
        if input.contains("://") {
            return Err(SourceUrlError::UnsupportedScheme(input.to_string()));
        }
        Self::parse_scp(input)
    }

    fn parse_network(input: &str, scheme: &str, rest: &str) -> Result<SourceUrl, SourceUrlError> {
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, ""),
        };
        let (user, host_port) = match authority.rsplit_once('@') {
            Some((user, host_port)) => (Some(user), host_port),
            None => (None, authority),
        };
        if let Some(user) = user {
            // `ssh://git@host/...` is ordinary; a password or any userinfo on https is not.
            if scheme == "https" || user.contains(':') {
                return Err(SourceUrlError::EmbeddedCredentials);
            }
        }
        let (host, port) = match host_port.rsplit_once(':') {
            Some((host, port)) => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(SourceUrlError::Invalid(input.to_string()));
                }
                (host, Some(port))
            }
            None => (host_port, None),
        };
        if host.is_empty() {
            return Err(SourceUrlError::EmptyHost(input.to_string()));
        }
        if !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(SourceUrlError::Invalid(input.to_string()));
        }
        let path = strip_repository_suffixes(path);
        if path.trim_start_matches('/').is_empty() {
            return Err(SourceUrlError::EmptyPath(input.to_string()));
        }
        let mut normalized = format!("{scheme}://");
        if let Some(user) = user {
            normalized.push_str(user);
            normalized.push('@');
        }
        normalized.push_str(&host.to_ascii_lowercase());
        if let Some(port) = port {
            normalized.push(':');
            normalized.push_str(port);
        }
        normalized.push_str(path);
        Ok(SourceUrl { normalized })
    }

    fn parse_scp(input: &str) -> Result<SourceUrl, SourceUrlError> {
        let Some((user_host, path)) = input.split_once(':') else {
            return Err(SourceUrlError::UnsupportedScheme(input.to_string()));
        };
        let Some((user, host)) = user_host.split_once('@') else {
            return Err(SourceUrlError::UnsupportedScheme(input.to_string()));
        };
        if user.is_empty() || user.contains('/') {
            return Err(SourceUrlError::Invalid(input.to_string()));
        }
        if host.is_empty() {
            return Err(SourceUrlError::EmptyHost(input.to_string()));
        }
        if !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Err(SourceUrlError::Invalid(input.to_string()));
        }
        let path = strip_repository_suffixes(path);
        if path.trim_start_matches('/').is_empty() {
            return Err(SourceUrlError::EmptyPath(input.to_string()));
        }
        Ok(SourceUrl {
            normalized: format!("{user}@{}:{path}", host.to_ascii_lowercase()),
        })
    }

    /// The normalized form, used for comparison and display.
    pub fn as_str(&self) -> &str {
        &self.normalized
    }

    /// The lowercase host, or `None` for `file://` URLs.
    pub fn host(&self) -> Option<&str> {
        let rest = if let Some(rest) = self.normalized.strip_prefix("https://") {
            rest
        } else if let Some(rest) = self.normalized.strip_prefix("ssh://") {
            rest
        } else if self.normalized.starts_with("file://") {
            return None;
        } else {
            let (_, host_path) = self.normalized.split_once('@')?;
            return host_path.split(':').next();
        };
        let authority = rest.split('/').next()?;
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        Some(host_port.split(':').next().unwrap_or(host_port))
    }
}

impl fmt::Display for SourceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.normalized)
    }
}

fn strip_repository_suffixes(mut path: &str) -> &str {
    loop {
        let trimmed = path.trim_end_matches('/');
        let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
        if trimmed.len() == path.len() {
            return path;
        }
        path = trimmed;
    }
}

/// Checks that `path` is a relative subdirectory with no `..` components.
pub fn validate_relative_path(path: &str) -> Result<(), SourceUrlError> {
    let invalid = || SourceUrlError::InvalidSubdirectory(path.to_string());
    if path.is_empty() || path.contains('\\') {
        return Err(invalid());
    }
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

/// Which revision of a repository to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevSpec {
    /// A full 40- or 64-digit hex commit, lowercased.
    Commit(String),
    /// A tag or branch name.
    Name(String),
    /// None given: the repository's default branch.
    Default,
}

impl RevSpec {
    pub fn parse(input: Option<&str>) -> Result<RevSpec, SourceUrlError> {
        let input = input.map(str::trim).unwrap_or_default();
        if input.is_empty() {
            return Ok(RevSpec::Default);
        }
        if input.bytes().all(|b| b.is_ascii_hexdigit()) {
            return match input.len() {
                40 | 64 => Ok(RevSpec::Commit(input.to_ascii_lowercase())),
                4..=63 => Err(SourceUrlError::ShortCommit(input.to_string())),
                _ => Ok(RevSpec::Name(input.to_string())),
            };
        }
        Ok(RevSpec::Name(input.to_string()))
    }
}
