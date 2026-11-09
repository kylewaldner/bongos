//! One chat launch up to the terminal (design D10 and D11): what a launch is asked
//! ([`LaunchRequest`]), why it can be refused ([`LaunchError`]), the rendering of the chat's
//! runtime directory and the command line, the writer of that directory (`0700` directories,
//! `0600` files, regenerated on every launch, never copied), and the `SpawnInTerminal` that runs
//! the CLI.
//!
//! Everything the files and the environment hold comes from the renderer of
//! `bongos_chat_model`, whose only source of artifact paths is the request's `SessionContext`
//! (the task's shared artifacts). Nothing here lists a task directory.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bongos_chat_model::{
    Agent, Capabilities, ChatRecord, Fallback, GeneratedFiles, HandoffParent, LaunchKind,
    LaunchPlan, LaunchSpec, RenderInput, ResolvedType, TaskFacts, adapter_for, context_digest,
    render_codex_instructions, render_context, render_env, render_settings, render_system,
};
use bongos_work_model::{SessionContext, SharedArtifact};
use gpui::{App, Global};
use task::{HideStrategy, RevealStrategy, SpawnInTerminal, TaskId};

use crate::{AgentCommand, ChatWriteError};

/// The file the hook prints and the env names: the scope pointer and the shared artifacts.
pub const CONTEXT_FILE: &str = "context.md";
/// Claude's appended system prompt.
pub const SYSTEM_FILE: &str = "system.md";
/// Codex's instructions file.
pub const CODEX_INSTRUCTIONS_FILE: &str = "codex-instructions.md";
/// Claude's `--settings` file: the `SessionStart` hook and the bell.
pub const CLAUDE_SETTINGS_FILE: &str = "claude-settings.json";
/// Claude's plugin directory, holding the type's skills.
pub const PLUGIN_DIR: &str = "plugin";
/// The name of the plugin bongos gives Claude.
const PLUGIN_NAME: &str = "bongos";

/// Everything one launch needs. The caller builds it from the record, the Work snapshot and the
/// resolved session type; the runtime reads nothing else from a task.
#[derive(Clone, Debug)]
pub struct LaunchRequest {
    /// The chat. Its `cwd` is where the CLI runs.
    pub record: ChatRecord,
    /// The record file, which must exist: a launch writes it before the CLI starts.
    pub record_path: PathBuf,
    /// The task or project directory the record lives in.
    pub scope_dir: PathBuf,
    pub task_id: Option<String>,
    pub kind: LaunchKind,
    /// What the task shares with chats, read at launch time (`session_context`).
    pub context: SessionContext,
    pub facts: TaskFacts,
    pub resolved: ResolvedType,
    /// Set for a handoff fork.
    pub handoff: Option<HandoffParent>,
    /// The CLI's capabilities; `None` takes them from the global `ChatHealth`.
    pub caps: Option<Capabilities>,
    /// Fallbacks already known to be needed (see `LaunchPlan::force`).
    pub force: Vec<Fallback>,
    /// A section the context extension (see [`set_context_extension`]) adds at the end of the
    /// context; `None` without an extension.
    pub extra_context: Option<String>,
}

/// Adds a section to the end of the context of a chat, after the shared artifacts. It never sees
/// the artifacts: hidden ones stay out of the context whatever an extension writes.
pub trait ContextExtension {
    /// The section for a `chat_type` chat on `task_id` (`None` for a project chat), or `None` for
    /// no section.
    fn section(&self, chat_type: &str, task_id: Option<&str>, cx: &App) -> Option<String>;
}

struct GlobalContextExtension(Rc<dyn ContextExtension>);

impl Global for GlobalContextExtension {}

/// Installs the context extension. Replaces an earlier one.
pub fn set_context_extension(ext: impl ContextExtension + 'static, cx: &mut App) {
    cx.set_global(GlobalContextExtension(Rc::new(ext)));
}

/// The installed extension's section for a launch, `None` without an extension.
pub(crate) fn extension_section(
    chat_type: &str,
    task_id: Option<&str>,
    cx: &App,
) -> Option<String> {
    let ext = cx.try_global::<GlobalContextExtension>()?.0.clone();
    ext.section(chat_type, task_id, cx)
}

/// Why a launch did not start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchError {
    /// `chats.enabled` is false.
    Off,
    /// No open project is local.
    NoLocalProject,
    /// The chat is already running (or starting).
    AlreadyRunning,
    /// The record names an agent this version does not know.
    UnknownAgent(String),
    /// The chat id cannot name a directory.
    InvalidId(String),
    /// The CLI was not found; the text says what to do.
    NotFound(String),
    /// The CLI's capabilities are not known yet; the text says what to do.
    NotProbed(String),
    /// Rendering or writing the runtime directory failed.
    Files(String),
    /// Writing the record failed.
    Record(ChatWriteError),
    /// The terminal did not start.
    Spawn(String),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaunchError::Off => write!(f, "Chats are off (chats.enabled)"),
            LaunchError::NoLocalProject => {
                write!(f, "Chats run locally; open a local project first")
            }
            LaunchError::AlreadyRunning => write!(f, "This chat is already running"),
            LaunchError::UnknownAgent(agent) => write!(f, "Unknown agent `{agent}`"),
            LaunchError::InvalidId(id) => write!(f, "`{id}` is not a usable chat id"),
            LaunchError::NotFound(fix) | LaunchError::NotProbed(fix) => write!(f, "{fix}"),
            LaunchError::Files(reason) => {
                write!(f, "Could not write the chat's launch files: {reason}")
            }
            LaunchError::Record(error) => write!(f, "{error}"),
            LaunchError::Spawn(reason) => write!(f, "Could not start the chat: {reason}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// A launch rendered in memory: the files to write and the command line to run.
pub(crate) struct Prepared {
    pub agent: Agent,
    /// The request's record with the session id the launch gives it.
    pub record: ChatRecord,
    /// `<runtime>/<chat id>`.
    pub dir: PathBuf,
    /// Paths relative to `dir`, with their text.
    pub files: Vec<(PathBuf, String)>,
    pub spec: LaunchSpec,
    /// The shared artifacts the chat is given; kept in memory, never written.
    pub given: Vec<SharedArtifact>,
    pub digest: String,
    /// The skills the session type lists that were not found; the row says so.
    pub missing_skills: Vec<String>,
}

/// Whether `id` is one plain path component.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

/// Renders the launch of `request` under `runtime_root` (the chats runtime directory) for an agent
/// run as `command` with `caps`. `home` is where `~/.bongos` is.
pub(crate) fn prepare(
    request: &LaunchRequest,
    runtime_root: &Path,
    home: &Path,
    command: &AgentCommand,
    caps: &Capabilities,
) -> Result<Prepared, LaunchError> {
    let agent = request
        .record
        .known_agent()
        .ok_or_else(|| LaunchError::UnknownAgent(request.record.agent.clone()))?;
    if !valid_id(&request.record.id) {
        return Err(LaunchError::InvalidId(request.record.id.clone()));
    }
    let adapter = adapter_for(agent, &command.command);
    let shape = adapter.shape(caps);

    let mut record = request.record.clone();
    // Claude takes the id of a new session on the command line, so it is chosen here and known
    // before the CLI starts: for a new chat and for a fork, which is a conversation of its own
    // (the source's id is in the launch kind). A resume keeps the id it resumes, and one with no
    // stored id never makes up another.
    if agent == Agent::Claude && request.kind != LaunchKind::Resume && record.session_id.is_none() {
        record.session_id = Some(uuid::Uuid::new_v4().to_string());
    }

    let dir = runtime_root.join(&record.id);
    let context_path = dir.join(CONTEXT_FILE);
    let bongos_root = home.join(".bongos");
    let skills_as_plugin =
        agent == Agent::Claude && shape.skills_as_plugin && !request.resolved.skills.is_empty();
    let input = RenderInput {
        context: &request.context,
        facts: &request.facts,
        record: &record,
        resolved: &request.resolved,
        scope_dir: &request.scope_dir,
        task_id: request.task_id.as_deref(),
        record_path: &request.record_path,
        context_path: &context_path,
        bongos_root: Some(&bongos_root),
        skills_as_plugin,
        handoff: request.handoff.as_ref(),
        extra_context: request.extra_context.as_deref(),
    };

    let context_text = render_context(&input);
    let mut files = vec![(PathBuf::from(CONTEXT_FILE), context_text.clone())];
    let mut settings_path = None;
    let mut plugin_dir = None;
    let (system_name, system_text) = match agent {
        Agent::Claude => {
            let mut text = render_system(&input);
            if shape.context_in_system {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&context_text);
            } else {
                let settings = render_settings(&context_path)
                    .map_err(|error| LaunchError::Files(error.to_string()))?;
                files.push((PathBuf::from(CLAUDE_SETTINGS_FILE), settings));
                settings_path = Some(dir.join(CLAUDE_SETTINGS_FILE));
            }
            if skills_as_plugin {
                files.extend(plugin_files(&request.resolved));
                plugin_dir = Some(dir.join(PLUGIN_DIR));
            }
            (SYSTEM_FILE, text)
        }
        Agent::Codex => (CODEX_INSTRUCTIONS_FILE, render_codex_instructions(&input)),
    };
    files.push((PathBuf::from(system_name), system_text.clone()));

    let env = render_env(&input);
    let mut extra_args = command.extra_args.clone();
    extra_args.extend(request.resolved.overrides.extra_args.iter().cloned());
    let plan = LaunchPlan {
        kind: request.kind.clone(),
        chat: record.clone(),
        files: GeneratedFiles {
            system_path: dir.join(system_name),
            system_text,
            settings_path,
            plugin_dir,
            has_skills: !request.resolved.skills.is_empty(),
        },
        model: record
            .model
            .clone()
            .or_else(|| request.resolved.overrides.model.clone()),
        extra_args,
        env,
        force: request.force.clone(),
    };
    let spec = adapter.launch(&plan, caps);
    let given = request.context.artifacts.clone();
    let digest = context_digest(&given);
    Ok(Prepared {
        agent,
        record,
        dir,
        files,
        spec,
        given,
        digest,
        missing_skills: request.resolved.missing_skills.clone(),
    })
}

/// The plugin Claude loads with `--plugin-dir`: a manifest and one `SKILL.md` per skill.
fn plugin_files(resolved: &ResolvedType) -> Vec<(PathBuf, String)> {
    let plugin = PathBuf::from(PLUGIN_DIR);
    let manifest = format!(
        "{{\n  \"name\": \"{PLUGIN_NAME}\",\n  \"version\": \"1.0.0\",\n  \"description\": \
         \"The skills bongos gives this chat\"\n}}\n"
    );
    let mut files = vec![(plugin.join(".claude-plugin").join("plugin.json"), manifest)];
    for skill in &resolved.skills {
        files.push((
            plugin.join("skills").join(&skill.name).join("SKILL.md"),
            skill.text.clone(),
        ));
    }
    files
}

/// Writes `files` into `dir`, a directory under `runtime_root`, replacing whatever an earlier
/// launch left there. `runtime_root`, `dir` and every directory below it are `0700`; the files
/// are `0600`.
pub(crate) fn write_runtime_dir(
    runtime_root: &Path,
    dir: &Path,
    files: &[(PathBuf, String)],
) -> io::Result<()> {
    ensure_private_root(runtime_root)?;
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    create_private_dir(dir)?;
    for (relative, text) in files {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            create_private_dir(parent)?;
        }
        let mut file = new_private_file(&path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    Ok(())
}

/// Makes sure `root` is a real directory of ours that only we can use: created `0700` (so there is
/// no moment it is wider), then checked with `symlink_metadata`, which refuses a symbolic link, a
/// directory of another user and a mode that is not `0700`. Missing parents are created normally.
#[cfg(unix)]
fn ensure_private_root(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(DIR_MODE).create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(root)?;
    let refuse = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {why}", root.display()),
        ))
    };
    if !metadata.file_type().is_dir() {
        return refuse("is not a real directory");
    }
    if metadata.uid() != nix::unistd::geteuid().as_raw() {
        return refuse("belongs to another user");
    }
    if metadata.mode() & 0o777 != DIR_MODE {
        return refuse("must have mode 0700");
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_root(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root)
}

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)?;
    set_private(dir, DIR_MODE)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn new_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)
}

#[cfg(not(unix))]
fn new_private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Sets the mode exactly, whatever the umask made of it.
#[cfg(unix)]
fn set_private(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_private(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// The terminal task that runs `program` (the CLI found on the terminal environment's `PATH`) as
/// `spec` says. `base_env` is the terminal environment; the launch's own variables win over it.
pub(crate) fn spawn_in_terminal(
    chat_id: &str,
    label: String,
    program: &Path,
    spec: &LaunchSpec,
    base_env: &BTreeMap<String, String>,
) -> SpawnInTerminal {
    let mut spawn = SpawnInTerminal {
        id: TaskId(format!("bongos-chat-{chat_id}")),
        full_label: label.clone(),
        command_label: label.clone(),
        label,
        command: Some(program.to_string_lossy().into_owned()),
        args: spec.args.clone(),
        cwd: Some(spec.cwd.clone()),
        use_new_terminal: true,
        allow_concurrent_runs: true,
        reveal: RevealStrategy::Never,
        hide: HideStrategy::Never,
        show_summary: false,
        show_command: false,
        show_rerun: false,
        ..SpawnInTerminal::default()
    };
    spawn.env.extend(
        base_env
            .iter()
            .chain(&spec.env)
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    spawn
}
