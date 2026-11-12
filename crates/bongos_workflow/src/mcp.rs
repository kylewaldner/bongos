//! MCP server discovery for agent nodes (feature 20, D8).
//!
//! The servers a node may use are never named by the definition: they are whatever the agent CLI
//! has configured. [`discover`] asks the CLI (`<cli> mcp list`, parsed for names) and, when the CLI
//! has no `mcp` command, does not answer, or answers with text that is not a list, reads the CLI's
//! own configuration files instead (`~/.claude.json` and the project's `.mcp.json` for Claude,
//! `~/.codex/config.toml` for Codex). [`McpCache`] keeps an answer for [`CACHE_TTL`].
//!
//! Formats, as checked or assumed (see the module tests and their fixtures):
//! - Claude Code 2.1.322 (recorded): one line per server, `<name>: <command or url> [(HTTP|SSE)] -
//!   <status mark> <status text>`, after a `Checking MCP server health…` line and a blank line;
//!   `No MCP servers configured. Use `claude mcp add` to add a server.` when there are none.
//! - Codex (assumed, not installed here): one table per transport with a header row starting at
//!   `Name`, one row per server whose first column is the name, and `No MCP servers configured
//!   yet.` when there are none.
//!
//! [`check_start`] is the start-time check: an allowed server that is not installed or is excluded
//! by `workflows.mcp_exclude` refuses the start naming it, and on Codex a single-tool entry is
//! refused too (it limits MCP by server only). [`validation_servers`] gives validation the same
//! sets.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bongos_workflow_model::definition::AgentKind;
use bongos_workflow_model::{Failure, FailureReason, NodeKey};

use crate::process::NodeProcess;

/// How long a discovery is reused.
pub const CACHE_TTL: Duration = Duration::from_secs(60);

/// How long `<cli> mcp list` may take; it health-checks every server, so it can be slow.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the names came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `<cli> mcp list`.
    List,
    /// The CLI's configuration files.
    Config,
}

/// The MCP servers one CLI has, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub servers: BTreeSet<String>,
    pub source: Source,
}

/// Where discovery looks: the directory the CLI runs in (project servers) and the two homes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub cwd: PathBuf,
    /// The home directory holding `.claude.json`.
    pub home: PathBuf,
    /// The Codex home (`$CODEX_HOME`, else `<home>/.codex`).
    pub codex_home: PathBuf,
}

impl Location {
    pub fn new(cwd: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            cwd: cwd.into(),
            codex_home: home.join(".codex"),
            home,
        }
    }
}

/// Runs `<cli> mcp list` and returns its standard output, or `None` when it cannot be run, does
/// not finish in time or exits nonzero.
pub trait Lister {
    fn list(&self, cli: AgentKind, cwd: &Path) -> Option<String>;
}

/// The name a CLI is run by and called in messages.
pub fn cli_name(cli: AgentKind) -> &'static str {
    match cli {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
    }
}

/// The servers `cli` has. `use_list` says whether the CLI has the `mcp` subcommand (from its
/// capabilities); without it, or when the list cannot be read, the configuration files answer.
pub fn discover(
    cli: AgentKind,
    use_list: bool,
    lister: &dyn Lister,
    location: &Location,
) -> Discovery {
    if use_list
        && let Some(text) = lister.list(cli, &location.cwd)
        && let Some(servers) = parse_list(cli, &text)
    {
        return Discovery {
            servers,
            source: Source::List,
        };
    }
    Discovery {
        servers: read_config(cli, location),
        source: Source::Config,
    }
}

/// Names from the text of `<cli> mcp list`; `None` when it is not a list (an error message, an
/// empty answer), so the caller falls back to the configuration files.
pub fn parse_list(cli: AgentKind, text: &str) -> Option<BTreeSet<String>> {
    if text.contains("No MCP servers configured") {
        return Some(BTreeSet::new());
    }
    let servers = match cli {
        AgentKind::Claude => parse_claude_list(text),
        AgentKind::Codex => parse_codex_list(text),
    };
    (!servers.is_empty()).then_some(servers)
}

/// A server name: what `mcp add` accepts. No separators, so it is safe inside `mcp__<name>__*`.
fn is_name(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Claude lines are `<name>: <target> - <status>` at column zero; the name ends at the first `: `.
fn parse_claude_list(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| line.split_once(": "))
        // The rest holds the status after ` - `; an error line does not.
        .filter(|(_, rest)| rest.contains(" - "))
        .map(|(name, _)| name)
        .filter(|name| is_name(name))
        .map(str::to_string)
        .collect()
}

/// Codex rows follow a header row whose first column is `Name`; a blank line ends the table.
fn parse_codex_list(text: &str) -> BTreeSet<String> {
    let mut servers = BTreeSet::new();
    let mut in_table = false;
    for line in text.lines() {
        let first = line.split_whitespace().next();
        match first {
            None => in_table = false,
            Some("Name") => in_table = true,
            Some(name) if in_table && is_name(name) => {
                servers.insert(name.to_string());
            }
            Some(_) => {}
        }
    }
    servers
}

fn read_config(cli: AgentKind, location: &Location) -> BTreeSet<String> {
    match cli {
        AgentKind::Claude => claude_config(location),
        AgentKind::Codex => codex_config(&location.codex_home),
    }
}

/// `mcpServers` at the top of `~/.claude.json`, under the project entry for the working directory
/// (and its parents), and in the project's `.mcp.json`.
fn claude_config(location: &Location) -> BTreeSet<String> {
    let mut servers = BTreeSet::new();
    let mut take = |value: Option<&serde_json::Value>| {
        if let Some(map) = value.and_then(|value| value.as_object()) {
            servers.extend(map.keys().filter(|name| is_name(name)).cloned());
        }
    };
    let config = read_json(&location.home.join(".claude.json"));
    take(config.as_ref().and_then(|config| config.get("mcpServers")));
    for directory in location.cwd.ancestors() {
        let project = config
            .as_ref()
            .and_then(|config| config.get("projects"))
            .and_then(|projects| projects.get(directory.to_string_lossy().as_ref()));
        take(project.and_then(|project| project.get("mcpServers")));
        let file = read_json(&directory.join(".mcp.json"));
        take(file.as_ref().and_then(|file| file.get("mcpServers")));
    }
    servers
}

/// The tables under `[mcp_servers]` of `<codex home>/config.toml`.
fn codex_config(codex_home: &Path) -> BTreeSet<String> {
    let Ok(text) = fs::read_to_string(codex_home.join("config.toml")) else {
        return BTreeSet::new();
    };
    let Ok(config) = text.parse::<toml::Table>() else {
        return BTreeSet::new();
    };
    config
        .get("mcp_servers")
        .and_then(|servers| servers.as_table())
        .map(|servers| {
            servers
                .keys()
                .filter(|name| is_name(name))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Discoveries kept for [`CACHE_TTL`], per CLI and working directory.
#[derive(Debug)]
pub struct McpCache {
    ttl: Duration,
    entries: BTreeMap<(AgentKind, PathBuf), (Instant, Discovery)>,
}

impl Default for McpCache {
    fn default() -> Self {
        Self::new(CACHE_TTL)
    }
}

impl McpCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: BTreeMap::new(),
        }
    }

    /// The kept discovery, if it is younger than the time to live at `now`.
    pub fn get(&self, cli: AgentKind, cwd: &Path, now: Instant) -> Option<&Discovery> {
        let (at, discovery) = self.entries.get(&(cli, cwd.to_path_buf()))?;
        (now.saturating_duration_since(*at) < self.ttl).then_some(discovery)
    }

    /// The cached discovery, or [`discover`] and keep it.
    pub fn discover(
        &mut self,
        now: Instant,
        cli: AgentKind,
        use_list: bool,
        lister: &dyn Lister,
        location: &Location,
    ) -> Discovery {
        if let Some(kept) = self.get(cli, &location.cwd, now) {
            return kept.clone();
        }
        let found = discover(cli, use_list, lister, location);
        self.entries
            .insert((cli, location.cwd.clone()), (now, found.clone()));
        found
    }

    /// Forgets everything (after `mcp add`, or when settings change).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// [`Lister`] that runs the real CLI: `<program> mcp list` in the directory, with the given
/// environment only, standard output captured in a file under `scratch` (removed after reading).
pub struct ProcessLister {
    pub programs: BTreeMap<AgentKind, PathBuf>,
    pub env: BTreeMap<String, String>,
    pub scratch: PathBuf,
    pub timeout: Duration,
}

impl Lister for ProcessLister {
    fn list(&self, cli: AgentKind, cwd: &Path) -> Option<String> {
        let program = self.programs.get(&cli)?;
        fs::create_dir_all(&self.scratch).ok()?;
        let file = self.scratch.join(format!("mcp-list-{}.txt", cli_name(cli)));
        let stdout = File::create(&file).ok()?;
        let mut command = Command::new(program);
        command
            .args(["mcp", "list"])
            .current_dir(cwd)
            .env_clear()
            .envs(&self.env);
        let mut process =
            NodeProcess::spawn(command, Stdio::null(), Stdio::from(stdout), Stdio::null()).ok()?;
        let deadline = Instant::now() + self.timeout;
        while !process.has_exited() {
            if Instant::now() >= deadline {
                process.stop();
                fs::remove_file(&file).ok();
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let ok = matches!(process.child().try_status(), Ok(Some(status)) if status.success());
        let text = fs::read_to_string(&file).ok();
        fs::remove_file(&file).ok();
        text.filter(|_| ok)
    }
}

/// The servers a node of `cli` may use: what was discovered less `workflows.mcp_exclude`.
pub fn available(discovery: &Discovery, exclude: &[String]) -> BTreeSet<String> {
    discovery
        .servers
        .iter()
        .filter(|server| !exclude.contains(server))
        .cloned()
        .collect()
}

/// What validation takes as `ValidationContext::mcp_servers`: each CLI's available servers.
pub fn validation_servers(
    discovered: &BTreeMap<AgentKind, Discovery>,
    exclude: &[String],
) -> BTreeMap<AgentKind, BTreeSet<String>> {
    discovered
        .iter()
        .map(|(cli, discovery)| (*cli, available(discovery, exclude)))
        .collect()
}

/// The MCP servers the allowed tools name, with the tool part (`*` for the whole server).
pub fn allowed_servers(allowed_tools: &[String]) -> Vec<(String, String)> {
    allowed_tools
        .iter()
        .filter_map(|tool| tool.strip_prefix("mcp__"))
        .map(|rest| match rest.split_once("__") {
            Some((server, tool)) => (server.to_string(), tool.to_string()),
            None => (rest.to_string(), String::new()),
        })
        .collect()
}

/// The start-time check of a node's MCP tools against the servers discovered for its agent
/// (`installed`, before exclusion) and `workflows.mcp_exclude`. Refuses with `mcp server
/// unavailable`, naming the first server that is not installed or is excluded, a bare server
/// name, or on Codex a single-tool entry.
pub fn check_start(
    agent: AgentKind,
    key: Option<&NodeKey>,
    allowed_tools: &[String],
    installed: &BTreeSet<String>,
    exclude: &[String],
) -> Result<(), Failure> {
    let refuse =
        |detail: String| Failure::new(FailureReason::McpServerUnavailable, key.cloned(), detail);
    let cli = cli_name(agent);
    for (server, tool) in allowed_servers(allowed_tools) {
        if server.is_empty() || tool.is_empty() {
            let entry = if tool.is_empty() && !server.is_empty() {
                format!("mcp__{server}")
            } else {
                "mcp__".to_string()
            };
            return Err(refuse(format!(
                "'{entry}' names a server, not a tool; use mcp__<server>__<tool> or \
                 mcp__<server>__*"
            )));
        }
        if exclude.contains(&server) {
            return Err(refuse(format!(
                "{server}: excluded by workflows.mcp_exclude; remove it from allowed_tools or \
                 from workflows.mcp_exclude"
            )));
        }
        if !installed.contains(&server) {
            return Err(refuse(format!(
                "{server}: MCP server not installed for {cli}; install it with `{cli} mcp \
                 add`, or remove it from allowed_tools"
            )));
        }
        if agent == AgentKind::Codex && tool != "*" {
            return Err(refuse(format!(
                "'mcp__{server}__{tool}': codex limits MCP by server; use mcp__{server}__*"
            )));
        }
    }
    Ok(())
}

/// `-c` overrides that disable servers on Codex: every server of `installed` that no allowed
/// entry names, and every excluded server even when it is named. A name that is not a plain TOML
/// key is quoted.
pub fn codex_disable_args(
    installed: &BTreeSet<String>,
    allowed_tools: &[String],
    exclude: &[String],
) -> Vec<String> {
    let allowed: BTreeSet<String> = allowed_servers(allowed_tools)
        .into_iter()
        .map(|(server, _)| server)
        .collect();
    let mut disabled: BTreeSet<&String> = installed
        .iter()
        .filter(|server| !allowed.contains(*server))
        .collect();
    disabled.extend(exclude);
    disabled
        .into_iter()
        .flat_map(|server| {
            let key = if server
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                server.clone()
            } else {
                format!("\"{}\"", server.replace('\\', "\\\\").replace('"', "\\\""))
            };
            ["-c".to_string(), format!("mcp_servers.{key}.enabled=false")]
        })
        .collect()
}
