//! The Claude Code adapter (design D8), encoded from the step 4 spike on claude 2.1.319.
//!
//! Facts it rests on, as the spike found them:
//! - `--append-system-prompt-file` works but is not in `--help` (only `--append-system-prompt` is).
//!   It is treated as present from minimum version 2.1.319, which is known good; the first version
//!   that has it is unknown.
//! - `--resume <id>` works from any directory and appends to the original transcript file.
//! - `--resume <src> --fork-session --session-id <new uuid>` is accepted and names the file
//!   `<uuid>.jsonl` in the fork's cwd, so no id capture is needed for a fork on 2.1.319. An error
//!   exit on that combination means the fallback: fork without `--session-id` and read the new id
//!   from the newest transcript file of the cwd.
//! - `SessionStart` sources are `startup`, `resume` and `fork` (a fork fires `fork`, not `resume`);
//!   `clear` and `compact` are unverified. The settings file has one hook with no matcher, which
//!   matches every source.
//! - `claude auth status` prints JSON; only the boolean `loggedIn` is read. A nonzero exit, or
//!   anything but `true`, means not logged in. The logged-out shape was not exercised.
//! - The transcript of a session is `~/.claude/projects/<cwd slug>/<session id>.jsonl`, the slug
//!   being the cwd with every `/` and `.` replaced by `-`.
//! - The bell setting key is `preferredNotifChannel` = `terminal_bell`, confirmed only by the CLI's
//!   settings schema; whether a bell reaches a terminal from a `--settings` file is unverified.

use std::path::Path;

use crate::adapter::{
    AgentAdapter, Capabilities, CaptureQuery, Fallback, HiddenFlag, LaunchKind, LaunchPlan,
    LaunchSpec, Login, Shape, launch_env, note,
};
use crate::capture::{Candidate, created_at, is_uuid, pick_newest};
use crate::record::Agent;

/// `--help` hides these flags that the CLI accepts.
pub const HIDDEN_FLAGS: &[HiddenFlag] = &[HiddenFlag {
    flag: "--append-system-prompt-file",
    min_version: "2.1.319",
}];

/// The settings key and value that turn on the terminal bell.
pub const BELL_KEY: &str = "preferredNotifChannel";
pub const BELL_VALUE: &str = "terminal_bell";

const SYSTEM_FILE_FLAG: &str = "--append-system-prompt-file";
const SYSTEM_TEXT_FLAG: &str = "--append-system-prompt";

pub struct ClaudeAdapter {
    command: String,
}

impl ClaudeAdapter {
    pub fn new(command: &str) -> Self {
        Self {
            command: command.to_string(),
        }
    }

    fn injection(
        &self,
        plan: &LaunchPlan,
        caps: &Capabilities,
        args: &mut Vec<String>,
        fallbacks: &mut Vec<Fallback>,
    ) {
        let files = &plan.files;
        if caps.has_flag(SYSTEM_FILE_FLAG) {
            args.push(SYSTEM_FILE_FLAG.to_string());
            args.push(files.system_path.to_string_lossy().into_owned());
        } else if caps.has_flag(SYSTEM_TEXT_FLAG) {
            // The one case where argv carries the system text, and so possibly the paths of shared
            // artifacts (never a human-only one): allowed by D8, labeled on the row.
            args.push(SYSTEM_TEXT_FLAG.to_string());
            args.push(files.system_text.clone());
            note(fallbacks, Fallback::SystemPromptAsText);
        } else {
            note(fallbacks, Fallback::SystemPromptUnavailable);
        }
        match (&files.settings_path, caps.has_flag("--settings")) {
            (Some(path), true) => {
                args.push("--settings".to_string());
                args.push(path.to_string_lossy().into_owned());
            }
            _ => note(fallbacks, Fallback::ContextInSystemPrompt),
        }
        if caps.has_flag("--plugin-dir") {
            if let Some(dir) = &files.plugin_dir {
                args.push("--plugin-dir".to_string());
                args.push(dir.to_string_lossy().into_owned());
            }
        } else if files.has_skills {
            note(fallbacks, Fallback::SkillsAsText);
        }
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn agent(&self) -> Agent {
        Agent::Claude
    }

    fn capabilities_from_help(&self, version: &str, help: &str) -> Capabilities {
        Capabilities::from_help(version, help, HIDDEN_FLAGS)
    }

    fn shape(&self, caps: &Capabilities) -> Shape {
        Shape {
            skills_as_plugin: caps.has_flag("--plugin-dir"),
            context_in_system: !caps.has_flag("--settings"),
            native_fork: caps.has_flag("--fork-session"),
        }
    }

    fn launch(&self, plan: &LaunchPlan, caps: &Capabilities) -> LaunchSpec {
        let mut args: Vec<String> = Vec::new();
        let mut fallbacks = plan.force.clone();
        let mut capture = false;
        let session_id = plan.chat.session_id.as_deref();
        let push = |args: &mut Vec<String>, flag: &str, value: &str| {
            args.push(flag.to_string());
            args.push(value.to_string());
        };

        let mut resumed = false;
        let mut forked = false;
        match &plan.kind {
            LaunchKind::Resume => match session_id {
                Some(id) => {
                    push(&mut args, "--resume", id);
                    resumed = true;
                }
                None => note(&mut fallbacks, Fallback::RestartNoSession),
            },
            LaunchKind::Fork {
                source_session,
                native: true,
            } if caps.has_flag("--fork-session") => {
                push(&mut args, "--resume", source_session);
                args.push("--fork-session".to_string());
                forked = true;
            }
            LaunchKind::Fork { native: true, .. } => {
                note(&mut fallbacks, Fallback::ForkUnsupported)
            }
            LaunchKind::Fork { native: false, .. } | LaunchKind::New => {}
        }

        if !resumed {
            let skip_id = forked && fallbacks.contains(&Fallback::CaptureSessionId);
            match session_id {
                Some(id) if caps.has_flag("--session-id") && !skip_id => {
                    push(&mut args, "--session-id", id);
                }
                _ => {
                    note(&mut fallbacks, Fallback::CaptureSessionId);
                    capture = true;
                }
            }
            if caps.has_flag("--name") {
                push(&mut args, "--name", &plan.chat.name);
            } else {
                note(&mut fallbacks, Fallback::NameOmitted);
            }
        }

        self.injection(plan, caps, &mut args, &mut fallbacks);

        if let Some(model) = &plan.model {
            if caps.has_flag("--model") {
                push(&mut args, "--model", model);
            } else {
                note(&mut fallbacks, Fallback::ModelOmitted);
            }
        }
        args.extend(plan.extra_args.iter().cloned());

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
        caps.has_subcommand("auth")
            .then(|| vec!["auth".to_string(), "status".to_string()])
    }

    fn parse_login(&self, output: &str, status: i32) -> Login {
        if status != 0 {
            return Login::LoggedOut;
        }
        let logged_in = serde_json::from_str::<serde_json::Value>(output.trim())
            .ok()
            .and_then(|value| value.get("loggedIn").and_then(serde_json::Value::as_bool));
        match logged_in {
            Some(true) => Login::LoggedIn,
            _ => Login::LoggedOut,
        }
    }

    fn capture_session_id(&self, query: &CaptureQuery) -> Option<String> {
        // The newest transcript created after launch, unless one of them names this chat.
        pick_newest(query, transcripts(query))
    }
}

/// The directory name Claude gives a cwd: every `/` and `.` becomes `-`.
pub fn project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

fn transcripts(query: &CaptureQuery) -> Vec<Candidate> {
    let dir = query
        .home
        .join(".claude")
        .join("projects")
        .join(project_slug(query.cwd));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "jsonl" {
                return None;
            }
            let id = path.file_stem()?.to_str()?.to_string();
            let created = created_at(&path)?;
            (is_uuid(&id) && created >= query.since && !query.claimed.contains(&id))
                .then_some(Candidate { id, path, created })
        })
        .collect()
}
