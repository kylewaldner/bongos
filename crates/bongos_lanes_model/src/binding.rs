//! Expansion of the portable tokens a lane binding uses in environment values.
//!
//! Tokens: `{lane}`, `{kind}`, `{lane_index}`, `{project}`, `{repo}`, `{live_lanes}`, `{cpus}`,
//! `{dir.<name>}` and one integer division `{a/b}` of two numeric tokens (or integers), floored and
//! at least 1. Expansion is pure: the caller supplies every value.

use std::collections::BTreeMap;

use thiserror::Error;

/// The values tokens expand to for one lane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenValues {
    pub lane: String,
    pub kind: String,
    pub lane_index: u32,
    /// The project id's first 12 hex digits.
    pub project: String,
    pub repo: String,
    pub live_lanes: u32,
    pub cpus: u32,
    /// Named directory to its resolved path.
    pub dirs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TokenError {
    #[error("unknown token {{{0}}}")]
    Unknown(String),
    #[error("token {{{0}}} is not closed")]
    Unterminated(String),
    #[error("{{{token}}} divides by something that is not a number: {side}")]
    NonNumeric { token: String, side: String },
    #[error("{{{token}}} has a zero side")]
    Zero { token: String },
}

/// Replaces every `{...}` in `input` with its value.
pub fn expand_tokens(input: &str, values: &TokenValues) -> Result<String, TokenError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(TokenError::Unterminated(after.to_string()));
        };
        out.push_str(&expand_one(&after[..close], values)?);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Checks that every token in `input` exists, with `dir_names` as the named directories the
/// lane binds. Used by validation, which has no values.
pub fn check_tokens(input: &str, dir_names: &[&str]) -> Result<(), TokenError> {
    let values = TokenValues {
        lane: "lane".into(),
        kind: "kind".into(),
        lane_index: 1,
        project: "project".into(),
        repo: "repo".into(),
        live_lanes: 1,
        cpus: 1,
        dirs: dir_names
            .iter()
            .map(|name| (name.to_string(), String::new()))
            .collect(),
    };
    expand_tokens(input, &values).map(drop)
}

fn expand_one(token: &str, values: &TokenValues) -> Result<String, TokenError> {
    if let Some((left, right)) = token.split_once('/') {
        let left_value = number(token, left, values)?;
        let right_value = number(token, right, values)?;
        if left_value == 0 || right_value == 0 {
            return Err(TokenError::Zero {
                token: token.to_string(),
            });
        }
        return Ok((left_value / right_value).max(1).to_string());
    }
    if let Some(name) = token.strip_prefix("dir.") {
        return values
            .dirs
            .get(name)
            .cloned()
            .ok_or_else(|| TokenError::Unknown(token.to_string()));
    }
    match token {
        "lane" => Ok(values.lane.clone()),
        "kind" => Ok(values.kind.clone()),
        "lane_index" => Ok(values.lane_index.to_string()),
        "project" => Ok(values.project.clone()),
        "repo" => Ok(values.repo.clone()),
        "live_lanes" => Ok(values.live_lanes.to_string()),
        "cpus" => Ok(values.cpus.to_string()),
        _ => Err(TokenError::Unknown(token.to_string())),
    }
}

/// One side of a division: an integer, or a numeric token.
fn number(token: &str, side: &str, values: &TokenValues) -> Result<u64, TokenError> {
    let side = side.trim();
    if !side.is_empty() && side.bytes().all(|byte| byte.is_ascii_digit()) {
        return side.parse().map_err(|_| TokenError::NonNumeric {
            token: token.to_string(),
            side: side.to_string(),
        });
    }
    match side {
        "lane_index" => Ok(values.lane_index.into()),
        "live_lanes" => Ok(values.live_lanes.into()),
        "cpus" => Ok(values.cpus.into()),
        "lane" | "kind" | "project" | "repo" => Err(TokenError::NonNumeric {
            token: token.to_string(),
            side: side.to_string(),
        }),
        _ if side.starts_with("dir.") && expand_one(side, values).is_ok() => {
            Err(TokenError::NonNumeric {
                token: token.to_string(),
                side: side.to_string(),
            })
        }
        _ => Err(TokenError::Unknown(side.to_string())),
    }
}
