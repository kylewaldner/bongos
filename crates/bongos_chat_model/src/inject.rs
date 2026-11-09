//! The renderer of everything a chat launch injects (design D10): `context.md`, `system.md` (Claude),
//! `codex-instructions.md` (Codex), `claude-settings.json`, the environment and the handoff note.
//!
//! It is pure: it takes values and returns text, never reads a directory and never lists a task's
//! files. The promise that nothing human-only reaches the agent rests on one rule: the only artifact
//! paths and descriptions that can appear in any output are the ones in `SessionContext::artifacts`
//! (which is `shared_artifacts`, empty for a broken manifest). `TaskFacts` carries none, and the
//! handoff note carries the parent's name, type, agent and transcript path only.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use bongos_work_model::{SessionContext, SharedArtifact, layout};
use serde_json::json;

use crate::error::{Error, ErrorKind, Result};
use crate::facts::TaskFacts;
use crate::record::ChatRecord;
use crate::types::ResolvedType;

/// The chat a handoff fork continues. Never its artifacts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffParent {
    pub name: String,
    pub chat_type: String,
    pub agent: String,
    /// The parent's own conversation file, when the agent's session file was found.
    pub transcript: Option<PathBuf>,
}

/// Everything the renderer reads. All of it is data in memory.
#[derive(Clone, Copy, Debug)]
pub struct RenderInput<'a> {
    /// What the task's shared-artifact rules allow the chat to see.
    pub context: &'a SessionContext,
    pub facts: &'a TaskFacts,
    pub record: &'a ChatRecord,
    pub resolved: &'a ResolvedType,
    /// The task or project directory the chat's record lives in; where its default prompts file is.
    pub scope_dir: &'a Path,
    pub task_id: Option<&'a str>,
    pub record_path: &'a Path,
    /// Where the rendered `context.md` is written.
    pub context_path: &'a Path,
    /// `~/.bongos`.
    pub bongos_root: Option<&'a Path>,
    /// The skills are offered to the agent as a plugin, so their text is not put in the prompt.
    pub skills_as_plugin: bool,
    /// Set for a handoff fork.
    pub handoff: Option<&'a HandoffParent>,
    /// Text a caller adds at the very end of the context, after the shared artifacts. It is
    /// rendered as given and is the caller's to keep free of anything hidden.
    pub extra_context: Option<&'a str>,
}

impl RenderInput<'_> {
    fn prompts_file(&self) -> PathBuf {
        match &self.record.prompts_file {
            Some(path) => path.clone(),
            None => layout::prompts_file(self.scope_dir),
        }
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn artifact_line(artifact: &SharedArtifact) -> String {
    let description = one_line(&artifact.description);
    if description.is_empty() {
        format!("- {}", artifact.path.display())
    } else {
        format!("- {}: {description}", artifact.path.display())
    }
}

/// `context.md`: the scope pointer, then per the type the notes directory, dependencies, PR links
/// and the thinking line, then the shared artifacts, then `extra_context`.
pub fn render_context(input: &RenderInput) -> String {
    let context = input.context;
    let facts = input.facts;
    let config = &input.resolved.config;
    let mut out = String::new();

    let _ = writeln!(out, "# Task context\n");
    let _ = writeln!(out, "Goal: {}", one_line(&context.goal));
    let _ = writeln!(out, "Task file: {}", context.task_toml.display());
    let _ = writeln!(out, "Task directory: {}", context.task_dir.display());
    if let Some(worktree) = &context.worktree {
        let _ = writeln!(out, "Worktree: {}", worktree.display());
    }
    if let Some(group) = &context.parent_goal {
        let _ = writeln!(out, "Group goal: {}", one_line(group));
    }
    if let Some(title) = &facts.project_title {
        let _ = writeln!(out, "Project: {}", one_line(title));
    }
    if let Some(dir) = &facts.project_dir {
        let _ = writeln!(out, "Project directory: {}", dir.display());
    }

    if config.task_context {
        if let Some(notes) = &facts.notes_dir {
            let _ = writeln!(out, "Notes directory: {}", notes.display());
        }
        if !facts.deps.is_empty() {
            let _ = writeln!(out, "\n## Dependencies\n");
            for (goal, state) in &facts.deps {
                let _ = writeln!(out, "- {} ({})", one_line(goal), one_line(state));
            }
        }
    }
    if config.review_context && !facts.pr_links.is_empty() {
        let _ = writeln!(out, "\n## Pull requests\n");
        for link in &facts.pr_links {
            let _ = writeln!(out, "- {}", one_line(link));
        }
    }
    if config.thinking {
        let _ = writeln!(
            out,
            "\nThis task is in thinking mode: help think it through (ideas, diagrams, documents) \
             and do not start implementing it unless asked."
        );
    }

    if config.artifacts {
        let _ = writeln!(out, "\n## Shared artifacts\n");
        if context.artifacts.is_empty() {
            let _ = writeln!(out, "None are shared with this chat.");
        } else {
            for artifact in &context.artifacts {
                let _ = writeln!(out, "{}", artifact_line(artifact));
            }
            let prompts = input.prompts_file();
            if context
                .artifacts
                .iter()
                .any(|artifact| artifact.path == prompts)
            {
                let _ = writeln!(out, "\nPrompts file: {}", prompts.display());
            }
        }
    }
    if let Some(extra) = input.extra_context.map(str::trim_end)
        && !extra.is_empty()
    {
        let _ = writeln!(out, "\n{extra}");
    }
    out
}

/// The note a handoff fork starts with: who it continues and where that conversation is.
pub fn handoff_note(parent: &HandoffParent) -> String {
    let mut note = format!(
        "# Handoff\n\nThis chat continues \"{}\", a {} chat run with {}.",
        one_line(&parent.name),
        one_line(&parent.chat_type),
        one_line(&parent.agent),
    );
    match &parent.transcript {
        Some(path) => {
            let _ = write!(
                note,
                " Its conversation transcript is at {}; read it first to pick up where it stopped.",
                path.display()
            );
        }
        None => note.push_str(" Its transcript could not be found."),
    }
    note.push('\n');
    note
}

fn prompt_body(input: &RenderInput) -> String {
    let resolved = input.resolved;
    let mut parts: Vec<String> = Vec::new();
    if !resolved.context.trim().is_empty() {
        parts.push(resolved.context.trim_end().to_string());
    }
    if let Some(text) = &resolved.agent_context
        && !text.trim().is_empty()
    {
        parts.push(text.trim_end().to_string());
    }
    if !input.skills_as_plugin {
        for skill in &resolved.skills {
            parts.push(format!(
                "# Skill: {}\n\n{}",
                one_line(&skill.name),
                skill.text.trim_end()
            ));
        }
    }
    if let Some(parent) = input.handoff {
        parts.push(handoff_note(parent).trim_end().to_string());
    }
    parts.join("\n\n")
}

/// `system.md` for Claude: the type's context, the agent override, skill text unless offered as a
/// plugin, and the handoff note. The context file reaches Claude through the `SessionStart` hook.
pub fn render_system(input: &RenderInput) -> String {
    let mut out = prompt_body(input);
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// The line that carries the bongos chat id in `codex-instructions.md`, so the session file Codex
/// writes can be matched to its chat (see `capture.rs`). Whether Codex stores the instructions
/// text in its session files is unverified.
pub const CHAT_ID_LABEL: &str = "chat id: ";

/// `codex-instructions.md`: a `chat id: <id>` line, the same as `system.md`, and `context.md` as
/// well because Codex has no hook to print it. The file is also what the first-message fallback
/// points Codex at, so the id line is in it on every path.
pub fn render_codex_instructions(input: &RenderInput) -> String {
    let mut parts = vec![format!("{CHAT_ID_LABEL}{}", input.record.id)];
    parts.push(prompt_body(input));
    parts.push(render_context(input).trim_end().to_string());
    let mut out = parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    out.push('\n');
    out
}

/// `path` in single quotes for a POSIX shell. A path that cannot be quoted without doubt (it has a
/// single quote, a control character such as a newline or NUL, or is not UTF-8) is refused.
pub fn shell_quote(path: &Path) -> Result<String> {
    let refuse = || Error::new(ErrorKind::Unquotable(path.to_string_lossy().into_owned()));
    let text = path.to_str().ok_or_else(refuse)?;
    if text.is_empty() || text.contains('\'') || text.chars().any(char::is_control) {
        return Err(refuse());
    }
    Ok(format!("'{text}'"))
}

/// `claude-settings.json`: one `SessionStart` hook that prints `context.md`, and the terminal bell
/// notification channel so "waiting for input" rings (see `claude.rs` for what is verified).
pub fn render_settings(context_path: &Path) -> Result<String> {
    let command = format!("cat {}", shell_quote(context_path)?);
    let settings = json!({
        crate::claude::BELL_KEY: crate::claude::BELL_VALUE,
        "hooks": {
            "SessionStart": [
                { "hooks": [ { "type": "command", "command": command } ] }
            ]
        }
    });
    let mut text = serde_json::to_string_pretty(&settings)
        .map_err(|error| Error::new(ErrorKind::Serialize(error.to_string())))?;
    text.push('\n');
    Ok(text)
}

/// The `BONGOS_*` environment. Variables that do not apply are left out. It holds no secret and no
/// artifact path.
pub fn render_env(input: &RenderInput) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let mut set = |name: &str, value: String| {
        env.insert(name.to_string(), value);
    };
    let text = |path: &Path| path.to_string_lossy().into_owned();
    set("BONGOS_CHAT_ID", input.record.id.clone());
    set("BONGOS_CHAT_TYPE", input.record.chat_type.clone());
    set("BONGOS_CHAT_AGENT", input.record.agent.clone());
    set("BONGOS_CHAT_RECORD", text(input.record_path));
    set("BONGOS_CHAT_CONTEXT", text(input.context_path));
    if let Some(id) = input.task_id {
        set("BONGOS_TASK_ID", id.to_string());
    }
    set("BONGOS_TASK_DIR", text(&input.context.task_dir));
    set("BONGOS_TASK_FILE", text(&input.context.task_toml));
    if let Some(dir) = &input.facts.project_dir {
        set("BONGOS_PROJECT_DIR", text(dir));
    }
    set("BONGOS_WORKTREE", text(&input.record.cwd));
    if let Some(root) = input.bongos_root {
        set("BONGOS_ROOT", text(root));
    }
    env
}
