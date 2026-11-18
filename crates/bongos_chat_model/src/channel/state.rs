//! The live state of a chat (design D10): one pure reducer that folds channel reports and terminal
//! signals into what a chat is doing and how sure bongos is of it.
//!
//! A report from the agent's own hooks is exact. A terminal signal (bell, output, the idle timer,
//! keyboard input, a send's Enter) is a guess, so its result is estimated, and it is only listened to
//! where the channel cannot answer: when the channel is not live, when the adapter's channel does
//! not cover the state the signal points at, or when the current state is itself estimated. Two
//! signals may still end an exact state on a live channel, because an agent does not report every
//! turn end (an interrupted turn or a denied permission ends without one): the idle timer ends exact
//! running, and the first keyboard input ends exact waiting. Both leave an estimated state that the
//! next report makes exact again.
//!
//! Times are milliseconds on the caller's clock (reports carry the reporter's wall clock); the reducer
//! never reads a clock itself.

use std::fmt;
use std::time::Duration;

use crate::channel::report::{Event, Report, ReportedState, SessionSource};

/// Output this soon after a bell belongs to the bell (the terminal reports the output of one read
/// after its bell), so it does not end the waiting state.
pub const BELL_GRACE: Duration = Duration::from_millis(500);

/// Without an exact state, a running chat with output this recent is mid-turn.
pub const MID_TURN: Duration = Duration::from_secs(2);

/// Why a state is estimated after a turn ended without its report.
pub const NO_TURN_END: &str = "no turn end reported (interrupted or denied)";

/// Why a state is estimated when a terminal signal set it.
pub const FROM_TERMINAL: &str = "from terminal activity";

/// The `needs_input` detail that means the agent sits at its idle prompt.
pub const IDLE_PROMPT: &str = "idle_prompt";

/// What a chat is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatState {
    /// The agent is working on a turn.
    Running,
    /// The agent wants input (a permission, an answer).
    Waiting,
    /// The agent finished its turn.
    Idle,
    /// The process ended; `code` is its exit code, when it reported one.
    Exited { code: Option<i32> },
    /// No live terminal.
    Closed,
}

impl ChatState {
    /// Whether nothing changes this state any more: exited or closed.
    pub fn is_final(self) -> bool {
        matches!(self, ChatState::Exited { .. } | ChatState::Closed)
    }
}

impl From<ReportedState> for ChatState {
    fn from(state: ReportedState) -> Self {
        match state {
            ReportedState::Running => ChatState::Running,
            ReportedState::Waiting => ChatState::Waiting,
            ReportedState::Idle => ChatState::Idle,
        }
    }
}

impl fmt::Display for ChatState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatState::Running => write!(f, "running"),
            ChatState::Waiting => write!(f, "waiting"),
            ChatState::Idle => write!(f, "idle"),
            ChatState::Exited { code: Some(code) } => write!(f, "exited (code {code})"),
            ChatState::Exited { code: None } => write!(f, "exited"),
            ChatState::Closed => write!(f, "closed"),
        }
    }
}

/// How bongos knows a chat's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The agent reported it (or the process ended).
    Exact,
    /// A terminal signal suggested it, for this reason.
    Estimated(&'static str),
}

/// The chat channel of one live chat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelStatus {
    /// The launch has no channel (the runtime keeps the reason, which is built at launch).
    Off,
    /// The launch has a channel but no report was accepted yet.
    Pending,
    /// A report was accepted from this launch.
    Live,
}

/// Which states (and whether the session id) an adapter's channel reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelCoverage {
    pub running: bool,
    pub waiting: bool,
    pub idle: bool,
    pub session_id: bool,
}

impl ChannelCoverage {
    /// Claude's hooks report every state and the session id.
    pub fn claude() -> Self {
        Self {
            running: true,
            waiting: true,
            idle: true,
            session_id: true,
        }
    }

    /// A channel that reports nothing.
    pub fn none() -> Self {
        Self {
            running: false,
            waiting: false,
            idle: false,
            session_id: false,
        }
    }

    /// Whether the channel reports `state`. Exited and closed come from the process, never a report.
    pub fn covers(self, state: ChatState) -> bool {
        match state {
            ChatState::Running => self.running,
            ChatState::Waiting => self.waiting,
            ChatState::Idle => self.idle,
            ChatState::Exited { .. } | ChatState::Closed => false,
        }
    }
}

/// The reducer's view of one live chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatLive {
    pub state: ChatState,
    pub source: Source,
    pub channel: ChannelStatus,
    pub coverage: ChannelCoverage,
    /// The `at` of the last applied report; an older report is dropped.
    pub last_report_at: Option<u64>,
    /// When the terminal last rang the bell, for [`BELL_GRACE`].
    pub last_bell_at: Option<u64>,
    /// When the terminal last produced output, for [`is_mid_turn`].
    pub last_output_at: Option<u64>,
}

impl ChatLive {
    /// A chat whose process started at `now`: estimated running, as its first output is expected.
    pub fn start(channel: ChannelStatus, coverage: ChannelCoverage, now: u64) -> Self {
        Self {
            state: ChatState::Running,
            source: Source::Estimated(FROM_TERMINAL),
            channel,
            coverage,
            last_report_at: None,
            last_bell_at: None,
            last_output_at: Some(now),
        }
    }

    /// Notes the time of a bell or output (always, even when the signal changes nothing), then
    /// applies `input` and returns the change it made.
    pub fn step(&mut self, input: Input) -> Option<Change> {
        match input {
            Input::Bell { now } => self.last_bell_at = Some(now),
            Input::Output { now } => self.last_output_at = Some(now),
            _ => {}
        }
        let change = apply(self, input)?;
        self.state = change.state;
        self.source = change.source;
        self.last_report_at = change.last_report_at;
        Some(change)
    }
}

/// One thing that happened to a live chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// A report accepted from the chat's channel. The reducer applies a report whatever the channel
    /// status: the caller forwards only reports it accepted, and marks the channel live on the first.
    Report(Report),
    /// The terminal rang the bell at `now`.
    Bell { now: u64 },
    /// The terminal produced output at `now`.
    Output { now: u64 },
    /// No output for `chats.idle_after_secs`.
    IdleTimer,
    /// The first keyboard input since the last state change.
    KeyboardInput,
    /// A send pressed Enter in the chat.
    SendEnter,
    /// The process ended.
    Exit { code: Option<i32> },
}

/// What an input changed: the chat's new state, source and last applied report time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub state: ChatState,
    pub source: Source,
    pub last_report_at: Option<u64>,
}

/// Applies `input` to `chat`, or `None` when nothing changes. A report that confirms the current
/// state and source still advances `last_report_at`, so it returns a change with the same state.
///
/// Precedence: an exit always applies and nothing changes an exited chat; a report applies unless
/// it is older than the last applied one (ties apply in arrival order) and its result is exact,
/// except `needs_input idle_prompt` ending a turn; a terminal signal applies only where the channel
/// cannot answer (see the module documentation) and its result is estimated.
pub fn apply(chat: &ChatLive, input: Input) -> Option<Change> {
    if chat.state.is_final() {
        return None;
    }
    let change = match input {
        Input::Exit { code } => Change {
            state: ChatState::Exited { code },
            source: Source::Exact,
            last_report_at: chat.last_report_at,
        },
        Input::Report(report) => {
            if chat.last_report_at.is_some_and(|last| report.at < last) {
                return None;
            }
            let (state, source) = report_target(chat, &report.event);
            Change {
                state,
                source,
                last_report_at: Some(report.at),
            }
        }
        signal => {
            let (state, ends_turn) = signal_target(chat, &signal)?;
            let denial_continues =
                matches!(signal, Input::IdleTimer) && chat.source == Source::Estimated(NO_TURN_END);
            let source = if ends_turn || denial_continues {
                Source::Estimated(NO_TURN_END)
            } else if signal_applies(chat, state) {
                Source::Estimated(FROM_TERMINAL)
            } else {
                return None;
            };
            Change {
                state,
                source,
                last_report_at: chat.last_report_at,
            }
        }
    };
    let same = change.state == chat.state
        && change.source == chat.source
        && change.last_report_at == chat.last_report_at;
    (!same).then_some(change)
}

/// The state and source a report sets.
fn report_target(chat: &ChatLive, event: &Event) -> (ChatState, Source) {
    match event {
        Event::SessionStart {
            source: SessionSource::Compact,
        } => (chat.state, chat.source),
        Event::SessionStart { .. } | Event::TurnDone => (ChatState::Idle, Source::Exact),
        Event::PromptSubmitted => (ChatState::Running, Source::Exact),
        Event::NeedsInput { detail } => {
            if detail
                .as_ref()
                .is_some_and(|detail| detail.as_str() == IDLE_PROMPT)
            {
                if matches!(chat.state, ChatState::Running | ChatState::Waiting) {
                    (ChatState::Idle, Source::Estimated(NO_TURN_END))
                } else {
                    (ChatState::Idle, Source::Exact)
                }
            } else {
                (ChatState::Waiting, Source::Exact)
            }
        }
        Event::ToolDone => {
            if matches!(chat.state, ChatState::Running | ChatState::Waiting) {
                (ChatState::Running, Source::Exact)
            } else {
                (chat.state, chat.source)
            }
        }
        Event::State { state } => ((*state).into(), Source::Exact),
    }
}

/// The state a terminal signal points at from the chat's current state, and whether it is one of
/// the turn ends a live channel accepts from an exact state (the idle timer from exact running, the
/// first keyboard input from exact waiting).
fn signal_target(chat: &ChatLive, signal: &Input) -> Option<(ChatState, bool)> {
    let live_exact = chat.channel == ChannelStatus::Live && chat.source == Source::Exact;
    let from_waiting_or_idle = matches!(chat.state, ChatState::Waiting | ChatState::Idle);
    match signal {
        Input::Bell { .. } => Some((ChatState::Waiting, false)),
        Input::Output { now } => {
            let just_rang = chat
                .last_bell_at
                .is_some_and(|bell| Duration::from_millis(now.saturating_sub(bell)) < BELL_GRACE);
            (from_waiting_or_idle && !just_rang).then_some((ChatState::Running, false))
        }
        Input::IdleTimer => {
            (chat.state == ChatState::Running).then_some((ChatState::Idle, live_exact))
        }
        Input::KeyboardInput => from_waiting_or_idle.then_some((
            ChatState::Running,
            live_exact && chat.state == ChatState::Waiting,
        )),
        Input::SendEnter => from_waiting_or_idle.then_some((ChatState::Running, false)),
        Input::Report(_) | Input::Exit { .. } => None,
    }
}

/// Whether a terminal signal pointing at `target` is listened to: the channel is not live, or it
/// does not cover `target`, or the current state is itself estimated.
fn signal_applies(chat: &ChatLive, target: ChatState) -> bool {
    chat.channel != ChannelStatus::Live
        || !chat.coverage.covers(target)
        || matches!(chat.source, Source::Estimated(_))
}

/// Whether the chat is working on a turn at `now`: on a live channel with an exact state, exactly
/// when it is running; otherwise when it is running with output in the last [`MID_TURN`].
pub fn is_mid_turn(chat: &ChatLive, now: u64) -> bool {
    if chat.state != ChatState::Running {
        return false;
    }
    if chat.channel == ChannelStatus::Live && chat.source == Source::Exact {
        return true;
    }
    chat.last_output_at
        .is_some_and(|output| Duration::from_millis(now.saturating_sub(output)) < MID_TURN)
}
