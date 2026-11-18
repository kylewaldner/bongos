//! The agent adapters (design D7): what each CLI can do (`Capabilities`, read from its own `--help`),
//! the command line, environment and fallbacks of one launch, the offline login probe and the
//! recovery of a session id the CLI does not report.
//!
//! Everything here is pure. A launch is computed from the plan and the capabilities and returns data;
//! nothing runs a process, and the only files read are the agents' own session files in
//! `capture_session_id`, under a `home` the caller passes in. No output of this module holds a
//! credential: the login parsers keep one boolean, and the environment is filtered to the `BONGOS_*`
//! names of D10. The one per-launch value that must stay private, the chat channel's token, lives in
//! a [`ChannelPlan`] and reaches only a launch's environment: it is never in a generated file, an
//! argument or a `Debug` rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::channel::report::Token;
use crate::claude::ClaudeAdapter;
use crate::codex::CodexAdapter;
use crate::inject::shell_quote;
use crate::record::{Agent, ChatRecord};

pub use crate::capture::CaptureQuery;

/// The environment variable that holds the chat channel's socket path.
pub const CHANNEL_SOCKET_ENV: &str = "BONGOS_CHAT_SOCKET";
/// The environment variable that holds the chat channel's per-launch token.
pub const CHANNEL_TOKEN_ENV: &str = "BONGOS_CHAT_TOKEN";

/// The only environment variables a launch sets (D10). Everything else in a plan's `env` is dropped.
pub const ENV_NAMES: &[&str] = &[
    "BONGOS_CHAT_ID",
    "BONGOS_CHAT_TYPE",
    "BONGOS_CHAT_AGENT",
    "BONGOS_CHAT_RECORD",
    "BONGOS_CHAT_CONTEXT",
    "BONGOS_TASK_ID",
    "BONGOS_TASK_DIR",
    "BONGOS_TASK_FILE",
    "BONGOS_PROJECT_DIR",
    "BONGOS_WORKTREE",
    "BONGOS_ROOT",
    CHANNEL_SOCKET_ENV,
    CHANNEL_TOKEN_ENV,
];

/// The names of [`ENV_NAMES`] that only a [`ChannelPlan`] sets. A plan's plain `env` never passes
/// them, so a launch without a channel has neither.
const CHANNEL_ENV_NAMES: &[&str] = &[CHANNEL_SOCKET_ENV, CHANNEL_TOKEN_ENV];

/// Why a launch has no chat channel. Shown through [`Fallback::ChannelOff`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelOff {
    /// A word of the reporter command (the bongos program path) cannot be single-quoted.
    UnquotablePath,
}

impl ChannelOff {
    pub fn reason(self) -> &'static str {
        match self {
            ChannelOff::UnquotablePath => "the bongos path cannot be quoted",
        }
    }
}

/// The chat channel of one launch (D6): the reporter command the agent's hooks run, the report
/// socket and the launch token.
///
/// The token is private and has no accessor: the only way out of a plan is [`ChannelPlan::env`],
/// which the adapter merges into the launch environment. The reporter command is built from the
/// reporter words alone, so the settings file and the command line, which read only
/// [`ChannelPlan::reporter_command`], cannot carry the token. `Debug` shows the token as `Token(..)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelPlan {
    reporter_command: String,
    socket: PathBuf,
    token: Token,
}

impl ChannelPlan {
    /// The plan for `reporter` (the program and its leading arguments, such as
    /// `[<bongos path>, "--chat-report"]`; the hook appends its mode), each word single-quoted with
    /// [`shell_quote`]. A word that cannot be quoted, or an empty `reporter`, turns the channel off.
    pub fn new(reporter: &[String], socket: PathBuf, token: Token) -> Result<Self, ChannelOff> {
        if reporter.is_empty() {
            return Err(ChannelOff::UnquotablePath);
        }
        let words = reporter
            .iter()
            .map(|word| shell_quote(Path::new(word)).map_err(|_| ChannelOff::UnquotablePath))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            reporter_command: words.join(" "),
            socket,
            token,
        })
    }

    /// The quoted reporter words, without a mode: `'<exe>' '--chat-report'`.
    pub fn reporter_command(&self) -> &str {
        &self.reporter_command
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The channel's environment for chat `chat_id`: the socket, the id and the token.
    pub(crate) fn env(&self, chat_id: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                CHANNEL_SOCKET_ENV.to_string(),
                self.socket.to_string_lossy().into_owned(),
            ),
            ("BONGOS_CHAT_ID".to_string(), chat_id.to_string()),
            (
                CHANNEL_TOKEN_ENV.to_string(),
                self.token.as_str().to_string(),
            ),
        ])
    }
}

/// The channel for a launch, or `None` with [`Fallback::ChannelOff`] and its reason pushed onto
/// `force` (so the launch is the one without a channel, labeled).
pub fn plan_channel(
    reporter: &[String],
    socket: PathBuf,
    token: Token,
    force: &mut Vec<Fallback>,
) -> Option<ChannelPlan> {
    match ChannelPlan::new(reporter, socket, token) {
        Ok(plan) => Some(plan),
        Err(off) => {
            note(force, Fallback::ChannelOff(off));
            None
        }
    }
}

/// A flag that an agent accepts but its `--help` does not list, present from `min_version` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HiddenFlag {
    pub flag: &'static str,
    pub min_version: &'static str,
}

/// One capability of a [`VersionTable`]: present from `min_version` on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionRow {
    /// A flag (starts with `-`, and then also answers `has_flag`) or any other capability name.
    pub name: &'static str,
    pub min_version: &'static str,
    /// Whether the row was checked against a real binary, as opposed to assumed.
    pub verified: bool,
}

/// What a CLI can do by version: capability rows, each with the version it first holds. A name may
/// have several rows; it is present when any of them is satisfied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionTable {
    pub rows: &'static [VersionRow],
}

impl VersionTable {
    pub const fn new(rows: &'static [VersionRow]) -> Self {
        Self { rows }
    }
}

/// The flags and subcommands one installed CLI version offers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    flags: BTreeSet<String>,
    subcommands: BTreeSet<String>,
    /// Capability names from a [`VersionTable`] (empty for the help-derived constructors).
    names: BTreeSet<String>,
    version: String,
}

impl Capabilities {
    /// Capabilities from the version alone, by `table`. A version that cannot be read (absent,
    /// empty, garbage) gives an empty set, so every fallback applies, quietly. A row holds when the
    /// version is at least its minimum, so a version newer than the table keeps every row.
    pub fn from_version(version: Option<&str>, table: &VersionTable) -> Self {
        let Some(token) = version
            .and_then(version_token)
            .filter(|token| version_parts(token).is_some())
        else {
            return Self::default();
        };
        let mut caps = Self {
            version: token.clone(),
            ..Self::default()
        };
        for row in table.rows {
            if version_at_least(&token, row.min_version) {
                caps.names.insert(row.name.to_string());
                if row.name.starts_with('-') {
                    caps.flags.insert(row.name.to_string());
                }
            }
        }
        caps
    }

    /// Reads capabilities from the output of `<cli> --version` and `<cli> --help`. Only option lines
    /// (a line that starts with `-`, names separated by commas) give flags and only bare command
    /// words (alone, or followed by two spaces or an argument bracket) give subcommands, so a
    /// prose mention of a flag counts for nothing. Each `hidden` flag is added when the version is
    /// known and at least its `min_version`, because `--help` has false negatives.
    pub fn from_help(version: &str, help: &str, hidden: &[HiddenFlag]) -> Self {
        let mut flags = BTreeSet::new();
        let mut subcommands = BTreeSet::new();
        for line in help.lines() {
            let line = line.trim();
            if line.starts_with('-') {
                flags.extend(option_names(line));
            } else if let Some(name) = subcommand_name(line) {
                subcommands.insert(name);
            }
        }
        let version = version_token(version).unwrap_or_else(|| version.trim().to_string());
        for hidden in hidden {
            if version_at_least(&version, hidden.min_version) {
                flags.insert(hidden.flag.to_string());
            }
        }
        Self {
            flags,
            subcommands,
            names: BTreeSet::new(),
            version,
        }
    }

    /// Capabilities from plain name lists: for tests, and for a CLI whose help was not read.
    pub fn from_names(version: &str, flags: &[&str], subcommands: &[&str]) -> Self {
        Self {
            flags: flags.iter().map(|name| name.to_string()).collect(),
            subcommands: subcommands.iter().map(|name| name.to_string()).collect(),
            names: BTreeSet::new(),
            version: version.to_string(),
        }
    }

    /// Whether a [`VersionTable`] row of this name holds (only `from_version` sets any).
    pub fn has(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }

    pub fn has_any_flag(&self, flags: &[&str]) -> bool {
        flags.iter().any(|flag| self.has_flag(flag))
    }

    pub fn has_subcommand(&self, name: &str) -> bool {
        self.subcommands.contains(name)
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

fn valid_flag(token: &str) -> bool {
    token.len() > 1
        && token.starts_with('-')
        && token != "--"
        && token.chars().all(|c| c == '-' || c.is_ascii_alphanumeric())
}

fn option_names(line: &str) -> Vec<String> {
    let mut names = Vec::new();
    for piece in line.split(',') {
        let piece = piece.trim();
        let token = piece.split_whitespace().next().unwrap_or("");
        let token = token.split('=').next().unwrap_or("");
        if !valid_flag(token) {
            break;
        }
        names.push(token.to_string());
        // Whitespace means a value placeholder or the description starts here.
        if piece.contains(char::is_whitespace) {
            break;
        }
    }
    names
}

fn subcommand_name(line: &str) -> Option<String> {
    let token = line.split_whitespace().next()?;
    let mut chars = token.chars();
    let first = chars.next()?;
    if !first.is_ascii_lowercase()
        || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return None;
    }
    let rest = &line[token.len()..];
    (rest.is_empty() || rest.starts_with("  ") || rest.starts_with(" [") || rest.starts_with(" <"))
        .then(|| token.to_string())
}

/// The first `1.2.3` shaped word of `text` (`2.1.319 (Claude Code)` gives `2.1.319`).
fn version_token(text: &str) -> Option<String> {
    text.split_whitespace().find_map(|word| {
        let word = word.trim_start_matches('v');
        let numeric = word.chars().all(|c| c.is_ascii_digit() || c == '.');
        (numeric && word.contains('.') && word.starts_with(|c: char| c.is_ascii_digit()))
            .then(|| word.trim_end_matches('.').to_string())
    })
}

fn version_parts(version: &str) -> Option<Vec<u64>> {
    let token = version_token(version)?;
    token.split('.').map(|part| part.parse().ok()).collect()
}

/// Whether `version` is known and not older than `minimum`. An unreadable version is never enough.
pub fn version_at_least(version: &str, minimum: &str) -> bool {
    let (Some(mut have), Some(mut need)) = (version_parts(version), version_parts(minimum)) else {
        return false;
    };
    let len = have.len().max(need.len());
    have.resize(len, 0);
    need.resize(len, 0);
    have >= need
}

/// The result of reading a login probe's output. Never carries account data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    LoggedIn,
    LoggedOut,
}

/// How a launch was changed because the CLI lacks something. Each is shown on the chat's row and in
/// `chat: show artifacts` through [`Fallback::label`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fallback {
    /// Claude: no `--append-system-prompt-file`, so the text goes in `--append-system-prompt`.
    SystemPromptAsText,
    /// Claude: neither system prompt flag exists, so nothing is appended to the system prompt.
    SystemPromptUnavailable,
    /// Claude: no `--settings`, so no hook prints `context.md`; it is part of the system prompt.
    ContextInSystemPrompt,
    /// Claude: no `--plugin-dir`, so the skill text is part of the system prompt.
    SkillsAsText,
    /// Claude: no `--name`; the IDE name stays authoritative.
    NameOmitted,
    /// No model flag in the help, so the model was not passed.
    ModelOmitted,
    /// Claude: the session id is not known before launch and is read from the CLI's own files.
    CaptureSessionId,
    /// A native fork was asked for and the CLI cannot do it; started as a handoff.
    ForkUnsupported,
    /// A resume was asked for with no known session id and no way to resume the latest.
    RestartNoSession,
    /// Codex: no id is known, so the latest session of the directory is resumed.
    ResumedLatest,
    /// Codex: no instructions key, so the instructions path is the first message.
    ContextAsFirstMessage,
    /// The launch has no chat channel, for the reason given.
    ChannelOff(ChannelOff),
}

impl Fallback {
    pub fn label(self) -> &'static str {
        match self {
            Fallback::SystemPromptAsText => "system prompt passed as text",
            Fallback::SystemPromptUnavailable => "no system prompt flag; context not appended",
            Fallback::ContextInSystemPrompt => "context appended to the system prompt (no hooks)",
            Fallback::SkillsAsText => "skills appended to the system prompt (no plugins)",
            Fallback::NameOmitted => "name not passed to the CLI",
            Fallback::ModelOmitted => "model not passed (no model flag)",
            Fallback::CaptureSessionId => "session id read from the CLI's files",
            Fallback::ForkUnsupported => "started as a handoff (no fork command)",
            Fallback::RestartNoSession => "started a new session (no session to resume)",
            Fallback::ResumedLatest => "resumed the latest Codex session in this directory",
            Fallback::ContextAsFirstMessage => "context sent as first message",
            Fallback::ChannelOff(off) => off.reason(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchKind {
    New,
    Resume,
    /// `native` is false for a handoff: a new conversation that starts from a note.
    Fork {
        source_session: String,
        native: bool,
    },
}

/// The files of the chat's runtime directory, already written by the caller. Paths go on the command
/// line; text is only used by fallbacks that cannot take a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedFiles {
    /// `system.md` or `codex-instructions.md`.
    pub system_path: PathBuf,
    /// Its text. When [`Shape::context_in_system`] was true it already holds `context.md`.
    pub system_text: String,
    /// `claude-settings.json`.
    pub settings_path: Option<PathBuf>,
    /// The plugin directory, when skills were offered as a plugin.
    pub plugin_dir: Option<PathBuf>,
    /// The type lists skills.
    pub has_skills: bool,
}

/// What the caller must do when rendering the files, decided from the capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Write the skills as a plugin rather than into the system text.
    pub skills_as_plugin: bool,
    /// Put `context.md` in the system text (no hook can print it).
    pub context_in_system: bool,
    /// A native fork can run; otherwise render the handoff note and pass `native: false`.
    pub native_fork: bool,
}

#[derive(Clone, Debug)]
pub struct LaunchPlan {
    pub kind: LaunchKind,
    /// For Claude, `session_id` is the id to resume, or the id a new or forked chat will have
    /// (a fresh UUID v4 chosen by the caller). For Codex it is the id to resume, or `None`.
    pub chat: ChatRecord,
    pub files: GeneratedFiles,
    pub model: Option<String>,
    pub extra_args: Vec<String>,
    /// The environment from `render_env`; filtered to [`ENV_NAMES`] by the adapter.
    pub env: BTreeMap<String, String>,
    /// Fallbacks already known to be needed, such as a Claude fork whose first try with a new session
    /// id ended in an error exit.
    pub force: Vec<Fallback>,
    /// The chat channel to configure, if any. The settings file must have been rendered with the
    /// same plan. Only the Claude adapter uses it; [`Fallback::ChannelOff`] in `force` turns it off.
    pub channel: Option<ChannelPlan>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    /// Holds the channel's token when `channel` is true; `Debug` redacts it.
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Shown on the row.
    pub fallbacks: Vec<Fallback>,
    /// The session id is unknown after launch: poll [`AgentAdapter::capture_session_id`].
    pub capture_session_id: bool,
    /// The launch configured the chat channel: its hooks are in the settings passed on the command
    /// line and its socket, chat id and token are in `env`.
    pub channel: bool,
}

impl fmt::Debug for LaunchSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env = self
            .env
            .iter()
            .map(|(name, value)| {
                let shown = if name == CHANNEL_TOKEN_ENV {
                    ".."
                } else {
                    value.as_str()
                };
                (name.as_str(), shown)
            })
            .collect::<BTreeMap<_, _>>();
        f.debug_struct("LaunchSpec")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &env)
            .field("cwd", &self.cwd)
            .field("fallbacks", &self.fallbacks)
            .field("capture_session_id", &self.capture_session_id)
            .field("channel", &self.channel)
            .finish()
    }
}

pub trait AgentAdapter {
    fn agent(&self) -> Agent;
    /// `version` is the output of `--version`, `help` that of `--help`.
    fn capabilities_from_help(&self, version: &str, help: &str) -> Capabilities;
    /// How the caller renders the files for these capabilities.
    fn shape(&self, caps: &Capabilities) -> Shape;
    fn launch(&self, plan: &LaunchPlan, caps: &Capabilities) -> LaunchSpec;
    /// The arguments (after the program) of an offline login check, or `None` when the CLI has
    /// none, in which case the health is Ready with "login not checked".
    fn login_probe(&self, caps: &Capabilities) -> Option<Vec<String>>;
    fn parse_login(&self, output: &str, status: i32) -> Login;
    /// The session id of a conversation the CLI did not report, from its own files.
    fn capture_session_id(&self, query: &CaptureQuery) -> Option<String>;
}

/// The adapter for `agent`, running the program `command`.
pub fn adapter_for(agent: Agent, command: &str) -> Box<dyn AgentAdapter> {
    match agent {
        Agent::Claude => Box::new(ClaudeAdapter::new(command)),
        Agent::Codex => Box::new(CodexAdapter::new(command)),
    }
}

/// `env` reduced to the D10 names, without the channel's names (only a [`ChannelPlan`] sets those).
pub(crate) fn launch_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter()
        .filter(|(name, _)| {
            ENV_NAMES.contains(&name.as_str()) && !CHANNEL_ENV_NAMES.contains(&name.as_str())
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Pushes `fallback` once.
pub(crate) fn note(fallbacks: &mut Vec<Fallback>, fallback: Fallback) {
    if !fallbacks.contains(&fallback) {
        fallbacks.push(fallback);
    }
}
