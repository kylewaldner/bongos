//! Confining agent steps (`confinement.agent_steps`): the Claude and Codex runners spawn the CLI
//! under a [`Confinement`] of [`JobKind::Agent`], as the program runner does a program pass.
//!
//! - Writable: the node's worktree, its scratch directory, its declared directories and the
//!   agent CLI's state paths (`confinement.agent_state_paths.<cli>`, `~` being the job's `HOME`).
//!   A configured state path that does not exist is dropped with a warning in the try's
//!   [`CONFINEMENT_LOG`], and the job starts all the same. The network is not restricted.
//! - Git: a run or branch worktree gets private git ([`TreeGit::Private`]), prepared last before
//!   the spawn and imported into the shared repository once the process group is gone; then the
//!   confinement is finished. A refused entry or a failed import fails the node, as for a program.
//! - When the OS sandbox cannot be created the CLI runs as it is, and the log says
//!   "not sandboxed: <reason>"; a job in a main worktree is then refused.
//! - The appended system prompt gets one line naming the writable roots and the rule.
//! - Once the try ends, every line of its output reporting a write the sandbox refused is
//!   annotated with the rule ([`annotate::refusal`]) in the log; the annotations are the try's
//!   `confinement_refusals`.
//!
//! While the switch is off a node carries no [`AgentJob`] and nothing here runs.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use bongos_confine::private_git::{self, PrivateGit};
use bongos_confine::safe_git::SafeGit;
use bongos_confine::{
    Confinement, HostGit, JobDeclaration, JobKind, JobTree, RULE, TreeAccess, TreeGit, annotate,
};
use bongos_workflow_model::{Failure, FailureReason, NodeKey};
use sandbox::{CommandAndArgs, Sandbox, SandboxNetPolicy};
use serde_json::Value;

use crate::runner_program::SandboxProbe;

/// The log of a confined try, in its record directory: the dropped state paths, the sandbox
/// state, the writable roots, the git import and the refused writes.
pub const CONFINEMENT_LOG: &str = "confinement.log";

/// `confinement.agent_steps` and `confinement.agent_state_paths`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentConfinementSettings {
    /// Whether agent steps run confined.
    pub agent_steps: bool,
    pub claude_state_paths: Vec<String>,
    pub codex_state_paths: Vec<String>,
}

/// What a confined agent step may write, prepared by the engine. A node carries one only while
/// `confinement.agent_steps` is on.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentJob {
    /// The node's scratch directory: `TMPDIR` and the private git directory.
    pub scratch: PathBuf,
    /// Directories the node declared it writes, besides its worktree and scratch.
    pub declared_dirs: Vec<PathBuf>,
    pub tree_access: TreeAccess,
    pub tree_git: TreeGit,
    /// What the host read about the repository from the run's record: the common git directory
    /// first, then the worktree's linked git directory; and the run's record directory.
    pub host_git: HostGit,
    /// The CLI's state paths as configured; `~` is the job's `HOME`.
    pub state_paths: Vec<String>,
}

/// The configured state paths with `~` expanded from `home`, and a warning for each one dropped:
/// one that does not exist, is not absolute, or names `~` with no `HOME`.
pub fn resolve_state_paths(
    configured: &[String],
    home: Option<&str>,
) -> (Vec<PathBuf>, Vec<String>) {
    let mut paths = Vec::new();
    let mut warnings = Vec::new();
    let home = home.filter(|home| !home.is_empty()).map(PathBuf::from);
    for entry in configured {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let expanded = if entry == "~" {
            home.clone()
        } else if let Some(rest) = entry.strip_prefix("~/") {
            home.as_ref().map(|home| home.join(rest))
        } else {
            Some(PathBuf::from(entry))
        };
        let Some(path) = expanded else {
            warnings.push(format!(
                "warning: the agent state path {entry} is dropped: the job has no HOME"
            ));
            continue;
        };
        if !path.is_absolute() {
            warnings.push(format!(
                "warning: the agent state path {entry} is dropped: it is not absolute"
            ));
        } else if !path.exists() {
            warnings.push(format!(
                "warning: the agent state path {entry} ({}) is dropped: it does not exist",
                path.display()
            ));
        } else {
            paths.push(path);
        }
    }
    (paths, warnings)
}

/// The line appended to the agent's system prompt.
pub fn prompt_line(roots: &[PathBuf]) -> String {
    let roots = roots
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("Bongos confines this step: write only in {roots}; any other write fails ({RULE}).")
}

/// A confined try's command, built but not spawned yet.
pub(crate) struct Prepared {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// The line for the system prompt ([`prompt_line`]).
    pub prompt_line: String,
    confinement: Confinement,
    sandbox: Option<Sandbox>,
    tree: PathBuf,
    scratch: PathBuf,
    linked_git_dir: Option<PathBuf>,
    log: PathBuf,
}

/// A confined try in flight: kept until its process group is gone, then settled.
pub(crate) struct Running {
    confinement: Confinement,
    /// Kept alive while the process runs.
    sandbox: Option<Sandbox>,
    private_git: Option<PrivateGit>,
    log: PathBuf,
}

/// Builds the confinement of `job` in `worktree` and wraps `program` with `args` and `env` in
/// its sandbox. `env`'s `TMPDIR` becomes the scratch directory's. A refusal is the failure the
/// start returns; nothing is left to finish then.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare(
    job: &AgentJob,
    key: &NodeKey,
    worktree: &Path,
    try_dir: &Path,
    probe: &SandboxProbe,
    program: String,
    args: Vec<String>,
    mut env: BTreeMap<String, String>,
) -> Result<Prepared, Failure> {
    let refuse =
        |reason: FailureReason, detail: String| Failure::new(reason, Some(key.clone()), detail);
    if !worktree.is_dir() {
        return Err(refuse(
            FailureReason::WorktreeUnavailable,
            format!("{} is missing", worktree.display()),
        ));
    }
    let tmp = job.scratch.join(crate::worktrees::SCRATCH_TMP_DIR);
    fs::create_dir_all(&tmp).map_err(|error| {
        refuse(
            FailureReason::WorktreeUnavailable,
            format!("{}: {error}", tmp.display()),
        )
    })?;
    let log = try_dir.join(CONFINEMENT_LOG);
    let (extra_writable, warnings) =
        resolve_state_paths(&job.state_paths, env.get("HOME").map(String::as_str));
    let declaration = JobDeclaration {
        kind: JobKind::Agent,
        trees: vec![JobTree {
            path: worktree.to_path_buf(),
            access: job.tree_access,
            git: job.tree_git,
        }],
        scratch: job.scratch.clone(),
        declared_dirs: job.declared_dirs.clone(),
        extra_writable,
        network: SandboxNetPolicy::Unrestricted,
    };
    let confinement = Confinement::build(&declaration, &job.host_git)
        .map_err(|error| refuse(FailureReason::PermissionDenied, error.to_string()))?;
    let linked_git_dir = match job.tree_git {
        TreeGit::Private => match job.host_git.git_dirs.get(1) {
            Some(dir) => Some(dir.clone()),
            None => {
                finish(&confinement);
                return Err(refuse(
                    FailureReason::WorktreeUnavailable,
                    format!(
                        "the run's record names no git directory for {}",
                        worktree.display()
                    ),
                ));
            }
        },
        TreeGit::ReadOnly | TreeGit::None => None,
    };
    env.insert("TMPDIR".to_string(), tmp.to_string_lossy().into_owned());

    let policy = confinement.sandbox_policy();
    let wrapped = probe(&policy, worktree).and_then(|()| {
        let mut sandbox = Sandbox::new(policy).map_err(|error| error.to_string())?;
        let command = CommandAndArgs {
            program: program.clone(),
            args: args.clone(),
            env: env.clone().into_iter().collect::<HashMap<_, _>>(),
            cwd: Some(worktree.to_path_buf()),
        };
        let wrapped = futures::executor::block_on(sandbox.wrap(&command))
            .map_err(|error| error.to_string())?;
        Ok((wrapped, sandbox))
    });
    let (program, args, env, sandbox, note) = match wrapped {
        Ok((wrapped, sandbox)) => (
            wrapped.program,
            wrapped.args,
            wrapped.env.into_iter().collect(),
            Some(sandbox),
            "sandboxed".to_string(),
        ),
        Err(reason) => {
            log::warn!("workflows: an agent step runs without the OS sandbox: {reason}");
            (program, args, env, None, format!("not sandboxed: {reason}"))
        }
    };
    if sandbox.is_none() && confinement.requires_sandbox() {
        finish(&confinement);
        return Err(refuse(
            FailureReason::PermissionDenied,
            format!(
                "a job in a main worktree runs only in the OS sandbox ({note}): {}",
                worktree.display()
            ),
        ));
    }
    let mut lines = warnings;
    lines.push(note);
    lines.push(format!(
        "writable: {}",
        confinement
            .writable_roots()
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if let Err(error) = append_log(&log, &lines) {
        finish(&confinement);
        return Err(refuse(
            FailureReason::WorktreeUnavailable,
            format!("{}: {error}", log.display()),
        ));
    }
    Ok(Prepared {
        program,
        args,
        env,
        prompt_line: prompt_line(confinement.writable_roots()),
        confinement,
        sandbox,
        tree: worktree.to_path_buf(),
        scratch: job.scratch.clone(),
        linked_git_dir,
        log,
    })
}

impl Prepared {
    /// The command to spawn, in the worktree with only [`Prepared::env`].
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .current_dir(&self.tree)
            .env_clear()
            .envs(&self.env);
        command
    }

    /// Gives the worktree its private git, last before the spawn. A failure finishes the
    /// confinement and refuses the start.
    pub fn start(self, key: &NodeKey) -> Result<Running, Failure> {
        let private_git = match &self.linked_git_dir {
            Some(linked_git_dir) => {
                match futures::executor::block_on(private_git::prepare(
                    &self.tree,
                    linked_git_dir,
                    &self.scratch,
                )) {
                    Ok(private_git) => Some(private_git),
                    Err(error) => {
                        finish(&self.confinement);
                        return Err(Failure::new(
                            FailureReason::WorktreeUnavailable,
                            Some(key.clone()),
                            format!("could not give the node its own git: {error:#}"),
                        ));
                    }
                }
            }
            None => None,
        };
        Ok(Running {
            confinement: self.confinement,
            sandbox: self.sandbox,
            private_git,
            log: self.log,
        })
    }

    /// Ends a try that never started: finishes the confinement.
    pub fn abandon(self) {
        finish(&self.confinement);
    }
}

/// What the end of a confined try found.
#[derive(Debug, Default)]
pub(crate) struct Settled {
    /// The annotations of the writes the sandbox refused, in output order, each once.
    pub refusals: Vec<String>,
    /// Why the git fails the node: a refused entry, or an import that failed.
    pub failure: Option<Failure>,
}

impl Running {
    /// Ends a try whose spawn failed: switches the worktree back to shared git and finishes the
    /// confinement.
    pub fn abandon(mut self) {
        if let Some(private_git) = self.private_git.take()
            && let Err(error) = private_git.abandon()
        {
            log::error!(
                "workflows: could not switch an agent's worktree back to shared git: {error:#}"
            );
        }
        drop(self.sandbox.take());
        finish(&self.confinement);
    }

    /// Ends the try once its process group is gone: imports the private git, finishes the
    /// confinement, and annotates the refused writes the files at `outputs` report.
    pub fn settle(mut self, key: &NodeKey, outputs: &[PathBuf]) -> Settled {
        drop(self.sandbox.take());
        let mut settled = Settled::default();
        let mut lines = Vec::new();
        if let Some(private_git) = self.private_git.take() {
            let git = SafeGit::without_work_tree(private_git.linked_git_dir());
            match futures::executor::block_on(private_git.import(&git)) {
                Ok(report) => {
                    lines.extend(
                        report
                            .imported
                            .iter()
                            .map(|entry| format!("git imported: {entry}")),
                    );
                    lines.extend(
                        report
                            .refused
                            .iter()
                            .map(|entry| format!("git refused: {entry}")),
                    );
                    lines.extend(report.notes.iter().map(|note| format!("git: {note}")));
                    if !report.refused.is_empty() {
                        let mut detail = report.refused.join("; ");
                        if let Some(kept) = &report.kept_private_dir {
                            detail.push_str(&format!(
                                "; the node's git directory {} is kept",
                                kept.display()
                            ));
                        }
                        settled.failure = Some(Failure::new(
                            FailureReason::PermissionDenied,
                            Some(key.clone()),
                            detail,
                        ));
                    }
                }
                Err(error) => {
                    let detail = format!("could not import the node's commits: {error:#}");
                    lines.push(format!("git: {detail}"));
                    settled.failure = Some(Failure::new(
                        FailureReason::WorktreeUnavailable,
                        Some(key.clone()),
                        detail,
                    ));
                }
            }
        }
        finish(&self.confinement);
        settled.refusals = refusals(outputs, self.confinement.writable_roots());
        lines.extend(settled.refusals.iter().cloned());
        if let Err(error) = append_log(&self.log, &lines) {
            log::error!("workflows: could not write {}: {error}", self.log.display());
        }
        settled
    }
}

/// Finishes `confinement`; a failure is logged, since the job is over.
fn finish(confinement: &Confinement) {
    if let Err(error) = confinement.finish() {
        log::error!("workflows: could not finish an agent step's confinement: {error}");
    }
}

fn append_log(path: &Path, lines: &[String]) -> std::io::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut text = lines.join("\n");
    text.push('\n');
    file.write_all(text.as_bytes())
}

/// The annotation of every distinct write outside `roots` that a line of the files at `paths`
/// reports, in order. A line that is a JSON event (the CLI's stream) is read by its string
/// values, line by line, so a tool's output inside an event is read as the tool printed it.
pub fn refusals(paths: &[PathBuf], roots: &[PathBuf]) -> Vec<String> {
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    let mut note = |line: &str| {
        if let Some(annotation) = annotate::refusal(line, roots)
            && seen.insert(annotation.clone())
        {
            found.push(annotation);
        }
    };
    for path in paths {
        let Ok(file) = File::open(path) else {
            continue;
        };
        let mut reader = std::io::BufReader::new(file);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&buffer);
            let line = line.trim_end();
            match serde_json::from_str::<Value>(line) {
                Ok(event) => {
                    let mut texts = Vec::new();
                    strings_of(&event, &mut texts);
                    for text in texts {
                        text.lines().for_each(&mut note);
                    }
                }
                Err(_) => note(line),
            }
        }
    }
    found
}

fn strings_of<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => out.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings_of(item, out)),
        Value::Object(fields) => fields.values().for_each(|field| strings_of(field, out)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
