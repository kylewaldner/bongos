//! The reporter: the short-lived process an agent's hook runs (`bongos --chat-report <mode> ...`)
//! to send one [`Report`] over the chat channel socket.
//!
//! It reads the socket path, chat id and token from the environment, builds at most one report,
//! connects, writes the one request line and waits briefly for the reply, whose content it ignores.
//! It never writes to stdout or stderr and always ends cleanly, whatever happened: a hook that
//! fails or prints could disturb the agent, and a missed report only makes a chat's state an
//! estimate.
//!
//! Modes:
//! - `claude-hook`: reads Claude Code's hook input from stdin (at most 16 MiB is kept, the rest is
//!   drained) and sends the report [`translate_claude_hook`] gives, if any.
//! - `state <running|waiting|idle> [agent]`: reads nothing and sends a `state` report. The agent is
//!   the named one, else the valid `BONGOS_CHAT_AGENT` from the environment, else `claude`.
//!
//! Anything else (an unknown mode, state or agent, a missing argument) sends nothing.

use std::io::{self, Read, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::report::{AgentName, ChatId, Report, ReportedState, Token, render};
use super::translate::{ReportIdentity, read_bounded, state_report, translate_claude_hook};
use crate::adapter::{CHANNEL_SOCKET_ENV, CHANNEL_TOKEN_ENV};

/// The environment variable that holds the chat id a report is for.
pub const CHAT_ID_ENV: &str = "BONGOS_CHAT_ID";

/// The environment variable that names the agent a chat runs; launches export it.
pub const AGENT_ENV: &str = "BONGOS_CHAT_AGENT";

/// The mode Claude Code's hooks run.
pub const CLAUDE_HOOK_MODE: &str = "claude-hook";

/// The generic mode that names a state.
pub const STATE_MODE: &str = "state";

/// How long the reporter waits on the socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// The longest wait for the connection (a listener whose backlog is full blocks it).
    pub connect: Duration,
    /// The longest wait for each of the write and the reply.
    pub reply: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_millis(500),
            reply: Duration::from_millis(500),
        }
    }
}

/// What the reporter takes from its process, injectable for tests.
pub struct Process<'a> {
    /// Reads one environment variable.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub stdin: &'a mut dyn Read,
    /// Never written; held so tests can check that.
    pub stdout: &'a mut dyn Write,
    /// Never written; held so tests can check that.
    pub stderr: &'a mut dyn Write,
    /// The wall clock in milliseconds since the Unix epoch.
    pub now_ms: &'a dyn Fn() -> u64,
    pub timeouts: Timeouts,
}

/// The real reporter process: `mode` and `args` are the words after `--chat-report`. Uses the real
/// environment, stdin and clock, writes nothing anywhere, never panics out, and returns so the
/// caller exits with status 0.
pub fn main(mode: &str, args: &[String]) {
    // A panic must not print: the process is about to exit, so the hook is replaced for good.
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        run(
            mode,
            args,
            Process {
                env: &|name| std::env::var(name).ok(),
                stdin: &mut stdin,
                stdout: &mut io::sink(),
                stderr: &mut io::sink(),
                now_ms: &wall_clock_ms,
                timeouts: Timeouts::default(),
            },
        );
    }));
    // A panic is a missed report like any other failure; there is nowhere to tell it.
    drop(outcome);
}

fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The socket, chat and token a launch exports to its process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelTarget {
    pub socket: String,
    pub chat: ChatId,
    pub token: Token,
}

impl ChannelTarget {
    /// Reads the socket path, chat id and token through `env`; `None` when any is missing or
    /// invalid.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let socket = env(CHANNEL_SOCKET_ENV).filter(|socket| !socket.is_empty())?;
        let chat = env(CHAT_ID_ENV).as_deref().and_then(ChatId::new)?;
        let token = env(CHANNEL_TOKEN_ENV).as_deref().and_then(Token::new)?;
        Some(Self {
            socket,
            chat,
            token,
        })
    }
}

/// The reporter, given its process. Sends at most one report; every failure ends it quietly.
pub fn run(mode: &str, args: &[String], process: Process<'_>) {
    let at = (process.now_ms)();
    let env = process.env;
    let Some(ChannelTarget {
        socket,
        chat,
        token,
    }) = ChannelTarget::from_env(env)
    else {
        return;
    };
    let identity = ReportIdentity { chat, token, at };
    let Some(report) = build_report(mode, args, identity, process.stdin, env) else {
        return;
    };
    let Ok(line) = render(&report) else {
        return;
    };
    send(&socket, line.as_bytes(), process.timeouts);
}

fn build_report(
    mode: &str,
    args: &[String],
    identity: ReportIdentity,
    stdin: &mut dyn Read,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<Report> {
    match mode {
        CLAUDE_HOOK_MODE => {
            let mut stdin = stdin;
            let input = read_bounded(&mut stdin).ok()?;
            translate_claude_hook(&input, identity)
        }
        STATE_MODE => {
            let (state, agent) = match args {
                [state] => (state, None),
                [state, agent] => (state, Some(agent)),
                _ => return None,
            };
            let state = ReportedState::from_wire(state)?;
            let agent = match agent {
                Some(agent) => AgentName::new(agent)?,
                None => env(AGENT_ENV)
                    .as_deref()
                    .and_then(AgentName::new)
                    .unwrap_or_else(AgentName::claude),
            };
            Some(state_report(identity, agent, state))
        }
        _ => None,
    }
}

/// Connects to `socket`, waiting at most `timeout`; `None` when the connect failed or was late.
#[cfg(unix)]
pub(crate) fn connect(socket: &str, timeout: Duration) -> Option<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;

    // `std` has no connect timeout for a Unix socket, and a connect to a listener whose backlog is
    // full blocks. The connect runs on its own thread; when it is late the caller gives up and the
    // thread ends on its own (or with the process).
    let (sender, receiver) = mpsc::channel();
    let path = socket.to_string();
    let spawned = std::thread::Builder::new()
        .name("chat-report-connect".to_string())
        .spawn(move || {
            if let Err(mpsc::SendError(late)) = sender.send(UnixStream::connect(path)) {
                // The caller stopped waiting; a late connection is closed unused.
                drop(late);
            }
        });
    if spawned.is_err() {
        return None;
    }
    receiver.recv_timeout(timeout).ok()?.ok()
}

/// Connects, writes `line` and waits for the reply line, each step bounded by `timeouts`.
#[cfg(unix)]
fn send(socket: &str, line: &[u8], timeouts: Timeouts) {
    use std::time::Instant;

    use super::report::MAX_LINE_BYTES;

    let Some(mut stream) = connect(socket, timeouts.connect) else {
        return;
    };
    if stream.set_write_timeout(Some(timeouts.reply)).is_err() {
        return;
    }
    if stream.write_all(line).is_err() || stream.flush().is_err() {
        return;
    }
    // The reply is awaited only so the server sees the whole line before the reporter goes; what
    // it says changes nothing, so it is not parsed. One deadline covers the whole reply, so a
    // server that sends it a byte at a time cannot stretch the wait.
    let deadline = Instant::now() + timeouts.reply;
    let mut received = 0;
    let mut chunk = [0u8; 512];
    while received < MAX_LINE_BYTES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || stream.set_read_timeout(Some(remaining)).is_err() {
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) if chunk[..read].contains(&b'\n') => return,
            Ok(read) => received += read,
        }
    }
}

#[cfg(not(unix))]
fn send(_socket: &str, _line: &[u8], _timeouts: Timeouts) {}
