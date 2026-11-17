//! Verdicts: what a check run means. [`judge`] is the only producer of [`Verdict::Pass`], and
//! it fails closed: anything it cannot prove is a pass or a fail is a no verdict with its
//! reason. Also the reading of the files a script answers with: the result file, the scope
//! answer and the part plan, and [`combine`] for a check run in parts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::decl::CheckDecl;

/// How a process ended, as the engine saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exit {
    Code(i32),
    /// Ended by a signal.
    Signal(i32),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Pass,
    Fail,
    NoVerdict(NoVerdictReason),
}

impl Verdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoVerdictReason {
    /// An exit code the check lists under `environment`.
    Environment {
        code: i32,
    },
    /// An exit code in none of `pass`, `fail` and `environment` (or in `pass` and `fail` both).
    UndeclaredExit {
        code: i32,
    },
    Killed {
        signal: i32,
    },
    MissingSummary,
    InvalidResult(String),
    ToolNotFound(String),
    StartFailed(String),
    TimeBudgetReached,
    PoolWaitLimit {
        pool: String,
    },
    Cancelled,
    Interrupted,
    InvalidScope(String),
    InvalidPlan(String),
    TreeUnavailable(String),
}

impl NoVerdictReason {
    /// The reason in words, as the Checks section shows it after "no verdict: ".
    pub fn text(&self) -> String {
        match self {
            Self::Environment { code } => format!("environment (exit {code})"),
            Self::UndeclaredExit { code } => format!("undeclared exit code {code}"),
            Self::Killed { signal } => format!("killed by signal {signal}"),
            Self::MissingSummary => "no summary line in the output".to_string(),
            Self::InvalidResult(why) => format!("invalid result file: {why}"),
            Self::ToolNotFound(tool) => format!("tool not found: {tool}"),
            Self::StartFailed(why) => format!("could not start: {why}"),
            Self::TimeBudgetReached => "time budget reached".to_string(),
            Self::PoolWaitLimit { pool } => {
                format!("resource never became available (pool {pool})")
            }
            Self::Cancelled => "cancelled".to_string(),
            Self::Interrupted => "interrupted".to_string(),
            Self::InvalidScope(why) => format!("invalid scope answer: {why}"),
            Self::InvalidPlan(why) => format!("invalid part plan: {why}"),
            Self::TreeUnavailable(why) => format!("no tree to check: {why}"),
        }
    }
}

/// The result file a check writes (`BONGOS_CHECK_RESULT`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultFile {
    /// One line.
    pub summary: String,
    #[serde(default)]
    pub failing: Vec<String>,
    #[serde(default)]
    pub counts: BTreeMap<String, i64>,
}

/// Reads a result file. `limit_bytes` is `task_checks.result_file_limit_kb` in bytes. The error
/// is the reason, for [`NoVerdictReason::InvalidResult`]. Unknown keys, a missing or empty or
/// multi-line `summary`, and anything that is not the documented shape are refused.
pub fn parse_result_file(text: &[u8], limit_bytes: usize) -> Result<ResultFile, String> {
    if text.len() > limit_bytes {
        return Err(format!("larger than {limit_bytes} bytes"));
    }
    let text = std::str::from_utf8(text).map_err(|_| "not UTF-8".to_string())?;
    let file: ResultFile = serde_json::from_str(text).map_err(|error| error.to_string())?;
    if file.summary.trim().is_empty() {
        return Err("summary is empty".to_string());
    }
    if file.summary.contains(['\n', '\r']) {
        return Err("summary is more than one line".to_string());
    }
    Ok(file)
}

/// The judgment of one process run (one part). `summary_seen`: a line of the output matched the
/// declared `summary` regex (ignored when none is declared). `result`: what reading the result
/// file gave, `None` when there was no file; consulted only when the check declares one.
///
/// `Pass` needs a pass code, the summary line when declared and a valid result when declared.
/// `Fail` needs a fail code and, when a result file is declared, a valid one: a fail with an
/// invalid result is a no verdict, since the failing items cannot be trusted.
pub fn judge(
    exit: Exit,
    decl: &CheckDecl,
    summary_seen: bool,
    result: Option<Result<ResultFile, String>>,
) -> Verdict {
    let code = match exit {
        Exit::Signal(signal) => return Verdict::NoVerdict(NoVerdictReason::Killed { signal }),
        Exit::Code(code) => code,
    };
    if decl.environment_codes().contains(&code) {
        return Verdict::NoVerdict(NoVerdictReason::Environment { code });
    }
    let passes = decl.pass_codes().contains(&code);
    let fails = decl.fail_codes().contains(&code);
    if passes == fails {
        // In neither list, or in both: not a verdict either way.
        return Verdict::NoVerdict(NoVerdictReason::UndeclaredExit { code });
    }
    if decl.writes_result_file() {
        match result {
            Some(Ok(file)) if !file.summary.trim().is_empty() => {}
            Some(Ok(_)) => {
                return Verdict::NoVerdict(NoVerdictReason::InvalidResult(
                    "summary is empty".to_string(),
                ));
            }
            Some(Err(why)) => return Verdict::NoVerdict(NoVerdictReason::InvalidResult(why)),
            None => {
                return Verdict::NoVerdict(NoVerdictReason::InvalidResult(
                    "no result file was written".to_string(),
                ));
            }
        }
    }
    if fails {
        return Verdict::Fail;
    }
    if decl.summary.is_some() && !summary_seen {
        return Verdict::NoVerdict(NoVerdictReason::MissingSummary);
    }
    Verdict::Pass
}

/// A check's answer to its scope script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeAnswer {
    Items { items: Vec<String>, reason: String },
    All { reason: String },
    None { reason: String },
}

impl ScopeAnswer {
    pub fn reason(&self) -> &str {
        match self {
            Self::Items { reason, .. } | Self::All { reason } | Self::None { reason } => reason,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScope {
    scope: String,
    #[serde(default)]
    items: Option<Vec<String>>,
    reason: String,
}

/// Reads a scope script's answer (`BONGOS_SCOPE_RESULT`). Exit other than 0, a missing or
/// invalid file, an empty reason, or `items` with an empty list (or given for `all` or `none`)
/// is [`NoVerdictReason::InvalidScope`].
pub fn parse_scope_answer(
    exit: Exit,
    file: Option<&[u8]>,
    limit_bytes: usize,
) -> Result<ScopeAnswer, NoVerdictReason> {
    let invalid = |why: &str| NoVerdictReason::InvalidScope(why.to_string());
    match exit {
        Exit::Code(0) => {}
        Exit::Code(code) => return Err(NoVerdictReason::InvalidScope(format!("exit {code}"))),
        Exit::Signal(signal) => {
            return Err(NoVerdictReason::InvalidScope(format!(
                "killed by signal {signal}"
            )));
        }
    }
    let bytes = file.ok_or_else(|| invalid("no scope file was written"))?;
    if bytes.len() > limit_bytes {
        return Err(invalid("scope file is too large"));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("not UTF-8"))?;
    let raw: RawScope = serde_json::from_str(text)
        .map_err(|error| NoVerdictReason::InvalidScope(error.to_string()))?;
    if raw.reason.trim().is_empty() {
        return Err(invalid("reason is empty"));
    }
    match (raw.scope.as_str(), raw.items) {
        ("items", Some(items)) => {
            if items.is_empty() || items.iter().any(|item| item.trim().is_empty()) {
                Err(invalid("items must be a list of non-empty names"))
            } else {
                Ok(ScopeAnswer::Items {
                    items,
                    reason: raw.reason,
                })
            }
        }
        ("items", None) => Err(invalid("scope items needs an items list")),
        ("all", None) => Ok(ScopeAnswer::All { reason: raw.reason }),
        ("none", None) => Ok(ScopeAnswer::None { reason: raw.reason }),
        ("all" | "none", Some(_)) => Err(invalid("items only goes with scope items")),
        (other, _) => Err(NoVerdictReason::InvalidScope(format!(
            "unknown scope {other:?}"
        ))),
    }
}

/// The most parts a plan may ask for.
pub const MAX_PARTS: u32 = 1000;

/// Reads a `parts.plan` script's answer: exit 0 and one integer, 1 to 1000, on stdout.
pub fn parse_plan_answer(exit: Exit, stdout: &str) -> Result<u32, NoVerdictReason> {
    let invalid = |why: String| NoVerdictReason::InvalidPlan(why);
    match exit {
        Exit::Code(0) => {}
        Exit::Code(code) => return Err(invalid(format!("exit {code}"))),
        Exit::Signal(signal) => return Err(invalid(format!("killed by signal {signal}"))),
    }
    let text = stdout.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("expected one integer".to_string()));
    }
    match text.parse::<u32>() {
        Ok(n) if (1..=MAX_PARTS).contains(&n) => Ok(n),
        _ => Err(invalid(format!("{text} is not from 1 to {MAX_PARTS}"))),
    }
}

/// The verdict of a whole check run in parts, from `(part number, verdict)`: any `Fail` is
/// `Fail`; else any no verdict is that one (the first by part number); else `Pass`. No parts at
/// all is a no verdict, never a pass.
pub fn combine(parts: &[(u32, Verdict)]) -> Verdict {
    if parts.is_empty() {
        return Verdict::NoVerdict(NoVerdictReason::InvalidPlan("no parts ran".to_string()));
    }
    if parts.iter().any(|(_, verdict)| *verdict == Verdict::Fail) {
        return Verdict::Fail;
    }
    let first = parts
        .iter()
        .filter_map(|(n, verdict)| match verdict {
            Verdict::NoVerdict(reason) => Some((*n, reason)),
            _ => None,
        })
        .min_by_key(|(n, _)| *n);
    match first {
        Some((_, reason)) => Verdict::NoVerdict(reason.clone()),
        None => Verdict::Pass,
    }
}

/// The failing items of a run in parts: the union over parts, sorted, without repeats.
pub fn union_failing<'a>(parts: impl IntoIterator<Item = &'a [String]>) -> Vec<String> {
    let mut all: Vec<String> = parts.into_iter().flatten().cloned().collect();
    all.sort();
    all.dedup();
    all
}
