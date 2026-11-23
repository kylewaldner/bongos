//! [`ChatRuntime`]: the running chats (design D11 and D12).
//!
//! The runtime is global, not per workspace: it holds the strong handle of each chat's terminal,
//! and a project keeps only weak ones, so closing the workspace that started a chat does not end
//! it. A launch writes the chat's runtime directory, writes the record (the session id a Claude
//! launch chooses, the digest of the given artifacts, `last_active`), and runs the CLI through
//! `Project::create_terminal_task` on a local project, which resolves the shell environment of the
//! chat's directory. A remote project would start the CLI on the remote host, where the chat's
//! files do not exist, so it is never used.
//!
//! Live state comes from one reducer (`bongos_chat_model::channel::state`, design D10), fed with the
//! chat's verified reports and its terminal signals: a bell (waiting), output (running), no output
//! for `chats.idle_after_secs` (idle), new keyboard input (running) and the end of the process
//! (exited). A report sets an exact state; a terminal signal sets an estimated one, and only where
//! the channel cannot answer, except that the idle timer still ends exact running and keyboard input
//! still ends exact waiting, because an interrupted turn or a denied permission ends without a
//! report. Keyboard input is a polled counter of the terminal, read on every wakeup and at close;
//! once seen, the chat counts as prompted. A state change rewrites `last_active`, at most once
//! per [`ACTIVE_DEBOUNCE`] per chat, except a change caused only by terminal output and a change
//! back to the state of the last write; a title change never does. The newest change of any kind
//! is kept in memory ([`ChatRuntime::live_activity`]), which the lists sort live chats by.
//!
//! Agents that report their session id only in their own files (Codex) are launched one at a time
//! per directory: the next launch in a directory waits until the previous one's id was captured
//! and claimed, at most [`CAPTURE_WINDOW`].
//!
//! Nothing starts while `chats.enabled` is false.
//!
//! With `chats.channels` on and the report server running, a Claude launch whose version has hooks
//! gets a chat channel (design D2, D3, D9): a fresh token, registered with the server before the
//! terminal starts and forgotten when that launch exits, is closed or loses its channel. A resume
//! is a new launch with a new token, so the previous process's reports are rejected. A channel is
//! pending until its first verified report and live after it. A launch with a channel that exits
//! with an error within [`EARLY_EXIT`] before any report is started once more without it. Turning
//! `chats.channels` off moves every chat's channel to off. Codex never gets a channel (D7).
//! A report reaches only the chat it names, and only while it carries that chat's current token;
//! any other report is dropped silently (D4). A Claude chat's session id is chosen before launch
//! and needs no capture; a `session_start` report naming another id (a fork launched without
//! `--session-id`) replaces it (D11). A new chat with a live channel is judged by the cleanup from
//! its reports, without reading the CLI's files.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bongos_chat_model::channel::report::{Event, Report, Token};
use bongos_chat_model::channel::state::{
    self as reducer, BELL_GRACE, ChannelCoverage, ChatLive, Input,
};
pub use bongos_chat_model::channel::state::{ChannelStatus, ChatState, MID_TURN, Source};
use bongos_chat_model::{
    Agent, Capabilities, CaptureQuery, ChannelOff, ChannelPlan, Fallback, HOOKS, LaunchKind,
    NamedBy, VERSION_TABLE, adapter_for, from_title,
};
use bongos_work_model::{SharedArtifact, Timestamp};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{self, Shared};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task, WeakEntity,
};
use project::Project;
use settings::{Settings as _, SettingsStore};
use terminal::{Event as TerminalEvent, Terminal};

#[cfg(unix)]
use crate::channel_server::{ChannelServer, Registration};
use crate::cleanup::{Cleanup, cleanup_verdict_with_reports};
use crate::launch::{self, LaunchError, LaunchRequest, Prepared};
use crate::{ChatHealth, ChatStore, ChatsSettings, find_program, fix_not_found, fix_not_probed};

/// Without a report server there is nothing to register with.
#[cfg(not(unix))]
pub(crate) struct Registration;

/// The shortest time between two writes of one chat's `last_active`.
pub const ACTIVE_DEBOUNCE: Duration = Duration::from_secs(10);
/// How long the runtime looks for a session id the CLI does not report, polling every
/// [`CAPTURE_INTERVAL`]; it looks once more when the CLI exits.
pub const CAPTURE_WINDOW: Duration = Duration::from_secs(60);
pub const CAPTURE_INTERVAL: Duration = Duration::from_secs(1);
/// A launch with a channel that exits with an error this soon, before any report, is started once
/// more without the channel (design D9).
pub const EARLY_EXIT: Duration = Duration::from_secs(5);

/// Why a chat has no channel while `chats.channels` is false, or since it turned false.
pub const REPORTS_OFF: &str = "Reports are off (chats.channels)";

/// The runtime's clock for the reducer: milliseconds since the runtime was made. Tests move it
/// forward instead of waiting (see [`ChatRuntime::advance_clock`]).
#[derive(Clone)]
struct Clock {
    epoch: Instant,
    skew_ms: Arc<AtomicU64>,
}

impl Clock {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            skew_ms: Arc::default(),
        }
    }

    fn now_ms(&self) -> u64 {
        let elapsed = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        elapsed.saturating_add(self.skew_ms.load(Ordering::Relaxed))
    }
}

/// The first letter in upper case, for a reason shown on its own.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatRuntimeEvent {
    /// The live state, title or session id of the chat with this id changed, or it started or
    /// closed.
    Changed(String),
}

/// Counters of what the runtime did, for tests and logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeStats {
    pub launches: usize,
    /// `last_active` writes asked of the store.
    pub active_writes: usize,
    /// Session ids captured from the CLI's files.
    pub captures: usize,
    /// Records the never-prompted cleanup deleted.
    pub cleanups: usize,
    /// Launches started again without their channel after an early exit.
    pub relaunches: usize,
}

/// One running (or exited, still shown) chat.
pub struct LiveChat {
    terminal: Entity<Terminal>,
    agent: Agent,
    /// The reducer's view: state, source, channel status and the times it needs.
    live: ChatLive,
    /// Why the launch has no channel, while its channel is off.
    off_reason: Option<String>,
    clock: Clock,
    title: Option<String>,
    started: SystemTime,
    given: Vec<SharedArtifact>,
    fallbacks: Vec<Fallback>,
    missing_skills: Vec<String>,
    record_path: PathBuf,
    cwd: PathBuf,
    command: String,
    session_id: Option<String>,
    prompted: bool,
    /// A new chat, never prompted and with no session id before this launch: its conversation, if
    /// any, starts in this run (design D11, for the cleanup).
    fresh: bool,
    /// A `prompt_submitted` or `turn_done` report of this launch was accepted.
    prompt_reported: bool,
    /// How many `prompt_submitted` reports of this launch the reducer applied; a send compares it
    /// before and after its Enter to see the submit confirmed.
    prompt_submits: u64,
    /// Something was pasted through [`crate::send`]; the send names the chat, not a title.
    sent: bool,
    /// The chat has a name other than its default one, or a title already tried to name it.
    auto_named: bool,
    /// The terminal's keyboard input count already fed to the reducer.
    seen_inputs: u64,
    /// When (on the runtime's clock) the last report of the current launch was accepted; the idle
    /// timer counts quiet time from it as from output.
    last_report_seen: Option<u64>,
    /// When `last_active` was last written, on the runtime's clock.
    last_active_write: Option<u64>,
    /// The state when `last_active` was last written (or at launch).
    written_state: ChatState,
    /// The newest time of any state change, in memory; the record may lag it.
    last_active: Timestamp,
    pending_active_write: Option<Task<()>>,
    /// The id is still to be read from the CLI's files when it exits.
    capture_on_exit: bool,
    /// The never-prompted cleanup already looked at this run of the chat.
    cleanup_checked: bool,
    /// This launch's channel token while it is registered; never written anywhere.
    token: Option<Token>,
    /// Keeps the token registered with the report server; dropping it forgets the token.
    registration: Option<Registration>,
    /// When the terminal started, for [`EARLY_EXIT`].
    launched: Instant,
    /// What a launch with a channel needs to start again without it after an early exit.
    relaunch: Option<(LaunchRequest, WeakEntity<Project>)>,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl LiveChat {
    /// Whether the launch reports over the chat channel.
    pub fn channel(&self) -> ChannelStatus {
        self.live.channel
    }

    /// Why the launch has no channel, while its channel is off.
    pub fn channel_off_reason(&self) -> Option<&str> {
        self.off_reason.as_deref()
    }

    /// Where the state comes from.
    pub fn state_source(&self) -> Source {
        self.live.source
    }

    /// Why the state is estimated, as the text of the "estimated" tag's tooltip; `None` when the
    /// state is exact. The reason names the tool version where it has one.
    pub fn estimate_reason(&self) -> Option<String> {
        let Source::Estimated(reason) = self.live.source else {
            return None;
        };
        Some(match self.live.channel {
            ChannelStatus::Off => match &self.off_reason {
                Some(off) => off.clone(),
                None => sentence(reason),
            },
            ChannelStatus::Pending => {
                format!("No report from {} yet", sentence(self.agent.as_str()))
            }
            ChannelStatus::Live => sentence(reason),
        })
    }

    /// The reporter's time (`at`, in milliseconds) of the newest applied report.
    pub fn last_report_at(&self) -> Option<u64> {
        self.live.last_report_at
    }

    /// Whether a channel token of this launch is registered.
    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    /// Forgets the channel token (the server no longer accepts it) and turns the channel off because
    /// `chats.channels` is off. The state is kept, estimated.
    fn drop_channel(&mut self) {
        self.registration = None;
        self.token = None;
        self.relaunch = None;
        self.live.source = Source::Estimated(REPORTS_OFF);
        self.live.channel = ChannelStatus::Off;
        self.off_reason = Some(REPORTS_OFF.to_string());
    }

    /// Whether the bell rang less than [`BELL_GRACE`] ago.
    fn just_rang(&self) -> bool {
        let now = self.clock.now_ms();
        self.live
            .last_bell_at
            .is_some_and(|bell| Duration::from_millis(now.saturating_sub(bell)) < BELL_GRACE)
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn state(&self) -> ChatState {
        self.live.state
    }

    /// The title the CLI set with an OSC sequence.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn started(&self) -> SystemTime {
        self.started
    }

    /// The shared artifacts the chat was given at launch. Never written anywhere.
    pub fn given(&self) -> &[SharedArtifact] {
        &self.given
    }

    /// The fallbacks the launch used, for the row.
    pub fn fallbacks(&self) -> &[Fallback] {
        &self.fallbacks
    }

    /// The skills of the session type that were not found at launch, for the row.
    pub fn missing_skills(&self) -> &[String] {
        &self.missing_skills
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Whether anything was typed or sent into the chat.
    pub fn prompted(&self) -> bool {
        self.prompted
    }

    /// Whether the channel reported a prompt or a finished turn in this launch.
    pub fn prompt_reported(&self) -> bool {
        self.prompt_reported
    }

    /// What the reports say about a stored conversation, for the cleanup: `Some` only for a new
    /// Claude chat whose channel is live (it went live in this run and was never turned off).
    fn reported_conversation(&self) -> Option<bool> {
        (self.agent == Agent::Claude && self.fresh && self.live.channel == ChannelStatus::Live)
            .then_some(self.prompt_reported)
    }

    pub fn agent(&self) -> Agent {
        self.agent
    }

    /// Whether the CLI is working on a turn: exact running on a live channel, else running with
    /// output in the last [`MID_TURN`].
    pub fn is_mid_turn(&self) -> bool {
        reducer::is_mid_turn(&self.live, self.clock.now_ms())
    }

    pub(crate) fn record_path(&self) -> &PathBuf {
        &self.record_path
    }

    /// Whether the state comes from a live channel that reports `running`, so a send can wait for
    /// the report of its submit.
    pub(crate) fn reports_submits(&self) -> bool {
        self.live.channel == ChannelStatus::Live && self.live.coverage.covers(ChatState::Running)
    }

    /// The number of applied `prompt_submitted` reports of this launch.
    pub(crate) fn prompt_submits(&self) -> u64 {
        self.prompt_submits
    }

    /// When (on the runtime's clock) the terminal last produced output.
    pub(crate) fn last_output_at(&self) -> Option<u64> {
        self.live.last_output_at
    }
}

/// A launch's place in the per-directory queue of launches that capture their session id.
struct CaptureSlot {
    generation: usize,
    done: Shared<oneshot::Receiver<()>>,
}

/// Removes a chat id from the launching set when dropped: at the end of its launch task, or when
/// the task is dropped before it finished, so the chat can be launched again.
struct LaunchGuard {
    launching: Arc<Mutex<HashSet<String>>>,
    chat_id: String,
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        self.launching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.chat_id);
    }
}

/// The global runtime of chats. See the module documentation.
pub struct ChatRuntime {
    store: Entity<ChatStore>,
    home: PathBuf,
    live: HashMap<String, LiveChat>,
    /// Shared with each launch task's [`LaunchGuard`], which removes the id even when the task is
    /// dropped without being awaited.
    launching: Arc<Mutex<HashSet<String>>>,
    capture_slots: HashMap<PathBuf, CaptureSlot>,
    next_generation: usize,
    active_debounce: Duration,
    bracketed_wait: Duration,
    confirm_wait: Duration,
    /// Per chat, the last send in line and its number: a send waits until the one before it is
    /// done with its confirmation (confirmed, unconfirmed or given up), so two pastes never
    /// interleave.
    send_queues: HashMap<String, (usize, Shared<oneshot::Receiver<()>>)>,
    next_send: usize,
    /// The agent of the last chat `chat: new` started, offered first next time.
    last_agent: Option<Agent>,
    stats: RuntimeStats,
    clock: Clock,
    early_exit: Duration,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ChatRuntimeEvent> for ChatRuntime {}

struct GlobalChatRuntime(Entity<ChatRuntime>);

impl Global for GlobalChatRuntime {}

impl ChatRuntime {
    /// `home` is where `~/.bongos` and the agents' own session files are.
    pub fn new(store: Entity<ChatStore>, home: PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            store,
            home,
            live: HashMap::new(),
            launching: Arc::default(),
            capture_slots: HashMap::new(),
            next_generation: 0,
            active_debounce: ACTIVE_DEBOUNCE,
            bracketed_wait: crate::send::BRACKETED_WAIT,
            confirm_wait: crate::send::CONFIRM_WAIT,
            send_queues: HashMap::new(),
            next_send: 0,
            last_agent: None,
            stats: RuntimeStats::default(),
            clock: Clock::new(),
            early_exit: EARLY_EXIT,
            _subscriptions: vec![cx.observe_global::<SettingsStore>(|this, cx| {
                this.end_when_disabled(cx);
                this.channels_off_when_disabled(cx);
            })],
        }
    }

    /// Moves every chat with a channel to off once `chats.channels` (or `chats.enabled`) is false
    /// (design D2, "Live to Off"): its token is forgotten, its state is kept but estimated, and it
    /// does not get a channel back until its next launch.
    fn channels_off_when_disabled(&mut self, cx: &mut Context<Self>) {
        let settings = ChatsSettings::get_global(cx);
        if settings.enabled && settings.channels {
            return;
        }
        let mut changed = Vec::new();
        for (id, chat) in &mut self.live {
            if matches!(
                chat.live.channel,
                ChannelStatus::Pending | ChannelStatus::Live
            ) {
                chat.drop_channel();
                changed.push(id.clone());
            }
        }
        if changed.is_empty() {
            return;
        }
        for id in changed {
            cx.emit(ChatRuntimeEvent::Changed(id));
        }
        cx.notify();
    }

    /// A verified report from the server (design D3, D4, D10). It reaches only the chat it names,
    /// and counts only when it carries the token of that chat's current, still registered launch:
    /// a resume registers a new token after the server checked an older one, so a late report of
    /// the previous process is dropped here, silently. An accepted report goes to the reducer; the
    /// first one moves the channel from pending to live.
    pub(crate) fn on_report(&mut self, report: Report, cx: &mut Context<Self>) {
        let chat_id = report.chat.as_str().to_string();
        let now = self.clock.now_ms();
        let Some(chat) = self.live.get_mut(&chat_id) else {
            return;
        };
        let current = chat.token.is_some()
            && registration_is_current(chat.registration.as_ref(), &report.token)
            && matches!(
                chat.live.channel,
                ChannelStatus::Pending | ChannelStatus::Live
            );
        if !current {
            log::debug!("chats: dropped a report from an earlier launch");
            return;
        }
        chat.last_report_seen = Some(now);
        let went_live = chat.live.channel == ChannelStatus::Pending;
        chat.live.channel = ChannelStatus::Live;
        if matches!(report.event, Event::PromptSubmitted | Event::TurnDone) {
            chat.prompt_reported = true;
        }
        // The reducer drops a report older than the last one it applied; only an applied submit
        // confirms a send.
        let stale = chat
            .live
            .last_report_at
            .is_some_and(|last| report.at < last);
        if report.event == Event::PromptSubmitted && !stale && !chat.live.state.is_final() {
            chat.prompt_submits += 1;
        }
        let reported_session = match (&report.event, &report.session) {
            (Event::SessionStart { .. }, Some(session))
                if chat.session_id.as_deref() != Some(session.as_str()) =>
            {
                Some(session.as_str().to_string())
            }
            _ => None,
        };
        if let Some(session) = reported_session {
            self.write_reported_session(&chat_id, session, cx);
        }
        self.step(&chat_id, Input::Report(report), went_live, cx);
    }

    /// The `session_start` report of `chat_id` names `session`, which is not the id the chat has
    /// (only a fork launched without `--session-id` can do that): the reported id is the one the
    /// CLI stores the conversation under, so it replaces the chat's id, in the record too (D11).
    fn write_reported_session(&mut self, chat_id: &str, session: String, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        log::info!(
            "chats: {chat_id} reported session {session}, not {:?}; recording the reported id",
            chat.session_id
        );
        chat.session_id = Some(session.clone());
        let path = chat.record_path.clone();
        let write = self.store.update(cx, |store, cx| {
            store.update(
                path,
                move |record| {
                    record.session_id = Some(session);
                    Ok(())
                },
                cx,
            )
        });
        cx.background_spawn(async move {
            if let Err(error) = write.await {
                log::warn!("chats: could not record the reported session id: {error}");
            }
        })
        .detach();
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
    }

    /// Feeds `input` to the reducer of `chat_id`. A change of state or source is told, and a state
    /// change also touches `last_active`; a change that only advances the last applied report time
    /// is not told, unless `changed` says something else about the chat changed.
    fn step(&mut self, chat_id: &str, input: Input, changed: bool, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        let before = (chat.live.state, chat.live.source);
        let noise = matches!(input, Input::Output { .. });
        let typed = matches!(input, Input::KeyboardInput);
        chat.live.step(input);
        let after = (chat.live.state, chat.live.source);
        if before != after {
            // Keyboard input from before this change does not end the new state.
            chat.seen_inputs = chat.terminal.read(cx).keyboard_input_count();
        }
        if before.0 != after.0 || typed {
            chat.last_active = bongos_work_model::now();
        }
        // Output alone, and a return to the state of the last write, leave the record alone;
        // typing is activity whatever the state, written through the debounce.
        if (typed || before.0 != after.0) && !noise && (typed || after.0 != chat.written_state) {
            self.touch_active(chat_id, cx);
        }
        if before != after || changed {
            cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
            cx.notify();
        }
    }

    /// The channel of a launch of `agent` with `caps`, with its token, or why it has none.
    /// `force` gains [`Fallback::ChannelOff`] when the reporter's path cannot be quoted.
    fn plan_channel(
        &self,
        agent: Agent,
        caps: &Capabilities,
        force: &mut Vec<Fallback>,
        cx: &App,
    ) -> Result<(ChannelPlan, Token), String> {
        if !ChatsSettings::get_global(cx).channels {
            return Err(REPORTS_OFF.to_string());
        }
        if agent != Agent::Claude {
            return Err(format!("{} does not report its state yet", agent.as_str()));
        }
        if let Some(off) = forced_off(force) {
            return Err(sentence(off.reason()));
        }
        if !caps.has(HOOKS) {
            return Err(hooks_missing(agent, caps));
        }
        let socket = report_socket(cx)?;
        let reporter = crate::ChannelConfig::global(cx)
            .map(|config| config.reporter.clone())
            .ok_or_else(|| "No reporter command is configured".to_string())?;
        let token = new_token();
        match bongos_chat_model::plan_channel(&reporter, socket, token.clone(), force) {
            Some(plan) => Ok((plan, token)),
            None => Err(sentence(ChannelOff::UnquotablePath.reason())),
        }
    }

    /// Ends every running chat once `chats.enabled` is false (design D24). The store no longer
    /// writes by then, so nothing is written: the records stay as they were, and a later enabled
    /// period lists them again.
    fn end_when_disabled(&mut self, cx: &mut Context<Self>) {
        if ChatsSettings::get_global(cx).enabled || self.live.is_empty() {
            return;
        }
        let ids: Vec<String> = self.live.keys().cloned().collect();
        for id in ids {
            self.kill(&id, cx);
            self.live.remove(&id);
            cx.emit(ChatRuntimeEvent::Changed(id));
        }
        cx.notify();
    }

    /// The global runtime, once [`crate::init`] has run.
    pub fn global(cx: &App) -> Option<Entity<ChatRuntime>> {
        cx.try_global::<GlobalChatRuntime>()
            .map(|global| global.0.clone())
    }

    pub(crate) fn set_global(runtime: Entity<ChatRuntime>, cx: &mut App) {
        cx.set_global(GlobalChatRuntime(runtime));
    }

    /// The in-memory last activity of every live chat, newer than its record's `last_active`
    /// when a write is still debounced. The lists sort live chats by it.
    pub fn live_activity(&self) -> HashMap<String, Timestamp> {
        self.live
            .iter()
            .map(|(id, chat)| (id.clone(), chat.last_active))
            .collect()
    }

    /// Sets the in-memory last activity of `chat_id`, so a test can tell a later write from it.
    #[cfg(test)]
    pub(crate) fn set_live_activity(&mut self, chat_id: &str, at: Timestamp) {
        if let Some(chat) = self.live.get_mut(chat_id) {
            chat.last_active = at;
        }
    }

    /// A send pressed Enter in `chat_id`: it is active now, and the record says so at once.
    pub(crate) fn note_submitted(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        chat.last_active = bongos_work_model::now();
        cx.notify();
    }

    /// Replaces [`ACTIVE_DEBOUNCE`] (tests use a short one).
    pub fn set_active_debounce(&mut self, debounce: Duration) {
        self.active_debounce = debounce;
    }

    /// Replaces [`crate::send::BRACKETED_WAIT`] (tests use a short one).
    pub fn set_bracketed_wait(&mut self, wait: Duration) {
        self.bracketed_wait = wait;
    }

    /// Replaces [`EARLY_EXIT`] (tests use a long one so a slow start still counts as early).
    pub fn set_early_exit(&mut self, early_exit: Duration) {
        self.early_exit = early_exit;
    }

    /// Moves the runtime's clock forward by `by` and runs every chat's idle check, as if that much
    /// time had passed without output.
    #[cfg(test)]
    pub(crate) fn advance_clock(&mut self, by: Duration, cx: &mut Context<Self>) {
        let by = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.clock.skew_ms.fetch_add(by, Ordering::Relaxed);
        let ids: Vec<String> = self.live.keys().cloned().collect();
        for id in ids {
            self.check_idle(&id, cx);
        }
    }

    pub(crate) fn bracketed_wait(&self) -> Duration {
        self.bracketed_wait
    }

    /// Replaces [`crate::send::CONFIRM_WAIT`] (tests use a short one).
    pub fn set_confirm_wait(&mut self, wait: Duration) {
        self.confirm_wait = wait;
    }

    pub(crate) fn confirm_wait(&self) -> Duration {
        self.confirm_wait
    }

    /// The runtime's clock, in milliseconds.
    pub(crate) fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Puts a send to `chat_id` in line. Returns what it waits for (the send before it), the
    /// sender to drop once its confirmation is done (confirmed, unconfirmed or given up), and its
    /// number for [`Self::leave_send_queue`].
    pub(crate) fn join_send_queue(
        &mut self,
        chat_id: &str,
    ) -> (
        Option<Shared<oneshot::Receiver<()>>>,
        oneshot::Sender<()>,
        usize,
    ) {
        let (done, done_rx) = oneshot::channel();
        self.next_send += 1;
        let number = self.next_send;
        let previous = self
            .send_queues
            .insert(chat_id.to_string(), (number, done_rx.shared()));
        (previous.map(|(_, previous)| previous), done, number)
    }

    /// Forgets the queue of `chat_id` when send `number` is still its last.
    pub(crate) fn leave_send_queue(&mut self, chat_id: &str, number: usize) {
        if self
            .send_queues
            .get(chat_id)
            .is_some_and(|(last, _)| *last == number)
        {
            self.send_queues.remove(chat_id);
        }
    }

    pub(crate) fn store(&self) -> &Entity<ChatStore> {
        &self.store
    }

    pub fn stats(&self) -> RuntimeStats {
        self.stats
    }

    /// Where `~/.bongos` and the agents' own files are.
    pub fn home(&self) -> &PathBuf {
        &self.home
    }

    /// The agent of the last chat `chat: new` started in this session.
    pub fn last_agent(&self) -> Option<Agent> {
        self.last_agent
    }

    /// Remembers `agent` as the last one `chat: new` started, in memory and in the key value
    /// store, so the next run offers it first too.
    pub(crate) fn set_last_agent(&mut self, agent: Agent, cx: &mut Context<Self>) {
        self.last_agent = Some(agent);
        crate::view_state::write(
            crate::view_state::LAST_AGENT_KEY,
            Some(agent.as_str().to_string()),
            cx,
        );
    }

    /// The live chat with `chat_id`, running or exited.
    pub fn live(&self, chat_id: &str) -> Option<&LiveChat> {
        self.live.get(chat_id)
    }

    /// The ids of every live chat.
    pub fn live_ids(&self) -> impl Iterator<Item = &str> {
        self.live.keys().map(String::as_str)
    }

    /// The state of `chat_id`: [`ChatState::Closed`] when it has no live terminal.
    pub fn state(&self, chat_id: &str) -> ChatState {
        self.live
            .get(chat_id)
            .map_or(ChatState::Closed, |chat| chat.live.state)
    }

    /// Starts the chat of `request` in a terminal of the first local project of `projects` (the
    /// active workspace's project first). Refused when chats are off, when no project is local
    /// ("Chats run locally; open a local project first"), when the chat is live, or when the CLI
    /// cannot be found or was not probed yet.
    pub fn launch(
        &mut self,
        request: LaunchRequest,
        projects: &[Entity<Project>],
        cx: &mut Context<Self>,
    ) -> Task<Result<(), LaunchError>> {
        match self.begin_launch(request, projects, cx) {
            Ok(task) => task,
            Err(error) => Task::ready(Err(error)),
        }
    }

    fn begin_launch(
        &mut self,
        mut request: LaunchRequest,
        projects: &[Entity<Project>],
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<(), LaunchError>>, LaunchError> {
        let settings = ChatsSettings::get_global(cx).clone();
        if !settings.enabled {
            return Err(LaunchError::Off);
        }
        let chat_id = request.record.id.clone();
        if self.live.contains_key(&chat_id) || self.is_launching(&chat_id) {
            return Err(LaunchError::AlreadyRunning);
        }
        let project = projects
            .iter()
            .find(|project| project.read(cx).is_local())
            .cloned()
            .ok_or(LaunchError::NoLocalProject)?;
        let agent = request
            .record
            .known_agent()
            .ok_or_else(|| LaunchError::UnknownAgent(request.record.agent.clone()))?;
        let command = match agent {
            Agent::Claude => settings.claude,
            Agent::Codex => settings.codex,
        };
        let health = ChatHealth::global(cx);
        let base_env = health
            .as_ref()
            .map(|health| health.read(cx).env().clone())
            .unwrap_or_default();
        let caps = request
            .caps
            .clone()
            .or_else(|| {
                health
                    .as_ref()
                    .and_then(|health| health.read(cx).health(agent).chat_caps.clone())
            })
            .ok_or_else(|| LaunchError::NotProbed(fix_not_probed(agent)))?;
        let program = find_program(&command.command, &base_env)
            .ok_or_else(|| LaunchError::NotFound(fix_not_found(agent, &command.command)))?;
        let runtime_root = self.store.read(cx).dirs().runtime.clone();
        // Kept before the channel is planned, for the relaunch without it (design D9).
        let original = request.clone();
        // Only a chat that never had a conversation lets its reports answer the cleanup (D11): a
        // resume, a fork or a record with an id may hold one from before.
        let fresh = request.kind == LaunchKind::New
            && request.record.session_id.is_none()
            && !request.record.prompted;
        let (plan, token, off_reason) =
            match self.plan_channel(agent, &caps, &mut request.force, cx) {
                Ok((plan, token)) => (Some(plan), Some(token), None),
                Err(reason) => (None, None, Some(reason)),
            };
        let prepared = launch::prepare_with_channel(
            &request,
            &runtime_root,
            &self.home,
            &command,
            &caps,
            plan,
        )?;
        // The token is registered before the terminal starts, and only when the launch really
        // carries it; the registration lives as long as this launch (dropped if the launch fails).
        let channel = match token.filter(|_| prepared.spec.channel) {
            Some(token) => match register_launch(&chat_id, &token, cx) {
                Some(registration) => LaunchChannel {
                    status: ChannelStatus::Pending,
                    off_reason: None,
                    token: Some(token),
                    registration: Some(registration),
                    relaunch: Some((original, project.downgrade())),
                },
                None => LaunchChannel::off("report socket unavailable: the report server stopped"),
            },
            None => LaunchChannel::off(off_reason.as_deref().unwrap_or_else(|| {
                forced_off(&prepared.spec.fallbacks)
                    .map_or("the CLI takes no settings file", ChannelOff::reason)
            })),
        };

        // A launch that must capture its session id waits for the previous such launch in the
        // same directory to capture (and claim) its own.
        let capture = prepared.spec.capture_session_id.then(|| {
            let (done, done_rx) = oneshot::channel::<()>();
            self.next_generation += 1;
            let generation = self.next_generation;
            let previous = self.capture_slots.insert(
                prepared.spec.cwd.clone(),
                CaptureSlot {
                    generation,
                    done: done_rx.shared(),
                },
            );
            (generation, done, previous.map(|slot| slot.done))
        });
        let (capture_generation, capture_done, previous_capture) = match capture {
            Some((generation, done, previous)) => (Some(generation), Some(done), previous),
            None => (None, None, None),
        };

        self.launching_set().insert(chat_id.clone());
        let guard = LaunchGuard {
            launching: self.launching.clone(),
            chat_id: chat_id,
        };
        let label = format!("{}: {}", agent.as_str(), prepared.record.name);
        let store = self.store.clone();
        let record_path = request.record_path;
        let task_id = request.task_id;
        let started_chat = prepared.record.id.clone();
        Ok(cx.spawn(async move |this, cx| {
            let result = async {
                if let Some(previous) = previous_capture {
                    let timeout = cx.background_executor().timer(CAPTURE_WINDOW);
                    future::select(previous, timeout).await;
                }
                let files = prepared.files.clone();
                let dir = prepared.dir.clone();
                cx.background_spawn(async move {
                    launch::write_runtime_dir(&runtime_root, &dir, &files)
                })
                .await
                .map_err(|error| LaunchError::Files(error.to_string()))?;

                let session_id = prepared.record.session_id.clone();
                let digest = prepared.digest.clone();
                let write = store.update(cx, |store, cx| {
                    store.update(
                        record_path.clone(),
                        move |record| {
                            if session_id.is_some() {
                                record.session_id = session_id;
                            }
                            let now = bongos_work_model::now();
                            record.context_digest = Some(digest);
                            record.context_given = Some(now);
                            record.last_active = now;
                            Ok(())
                        },
                        cx,
                    )
                });
                write.await.map_err(LaunchError::Record)?;

                let spawn = launch::spawn_in_terminal(
                    &prepared.record.id,
                    label,
                    &program,
                    &prepared.spec,
                    &base_env,
                );
                let started = SystemTime::now();
                let terminal = project
                    .update(cx, |project, cx| project.create_terminal_task(spawn, cx))
                    .await
                    .map_err(|error| LaunchError::Spawn(format!("{error:#}")))?;
                this.update(cx, |this, cx| {
                    this.register(
                        prepared,
                        record_path,
                        command.command,
                        terminal,
                        started,
                        capture_done,
                        channel,
                        fresh,
                        cx,
                    );
                    if let Some(task_id) = &task_id {
                        this.start_todo(&started_chat, task_id, cx);
                    }
                })
                .map_err(|error| LaunchError::Spawn(format!("{error:#}")))?;
                Ok(())
            }
            .await;
            drop(guard);
            this.update(cx, |this, _| {
                if let Some(generation) = capture_generation
                    && result.is_err()
                {
                    this.release_capture_slot(generation);
                }
            })
            .ok();
            result
        }))
    }

    fn launching_set(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.launching.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_launching(&self, chat_id: &str) -> bool {
        self.launching_set().contains(chat_id)
    }

    /// The chat `chat_id` started on the leaf `task_id`: its current todo starts if it had not
    /// (design D23).
    fn start_todo(&mut self, chat_id: &str, task_id: &str, cx: &mut Context<Self>) {
        let work = self.store.read(cx).work().clone();
        let start = crate::live_modes::start_current_todo(&work, chat_id, task_id, cx);
        cx.spawn(async move |_, _| {
            if let Err(error) = start.await {
                log::warn!("chats: could not start the task's current todo: {error}");
            }
        })
        .detach();
    }

    fn release_capture_slot(&mut self, generation: usize) {
        self.capture_slots
            .retain(|_, slot| slot.generation != generation);
    }

    #[allow(clippy::too_many_arguments)]
    fn register(
        &mut self,
        prepared: Prepared,
        record_path: PathBuf,
        command: String,
        terminal: Entity<Terminal>,
        started: SystemTime,
        capture_done: Option<oneshot::Sender<()>>,
        channel: LaunchChannel,
        fresh: bool,
        cx: &mut Context<Self>,
    ) {
        let chat_id = prepared.record.id.clone();
        let subscription = cx.subscribe(&terminal, {
            let chat_id = chat_id.clone();
            move |this, terminal, event, cx| this.on_terminal_event(&chat_id, &terminal, event, cx)
        });
        let exit = terminal.read(cx).wait_for_completed_task(cx);
        let mut tasks = vec![
            cx.spawn({
                let chat_id = chat_id.clone();
                async move |this, cx| {
                    let status = exit.await;
                    this.update(cx, |this, cx| {
                        this.on_exit(&chat_id, status.and_then(|status| status.code()), cx)
                    })
                    .ok();
                }
            }),
            cx.spawn({
                let chat_id = chat_id.clone();
                async move |this, cx| {
                    loop {
                        let Ok(Some(delay)) =
                            this.update(cx, |this, cx| this.check_idle(&chat_id, cx))
                        else {
                            break;
                        };
                        cx.background_executor().timer(delay).await;
                    }
                }
            }),
        ];
        if let Some(done) = capture_done {
            let chat_id = chat_id.clone();
            tasks.push(cx.spawn(async move |this, cx| {
                let tries = (CAPTURE_WINDOW.as_millis() / CAPTURE_INTERVAL.as_millis()).max(1);
                let mut captured = false;
                for _ in 0..tries {
                    match capture_now(&this, &chat_id, cx).await {
                        Some(true) => {
                            captured = true;
                            break;
                        }
                        Some(false) => {}
                        None => return,
                    }
                    cx.background_executor().timer(CAPTURE_INTERVAL).await;
                }
                if !captured {
                    this.update(cx, |this, _| {
                        if let Some(chat) = this.live.get_mut(&chat_id) {
                            chat.capture_on_exit = true;
                        }
                    })
                    .ok();
                }
                // Dropping the sender lets the next launch in this directory go.
                drop(done);
            }));
        }

        // The terminal ran before this subscription: take the title it may already have.
        let title = terminal.read(cx).breadcrumb_text.trim().to_string();
        let seen_inputs = terminal.read(cx).keyboard_input_count();
        let coverage = match prepared.agent {
            Agent::Claude => ChannelCoverage::claude(),
            Agent::Codex => ChannelCoverage::none(),
        };
        let live = ChatLive::start(channel.status, coverage, self.clock.now_ms());
        let initial_state = live.state;
        let chat = LiveChat {
            terminal,
            agent: prepared.agent,
            live,
            off_reason: channel.off_reason,
            clock: self.clock.clone(),
            title: (!title.is_empty()).then_some(title),
            started,
            given: prepared.given,
            fallbacks: prepared.spec.fallbacks,
            missing_skills: prepared.missing_skills,
            record_path,
            cwd: prepared.spec.cwd,
            command,
            session_id: prepared.record.session_id,
            prompted: prepared.record.prompted,
            fresh,
            prompt_reported: false,
            prompt_submits: 0,
            sent: false,
            auto_named: prepared.record.named_by != NamedBy::None,
            seen_inputs,
            last_report_seen: None,
            last_active_write: Some(self.clock.now_ms()),
            written_state: initial_state,
            last_active: bongos_work_model::now(),
            pending_active_write: None,
            capture_on_exit: false,
            cleanup_checked: false,
            token: channel.token,
            registration: channel.registration,
            launched: Instant::now(),
            relaunch: channel.relaunch,
            _tasks: tasks,
            _subscriptions: vec![subscription],
        };
        self.live.insert(chat_id.clone(), chat);
        self.stats.launches += 1;
        // `chats.channels` may have turned off while the launch ran.
        self.channels_off_when_disabled(cx);
        cx.emit(ChatRuntimeEvent::Changed(chat_id));
        cx.notify();
    }

    fn on_terminal_event(
        &mut self,
        chat_id: &str,
        terminal: &Entity<Terminal>,
        event: &TerminalEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalEvent::Bell => {
                let now = self.clock.now_ms();
                self.step(chat_id, Input::Bell { now }, false, cx);
            }
            TerminalEvent::BreadcrumbsChanged => {
                let title = terminal.read(cx).breadcrumb_text.trim().to_string();
                let title = (!title.is_empty()).then_some(title);
                let Some(chat) = self.live.get_mut(chat_id) else {
                    return;
                };
                if chat.title != title {
                    chat.title = title;
                    self.name_from_title(chat_id, cx);
                    cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
                    cx.notify();
                }
            }
            TerminalEvent::Wakeup => {
                self.poll_input(chat_id, cx);
                let now = self.clock.now_ms();
                self.step(chat_id, Input::Output { now }, false, cx);
            }
            _ => {}
        }
    }

    /// Names a chat that is still called by its default name and was prompted only by typing from
    /// its title (design D16), once: the first title that is not the program's name or a spinner.
    fn name_from_title(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        let typed = chat.prompted || chat.terminal.read(cx).keyboard_input_sent();
        if chat.auto_named || chat.sent || !typed {
            return;
        }
        let Some(title) = chat.title.as_deref() else {
            return;
        };
        let program = std::path::Path::new(&chat.command)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| chat.command.clone());
        let name = from_title(title, &program)
            .filter(|_| from_title(title, crate::health::agent_title(chat.agent)).is_some())
            .filter(|_| from_title(title, chat.agent.as_str()).is_some());
        let Some(name) = name else {
            return;
        };
        chat.auto_named = true;
        let path = chat.record_path.clone();
        let write = self.store.update(cx, |store, cx| {
            store.update(
                path,
                move |record| {
                    crate::send::apply_name(record, name);
                    Ok(())
                },
                cx,
            )
        });
        cx.background_spawn(async move {
            if let Err(error) = write.await {
                log::warn!("chats: could not name the chat from its title: {error}");
            }
        })
        .detach();
    }

    /// The text of a send was pasted into the chat: it counts as prompted, and the send, not a
    /// title, names it.
    pub(crate) fn note_pasted(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        chat.sent = true;
        chat.auto_named = true;
        if !chat.prompted {
            chat.prompted = true;
            cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
            cx.notify();
        }
    }

    /// Writes bytes of a send into `terminal`, the chat's. Keyboard input typed before them still
    /// reaches the reducer first; the send's own bytes do not count as keyboard input (so a paste
    /// does not end an exact waiting state), and an Enter is told to the reducer as a send's Enter.
    pub(crate) fn write_send_input(
        &mut self,
        chat_id: &str,
        terminal: &Entity<Terminal>,
        enter: bool,
        write: impl FnOnce(&mut Terminal),
        cx: &mut Context<Self>,
    ) {
        self.poll_input(chat_id, cx);
        terminal.update(cx, |terminal, _| write(terminal));
        let count = terminal.read(cx).keyboard_input_count();
        if let Some(chat) = self.live.get_mut(chat_id)
            && chat.terminal == *terminal
        {
            chat.seen_inputs = count;
            if enter {
                self.step(chat_id, Input::SendEnter, false, cx);
            }
        }
    }

    /// Reads the terminal's keyboard input count. New input goes to the reducer, unless a bell just
    /// came: then the input is older than the bell's question. The first input marks the chat
    /// prompted.
    fn poll_input(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        let count = chat.terminal.read(cx).keyboard_input_count();
        if count == chat.seen_inputs {
            return;
        }
        chat.seen_inputs = count;
        let first = !std::mem::replace(&mut chat.prompted, true);
        let just_rang = chat.just_rang();
        if just_rang {
            chat.last_active = bongos_work_model::now();
            self.touch_active(chat_id, cx);
            if first {
                cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
                cx.notify();
            }
        } else {
            self.step(chat_id, Input::KeyboardInput, first, cx);
        }
    }

    /// Gives the reducer the idle timer when the chat had no output (and no report) for
    /// `chats.idle_after_secs`. Returns when to look again, or `None` once the chat is gone or
    /// exited.
    fn check_idle(&mut self, chat_id: &str, cx: &mut Context<Self>) -> Option<Duration> {
        let idle_after = Duration::from_secs(ChatsSettings::get_global(cx).idle_after_secs.max(1));
        let now = self.clock.now_ms();
        let chat = self.live.get(chat_id)?;
        if chat.live.state.is_final() {
            return None;
        }
        let last = chat
            .live
            .last_output_at
            .max(chat.last_report_seen)
            .unwrap_or(now);
        let quiet = Duration::from_millis(now.saturating_sub(last));
        if quiet < idle_after {
            return Some(idle_after - quiet);
        }
        self.step(chat_id, Input::IdleTimer, false, cx);
        Some(idle_after)
    }

    fn on_exit(&mut self, chat_id: &str, code: Option<i32>, cx: &mut Context<Self>) {
        self.poll_input(chat_id, cx);
        let early_exit = self.early_exit;
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        chat.live.step(Input::Exit { code });
        chat.last_active = bongos_work_model::now();
        let early_error = code.is_some_and(|code| code != 0)
            && chat.live.last_report_at.is_none()
            && chat.launched.elapsed() < early_exit;
        // The launch ended: its token is forgotten whatever happens next.
        chat.registration = None;
        chat.token = None;
        let relaunch = chat.relaunch.take().filter(|_| {
            early_error
                && matches!(
                    chat.live.channel,
                    ChannelStatus::Pending | ChannelStatus::Live
                )
        });
        if let Some(relaunch) = relaunch {
            // Started once more without the channel, in case the CLI rejected the hooks. The
            // record stays: the cleanup does not judge a launch that never got going.
            chat.cleanup_checked = true;
            log::warn!(
                "chats: {chat_id} exited with code {code:?} right after a launch with reports; \
                 starting it again without them"
            );
            let id = chat_id.to_string();
            cx.spawn(async move |this, cx| {
                this.update(cx, |this, cx| this.relaunch(&id, relaunch, cx))
                    .ok();
            })
            .detach();
            cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
            cx.notify();
            return;
        }
        let capture = std::mem::take(&mut chat.capture_on_exit);
        self.write_active_now(chat_id, cx);
        self.clean_up(chat_id, cx);
        if capture {
            let chat_id = chat_id.to_string();
            cx.spawn(async move |this, cx| {
                capture_now(&this, &chat_id, cx).await;
            })
            .detach();
        }
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
    }

    /// Starts `chat_id`, which exited early with a channel, once more without it (design D9):
    /// `request` gains [`ChannelOff::ExitedEarly`], so that launch never relaunches. Nothing happens
    /// when the chat was closed meanwhile or its project is gone.
    fn relaunch(
        &mut self,
        chat_id: &str,
        (mut request, project): (LaunchRequest, WeakEntity<Project>),
        cx: &mut Context<Self>,
    ) {
        let exited = self
            .live
            .get(chat_id)
            .is_some_and(|chat| matches!(chat.live.state, ChatState::Exited { .. }));
        let Some(project) = project.upgrade().filter(|_| exited) else {
            return;
        };
        self.live.remove(chat_id);
        let off = Fallback::ChannelOff(ChannelOff::ExitedEarly);
        if !request.force.contains(&off) {
            request.force.push(off);
        }
        self.stats.relaunches += 1;
        let launch = self.launch(request, &[project], cx);
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
        cx.spawn(async move |_, _| {
            if let Err(error) = launch.await {
                log::warn!("chats: the relaunch without reports failed: {error}");
            }
        })
        .detach();
    }

    /// Ends the chat's process (its foreground process group and the process itself). The chat
    /// stays live, exited, so its last output can still be shown. Returns whether it was live.
    pub fn kill(&mut self, chat_id: &str, cx: &mut Context<Self>) -> bool {
        let Some(chat) = self.live.get(chat_id) else {
            return false;
        };
        chat.terminal
            .update(cx, |terminal, _| terminal.kill_active_task());
        true
    }

    /// Ends the chat's process and drops its terminal: the chat is closed. `last_active` is written
    /// now, and the record stays unless the never-prompted cleanup is sure nothing was said in it
    /// (see [`crate::cleanup`]). Returns whether it was live.
    pub fn close(&mut self, chat_id: &str, cx: &mut Context<Self>) -> bool {
        if !self.live.contains_key(chat_id) {
            return false;
        }
        self.poll_input(chat_id, cx);
        self.kill(chat_id, cx);
        self.write_active_now(chat_id, cx);
        self.clean_up(chat_id, cx);
        // Dropping the terminal also ends the process if the kill did not.
        self.live.remove(chat_id);
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
        true
    }

    /// [`ChatRuntime::close`] for a chat that is about to be archived or deleted by Kyle: the
    /// never-prompted cleanup does not run, so the record is left to the caller.
    pub fn close_keeping_record(&mut self, chat_id: &str, cx: &mut Context<Self>) -> bool {
        if let Some(chat) = self.live.get_mut(chat_id) {
            chat.cleanup_checked = true;
        }
        self.close(chat_id, cx)
    }

    /// Asks for a `last_active` write: now when the last one is older than the debounce, else
    /// once the debounce has passed.
    fn touch_active(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let debounce = self.active_debounce;
        let now = self.clock.now_ms();
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        if chat.pending_active_write.is_some() {
            return;
        }
        let wait = chat
            .last_active_write
            .map(|last| debounce.saturating_sub(Duration::from_millis(now.saturating_sub(last))))
            .unwrap_or_default();
        if wait.is_zero() {
            self.write_active_now(chat_id, cx);
            return;
        }
        let id = chat_id.to_string();
        chat.pending_active_write = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |this, cx| this.write_active_now(&id, cx))
                .ok();
        }));
    }

    /// Writes `last_active` (and `prompted`, once set) now, replacing a pending write.
    fn write_active_now(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        if let Some(write) = self.write_active(chat_id, cx) {
            cx.background_spawn(write).detach();
        }
    }

    /// Queues the write of [`ChatRuntime::write_active_now`]; the future resolves when it is on
    /// disk (a failure is logged). `None` when the chat is not live.
    fn write_active(
        &mut self,
        chat_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<impl Future<Output = ()> + use<>> {
        let (path, prompted, active) = self.note_active_write(chat_id)?;
        let write = self.store.update(cx, |store, cx| {
            store.update(path, active_change(active, prompted), cx)
        });
        Some(async move {
            if let Err(error) = write.await {
                log::warn!("chats: could not write last_active: {error}");
            }
        })
    }

    /// Books an active write of `chat_id` (replacing a pending one): its record path and whether
    /// it is prompted, and its in-memory `last_active`. `None` when the chat is not live.
    fn note_active_write(&mut self, chat_id: &str) -> Option<(PathBuf, bool, Timestamp)> {
        let now = self.clock.now_ms();
        let chat = self.live.get_mut(chat_id)?;
        chat.pending_active_write = None;
        chat.last_active_write = Some(now);
        chat.written_state = chat.live.state;
        self.stats.active_writes += 1;
        Some((chat.record_path.clone(), chat.prompted, chat.last_active))
    }

    /// The names of the chats that are running (not exited), for the quit prompt. Empty while
    /// `chats.enabled` is false.
    pub fn running_names(&self, cx: &App) -> Vec<String> {
        if !ChatsSettings::get_global(cx).enabled {
            return Vec::new();
        }
        let store = self.store.read(cx);
        let mut names: Vec<String> = self
            .live
            .iter()
            .filter(|(_, chat)| !matches!(chat.live.state, ChatState::Exited { .. }))
            .map(|(id, _)| {
                store
                    .find(id)
                    .and_then(|found| found.entry.record())
                    .map_or_else(|| id.clone(), |record| record.name.clone())
            })
            .collect();
        names.sort();
        names
    }

    /// The app is quitting (design D18): ends every running chat's process and writes its
    /// `last_active`, so the Chats panel lists the chats after the next launch and `chat: open`
    /// resumes them. The records stay open, and the never-prompted cleanup does not run. The
    /// future resolves once the writes are on disk. It waits on the writer's own reply channels
    /// only, never on a main-thread task: `App::shutdown` blocks the main thread on it, and
    /// nothing on that thread runs meanwhile. Nothing happens while `chats.enabled` is false.
    pub(crate) fn end_for_quit(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl Future<Output = ()> + use<> {
        let mut writes = Vec::new();
        if ChatsSettings::get_global(cx).enabled {
            let ids: Vec<String> = self.live.keys().cloned().collect();
            for id in ids {
                self.poll_input(&id, cx);
                if let Some(chat) = self.live.get_mut(&id) {
                    chat.cleanup_checked = true;
                }
                self.kill(&id, cx);
                let Some((path, prompted, active)) = self.note_active_write(&id) else {
                    continue;
                };
                writes.push(self.store.update(cx, |store, _| {
                    store.update_reply(path, active_change(active, prompted))
                }));
            }
        }
        async move {
            for reply in future::join_all(writes).await {
                match reply {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => log::warn!("chats: could not write last_active: {error}"),
                    Err(_) => log::warn!("chats: the last_active write was dropped"),
                }
            }
        }
    }

    /// The never-prompted cleanup of `chat_id`, which ended or is being closed (design D17), once
    /// per run of the chat. It runs on the store's writer after the writes queued before it, so the
    /// record it reads already has the `prompted` the runtime saw. A deleted record's runtime
    /// directory goes too.
    fn clean_up(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let capture = self.capture_query(chat_id, cx);
        let runtime_dir = self.store.read(cx).dirs().runtime.join(chat_id);
        let home = self.home.clone();
        let env = ChatHealth::global(cx)
            .map(|health| health.read(cx).env().clone())
            .unwrap_or_default();
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        if std::mem::replace(&mut chat.cleanup_checked, true) {
            return;
        }
        let saw_input = chat.prompted || chat.terminal.read(cx).keyboard_input_sent();
        if saw_input {
            return;
        }
        let live_session = chat.session_id.clone();
        let reported = chat.reported_conversation();
        let path = chat.record_path.clone();
        let delete = self.store.update(cx, |store, cx| {
            store.delete_if(
                path,
                move |record| {
                    let verdict = cleanup_verdict_with_reports(
                        record,
                        saw_input,
                        reported,
                        live_session.as_deref(),
                        &home,
                        &env,
                        || capture.as_ref().and_then(CaptureInput::run),
                    );
                    log::info!("chats: cleanup of an unprompted chat: {verdict:?}");
                    verdict == Cleanup::Delete
                },
                cx,
            )
        });
        let chat_id = chat_id.to_string();
        cx.spawn(async move |this, cx| match delete.await {
            Ok(true) => {
                let removed = cx
                    .background_spawn(async move { std::fs::remove_dir_all(runtime_dir) })
                    .await;
                if let Err(error) = removed
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    log::warn!("chats: could not remove a deleted chat's launch files: {error}");
                }
                this.update(cx, |this, cx| {
                    this.stats.cleanups += 1;
                    cx.emit(ChatRuntimeEvent::Changed(chat_id));
                    cx.notify();
                })
                .ok();
            }
            Ok(false) => {}
            Err(error) => log::warn!("chats: the cleanup could not run: {error}"),
        })
        .detach();
    }

    /// What a capture of `chat_id`'s session id reads: `None` when the chat is gone or its id is
    /// known.
    fn capture_query(&self, chat_id: &str, cx: &App) -> Option<CaptureInput> {
        let chat = self.live.get(chat_id)?;
        if chat.session_id.is_some() {
            return None;
        }
        let mut claimed: BTreeSet<String> = self
            .store
            .read(cx)
            .index()
            .values()
            .filter(|indexed| indexed.entry.id() != chat_id)
            .filter_map(|indexed| indexed.entry.record()?.session_id.clone())
            .collect();
        claimed.extend(
            self.live
                .iter()
                .filter(|(id, _)| id.as_str() != chat_id)
                .filter_map(|(_, live)| live.session_id.clone()),
        );
        Some(CaptureInput {
            agent: chat.agent,
            command: chat.command.clone(),
            chat_id: chat_id.to_string(),
            cwd: chat.cwd.clone(),
            since: chat.started,
            claimed,
            home: self.home.clone(),
        })
    }

    fn set_session_id(&mut self, chat_id: &str, session_id: String, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        if chat.session_id.is_some() {
            return;
        }
        chat.session_id = Some(session_id.clone());
        self.stats.captures += 1;
        let path = chat.record_path.clone();
        let write = self.store.update(cx, |store, cx| {
            store.update(
                path,
                move |record| {
                    record.session_id = Some(session_id);
                    Ok(())
                },
                cx,
            )
        });
        cx.background_spawn(async move {
            if let Err(error) = write.await {
                log::warn!("chats: could not record the captured session id: {error}");
            }
        })
        .detach();
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
    }
}

pub(crate) struct CaptureInput {
    agent: Agent,
    command: String,
    chat_id: String,
    cwd: PathBuf,
    since: SystemTime,
    claimed: BTreeSet<String>,
    home: PathBuf,
}

impl CaptureInput {
    pub(crate) fn run(&self) -> Option<String> {
        adapter_for(self.agent, &self.command).capture_session_id(&CaptureQuery {
            chat_id: &self.chat_id,
            cwd: &self.cwd,
            since: self.since,
            claimed: &self.claimed,
            home: &self.home,
        })
    }
}

/// One look for `chat_id`'s session id in the CLI's files. `Some(true)` when it was found (or is
/// known), `Some(false)` when not yet, `None` when the chat is gone.
async fn capture_now(
    this: &gpui::WeakEntity<ChatRuntime>,
    chat_id: &str,
    cx: &mut gpui::AsyncApp,
) -> Option<bool> {
    let input = this
        .update(cx, |this, cx| {
            let known = this.live.get(chat_id)?.session_id.is_some();
            Some(if known {
                None
            } else {
                this.capture_query(chat_id, cx)
            })
        })
        .ok()??;
    let Some(input) = input else {
        return Some(true);
    };
    let found = cx.background_spawn(async move { input.run() }).await;
    let Some(session_id) = found else {
        return Some(false);
    };
    this.update(cx, |this, cx| this.set_session_id(chat_id, session_id, cx))
        .ok()?;
    Some(true)
}

/// The channel side of one launch, from the plan to the live chat.
struct LaunchChannel {
    status: ChannelStatus,
    off_reason: Option<String>,
    token: Option<Token>,
    registration: Option<Registration>,
    relaunch: Option<(LaunchRequest, WeakEntity<Project>)>,
}

impl LaunchChannel {
    fn off(reason: &str) -> Self {
        Self {
            status: ChannelStatus::Off,
            off_reason: Some(sentence(reason)),
            token: None,
            registration: None,
            relaunch: None,
        }
    }
}

/// The reason of a [`Fallback::ChannelOff`] in `fallbacks`, if one is there.
fn forced_off(fallbacks: &[Fallback]) -> Option<ChannelOff> {
    fallbacks.iter().find_map(|fallback| match fallback {
        Fallback::ChannelOff(off) => Some(*off),
        _ => None,
    })
}

/// Why `caps` give `agent` no hooks: its version is older than the table's first hooks row, or
/// could not be read.
fn hooks_missing(agent: Agent, caps: &Capabilities) -> String {
    let min = VERSION_TABLE
        .rows
        .iter()
        .find(|row| row.name == HOOKS)
        .map_or("unknown", |row| row.min_version);
    if caps.version().is_empty() {
        format!("The {} version could not be read", agent.as_str())
    } else {
        format!(
            "{} {} is older than the first version with hooks bongos knows ({min})",
            agent.as_str(),
            caps.version()
        )
    }
}

/// A fresh launch token: 32 random hex digits.
fn new_token() -> Token {
    Token::new(&uuid::Uuid::new_v4().simple().to_string()).expect("a simple UUID is a valid token")
}

/// The socket of the running report server, or why there is none.
#[cfg(unix)]
fn report_socket(cx: &App) -> Result<PathBuf, String> {
    let server = ChannelServer::global(cx)
        .ok_or_else(|| "Report socket unavailable: no report server".to_string())?;
    let server = server.read(cx);
    match server.socket_path() {
        Some(path) => Ok(path.to_path_buf()),
        None => Err(sentence(&server.unavailable_reason().unwrap_or_else(
            || "report socket unavailable: the report server is not running".to_string(),
        ))),
    }
}

#[cfg(not(unix))]
fn report_socket(_cx: &App) -> Result<PathBuf, String> {
    Err("Report socket unavailable: reports need a Unix socket".to_string())
}

/// Registers `token` as `chat_id`'s current launch token with the global server.
#[cfg(unix)]
fn register_launch(chat_id: &str, token: &Token, cx: &mut App) -> Option<Registration> {
    let chat = bongos_chat_model::channel::report::ChatId::new(chat_id)?;
    ChannelServer::global(cx)?.update(cx, |server, _| server.register_launch(chat, token.clone()))
}

#[cfg(not(unix))]
fn register_launch(_chat_id: &str, _token: &Token, _cx: &mut App) -> Option<Registration> {
    None
}

/// Whether `token` is the token `registration` still holds registered.
#[cfg(unix)]
fn registration_is_current(registration: Option<&Registration>, token: &Token) -> bool {
    registration.is_some_and(|registration| registration.is_current(token))
}

#[cfg(not(unix))]
fn registration_is_current(_registration: Option<&Registration>, _token: &Token) -> bool {
    false
}

/// Sends the verified reports of `server` to `runtime`.
#[cfg(unix)]
pub(crate) fn deliver_reports(
    server: &Entity<ChannelServer>,
    runtime: &Entity<ChatRuntime>,
    cx: &mut App,
) {
    let runtime = runtime.downgrade();
    server.update(cx, |server, _| {
        server.set_report_handler(move |report, cx| {
            runtime
                .update(cx, |runtime, cx| runtime.on_report(report, cx))
                .ok();
        });
    });
}

/// The change of a `last_active` write: the chat's in-memory last activity, and `prompted` once
/// the chat is.
fn active_change(
    active: Timestamp,
    prompted: bool,
) -> impl FnOnce(&mut bongos_chat_model::ChatRecord) -> bongos_chat_model::Result<()> + Send + 'static
{
    move |record| {
        record.last_active = active;
        if prompted {
            record.prompted = true;
        }
        Ok(())
    }
}
