//! The program runner (feature 20, D9): `runner::program` runs a node's Python script with no
//! model, as `<python> -I -B shim.py <policy.json> <script>` in the node's worktree with stdin
//! closed. It has the shape of the agent runners: `start`, `poll` and `stop`.
//!
//! - Interpreter: `workflows.programs.python`, or `python3` then `python` on `PATH`, at least
//!   Python 3.8 ([`Interpreter::find`]); otherwise nothing starts ("program interpreter missing").
//! - Inputs: the read-only state document (`state.json`, mode 0444, in the try directory) at
//!   `$BONGOS_STATE`; the script writes its typed result to `$BONGOS_RESULT` in the node's scratch
//!   directory. The run record is never in a writable path.
//! - Permissions (`permissions = { commands, network, mcp }`) are enforced twice. First by the OS
//!   sandbox (Bubblewrap on Linux): writes only in an allowlist ([`ProgramNode::writable_paths`]:
//!   the node's worktree, its scratch directory, the common git directory's `objects/`, `refs/`
//!   and `logs/`, and the worktree's own git directory; and the sandbox's own `/tmp`), network
//!   blocked unless declared and then restricted to the declared hosts. When the sandbox cannot be
//!   created the script runs with the second layer alone and the try's `summary.json` says
//!   "not sandboxed: <reason>". Second by the embedded shim's audit hook (`shim.py`), which denies
//!   unlisted programs, writes outside the same allowlist and undeclared connections. A denial
//!   reaches bongos through a pipe (file descriptor 3, kept in `denials.jsonl`), so it fails the
//!   node even when the script caught the `PermissionError`; the shim ends the log with a marker,
//!   and a clean exit without it fails the node too. MCP is refused in v1.
//! - The shim is a guardrail, not a security boundary: code in the same Python process can always
//!   get around an audit hook. The OS sandbox is the boundary, so a run that is "not sandboxed"
//!   is not hard-confined. Known gaps of the shim, accepted because the sandbox holds: a walk
//!   through closures (`__closure__`, `cell_contents`) reaches its state; `os.link` checks only
//!   the destination; binding a unix socket creates a file anywhere; the arguments and
//!   environment of a listed command are not checked (`git -c core.fsmonitor=...` runs).
//! - Git: the common git directory itself is never writable (the sandbox's root is read-only),
//!   so a script cannot change `config`, `hooks/`, `packed-refs`, plant `commondir` or touch
//!   another worktree's git directory; a host git command run while the script runs reads only
//!   what bongos left there. Inside the writable paths the worktree's `.git` file and its git
//!   directory's `gitdir`, `commondir` and `config.worktree` (made empty first when missing) stay
//!   read-only ([`ProgramNode::read_only_paths`]). A commit and a branch ref update work; branch
//!   deletion, `pack-refs`, `git gc --auto` and `git config` writes are refused. In the main
//!   worktree, whose `.git` is the common git directory, the whole git directory is read-only, so
//!   a script there cannot commit. Elsewhere than Linux the sandbox cannot keep a path inside the
//!   writable ones read-only, so only the shim refuses those writes. A run without the sandbox is
//!   checked after every pass instead ([`GitMetadata`]): changed metadata is put back and fails
//!   the node ("program changed git metadata"). This runner calls no host git itself.
//! - Caps: `max_minutes` (the node's, else `workflows.programs.max_minutes`), the captured output
//!   (`workflows.programs.max_output_mb`; the rest is dropped and the process stopped) and the
//!   result size ([`RESULT_CAP`]). Halt and cancel stop the process group within 5 s.
//! - Polling: a result whose field equals the `polls` value is reported (the pass is recorded and
//!   the scheduler does not route it), then the node sleeps with no process and no cost and the
//!   runner reruns it after the interval, until a result other than the value or until the
//!   give-up time passes ("wait timed out").
//!
//! The environment is the agent runners' (`PATH`, `HOME`, `LANG`, the node's `BONGOS_*` names)
//! plus `BONGOS_STATE` and `BONGOS_RESULT`; `TMPDIR` points at `tmp/` in the node's scratch
//! directory, the one temporary directory both layers let it write. A program costs nothing.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bongos_chat::find_program;
use bongos_workflow_model::definition::{Node, OutputField, Permissions, Polls};
use bongos_workflow_model::{Failure, FailureReason, NodeKey, StateDoc, TypedResult, schema};
use sandbox::{
    CommandAndArgs, GitSandboxPolicy, Sandbox, SandboxFsPolicy, SandboxNetPolicy, SandboxPolicy,
};
use serde_json::{Value, json};

use crate::process::{NodeProcess, ProcessRecord, group_alive, stop_all};
use crate::runner_claude::{NodeEnv, STDERR_TAIL, minutes, node_env, read_tail};
use crate::runtime::Finish;

/// The shim every program runs under, embedded and written into each try directory.
pub const SHIM: &str = include_str!("shim.py");

/// The files a try writes in its directory.
pub const SHIM_FILE: &str = "shim.py";
pub const POLICY_FILE: &str = "policy.json";
pub const STATE_FILE: &str = "state.json";
pub const STDOUT_FILE: &str = "stdout.log";
pub const STDERR_FILE: &str = "stderr.log";
pub const DENIALS_FILE: &str = "denials.jsonl";
pub const SUMMARY_FILE: &str = "summary.json";
/// The line the shim writes to the denials pipe when the script ends.
pub const DENIALS_END: &str = r#"{"end": true}"#;
/// The checked result, kept once the try succeeded.
pub const RESULT_FILE: &str = "result.json";

/// The file in the node's scratch directory the script writes its result to (`$BONGOS_RESULT`).
pub const SCRATCH_RESULT_FILE: &str = "result.json";

/// The variables the program runner sets besides the agent runners' `BONGOS_*` names.
pub const PROGRAM_ENV_NAMES: &[&str] = &["BONGOS_STATE", "BONGOS_RESULT"];

/// The largest result a script may write.
pub const RESULT_CAP: u64 = 256 * 1024;

/// The oldest Python the shim runs on.
pub const MIN_PYTHON: (u32, u32) = (3, 8);

/// What to do when no usable interpreter is found.
pub const INTERPRETER_FIX: &str = "install Python 3.8+ or set workflows.programs.python";

/// Why a node with MCP permissions does not start in v1.
pub const MCP_REFUSAL: &str = "MCP from program nodes needs the bongos MCP server (feature 21)";

/// The file descriptor the shim writes denials to.
const DENIALS_FD: i32 = 3;

/// The most of `denials.jsonl` that is kept.
const DENIALS_CAP: u64 = 1024 * 1024;

/// How long a finished try waits for its output to drain after the process group is gone.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

const MB: u64 = 1024 * 1024;

/// The program settings (`workflows.programs.*`, `workflows.poll_interval_minutes` and
/// `workflows.wait_give_up_days`).
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramSettings {
    /// The interpreter; `None` looks up `python3`, then `python`.
    pub python: Option<String>,
    pub max_minutes: u32,
    pub max_output_mb: u64,
    pub poll_interval_minutes: u32,
    pub wait_give_up_days: u32,
}

impl Default for ProgramSettings {
    fn default() -> Self {
        Self {
            python: None,
            max_minutes: 10,
            max_output_mb: 10,
            poll_interval_minutes: 5,
            wait_give_up_days: 7,
        }
    }
}

/// The Python a program runs with.
#[derive(Debug, Clone, PartialEq)]
pub struct Interpreter {
    pub program: PathBuf,
    pub version: (u32, u32),
}

impl Interpreter {
    /// The configured interpreter, or `python3` then `python` on the `PATH` of `env`, when it is
    /// Python 3.8 or newer; otherwise `program interpreter missing` with what to do.
    pub fn find(configured: Option<&str>, env: &BTreeMap<String, String>) -> Result<Self, Failure> {
        let candidates: Vec<&str> = match configured.map(str::trim).filter(|c| !c.is_empty()) {
            Some(configured) => vec![configured],
            None => vec!["python3", "python"],
        };
        let mut too_old = None;
        for candidate in candidates {
            let Some(program) = find_program(candidate, env) else {
                continue;
            };
            match python_version(&program, env) {
                Some(version) if version >= MIN_PYTHON => return Ok(Self { program, version }),
                Some((major, minor)) => {
                    too_old = Some(format!(
                        "{} is Python {major}.{minor}; {INTERPRETER_FIX}",
                        program.display()
                    ));
                }
                None => {}
            }
        }
        Err(Failure::new(
            FailureReason::ProgramInterpreterMissing,
            None,
            too_old.unwrap_or_else(|| INTERPRETER_FIX.to_string()),
        ))
    }
}

/// The `major.minor` version `program` reports, or `None` when it does not run as Python.
#[allow(
    clippy::disallowed_methods,
    reason = "a short probe of the interpreter when a program node starts"
)]
fn python_version(program: &Path, env: &BTreeMap<String, String>) -> Option<(u32, u32)> {
    let output = Command::new(program)
        .args([
            "-I",
            "-c",
            "import sys; sys.stdout.write('%d.%d' % sys.version_info[:2])",
        ])
        .env_clear()
        .envs(
            env.iter()
                .filter(|(name, _)| *name == "PATH" || *name == "LANG"),
        )
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (major, minor) = text.trim().split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// What a program node may spend.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramCaps {
    pub max_minutes: u32,
    /// The runner's timer. `max_minutes` unless a test shortens it.
    pub max_time: Duration,
    /// `workflows.programs.max_output_mb` in bytes: stdout and stderr together.
    pub max_output_bytes: u64,
}

impl ProgramCaps {
    /// The node's `max_minutes`, else the setting; the output cap from the setting.
    pub fn resolve(node: &Node, settings: &ProgramSettings) -> Self {
        let max_minutes = node.settings.max_minutes.unwrap_or(settings.max_minutes);
        Self {
            max_minutes,
            max_time: Duration::from_secs(u64::from(max_minutes) * 60),
            max_output_bytes: settings.max_output_mb.saturating_mul(MB),
        }
    }
}

/// A node's `polls`, with the settings' defaults filled in.
#[derive(Debug, Clone, PartialEq)]
pub struct PollPlan {
    /// The result value that means "not yet".
    pub value: String,
    pub interval: Duration,
    pub give_up: Duration,
    pub interval_minutes: u32,
    pub give_up_days: u32,
}

impl PollPlan {
    pub fn resolve(polls: &Polls, settings: &ProgramSettings) -> Self {
        let interval_minutes = polls
            .interval_minutes
            .unwrap_or(settings.poll_interval_minutes)
            .max(1);
        let give_up_days = polls.give_up_days.unwrap_or(settings.wait_give_up_days);
        Self {
            value: polls.value.clone(),
            interval: Duration::from_secs(u64::from(interval_minutes) * 60),
            give_up: Duration::from_secs(u64::from(give_up_days) * 24 * 60 * 60),
            interval_minutes,
            give_up_days,
        }
    }

    /// Whether `result` says "not yet": a field holds the value (as the scheduler reads it).
    pub fn is_waiting(&self, result: &TypedResult) -> bool {
        result
            .fields
            .iter()
            .any(|field| field.value.as_str() == Some(self.value.as_str()))
    }
}

/// Everything one program try needs, prepared by the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramNode {
    /// The script, resolved to a file.
    pub script: PathBuf,
    pub output: Vec<OutputField>,
    pub permissions: Permissions,
    pub caps: ProgramCaps,
    pub polls: Option<PollPlan>,
    /// The state document; its `paths` are `worktree` and `scratch`.
    pub state: StateDoc,
    /// The node's worktree: the working directory, writable.
    pub worktree: PathBuf,
    /// The node's scratch directory, writable; holds `$BONGOS_RESULT` and `TMPDIR`.
    pub scratch: PathBuf,
    /// The repository's common git directory, writable so the script can commit.
    pub git_common_dir: Option<PathBuf>,
    /// The try's record directory (`nodes/<node>/<branch>/<pass>-<try>/`); never writable.
    pub try_dir: PathBuf,
    pub task_id: String,
    pub task_dir: PathBuf,
    /// `BONGOS_NODE_RESULT`.
    pub node_result: PathBuf,
}

impl ProgramNode {
    /// `$BONGOS_RESULT`.
    pub fn result_path(&self) -> PathBuf {
        self.scratch.join(SCRATCH_RESULT_FILE)
    }

    /// Where the worktree's git metadata lives (see [`GitLayout`]).
    pub fn git_layout(&self) -> GitLayout {
        let gitlink = self.worktree.join(".git");
        let Ok(metadata) = fs::symlink_metadata(&gitlink) else {
            return GitLayout::None;
        };
        if metadata.file_type().is_dir() {
            return GitLayout::Whole(vec![gitlink]);
        }
        let Some(git_dir) = linked_git_dir(&self.worktree) else {
            return GitLayout::Whole(vec![gitlink]);
        };
        let canonical = |path: &Path| fs::canonicalize(path).ok();
        let is_file = |path: &Path| {
            fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
        };
        // A linked git directory is trusted only where git puts it, `<common>/worktrees/<name>`,
        // with its `gitdir` and `commondir`; anything else is read-only as a whole.
        let trusted = self.git_common_dir.as_deref().is_some_and(|common| {
            let parent = git_dir.parent().and_then(canonical);
            parent.is_some() && parent == canonical(&common.join("worktrees"))
        }) && fs::symlink_metadata(&git_dir)
            .is_ok_and(|metadata| metadata.file_type().is_dir())
            && is_file(&git_dir.join("gitdir"))
            && is_file(&git_dir.join("commondir"));
        if trusted {
            GitLayout::Linked { git_dir }
        } else {
            GitLayout::Whole(vec![gitlink, git_dir])
        }
    }

    /// The paths the script may write, an allowlist: the worktree, the scratch directory and, in
    /// a linked worktree, the common git directory's `objects/`, `refs/` and `logs/` and the
    /// worktree's own git directory. The rest of the common git directory (`config`, `hooks/`,
    /// `commondir`, `packed-refs`, other worktrees' git directories) is never writable.
    pub fn writable_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.worktree.clone(), self.scratch.clone()];
        let Some(common) = &self.git_common_dir else {
            return paths;
        };
        let common_writable = COMMON_WRITABLE.iter().map(|name| common.join(name));
        match self.git_layout() {
            GitLayout::Linked { git_dir } => {
                paths.extend(common_writable);
                paths.push(git_dir);
            }
            GitLayout::None => paths.extend(common_writable),
            // The main worktree's git directory is read-only as a whole; a writable `objects/`
            // inside it would be hidden by that bind anyway.
            GitLayout::Whole(_) => {}
        }
        paths
    }

    /// The paths inside the writable ones that stay read-only: the worktree's `.git` file and
    /// its git directory's `gitdir`, `commondir` and `config.worktree`, which point git at a git
    /// directory or configure it; or the whole git directory when it is inside the worktree (the
    /// main worktree) or not where git puts a linked one.
    pub fn read_only_paths(&self) -> Vec<PathBuf> {
        match self.git_layout() {
            GitLayout::None => Vec::new(),
            GitLayout::Linked { git_dir } => {
                let mut paths = vec![self.worktree.join(".git")];
                paths.extend(PER_WORKTREE_FILES.iter().map(|name| git_dir.join(name)));
                paths
            }
            GitLayout::Whole(paths) => paths,
        }
    }

    /// Makes what the sandbox must bind before it starts: `logs/` in the common git directory
    /// (writable) and an empty `config.worktree` in a linked worktree's git directory, since a
    /// missing path cannot be bound read-only and the script could then create it.
    fn prepare_git(&self) {
        let GitLayout::Linked { git_dir } = self.git_layout() else {
            return;
        };
        let mut made = Vec::new();
        if let Some(common) = &self.git_common_dir {
            let logs = common.join("logs");
            if fs::symlink_metadata(&logs).is_err() {
                made.push((logs.clone(), fs::create_dir(&logs)));
            }
        }
        let config = git_dir.join("config.worktree");
        if fs::symlink_metadata(&config).is_err() {
            let created = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&config)
                .map(drop);
            made.push((config, created));
        }
        for (path, result) in made {
            if let Err(error) = result {
                log::error!("workflows: could not create {}: {error}", path.display());
            }
        }
    }

    /// The OS sandbox policy of the node: [`ProgramNode::writable_paths`] writable, and on Linux
    /// [`ProgramNode::read_only_paths`] as protected git directories, which Bubblewrap binds
    /// read-only over the writable ones. Elsewhere a protected path cannot be read either (git
    /// needs them), so there only the allowlist applies.
    pub fn sandbox_policy(&self) -> SandboxPolicy {
        let writable_paths = self.writable_paths();
        let git = if cfg!(target_os = "linux") {
            GitSandboxPolicy::Denied {
                git_dirs: self.read_only_paths(),
            }
        } else {
            GitSandboxPolicy::default()
        };
        SandboxPolicy {
            fs: SandboxFsPolicy::Restricted { writable_paths },
            network: if self.permissions.network.is_empty() {
                SandboxNetPolicy::Blocked
            } else {
                SandboxNetPolicy::Restricted {
                    allowed_domains: self.permissions.network.clone(),
                }
            },
            git,
        }
    }
}

/// The files of a worktree's git directory that point git elsewhere or configure it.
const PER_WORKTREE_FILES: &[&str] = &["gitdir", "commondir", "config.worktree"];

/// The parts of the common git directory a commit or a branch update writes.
const COMMON_WRITABLE: &[&str] = &["objects", "refs", "logs"];

/// Where a worktree's git metadata lives.
#[derive(Debug, Clone, PartialEq)]
pub enum GitLayout {
    /// The worktree has no `.git`.
    None,
    /// A linked worktree whose `.git` file names `<common>/worktrees/<name>`.
    Linked { git_dir: PathBuf },
    /// The git directory is inside the worktree (the main worktree, whose `.git` is the common
    /// git directory) or not where git puts a linked one: these paths stay read-only as a whole,
    /// so a script there cannot commit (git writes `index.lock` and `HEAD.lock` beside them).
    Whole(Vec<PathBuf>),
}

/// The git directory a linked worktree's `.git` file names (`gitdir: <path>`), or `None` when
/// `.git` is not such a file.
fn linked_git_dir(worktree: &Path) -> Option<PathBuf> {
    let gitlink = worktree.join(".git");
    if !fs::symlink_metadata(&gitlink).ok()?.file_type().is_file() {
        return None;
    }
    let text = fs::read_to_string(&gitlink).ok()?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    Some(worktree.join(target))
}

/// What one path of the git metadata held.
#[derive(Debug, Clone, PartialEq)]
enum Snap {
    Missing,
    File(Vec<u8>),
    Symlink(PathBuf),
    /// A directory by identity, with its entries when they are watched too (`hooks/`).
    Dir {
        id: (u64, u64),
        entries: Option<Vec<(std::ffi::OsString, Snap)>>,
    },
    Other,
}

impl Snap {
    fn take(path: &Path, list: bool) -> Self {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return Self::Missing;
        };
        let kind = metadata.file_type();
        if kind.is_symlink() {
            return fs::read_link(path).map_or(Self::Other, Self::Symlink);
        }
        if kind.is_file() {
            return fs::read(path).map_or(Self::Other, Self::File);
        }
        if !kind.is_dir() {
            return Self::Other;
        }
        let entries = list.then(|| {
            let mut entries: Vec<_> = fs::read_dir(path)
                .map(|read| {
                    read.flatten()
                        .map(|entry| (entry.file_name(), Self::take(&entry.path(), false)))
                        .collect()
                })
                .unwrap_or_default();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
        });
        Self::Dir {
            id: dir_id(&metadata),
            entries,
        }
    }

    /// Puts `path` back as it was, as far as it can: a file, a symlink, a missing path or the
    /// entries of a watched directory. A directory replaced by another stays changed. Never
    /// writes through a symlink.
    fn restore(&self, path: &Path) {
        let parent_is_dir = path
            .parent()
            .and_then(|parent| fs::symlink_metadata(parent).ok())
            .is_some_and(|metadata| metadata.file_type().is_dir());
        if !parent_is_dir {
            return;
        }
        let result = match self {
            Self::Missing => remove_any(path),
            Self::File(bytes) => remove_any(path).and_then(|()| {
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                options.open(path)?.write_all(bytes)
            }),
            Self::Symlink(target) => remove_any(path).and_then(|()| symlink(target, path)),
            Self::Dir {
                entries: Some(entries),
                ..
            } => {
                let is_dir =
                    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir());
                let made = if is_dir {
                    Ok(())
                } else {
                    remove_any(path).and_then(|()| fs::create_dir(path))
                };
                made.map(|()| {
                    if let Ok(read) = fs::read_dir(path) {
                        for entry in read.flatten() {
                            let name = entry.file_name();
                            let kept = entries.iter().any(|(kept, _)| *kept == name);
                            if !kept {
                                remove_any(&entry.path()).ok();
                            }
                        }
                    }
                    for (name, snap) in entries {
                        let entry = path.join(name);
                        if Self::take(&entry, false) != *snap {
                            snap.restore(&entry);
                        }
                    }
                })
            }
            Self::Dir { entries: None, .. } | Self::Other => Ok(()),
        };
        if let Err(error) = result {
            log::error!("workflows: could not restore {}: {error}", path.display());
        }
    }
}

#[cfg(unix)]
fn dir_id(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn dir_id(_metadata: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

#[cfg(unix)]
fn symlink(target: &Path, path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Removes whatever is at `path` (a directory with its contents), without following a symlink.
fn remove_any(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    }
}

/// The git metadata a script must leave alone, as it was before the script started: the
/// worktree's `.git`, the common git directory with its `config` and `hooks/`, and the worktree's
/// git directory with its `gitdir`, `commondir` and `config.worktree`. Used only for a run
/// without the OS sandbox (where a listed command writes past the shim): after the pass the
/// runner compares, puts back what it can and fails the node. It is no boundary, since a host
/// git command run while the script runs reads what the script planted; a sandboxed run relies
/// on the sandbox's allowlist instead.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitMetadata {
    entries: Vec<(PathBuf, bool, Snap)>,
}

impl GitMetadata {
    pub fn take(node: &ProgramNode) -> Self {
        let gitlink = node.worktree.join(".git");
        let mut paths = vec![(gitlink.clone(), false)];
        if let Some(common) = &node.git_common_dir {
            paths.push((common.clone(), false));
            paths.push((common.join("config"), false));
            paths.push((common.join("hooks"), true));
        }
        let git_dir = linked_git_dir(&node.worktree).or_else(|| {
            fs::symlink_metadata(&gitlink)
                .is_ok_and(|metadata| metadata.file_type().is_dir())
                .then_some(gitlink)
        });
        if let Some(dir) = git_dir {
            paths.push((dir.clone(), false));
            paths.extend(
                PER_WORKTREE_FILES
                    .iter()
                    .map(|name| (dir.join(name), false)),
            );
        }
        Self {
            entries: paths
                .into_iter()
                .map(|(path, list)| {
                    let snap = Snap::take(&path, list);
                    (path, list, snap)
                })
                .collect(),
        }
    }

    /// The paths that no longer hold what they held.
    pub fn changed(&self) -> Vec<PathBuf> {
        self.entries
            .iter()
            .filter(|(path, list, snap)| Snap::take(path, *list) != *snap)
            .map(|(path, _, _)| path.clone())
            .collect()
    }

    /// Puts back what changed, then returns the paths that had changed (empty when nothing did).
    pub fn check_and_restore(&self) -> Vec<PathBuf> {
        let changed = self.changed();
        for (path, _, snap) in &self.entries {
            if changed.contains(path) {
                snap.restore(path);
            }
        }
        changed
    }
}

/// The failure of a pass that changed git metadata.
fn git_metadata_failure(key: &NodeKey, changed: &[PathBuf]) -> Failure {
    let paths: Vec<String> = changed
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    Failure::new(
        FailureReason::PermissionDenied,
        Some(key.clone()),
        format!("program changed git metadata: {}", paths.join(", ")),
    )
}

/// The environment of a program: the agent runners' names, `BONGOS_STATE`, `BONGOS_RESULT` and
/// `TMPDIR` in the scratch directory.
pub fn launch_env(
    base: &BTreeMap<String, String>,
    run: &str,
    key: &NodeKey,
    node: &ProgramNode,
    state_file: &Path,
) -> BTreeMap<String, String> {
    let mut env = node_env(
        base,
        run,
        key,
        &NodeEnv {
            task_id: &node.task_id,
            task_dir: &node.task_dir,
            worktree: &node.worktree,
            node_result: &node.node_result,
        },
    );
    let path = |path: &Path| path.to_string_lossy().into_owned();
    env.insert("BONGOS_STATE".to_string(), path(state_file));
    env.insert("BONGOS_RESULT".to_string(), path(&node.result_path()));
    env.insert("TMPDIR".to_string(), path(&node.scratch.join("tmp")));
    env
}

/// The runner's clock; tests advance their own.
pub trait Clock {
    fn now(&self) -> Instant;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Whether the OS sandbox can be created for a policy in a working directory, or why not.
pub type SandboxProbe = Arc<dyn Fn(&SandboxPolicy, &Path) -> Result<(), String>>;

/// The real probe ([`Sandbox::can_create`]; on Linux a brief `bwrap` run).
pub fn os_sandbox_probe() -> SandboxProbe {
    Arc::new(|policy, cwd| {
        Sandbox::can_create(policy, Some(cwd)).map_err(|error| error.to_string())
    })
}

/// How the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed by a signal (the output cap stops it this way), or its status could not be read.
    Abnormal,
    /// The runner's timer stopped it.
    TimedOut,
}

/// What `$BONGOS_RESULT` held after the script exited.
#[derive(Debug, Clone, PartialEq)]
pub enum ResultFile {
    Missing,
    /// Not a regular file (a FIFO, a device, a directory or a symlink).
    NotRegular,
    /// Over [`RESULT_CAP`]; the size.
    TooLarge(u64),
    Text(String),
}

impl ResultFile {
    /// Reads the result without following a symlink or blocking on a FIFO or device.
    pub fn read(path: &Path) -> Self {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => return Self::NotRegular,
            Err(_) => return Self::Missing,
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let Ok(file) = options.open(path) else {
            return Self::NotRegular;
        };
        // The path may have been swapped between the two looks.
        let Ok(metadata) = file.metadata() else {
            return Self::Missing;
        };
        if !metadata.file_type().is_file() {
            return Self::NotRegular;
        }
        let size = metadata.len();
        if size > RESULT_CAP {
            return Self::TooLarge(size);
        }
        let mut bytes = Vec::new();
        if file.take(RESULT_CAP + 1).read_to_end(&mut bytes).is_err() {
            return Self::Missing;
        }
        if bytes.len() as u64 > RESULT_CAP {
            return Self::TooLarge(bytes.len() as u64);
        }
        Self::Text(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn size_text(bytes: u64) -> String {
    if bytes >= MB && bytes.is_multiple_of(MB) {
        format!("{} MB", bytes / MB)
    } else {
        format!("{bytes} bytes")
    }
}

/// The outcome of a finished try. Pure. In order: the time cap, the output cap, the first denial,
/// a nonzero exit, a denial log without its end marker, then the result (missing, not a regular
/// file, too large, not JSON or not matching the output).
pub fn classify(
    key: &NodeKey,
    node: &ProgramNode,
    exit: Exit,
    output_capped: bool,
    denials: &DenialLog,
    result: ResultFile,
    stderr: &str,
) -> Result<TypedResult, Failure> {
    let fail = |reason: FailureReason, detail: String| {
        Err(Failure::new(reason, Some(key.clone()), detail))
    };
    if exit == Exit::TimedOut {
        return fail(
            FailureReason::TimeCapHit,
            format!(
                "stopped at the time cap of {} (max_minutes)",
                minutes(node.caps.max_time)
            ),
        );
    }
    if output_capped {
        return fail(
            FailureReason::OutputCapHit,
            format!(
                "the output passed {} (workflows.programs.max_output_mb); the rest was dropped",
                size_text(node.caps.max_output_bytes)
            ),
        );
    }
    if let Some(what) = denials.denials.first() {
        return fail(FailureReason::PermissionDenied, what.clone());
    }
    if exit != Exit::Code(0) {
        let status = match exit {
            Exit::Code(code) => format!("exit code {code}"),
            Exit::Abnormal | Exit::TimedOut => "killed".to_string(),
        };
        let detail = match stderr.lines().rev().find(|line| !line.trim().is_empty()) {
            Some(line) => format!("{status}: {}", line.trim()),
            None => status,
        };
        return fail(FailureReason::ProgramExitedNonzero, detail);
    }
    if !denials.ended && cfg!(unix) {
        return fail(
            FailureReason::ProgramResultInvalid,
            "the shim's denial log ended early (its pipe was closed, or the script left with \
             os._exit), so the run cannot be trusted"
                .to_string(),
        );
    }
    let text = match result {
        ResultFile::Missing => {
            return fail(
                FailureReason::ProgramResultInvalid,
                "the script wrote no result to $BONGOS_RESULT".to_string(),
            );
        }
        ResultFile::NotRegular => {
            return fail(
                FailureReason::ProgramResultInvalid,
                "$BONGOS_RESULT is not a regular file".to_string(),
            );
        }
        ResultFile::TooLarge(size) => {
            return fail(
                FailureReason::ProgramResultInvalid,
                format!(
                    "the result is {}, over the cap of {}",
                    size_text(size),
                    size_text(RESULT_CAP)
                ),
            );
        }
        ResultFile::Text(text) => text,
    };
    let value = match serde_json::from_str::<Value>(text.trim()) {
        Ok(value) => value,
        Err(error) => {
            return fail(
                FailureReason::ProgramResultInvalid,
                format!("the result is not JSON: {error}"),
            );
        }
    };
    schema::check(&node.output, &value)
        .or_else(|problems| fail(FailureReason::ProgramResultInvalid, problems))
}

/// What the shim wrote to the denials pipe.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DenialLog {
    /// One `what` per denial.
    pub denials: Vec<String>,
    /// Whether the shim wrote [`DENIALS_END`] after the script.
    pub ended: bool,
}

impl DenialLog {
    /// A complete log with these denials.
    pub fn ended(denials: Vec<String>) -> Self {
        Self {
            denials,
            ended: true,
        }
    }
}

/// The denials the shim reported, one `what` per line of `denials.jsonl`, and whether the log
/// has its end marker.
pub fn read_denials(path: &Path) -> DenialLog {
    let Ok(text) = fs::read_to_string(path) else {
        return DenialLog::default();
    };
    let ended = text.lines().any(|line| line.trim() == DENIALS_END);
    let denials = text
        .lines()
        .filter(|line| !line.trim().is_empty() && line.trim() != DENIALS_END)
        .map(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| {
                    value
                        .get("what")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| line.trim().to_string())
        })
        .collect();
    DenialLog { denials, ended }
}

/// The output of one process: bytes seen and whether the cap dropped some.
#[derive(Default)]
struct Capture {
    bytes: AtomicU64,
    capped: AtomicBool,
}

/// Copies `from` into `path`, keeping at most `cap` bytes across every stream sharing `capture`;
/// the rest is read and dropped so the process never blocks on a full pipe.
fn copy_capped(
    mut from: impl Read + Send + 'static,
    path: PathBuf,
    capture: Arc<Capture>,
    cap: u64,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut file = File::create(&path).ok();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = match from.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let before = capture.bytes.fetch_add(read as u64, Ordering::SeqCst);
            let keep = cap.saturating_sub(before).min(read as u64) as usize;
            if keep < read {
                capture.capped.store(true, Ordering::SeqCst);
            }
            if keep > 0
                && let Some(file) = file.as_mut()
                && file.write_all(&buffer[..keep]).is_err()
            {
                log::error!("workflows: could not write {}", path.display());
            }
        }
    })
}

/// What to spawn for one pass, and the sandbox it runs in.
struct Launch {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    sandbox: Option<Sandbox>,
    sandbox_note: String,
}

/// One pass of a program node in flight.
struct Pass {
    run: String,
    key: NodeKey,
    try_number: u32,
    node: ProgramNode,
    /// This pass's record directory: the try directory, then `polls/<n>/` inside it.
    dir: PathBuf,
    /// 1 for the first run, then one more per poll rerun.
    number: u32,
    /// When the first `waiting` result came, for the give-up time.
    first_waiting: Option<Instant>,
    started: Instant,
    started_wall: Instant,
    capture: Arc<Capture>,
    readers: Vec<JoinHandle<()>>,
    sandbox_note: String,
    /// Kept alive while the process runs: it owns the network proxy.
    sandbox: Option<Sandbox>,
    /// The git metadata before an unsandboxed script started, checked after it ends.
    git: Option<GitMetadata>,
}

/// A polling node between passes: no process, no cost.
struct Sleeper {
    run: String,
    key: NodeKey,
    try_number: u32,
    node: ProgramNode,
    number: u32,
    first_waiting: Instant,
    due: Instant,
}

/// Runs program node tries.
pub struct ProgramRunner {
    interpreter: Interpreter,
    base_env: BTreeMap<String, String>,
    clock: Arc<dyn Clock>,
    probe: SandboxProbe,
    passes: Vec<(Pass, NodeProcess)>,
    sleepers: Vec<Sleeper>,
}

impl ProgramRunner {
    /// A runner with the system clock and the real sandbox probe.
    pub fn new(interpreter: Interpreter, base_env: BTreeMap<String, String>) -> Self {
        Self {
            interpreter,
            base_env,
            clock: Arc::new(SystemClock),
            probe: os_sandbox_probe(),
            passes: Vec::new(),
            sleepers: Vec::new(),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_sandbox_probe(mut self, probe: SandboxProbe) -> Self {
        self.probe = probe;
        self
    }

    pub fn interpreter(&self) -> &Interpreter {
        &self.interpreter
    }

    /// Starts try `try_number` of `key`. Refuses MCP permissions (v1).
    pub fn start(
        &mut self,
        run: &str,
        key: &NodeKey,
        try_number: u32,
        node: ProgramNode,
    ) -> Result<ProcessRecord, Failure> {
        if !node.permissions.mcp.is_empty() {
            return Err(Failure::new(
                FailureReason::PermissionDenied,
                Some(key.clone()),
                MCP_REFUSAL,
            ));
        }
        let dir = node.try_dir.clone();
        self.launch(run, key, try_number, node, dir, 1, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn launch(
        &mut self,
        run: &str,
        key: &NodeKey,
        try_number: u32,
        node: ProgramNode,
        dir: PathBuf,
        number: u32,
        first_waiting: Option<Instant>,
    ) -> Result<ProcessRecord, Failure> {
        let refuse =
            |reason: FailureReason, detail: String| Failure::new(reason, Some(key.clone()), detail);
        let io = |path: &Path, error: std::io::Error| {
            refuse(
                FailureReason::WorktreeUnavailable,
                format!("{}: {error}", path.display()),
            )
        };
        if !node.worktree.is_dir() {
            return Err(refuse(
                FailureReason::WorktreeUnavailable,
                format!("{} is missing", node.worktree.display()),
            ));
        }
        fs::create_dir_all(&dir).map_err(|error| io(&dir, error))?;
        let tmp = node.scratch.join("tmp");
        fs::create_dir_all(&tmp).map_err(|error| io(&tmp, error))?;
        let result_path = node.result_path();
        match fs::remove_file(&result_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(&result_path, error)),
        }
        node.prepare_git();
        let shim = dir.join(SHIM_FILE);
        fs::write(&shim, SHIM).map_err(|error| io(&shim, error))?;
        let state_file = dir.join(STATE_FILE);
        node.state
            .write_readonly(&state_file)
            .map_err(|error| io(&state_file, error))?;
        let policy_file = dir.join(POLICY_FILE);
        let policy = json!({
            "commands": node.permissions.commands,
            "network": node.permissions.network,
            "writable": node.writable_paths(),
            "protected": node.read_only_paths(),
            "denials_fd": DENIALS_FD,
        });
        fs::write(
            &policy_file,
            serde_json::to_string_pretty(&policy).unwrap_or_default(),
        )
        .map_err(|error| io(&policy_file, error))?;

        let path = |path: &Path| path.to_string_lossy().into_owned();
        let args = vec![
            "-I".to_string(),
            "-B".to_string(),
            path(&shim),
            path(&policy_file),
            path(&node.script),
        ];
        let env = launch_env(&self.base_env, run, key, &node, &state_file);
        let Launch {
            program,
            args,
            env,
            sandbox,
            sandbox_note,
        } = self.wrap(&node, path(&self.interpreter.program), args, env);
        let git = sandbox.is_none().then(|| GitMetadata::take(&node));
        write_summary(&dir, &json!({ "pass": number, "sandbox": sandbox_note }));

        let (stdout_read, stdout_write) = std::io::pipe().map_err(|error| io(&dir, error))?;
        let (stderr_read, stderr_write) = std::io::pipe().map_err(|error| io(&dir, error))?;
        let (denials_read, denials_write) = std::io::pipe().map_err(|error| io(&dir, error))?;
        let mut command = Command::new(&program);
        command
            .args(&args)
            .current_dir(&node.worktree)
            .env_clear()
            .envs(&env);
        pass_denials_fd(&mut command, &denials_write);
        let spawned = NodeProcess::spawn(
            command,
            Stdio::null(),
            Stdio::from(stdout_write),
            Stdio::from(stderr_write),
        );
        drop(denials_write);
        let process = spawned.map_err(|error| {
            let not_found = error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .any(|error| error.kind() == std::io::ErrorKind::NotFound);
            if not_found {
                refuse(
                    FailureReason::ProgramInterpreterMissing,
                    format!("{program}: {INTERPRETER_FIX}"),
                )
            } else {
                refuse(FailureReason::ProgramExitedNonzero, format!("{error:#}"))
            }
        })?;

        let capture = Arc::new(Capture::default());
        let cap = node.caps.max_output_bytes;
        let readers = vec![
            copy_capped(stdout_read, dir.join(STDOUT_FILE), capture.clone(), cap),
            copy_capped(stderr_read, dir.join(STDERR_FILE), capture.clone(), cap),
            copy_capped(
                denials_read,
                dir.join(DENIALS_FILE),
                Arc::new(Capture::default()),
                DENIALS_CAP,
            ),
        ];
        let record = process.record();
        self.passes.push((
            Pass {
                run: run.to_string(),
                key: key.clone(),
                try_number,
                node,
                dir,
                number,
                first_waiting,
                started: self.clock.now(),
                started_wall: Instant::now(),
                capture,
                readers,
                sandbox_note,
                sandbox,
                git,
            },
            process,
        ));
        Ok(record)
    }

    /// The command to spawn: inside the OS sandbox when it can be created, else as it is with
    /// the reason it is not sandboxed.
    fn wrap(
        &self,
        node: &ProgramNode,
        program: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Launch {
        let policy = node.sandbox_policy();
        let wrapped = (self.probe)(&policy, &node.worktree).and_then(|()| {
            let mut sandbox = Sandbox::new(policy).map_err(|error| error.to_string())?;
            let command = CommandAndArgs {
                program: program.clone(),
                args: args.clone(),
                env: env.clone().into_iter().collect::<HashMap<_, _>>(),
                cwd: Some(node.worktree.clone()),
            };
            let wrapped = futures::executor::block_on(sandbox.wrap(&command))
                .map_err(|error| error.to_string())?;
            Ok((wrapped, sandbox))
        });
        match wrapped {
            Ok((wrapped, sandbox)) => Launch {
                program: wrapped.program,
                args: wrapped.args,
                env: wrapped.env.into_iter().collect(),
                sandbox: Some(sandbox),
                sandbox_note: "sandboxed".to_string(),
            },
            Err(reason) => {
                log::warn!("workflows: a program runs without the OS sandbox: {reason}");
                Launch {
                    program,
                    args,
                    env,
                    sandbox: None,
                    sandbox_note: format!("not sandboxed: {reason}"),
                }
            }
        }
    }

    /// The passes that ended since the last call (a `waiting` result included: it is recorded,
    /// then the node sleeps), and the sleeping nodes that gave up. Reruns the sleeping nodes that
    /// are due.
    pub fn poll(&mut self) -> Vec<Finish> {
        let now = self.clock.now();
        let mut ended: Vec<(Pass, NodeProcess, Exit)> = Vec::new();
        let mut index = 0;
        while index < self.passes.len() {
            let (pass, process) = &mut self.passes[index];
            let exited = process.has_exited();
            let timed_out = now.saturating_duration_since(pass.started) >= pass.node.caps.max_time;
            let capped = pass.capture.capped.load(Ordering::SeqCst);
            if !exited && !timed_out && !capped {
                index += 1;
                continue;
            }
            let (pass, mut process) = self.passes.remove(index);
            let exit = if exited {
                match process.child().try_status() {
                    Ok(Some(status)) => status.code().map_or(Exit::Abnormal, Exit::Code),
                    _ => Exit::Abnormal,
                }
            } else if timed_out {
                Exit::TimedOut
            } else {
                Exit::Abnormal
            };
            ended.push((pass, process, exit));
        }
        // One shared stop for the passes the runner ended and for exited ones whose group still
        // has a live member; a group with no live member is left alone (its id may be reused).
        let mut stopping: Vec<NodeProcess> = Vec::new();
        let mut done: Vec<(Pass, Exit)> = Vec::new();
        for (pass, process, exit) in ended {
            if !matches!(exit, Exit::Code(_)) || group_alive(process.record().pid) {
                stopping.push(process);
            }
            done.push((pass, exit));
        }
        stop_all(&mut stopping);
        let mut finishes: Vec<Finish> = done
            .into_iter()
            .map(|(pass, exit)| self.finish(pass, exit, now))
            .collect();

        let (due, sleeping): (Vec<_>, Vec<_>) = std::mem::take(&mut self.sleepers)
            .into_iter()
            .partition(|sleeper| now >= sleeper.due);
        self.sleepers = sleeping;
        for sleeper in due {
            let polls = sleeper.node.polls.clone().expect("a sleeper polls");
            if now.saturating_duration_since(sleeper.first_waiting) >= polls.give_up {
                finishes.push(gave_up(
                    sleeper.run,
                    sleeper.key,
                    sleeper.try_number,
                    &polls,
                ));
                continue;
            }
            let number = sleeper.number + 1;
            let dir = sleeper.node.try_dir.join("polls").join(number.to_string());
            if let Err(failure) = self.launch(
                &sleeper.run,
                &sleeper.key,
                sleeper.try_number,
                sleeper.node,
                dir,
                number,
                Some(sleeper.first_waiting),
            ) {
                finishes.push(Finish {
                    outcome: Err(failure),
                    ..Finish::ok(sleeper.run, sleeper.key, sleeper.try_number, Value::Null)
                });
            }
        }
        finishes
    }

    /// Stops these tries, running or sleeping; they are never reported by [`ProgramRunner::poll`].
    pub fn stop(&mut self, tries: &[(String, NodeKey)]) {
        let matches = |run: &str, key: &NodeKey| {
            tries
                .iter()
                .any(|(stopped_run, stopped_key)| stopped_run == run && stopped_key == key)
        };
        self.sleepers
            .retain(|sleeper| !matches(&sleeper.run, &sleeper.key));
        let (stopped, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.passes)
            .into_iter()
            .partition(|(pass, _)| matches(&pass.run, &pass.key));
        self.passes = kept;
        let mut processes: Vec<NodeProcess> = Vec::new();
        let mut sandboxes = Vec::new();
        let mut gits = Vec::new();
        for (pass, process) in stopped {
            processes.push(process);
            sandboxes.push(pass.sandbox);
            gits.push(pass.git);
        }
        stop_all(&mut processes);
        drop(sandboxes);
        for git in gits.into_iter().flatten() {
            let changed = git.check_and_restore();
            if !changed.is_empty() {
                log::error!(
                    "workflows: a stopped program changed git metadata (put back): {changed:?}"
                );
            }
        }
    }

    /// The tries with a process, as `(run, key)`.
    pub fn in_flight(&self) -> Vec<(String, NodeKey)> {
        self.passes
            .iter()
            .map(|(pass, _)| (pass.run.clone(), pass.key.clone()))
            .collect()
    }

    /// The polling tries asleep between passes, as `(run, key)`.
    pub fn sleeping(&self) -> Vec<(String, NodeKey)> {
        self.sleepers
            .iter()
            .map(|sleeper| (sleeper.run.clone(), sleeper.key.clone()))
            .collect()
    }

    fn finish(&mut self, mut pass: Pass, exit: Exit, now: Instant) -> Finish {
        let deadline = Instant::now() + DRAIN_WAIT;
        for reader in pass.readers.drain(..) {
            while !reader.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if reader.is_finished() {
                reader.join().ok();
            }
        }
        drop(pass.sandbox.take());
        let capped = pass.capture.capped.load(Ordering::SeqCst);
        let denials = read_denials(&pass.dir.join(DENIALS_FILE));
        let git_changed = pass
            .git
            .as_ref()
            .map(GitMetadata::check_and_restore)
            .unwrap_or_default();
        let classified = classify(
            &pass.key,
            &pass.node,
            exit,
            capped,
            &denials,
            ResultFile::read(&pass.node.result_path()),
            &read_tail(&pass.dir.join(STDERR_FILE), STDERR_TAIL),
        );
        let classified = if git_changed.is_empty() {
            classified
        } else {
            Err(git_metadata_failure(&pass.key, &git_changed))
        };
        write_summary(
            &pass.dir,
            &json!({
                "pass": pass.number,
                "sandbox": pass.sandbox_note,
                "exit": match exit {
                    Exit::Code(code) => json!(code),
                    Exit::Abnormal => json!("killed"),
                    Exit::TimedOut => json!("timed out"),
                },
                "output_bytes": pass.capture.bytes.load(Ordering::SeqCst),
                "output_capped": capped,
                "denials": denials.denials,
                "denial_log_ended": denials.ended,
                "git_metadata_changed": git_changed,
            }),
        );
        let elapsed_ms = pass.started_wall.elapsed().as_millis() as u64;
        let finish = |outcome: Result<Value, Failure>| Finish {
            outcome,
            elapsed_ms,
            ..Finish::ok(
                pass.run.clone(),
                pass.key.clone(),
                pass.try_number,
                Value::Null,
            )
        };
        let typed = match classified {
            Ok(typed) => typed,
            Err(failure) => return finish(Err(failure)),
        };
        let result = typed.to_json();
        let path = pass.dir.join(RESULT_FILE);
        if let Err(error) = fs::write(
            &path,
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        ) {
            log::error!("workflows: could not write {}: {error}", path.display());
        }
        if let Some(polls) = pass.node.polls.clone()
            && polls.is_waiting(&typed)
        {
            let first_waiting = pass.first_waiting.unwrap_or(now);
            if now.saturating_duration_since(first_waiting) >= polls.give_up {
                return gave_up(pass.run, pass.key, pass.try_number, &polls);
            }
            let reported = finish(Ok(result));
            self.sleepers.push(Sleeper {
                run: pass.run,
                key: pass.key,
                try_number: pass.try_number,
                node: pass.node,
                number: pass.number,
                first_waiting,
                due: now + polls.interval,
            });
            return reported;
        }
        finish(Ok(result))
    }
}

fn gave_up(run: String, key: NodeKey, try_number: u32, polls: &PollPlan) -> Finish {
    let failure = Failure::new(
        FailureReason::WaitTimedOut,
        Some(key.clone()),
        format!(
            "still {} after {} days (give_up_days)",
            polls.value, polls.give_up_days
        ),
    );
    Finish {
        outcome: Err(failure),
        ..Finish::ok(run, key, try_number, Value::Null)
    }
}

fn write_summary(dir: &Path, summary: &Value) {
    let path = dir.join(SUMMARY_FILE);
    if let Err(error) = fs::write(
        &path,
        serde_json::to_string_pretty(summary).unwrap_or_default(),
    ) {
        log::error!("workflows: could not write {}: {error}", path.display());
    }
}

/// Hands the write end of the denials pipe to the process as [`DENIALS_FD`].
#[cfg(unix)]
fn pass_denials_fd(command: &mut Command, pipe: &std::io::PipeWriter) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let fd = pipe.as_raw_fd();
    // SAFETY: only async-signal-safe calls (dup2, fcntl) run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if fd == DENIALS_FD {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(fd, DENIALS_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn pass_denials_fd(_command: &mut Command, _pipe: &std::io::PipeWriter) {}
