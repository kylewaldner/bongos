//! The Codex adapter (design D9). Codex was not installed on the spike machine, so every line here is
//! the design's `unverified` assumption, kept in one table ([`CodexTable`]) so that a later check
//! changes data and not logic:
//! - unverified: `codex resume <id>`, `codex resume --last`, `codex fork <id>` (only when `fork` is
//!   among the subcommands of `--help`), `-m <model>`, `-c key="value"` where the value is a TOML
//!   string, and the instructions-file key `model_instructions_file`.
//! - unverified: Codex has no flag to name a session, and reports its session id only in its own
//!   files, `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, whose first line is believed to be a meta
//!   record holding `id` and `cwd`, either at the top or under `payload`.
//! - unverified: `codex login status` exits 0 when logged in and nonzero when not; its text is not
//!   relied on beyond a "not logged in" marker, and it is never kept.
//!
//! Two Codex chats started in one directory within a second are told apart by the bongos chat id when
//! the session file holds the instructions text; if it does not, the second waits until the first
//! has been captured (see [`crate::capture`]).

use std::fs;
use std::path::Path;

use chrono::{DateTime, NaiveDate, Utc};

use crate::adapter::{
    AgentAdapter, Capabilities, CaptureQuery, Fallback, LaunchKind, LaunchPlan, LaunchSpec, Login,
    Shape, launch_env, note,
};
use crate::capture::{Candidate, created_at, is_uuid, pick_unambiguous, read_head};
use crate::record::Agent;

/// Every unverified Codex fact, as data.
#[derive(Clone, Copy, Debug)]
pub struct CodexTable {
    /// The `config.toml` key that names an instructions file; `None` when there is none.
    pub instructions_key: Option<&'static str>,
    pub resume: &'static str,
    pub fork: &'static str,
    pub last_flag: &'static str,
    /// Tried in order; the first the help lists is used.
    pub model_flags: &'static [&'static str],
    pub config_flags: &'static [&'static str],
    /// The subcommand that must exist for the login probe to be offered.
    pub login_subcommand: &'static str,
    pub login_probe: &'static [&'static str],
    /// The text that marks a logged-out answer.
    pub logged_out_marker: &'static str,
    /// Where the sessions are, under the home directory.
    pub sessions_dir: &'static [&'static str],
    pub rollout_prefix: &'static str,
}

pub const CODEX_TABLE: CodexTable = CodexTable {
    instructions_key: Some("model_instructions_file"),
    resume: "resume",
    fork: "fork",
    last_flag: "--last",
    model_flags: &["-m", "--model"],
    config_flags: &["-c", "--config"],
    login_subcommand: "login",
    login_probe: &["login", "status"],
    logged_out_marker: "not logged in",
    sessions_dir: &[".codex", "sessions"],
    rollout_prefix: "rollout-",
};

pub struct CodexAdapter {
    command: String,
    table: CodexTable,
}

impl CodexAdapter {
    pub fn new(command: &str) -> Self {
        Self::with_table(command, CODEX_TABLE)
    }

    pub fn with_table(command: &str, table: CodexTable) -> Self {
        Self {
            command: command.to_string(),
            table,
        }
    }

    fn first_flag(&self, caps: &Capabilities, flags: &[&'static str]) -> Option<&'static str> {
        flags.iter().copied().find(|flag| caps.has_flag(flag))
    }
}

/// `value` as a TOML basic string, which is how `-c key=value` reads a path.
fn toml_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

impl AgentAdapter for CodexAdapter {
    fn agent(&self) -> Agent {
        Agent::Codex
    }

    fn capabilities_from_help(&self, version: &str, help: &str) -> Capabilities {
        Capabilities::from_help(version, help, &[])
    }

    fn shape(&self, caps: &Capabilities) -> Shape {
        Shape {
            skills_as_plugin: false,
            // No hook, so the instructions file carries `context.md` too.
            context_in_system: true,
            native_fork: caps.has_subcommand(self.table.fork),
        }
    }

    fn launch(&self, plan: &LaunchPlan, caps: &Capabilities) -> LaunchSpec {
        let table = &self.table;
        let mut args: Vec<String> = Vec::new();
        let mut fallbacks = plan.force.clone();
        let mut capture = false;

        match &plan.kind {
            LaunchKind::Resume => match plan.chat.session_id.as_deref() {
                Some(id) => args.extend([table.resume.to_string(), id.to_string()]),
                None if caps.has_flag(table.last_flag) => {
                    args.extend([table.resume.to_string(), table.last_flag.to_string()]);
                    note(&mut fallbacks, Fallback::ResumedLatest);
                }
                None => {
                    note(&mut fallbacks, Fallback::RestartNoSession);
                    capture = true;
                }
            },
            LaunchKind::Fork {
                source_session,
                native: true,
            } if caps.has_subcommand(table.fork) => {
                args.extend([table.fork.to_string(), source_session.clone()]);
                capture = true;
            }
            LaunchKind::Fork { native, .. } => {
                if *native {
                    note(&mut fallbacks, Fallback::ForkUnsupported);
                }
                capture = true;
            }
            LaunchKind::New => capture = true,
        }
        if capture {
            note(&mut fallbacks, Fallback::CaptureSessionId);
        }

        let system_path = plan.files.system_path.to_string_lossy().into_owned();
        let mut first_message = None;
        match (
            table.instructions_key,
            self.first_flag(caps, table.config_flags),
        ) {
            (Some(key), Some(flag)) => {
                args.push(flag.to_string());
                args.push(format!("{key}={}", toml_string(&system_path)));
            }
            _ => {
                note(&mut fallbacks, Fallback::ContextAsFirstMessage);
                first_message = Some(format!("Read {system_path} before anything else"));
            }
        }

        if let Some(model) = &plan.model {
            match self.first_flag(caps, table.model_flags) {
                Some(flag) => args.extend([flag.to_string(), model.clone()]),
                None => note(&mut fallbacks, Fallback::ModelOmitted),
            }
        }
        args.extend(plan.extra_args.iter().cloned());
        args.extend(first_message);

        LaunchSpec {
            program: self.command.clone(),
            args,
            env: launch_env(&plan.env),
            cwd: plan.chat.cwd.clone(),
            fallbacks,
            capture_session_id: capture,
        }
    }

    fn login_probe(&self, caps: &Capabilities) -> Option<Vec<String>> {
        caps.has_subcommand(self.table.login_subcommand).then(|| {
            self.table
                .login_probe
                .iter()
                .map(|arg| arg.to_string())
                .collect()
        })
    }

    fn parse_login(&self, output: &str, status: i32) -> Login {
        if status != 0 || output.to_lowercase().contains(self.table.logged_out_marker) {
            Login::LoggedOut
        } else {
            Login::LoggedIn
        }
    }

    fn capture_session_id(&self, query: &CaptureQuery) -> Option<String> {
        pick_unambiguous(query, &self.rollouts(query))
    }
}

impl CodexAdapter {
    /// The unclaimed session files created since the launch for the query's cwd. Day directories
    /// older than the launch day by more than one day are skipped (one day of slack for local time).
    fn rollouts(&self, query: &CaptureQuery) -> Vec<Candidate> {
        let mut root = query.home.to_path_buf();
        root.extend(self.table.sessions_dir);
        let earliest = DateTime::<Utc>::from(query.since).date_naive() - chrono::Days::new(1);
        let mut found = Vec::new();
        for year in numbered_dirs(&root) {
            for month in numbered_dirs(&year.1) {
                for day in numbered_dirs(&month.1) {
                    let date = NaiveDate::from_ymd_opt(year.0 as i32, month.0, day.0);
                    if date.is_none_or(|date| date < earliest) {
                        continue;
                    }
                    self.collect(&day.1, query, &mut found);
                }
            }
        }
        found
    }

    fn collect(&self, dir: &Path, query: &CaptureQuery, found: &mut Vec<Candidate>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for path in entries.flatten().map(|entry| entry.path()) {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with(self.table.rollout_prefix) || !name.ends_with(".jsonl") {
                continue;
            }
            let Some(created) = created_at(&path).filter(|created| *created >= query.since) else {
                continue;
            };
            let Some((id, cwd)) = session_meta(&path) else {
                continue;
            };
            if cwd.as_deref() != query.cwd.to_str() || query.claimed.contains(&id) {
                continue;
            }
            found.push(Candidate { id, path, created });
        }
    }
}

/// Subdirectories of `dir` whose names are numbers, with their value.
fn numbered_dirs(dir: &Path) -> Vec<(u32, std::path::PathBuf)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let value = entry.file_name().to_str()?.parse().ok()?;
            entry.path().is_dir().then(|| (value, entry.path()))
        })
        .collect()
}

/// The session id and working directory from the first line of a session file, which may hold them
/// at the top or under `payload`. The id falls back to the UUID that ends the file name.
fn session_meta(path: &Path) -> Option<(String, Option<String>)> {
    let head = read_head(path);
    let first = head.lines().next()?;
    let value: serde_json::Value = serde_json::from_str(first).ok()?;
    let field = |name: &str| {
        [
            value.get(name),
            value.get("payload").and_then(|p| p.get(name)),
        ]
        .into_iter()
        .flatten()
        .find_map(|v| v.as_str().map(str::to_string))
    };
    let id = field("id").or_else(|| {
        let stem = path.file_stem()?.to_str()?;
        let tail = stem.get(stem.len().checked_sub(36)?..)?;
        is_uuid(tail).then(|| tail.to_string())
    })?;
    Some((id, field("cwd")))
}
