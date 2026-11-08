//! CLI health (design D19): is each agent CLI installed, and is it logged in?
//!
//! [`ChatHealth`] finds the program (`chats.agents.<agent>.command`, on `PATH` unless it is a path),
//! runs `--version`, `--help` and the adapter's offline login probe on the background executor,
//! each under a timeout, and keeps one [`Health`] per agent. The capabilities read from `--help`
//! are cached per CLI version, so an unchanged CLI costs one `--version` call per refresh. When a
//! [`Health`] changes the store emits [`ChatHealthEvent::HealthChanged`].
//!
//! Nothing probes while `chats.enabled` is false, and a missing or broken CLI breaks nothing else:
//! every state is only information, and only `NotFound` and `LoggedOut` keep an agent out of the
//! `chat: new` pickers. Probe output is never stored or logged: the login parsers keep one boolean,
//! and the version is the only text of a CLI that reaches a [`Health`].

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bongos_chat_model::{Agent, Capabilities, Login, adapter_for};
use futures::future::{Either, select};
use gpui::{App, AppContext as _, BackgroundExecutor, Entity, EventEmitter, Global, Task};
use settings::{Settings as _, SettingsStore};
use util::command::{Stdio, new_command};

use crate::ChatsSettings;
use crate::store::expand_home;

/// How long one probe may run before the CLI counts as not answering.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The shortest time between two probe rounds asked for with [`ChatHealth::refresh`].
pub const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// The agents, in the order the panel lists them.
const AGENTS: [Agent; 2] = [Agent::Claude, Agent::Codex];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthState {
    /// The CLI answered `--version`. `login_checked` is false when the CLI has no offline login
    /// probe (or it did not answer): the chat may still work.
    Ready {
        version: String,
        login_checked: bool,
    },
    /// The program is not on `PATH` (or the configured path is not a program).
    NotFound,
    /// The CLI is there and says nobody is logged in.
    LoggedOut,
    /// Not probed yet, or the CLI did not answer `--version` in time.
    Unchecked,
}

/// What is known about one agent CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    pub agent: Agent,
    pub state: HealthState,
    /// What the user can do about it, or a note for `Ready`; empty when there is nothing to say.
    pub fix: String,
    /// The CLI's flags and subcommands, once its `--help` was read.
    pub caps: Option<Capabilities>,
}

impl Health {
    /// Whether `chat: new` may offer this agent. A CLI that is not known to be broken is offered:
    /// health never gets in the way of a chat that would work.
    pub fn is_offered(&self) -> bool {
        !matches!(self.state, HealthState::NotFound | HealthState::LoggedOut)
    }

    fn unchecked(agent: Agent, fix: String) -> Self {
        Self {
            agent,
            state: HealthState::Unchecked,
            fix,
            caps: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatHealthEvent {
    /// The [`Health`] of at least one agent changed.
    HealthChanged,
}

/// Counters of what the store did, for tests and logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HealthStats {
    /// Probe rounds started.
    pub rounds: usize,
    /// Times `--help` was read (a round for an unchanged version reads none).
    pub help_reads: usize,
}

/// The user-facing name of an agent's CLI.
fn agent_title(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude Code",
        Agent::Codex => "Codex",
    }
}

/// The fix for a program that was not found.
pub fn fix_not_found(agent: Agent, command: &str) -> String {
    let setting = format!("chats.agents.{}.command", agent.as_str());
    if is_path(command) {
        format!("{command} was not found; fix {setting}")
    } else {
        format!(
            "{command} was not found on PATH; install {} or set {setting}",
            agent_title(agent)
        )
    }
}

/// The fix for a CLI that is not logged in.
pub fn fix_logged_out(agent: Agent, command: &str) -> String {
    match agent {
        Agent::Claude => format!("Run `{command}` in a terminal and log in"),
        Agent::Codex => format!("Run `{command} login` in a terminal and log in"),
    }
}

/// The note of a CLI whose login could not be checked.
pub fn fix_login_unchecked(agent: Agent) -> String {
    format!("{} login not checked", agent_title(agent))
}

/// The fix of an agent that was never probed.
pub fn fix_not_probed(agent: Agent) -> String {
    format!("{} not checked yet", agent_title(agent))
}

fn fix_no_answer(agent: Agent, command: &str) -> String {
    format!(
        "{command} did not answer `--version`; {} is not checked and chats may still work",
        agent_title(agent)
    )
}

fn is_path(command: &str) -> bool {
    command.contains('/') || command.contains(std::path::MAIN_SEPARATOR)
}

/// The program `command` names: a path as written (after `~`), else the first match on the `PATH`
/// of `env` (the terminal environment), else on this process's `PATH`.
pub fn find_program(command: &str, env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    let command = expand_home(command);
    let path = env
        .get("PATH")
        .map(OsString::from)
        .or_else(|| std::env::var_os("PATH"));
    let cwd = std::env::current_dir().unwrap_or_default();
    which::which_in(command, path, cwd).ok()
}

/// How one probe ended.
#[derive(Debug)]
pub(crate) enum Probe {
    Done {
        status: i32,
        /// Standard output, or standard error when that is empty.
        text: String,
    },
    TimedOut,
    /// The program is gone or cannot run.
    NotFound,
    Failed,
}

/// Runs `program args` with `env` added to the environment and no stdin, and kills it when it
/// does not finish within `timeout`.
pub(crate) async fn run_probe(
    program: &std::path::Path,
    args: &[String],
    env: &BTreeMap<String, String>,
    executor: &BackgroundExecutor,
    timeout: Duration,
) -> Probe {
    let mut command = new_command(program);
    command
        .args(args)
        .envs(env)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let run = Box::pin(async move {
        match command.output().await {
            Ok(output) => {
                let text = if output.stdout.iter().any(|byte| !byte.is_ascii_whitespace()) {
                    output.stdout
                } else {
                    output.stderr
                };
                Probe::Done {
                    status: output.status.code().unwrap_or(-1),
                    text: String::from_utf8_lossy(&text).into_owned(),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Probe::NotFound,
            Err(_) => Probe::Failed,
        }
    });
    match select(run, Box::pin(executor.timer(timeout))).await {
        Either::Left((probe, _)) => probe,
        // Dropping `run` drops the child, which `kill_on_drop` kills.
        Either::Right(_) => Probe::TimedOut,
    }
}

/// The key of the capability cache: one entry per agent, program and version.
type CacheKey = (Agent, String, String);

struct Probed {
    health: Health,
    /// The capabilities newly read, to cache under the key.
    learned: Option<(CacheKey, Capabilities)>,
    help_reads: usize,
}

impl Probed {
    fn plain(health: Health) -> Self {
        Self {
            health,
            learned: None,
            help_reads: 0,
        }
    }
}

/// Probes one agent: program lookup, `--version`, `--help` (unless `cache` knows this version),
/// then the adapter's login probe.
async fn probe_agent(
    agent: Agent,
    command: String,
    env: BTreeMap<String, String>,
    cache: HashMap<CacheKey, Capabilities>,
    executor: BackgroundExecutor,
    timeout: Duration,
) -> Probed {
    let not_found = || {
        Probed::plain(Health {
            agent,
            state: HealthState::NotFound,
            fix: fix_not_found(agent, &command),
            caps: None,
        })
    };
    let Some(program) = find_program(&command, &env) else {
        return not_found();
    };

    let version = match run_probe(&program, &["--version".into()], &env, &executor, timeout).await {
        Probe::Done { status: 0, text } if !text.trim().is_empty() => {
            text.lines().next().unwrap_or("").trim().to_string()
        }
        Probe::NotFound => return not_found(),
        _ => {
            return Probed::plain(Health::unchecked(agent, fix_no_answer(agent, &command)));
        }
    };

    let key = (agent, command.clone(), version.clone());
    let mut help_reads = 0;
    let mut learned = None;
    let caps = match cache.get(&key) {
        Some(caps) => caps.clone(),
        None => {
            help_reads = 1;
            let help = match run_probe(&program, &["--help".into()], &env, &executor, timeout).await
            {
                Probe::Done { text, .. } => text,
                _ => String::new(),
            };
            let caps = adapter_for(agent, &command).capabilities_from_help(&version, &help);
            // An unreadable help is not cached, so the next round reads it again.
            if !help.trim().is_empty() {
                learned = Some((key, caps.clone()));
            }
            caps
        }
    };
    let shown_version = if caps.version().is_empty() {
        version
    } else {
        caps.version().to_string()
    };

    // The adapter is not `Send`: it is built again after each await instead of held across it.
    let login_args = adapter_for(agent, &command).login_probe(&caps);
    let login = match login_args {
        Some(args) => match run_probe(&program, &args, &env, &executor, timeout).await {
            Probe::Done { status, text } => {
                Some(adapter_for(agent, &command).parse_login(&text, status))
            }
            _ => None,
        },
        None => None,
    };

    let (state, fix) = match login {
        Some(Login::LoggedOut) => (HealthState::LoggedOut, fix_logged_out(agent, &command)),
        Some(Login::LoggedIn) => (
            HealthState::Ready {
                version: shown_version,
                login_checked: true,
            },
            String::new(),
        ),
        None => (
            HealthState::Ready {
                version: shown_version,
                login_checked: false,
            },
            fix_login_unchecked(agent),
        ),
    };
    Probed {
        health: Health {
            agent,
            state,
            fix,
            caps: Some(caps),
        },
        learned,
        help_reads,
    }
}

/// The global health of the agent CLIs. Probes nothing while `chats.enabled` is false.
pub struct ChatHealth {
    /// Added to the environment of every probe, and the source of the `PATH` to search. Empty means
    /// this process's own.
    env: BTreeMap<String, String>,
    timeout: Duration,
    enabled: bool,
    health: Vec<Health>,
    /// The commands the current `health` was probed with.
    probed_commands: Vec<String>,
    cache: HashMap<CacheKey, Capabilities>,
    last_round: Option<Instant>,
    probing: Option<Task<()>>,
    stats: HealthStats,
    _settings: gpui::Subscription,
}

impl EventEmitter<ChatHealthEvent> for ChatHealth {}

struct GlobalChatHealth(Entity<ChatHealth>);

impl Global for GlobalChatHealth {}

impl ChatHealth {
    pub fn new(env: BTreeMap<String, String>, cx: &mut gpui::Context<Self>) -> Self {
        Self::with_timeout(env, PROBE_TIMEOUT, cx)
    }

    /// As [`ChatHealth::new`], with another probe timeout from the first round on (tests use a
    /// short one).
    pub fn with_timeout(
        env: BTreeMap<String, String>,
        timeout: Duration,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let settings = cx.observe_global::<SettingsStore>(|this, cx| this.sync(cx));
        let mut this = Self {
            env,
            timeout,
            enabled: false,
            health: AGENTS
                .iter()
                .map(|agent| Health::unchecked(*agent, fix_not_probed(*agent)))
                .collect(),
            probed_commands: Vec::new(),
            cache: HashMap::new(),
            last_round: None,
            probing: None,
            stats: HealthStats::default(),
            _settings: settings,
        };
        this.sync(cx);
        this
    }

    /// The global store, once [`crate::init`] has run.
    pub fn global(cx: &App) -> Option<Entity<ChatHealth>> {
        cx.try_global::<GlobalChatHealth>()
            .map(|global| global.0.clone())
    }

    pub(crate) fn set_global(health: Entity<ChatHealth>, cx: &mut App) {
        cx.set_global(GlobalChatHealth(health));
    }

    /// Replaces the environment of the probes and probes again, as the terminal environment
    /// changed.
    pub fn set_env(&mut self, env: BTreeMap<String, String>, cx: &mut gpui::Context<Self>) {
        if self.env != env {
            self.env = env;
            self.refresh_now(cx);
        }
    }

    /// Whether a probe round is running.
    pub fn is_probing(&self) -> bool {
        self.probing.is_some()
    }

    pub fn stats(&self) -> HealthStats {
        self.stats
    }

    /// What is known about `agent`.
    pub fn health(&self, agent: Agent) -> &Health {
        self.health
            .iter()
            .find(|health| health.agent == agent)
            .expect("one health per agent")
    }

    /// The health of every agent, in listing order.
    pub fn all(&self) -> &[Health] {
        &self.health
    }

    /// The agents `chat: new` may offer.
    pub fn offered(&self) -> impl Iterator<Item = &Health> {
        self.health.iter().filter(|health| health.is_offered())
    }

    /// Probes again, unless a round is running or the last one started less than
    /// [`MIN_REFRESH_INTERVAL`] ago. Call when the panel opens and before `chat: new`. Does nothing
    /// while chats are off.
    pub fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        let recent = self
            .last_round
            .is_some_and(|started| started.elapsed() < MIN_REFRESH_INTERVAL);
        if self.probing.is_some() || recent {
            return;
        }
        self.refresh_now(cx);
    }

    /// Probes again now, replacing a round that is running. Does nothing while chats are off.
    pub fn refresh_now(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.enabled {
            return;
        }
        let commands = Self::commands(cx);
        self.probed_commands = commands
            .iter()
            .map(|(_, command)| command.clone())
            .collect();
        self.last_round = Some(Instant::now());
        self.stats.rounds += 1;
        let env = self.env.clone();
        let cache = self.cache.clone();
        let timeout = self.timeout;
        let executor = cx.background_executor().clone();
        self.probing = Some(cx.spawn(async move |this, cx| {
            let probes = commands.into_iter().map(|(agent, command)| {
                probe_agent(
                    agent,
                    command,
                    env.clone(),
                    cache.clone(),
                    executor.clone(),
                    timeout,
                )
            });
            let results = cx.background_spawn(futures::future::join_all(probes)).await;
            this.update(cx, |this, cx| this.apply(results, cx)).ok();
        }));
    }

    fn commands(cx: &App) -> Vec<(Agent, String)> {
        let settings = ChatsSettings::get_global(cx);
        vec![
            (Agent::Claude, settings.claude.command.clone()),
            (Agent::Codex, settings.codex.command.clone()),
        ]
    }

    fn apply(&mut self, results: Vec<Probed>, cx: &mut gpui::Context<Self>) {
        self.probing = None;
        let mut changed = false;
        for probed in results {
            self.stats.help_reads += probed.help_reads;
            if let Some((key, caps)) = probed.learned {
                self.cache.insert(key, caps);
            }
            changed |= self.set_health(probed.health);
        }
        if changed {
            cx.emit(ChatHealthEvent::HealthChanged);
            cx.notify();
        }
    }

    /// Stores `health`; whether it differs from what was there.
    fn set_health(&mut self, health: Health) -> bool {
        let slot = self
            .health
            .iter_mut()
            .find(|slot| slot.agent == health.agent)
            .expect("one health per agent");
        if *slot == health {
            return false;
        }
        log::info!(
            "chats: {} health is {:?}",
            health.agent.as_str(),
            health.state
        );
        *slot = health;
        true
    }

    /// Follows `chats.enabled` and the configured commands.
    fn sync(&mut self, cx: &mut gpui::Context<Self>) {
        let enabled = ChatsSettings::get_global(cx).enabled;
        if !enabled {
            if self.enabled {
                self.enabled = false;
                self.probing = None;
                self.last_round = None;
                self.probed_commands.clear();
                let mut changed = false;
                for agent in AGENTS {
                    changed |= self.set_health(Health::unchecked(agent, fix_not_probed(agent)));
                }
                if changed {
                    cx.emit(ChatHealthEvent::HealthChanged);
                    cx.notify();
                }
            }
            return;
        }
        let commands: Vec<String> = Self::commands(cx)
            .into_iter()
            .map(|(_, command)| command)
            .collect();
        if !self.enabled {
            self.enabled = true;
            self.refresh_now(cx);
        } else if commands != self.probed_commands {
            self.refresh_now(cx);
        }
    }
}
