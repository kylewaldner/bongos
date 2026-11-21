//! Launches with a chat channel: the runtime against a real report server in a short temporary
//! directory and the fake CLIs of `test-fixtures/` in a real PTY. Reports are written to the
//! server's socket from helper threads with the token the fake logged from its environment, as an
//! agent's hook would. Parking is allowed; waits poll on the executor's timers.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use bongos_chat_model::channel::report::{
    self, AgentName, ChatId, Detail, Event, Report, SessionId, SessionSource, Token,
};
use bongos_chat_model::channel::state::{FROM_TERMINAL, NO_TURN_END};
use bongos_chat_model::{
    Agent, ChannelOff, ClaudeAdapter, Fallback, LaunchKind, read_record, record_path,
};
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use settings::{ChatAgentContent, SettingsStore};
use tempfile::TempDir;

use crate::channel_server::ChannelServer;
use crate::channel_server_tests::{OK, exchange, inputs, refused, temp};
use crate::runtime_tests::{
    Harness, STARTUP, WITHIN, active_writes, age_record, fixture, harness, record_active, wait_for,
    write,
};
use crate::{
    CLAUDE_SETTINGS_FILE, ChannelConfig, ChannelStatus, ChatRuntimeEvent, ChatState, LaunchRequest,
    REPORTS_OFF, Source,
};

fn set_channels(cx: &mut TestAppContext, channels: bool) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.chats.get_or_insert_default().channels = Some(channels);
            });
        });
    });
}

/// Runs the Claude chats as `program` from now on.
fn set_claude_program(cx: &mut TestAppContext, program: &Path) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                let agents = content
                    .chats
                    .get_or_insert_default()
                    .agents
                    .get_or_insert_default();
                agents.claude = Some(ChatAgentContent {
                    command: Some(program.to_string_lossy().into_owned()),
                    extra_args: None,
                });
            });
        });
    });
}

pub(super) struct Channels {
    pub(super) h: Harness,
    _socket_temp: TempDir,
    server: Entity<ChannelServer>,
}

/// The runtime harness with `chats.channels` on, a running global report server whose reports go
/// to the runtime, and a reporter of `["true"]`.
pub(super) async fn channels(cx: &mut TestAppContext) -> Channels {
    channels_with(cx, true).await
}

/// `channels`, setting `chats.channels` only when `explicit`, so the default applies otherwise.
async fn channels_with(cx: &mut TestAppContext, explicit: bool) -> Channels {
    let h = harness(cx, true, 60, &[]).await;
    if explicit {
        set_channels(cx, true);
    }
    let (socket_temp, base) = temp();
    let server = cx.new(|cx| ChannelServer::new(inputs(&base, None), cx));
    cx.update(|cx| {
        ChannelServer::set_global(server.clone(), cx);
        crate::runtime::deliver_reports(&server, &h.runtime, cx);
        ChannelConfig {
            reporter: vec!["true".to_string()],
        }
        .set_global(cx);
    });
    assert!(server.read_with(cx, |server, _| server.is_running()));
    Channels {
        h,
        _socket_temp: socket_temp,
        server,
    }
}

impl Channels {
    /// A new chat of `agent` whose capabilities come from the version table at `version`.
    fn request(&self, agent: Agent, version: &str) -> LaunchRequest {
        let mut request = self.h.request(agent, "implement");
        if agent == Agent::Claude {
            request.caps = Some(ClaudeAdapter::new("claude").capabilities(Some(version)));
        }
        request
    }

    /// Launches `request` and waits until the fake printed `ready`. Returns the chat id.
    async fn start(&self, request: LaunchRequest, cx: &mut TestAppContext) -> String {
        let id = request.record.id.clone();
        let name = request.record.agent.clone();
        let starts = self.starts(&name);
        self.h.launch(request, &self.h.project, cx).await.unwrap();
        wait_for(cx, "the fake to start", STARTUP, |_| {
            self.starts(&name) > starts
        })
        .await;
        self.h.wait_content(&id, "ready", cx).await;
        id
    }

    fn starts(&self, name: &str) -> usize {
        self.h
            .log_lines(name)
            .iter()
            .filter(|line| *line == "start")
            .count()
    }

    /// The values of `name` in the `BONGOS_*` environment each Claude launch logged, in order.
    fn env(&self, name: &str) -> Vec<String> {
        let prefix = format!("{name}=");
        self.h
            .logged("claude", "env")
            .iter()
            .filter_map(|line| line.strip_prefix(&prefix).map(str::to_string))
            .collect()
    }

    fn socket(&self, cx: &mut TestAppContext) -> PathBuf {
        self.server
            .read_with(cx, |server, _| server.socket_path().map(Path::to_path_buf))
            .expect("the server runs")
    }

    /// Sends a `session_start` report for `chat` with `token` and returns the server's reply.
    async fn report(&self, chat: &str, token: &str, at: u64, cx: &mut TestAppContext) -> String {
        self.send(&report_of(chat, token, at), cx).await
    }

    /// Sends `report` through the server's socket and returns the server's reply.
    async fn send(&self, report: &Report, cx: &mut TestAppContext) -> String {
        let line = report::render(report).unwrap().into_bytes();
        let socket = self.socket(cx);
        let reply = exchange(&socket, line).await;
        cx.run_until_parked();
        reply
    }

    /// Hands `report` to the runtime as the server would after its own checks.
    pub(super) fn deliver(&self, report: Report, cx: &mut TestAppContext) {
        self.h
            .runtime
            .update(cx, |runtime, cx| runtime.on_report(report, cx));
    }

    fn channel(&self, id: &str, cx: &mut TestAppContext) -> Option<ChannelStatus> {
        self.h
            .runtime
            .read_with(cx, |runtime, _| runtime.live(id).map(|chat| chat.channel()))
    }

    fn off_reason(&self, id: &str, cx: &mut TestAppContext) -> Option<String> {
        self.h.runtime.read_with(cx, |runtime, _| {
            runtime
                .live(id)
                .and_then(|chat| chat.channel_off_reason().map(str::to_string))
        })
    }

    /// The chat's state and where it comes from.
    fn live_state(&self, id: &str, cx: &mut TestAppContext) -> (ChatState, Source) {
        self.h.runtime.read_with(cx, |runtime, _| {
            let chat = runtime.live(id).unwrap();
            (chat.state(), chat.state_source())
        })
    }

    /// Waits up to [`WITHIN`] for the chat to be `state` from `source`.
    async fn wait_live_state(
        &self,
        id: &str,
        state: ChatState,
        source: Source,
        cx: &mut TestAppContext,
    ) {
        let what = format!("{state} ({source:?})");
        wait_for(cx, &what, WITHIN, |cx| {
            self.live_state(id, cx) == (state, source)
        })
        .await;
    }

    /// Starts a Claude chat, makes its channel live with a `session_start` report at `at` 1, and
    /// returns its id and token.
    pub(super) async fn start_live(&self, cx: &mut TestAppContext) -> (String, String) {
        let id = self.start(self.request(Agent::Claude, "2.1.319"), cx).await;
        let token = self.env("BONGOS_CHAT_TOKEN").pop().unwrap();
        assert_eq!(self.report(&id, &token, 1, cx).await, OK);
        self.wait_live_state(&id, ChatState::Idle, Source::Exact, cx)
            .await;
        assert_eq!(self.channel(&id, cx), Some(ChannelStatus::Live));
        (id, token)
    }

    fn is_mid_turn(&self, id: &str, cx: &mut TestAppContext) -> bool {
        self.h
            .runtime
            .read_with(cx, |runtime, _| runtime.live(id).unwrap().is_mid_turn())
    }

    /// Moves the runtime's clock past `chats.idle_after_secs` (60 in this harness).
    fn pass_idle_time(&self, cx: &mut TestAppContext) {
        self.h.runtime.update(cx, |runtime, cx| {
            runtime.advance_clock(Duration::from_secs(61), cx)
        });
    }

    /// Counts the runtime's events for `id` from now on.
    fn count_events(&self, id: &str, cx: &mut TestAppContext) -> Rc<RefCell<usize>> {
        let count = Rc::new(RefCell::new(0));
        let id = id.to_string();
        let counter = count.clone();
        cx.update(|cx| {
            cx.subscribe(&self.h.runtime, move |_, event, _| {
                let ChatRuntimeEvent::Changed(changed) = event;
                if *changed == id {
                    *counter.borrow_mut() += 1;
                }
            })
            .detach();
        });
        count
    }

    fn last_report_at(&self, id: &str, cx: &mut TestAppContext) -> Option<u64> {
        self.h.runtime.read_with(cx, |runtime, _| {
            runtime.live(id).and_then(|chat| chat.last_report_at())
        })
    }
}

/// A `session_start` report naming the session `session`.
fn session_report(chat: &str, token: &str, at: u64, session: &str) -> Report {
    Report {
        session: Some(SessionId::new(session).unwrap()),
        ..report_of(chat, token, at)
    }
}

fn report_of(chat: &str, token: &str, at: u64) -> Report {
    event_report(
        chat,
        token,
        at,
        Event::SessionStart {
            source: SessionSource::Startup,
        },
    )
}

pub(super) fn event_report(chat: &str, token: &str, at: u64, event: Event) -> Report {
    Report {
        chat: ChatId::new(chat).unwrap(),
        token: Token::new(token).unwrap(),
        at,
        agent: AgentName::claude(),
        event,
        session: None,
    }
}

pub(super) fn needs_input(detail: &str) -> Event {
    Event::NeedsInput {
        detail: Some(Detail::new(detail).unwrap()),
    }
}

#[gpui::test]
async fn test_a_claude_launch_with_channels_carries_env_and_hooks_and_registers(
    cx: &mut TestAppContext,
) {
    let c = channels(cx).await;
    let id = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;

    let socket = c.socket(cx);
    assert_eq!(
        c.env("BONGOS_CHAT_SOCKET"),
        vec![socket.to_string_lossy().into_owned()]
    );
    assert_eq!(c.env("BONGOS_CHAT_ID"), vec![id.clone()]);
    let tokens = c.env("BONGOS_CHAT_TOKEN");
    assert_eq!(tokens.len(), 1);
    let token = &tokens[0];
    assert_eq!(token.len(), 32, "a simple UUID");

    let settings =
        std::fs::read_to_string(c.h.runtime_root.join(&id).join(CLAUDE_SETTINGS_FILE)).unwrap();
    for event in bongos_chat_model::REPORT_HOOK_EVENTS {
        assert!(
            settings.contains(event),
            "{event} hook missing:\n{settings}"
        );
    }
    assert!(settings.contains("'true' claude-hook"), "{settings}");
    assert!(
        !settings.contains(token.as_str()),
        "the token is never in a file"
    );
    let args = c.h.logged("claude", "arg");
    assert!(!args.iter().any(|arg| arg.contains(token.as_str())));

    // Pending until a report; the state still comes from the terminal.
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Pending));
    assert_eq!(c.off_reason(&id, cx), None);
    let source =
        c.h.runtime
            .read_with(cx, |runtime, _| runtime.live(&id).unwrap().state_source());
    assert_eq!(source, Source::Estimated(FROM_TERMINAL));

    // Another token is refused and changes nothing.
    let other = "0".repeat(32);
    assert_eq!(c.report(&id, &other, 5, cx).await, refused("bad token"));
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Pending));

    // The launch's own token is accepted: the channel goes live and the report sets the state.
    assert_eq!(c.report(&id, token, 7, cx).await, OK);
    c.wait_live_state(&id, ChatState::Idle, Source::Exact, cx)
        .await;
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Live));
    assert_eq!(c.last_report_at(&id, cx), Some(7));
}

#[gpui::test]
async fn test_default_settings_launch_a_claude_chat_with_hooks(cx: &mut TestAppContext) {
    let c = channels_with(cx, false).await;
    assert!(cx.update(|cx| {
        use settings::Settings as _;
        crate::ChatsSettings::get_global(cx).channels
    }));
    let id = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;
    let settings =
        std::fs::read_to_string(c.h.runtime_root.join(&id).join(CLAUDE_SETTINGS_FILE)).unwrap();
    assert!(settings.contains("claude-hook"), "{settings}");
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Pending));
}

#[gpui::test]
async fn test_exit_and_close_unregister_the_token(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let exiting = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;
    let closing = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;
    let tokens = c.env("BONGOS_CHAT_TOKEN");
    assert_eq!(tokens.len(), 2);
    assert_ne!(tokens[0], tokens[1], "each launch has its own token");
    assert_eq!(c.report(&exiting, &tokens[0], 1, cx).await, OK);
    assert_eq!(c.report(&closing, &tokens[1], 1, cx).await, OK);
    // One chat's token does not report on another.
    assert_eq!(
        c.report(&closing, &tokens[0], 2, cx).await,
        refused("bad token")
    );

    c.h.type_line(&exiting, "exit 0", cx);
    wait_for(cx, "the exit", STARTUP, |cx| {
        c.h.state(&exiting, cx) == ChatState::Exited { code: Some(0) }
    })
    .await;
    assert_eq!(
        c.report(&exiting, &tokens[0], 3, cx).await,
        refused("unknown chat")
    );
    assert!(
        !c.h.runtime
            .read_with(cx, |runtime, _| runtime.live(&exiting).unwrap().has_token())
    );

    assert!(
        c.h.runtime
            .update(cx, |runtime, cx| runtime.close(&closing, cx))
    );
    assert_eq!(
        c.report(&closing, &tokens[1], 3, cx).await,
        refused("unknown chat")
    );
    assert_eq!(
        c.h.runtime
            .read_with(cx, |runtime, _| runtime.stats().relaunches),
        0
    );
}

#[gpui::test]
async fn test_a_resume_gets_a_new_token_and_the_old_one_is_rejected(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let request = c.request(Agent::Claude, "2.1.319");
    let mut resume = request.clone();
    let id = c.start(request, cx).await;
    c.h.type_line(&id, "exit 0", cx);
    wait_for(cx, "the exit", STARTUP, |cx| {
        matches!(c.h.state(&id, cx), ChatState::Exited { .. })
    })
    .await;
    assert!(c.h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));

    resume.kind = LaunchKind::Resume;
    resume.record = read_record(&record_path(&c.h.scope, &id)).unwrap();
    assert!(resume.record.session_id.is_some());
    c.start(resume, cx).await;
    let tokens = c.env("BONGOS_CHAT_TOKEN");
    assert_eq!(tokens.len(), 2);
    assert_ne!(tokens[0], tokens[1]);

    assert_eq!(c.report(&id, &tokens[0], 9, cx).await, refused("bad token"));
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Pending));

    // A report the server checked against the earlier launch's token, delivered after the resume
    // registered the new one, is dropped on the main thread.
    c.h.runtime.update(cx, |runtime, cx| {
        runtime.on_report(report_of(&id, &tokens[0], 10), cx)
    });
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Pending));
    assert_eq!(c.last_report_at(&id, cx), None);

    assert_eq!(c.report(&id, &tokens[1], 11, cx).await, OK);
    wait_for(cx, "the channel to go live", STARTUP, |cx| {
        c.channel(&id, cx) == Some(ChannelStatus::Live)
    })
    .await;
    assert_eq!(c.last_report_at(&id, cx), Some(11));
}

#[gpui::test]
async fn test_an_early_error_exit_relaunches_once_without_the_channel(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    // A CLI that rejects the launch whenever it is given a channel.
    let wrapper = c.h.base.join("rejects-hooks");
    write(
        &wrapper,
        &format!(
            "#!/bin/sh\nif [ -n \"${{BONGOS_CHAT_TOKEN:-}}\" ]; then\n  \
             printf 'claude rejected\\n' >> \"$FAKE_CLI_LOG\"\n  exit 1\nfi\nexec '{}' \"$@\"\n",
            fixture("fake-claude").display()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    set_claude_program(cx, &wrapper);

    let request = c.request(Agent::Claude, "2.1.319");
    let id = request.record.id.clone();
    c.h.launch(request, &c.h.project, cx).await.unwrap();
    wait_for(cx, "the relaunch", STARTUP, |_| c.starts("claude") == 1).await;
    c.h.wait_content(&id, "ready", cx).await;

    let rejected = |c: &Channels| {
        c.h.log_lines("claude")
            .iter()
            .filter(|line| *line == "rejected")
            .count()
    };
    assert_eq!(rejected(&c), 1);
    assert!(
        c.env("BONGOS_CHAT_TOKEN").is_empty(),
        "the relaunch has no token"
    );
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Off));
    assert_eq!(
        c.off_reason(&id, cx).as_deref(),
        Some("Started without reports (the CLI exited when given them)")
    );
    let fallbacks = c.h.runtime.read_with(cx, |runtime, _| {
        runtime.live(&id).unwrap().fallbacks().to_vec()
    });
    assert!(fallbacks.contains(&Fallback::ChannelOff(ChannelOff::ExitedEarly)));
    assert_eq!(c.h.state(&id, cx), ChatState::Running);

    // An error exit of the relaunched chat does not start it again.
    c.h.type_line(&id, "exit 4", cx);
    wait_for(cx, "the exit", STARTUP, |cx| {
        c.h.state(&id, cx) == ChatState::Exited { code: Some(4) }
    })
    .await;
    cx.run_until_parked();
    assert_eq!(c.starts("claude"), 1);
    assert_eq!(rejected(&c), 1);
    assert_eq!(
        c.h.runtime
            .read_with(cx, |runtime, _| runtime.stats().relaunches),
        1
    );
}

#[gpui::test]
async fn test_turning_channels_off_moves_a_chat_to_off(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let id = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;
    let token = c.env("BONGOS_CHAT_TOKEN").pop().unwrap();
    assert_eq!(c.report(&id, &token, 1, cx).await, OK);
    wait_for(cx, "the channel to go live", STARTUP, |cx| {
        c.channel(&id, cx) == Some(ChannelStatus::Live)
    })
    .await;
    let state = c.h.state(&id, cx);

    set_channels(cx, false);
    cx.run_until_parked();
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Off));
    assert_eq!(c.off_reason(&id, cx).as_deref(), Some(REPORTS_OFF));
    let (source, has_token) = c.h.runtime.read_with(cx, |runtime, _| {
        let chat = runtime.live(&id).unwrap();
        (chat.state_source(), chat.has_token())
    });
    assert_eq!(source, Source::Estimated(REPORTS_OFF));
    assert!(!has_token);
    assert_eq!(c.h.state(&id, cx), state, "the state is kept");
    assert!(!c.server.read_with(cx, |server, _| server.is_running()));

    // On again: a new server, which does not know the chat; the chat stays off until it launches
    // again.
    set_channels(cx, true);
    cx.run_until_parked();
    assert_eq!(c.report(&id, &token, 2, cx).await, refused("unknown chat"));
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Off));
    assert_eq!(c.off_reason(&id, cx).as_deref(), Some(REPORTS_OFF));
    // A report the server let through before it stopped is dropped too.
    c.deliver(report_of(&id, &token, 3), cx);
    assert_eq!(c.live_state(&id, cx).1, Source::Estimated(REPORTS_OFF));
}

#[gpui::test]
async fn test_no_channel_for_codex_an_old_claude_or_channels_off(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let codex = c.start(c.request(Agent::Codex, ""), cx).await;
    assert_eq!(c.channel(&codex, cx), Some(ChannelStatus::Off));
    assert_eq!(
        c.off_reason(&codex, cx).as_deref(),
        Some("Codex does not report its state yet")
    );
    let old = c.start(c.request(Agent::Claude, "2.1.300"), cx).await;
    assert_eq!(c.channel(&old, cx), Some(ChannelStatus::Off));
    assert_eq!(
        c.off_reason(&old, cx).as_deref(),
        Some("Claude 2.1.300 is older than the first version with hooks bongos knows (2.1.319)")
    );
    set_channels(cx, false);
    let off = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;
    assert_eq!(c.channel(&off, cx), Some(ChannelStatus::Off));
    assert_eq!(c.off_reason(&off, cx).as_deref(), Some(REPORTS_OFF));
    let settings =
        std::fs::read_to_string(c.h.runtime_root.join(&off).join(CLAUDE_SETTINGS_FILE)).unwrap();
    assert!(!settings.contains("claude-hook"), "{settings}");

    let env: Vec<String> = ["codex", "claude"]
        .iter()
        .flat_map(|name| c.h.logged(name, "env"))
        .collect();
    assert!(
        !env.iter().any(|line| line.starts_with("BONGOS_CHAT_TOKEN=")
            || line.starts_with("BONGOS_CHAT_SOCKET=")),
        "{env:?}"
    );
}

#[gpui::test]
async fn test_reports_set_the_state_and_the_terminal_does_not_while_live(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let (id, token) = c.start_live(cx).await;

    let steps = [
        (Event::PromptSubmitted, ChatState::Running),
        (needs_input("permission_prompt"), ChatState::Waiting),
        (Event::ToolDone, ChatState::Running),
        (Event::TurnDone, ChatState::Idle),
    ];
    for (at, (event, state)) in (2..).zip(steps) {
        assert_eq!(c.send(&event_report(&id, &token, at, event), cx).await, OK);
        c.wait_live_state(&id, state, Source::Exact, cx).await;
    }
    assert!(!c.is_mid_turn(&id, cx));

    // A bell, keyboard input and output change nothing while the channel answers.
    c.h.type_line(&id, "bell", cx);
    c.h.type_line(&id, "hello", cx);
    c.h.wait_content(&id, "got hello", cx).await;
    cx.run_until_parked();
    assert_eq!(c.live_state(&id, cx), (ChatState::Idle, Source::Exact));

    // Exact running is mid-turn without any output.
    assert_eq!(
        c.send(&event_report(&id, &token, 9, Event::PromptSubmitted), cx)
            .await,
        OK
    );
    c.wait_live_state(&id, ChatState::Running, Source::Exact, cx)
        .await;
    assert!(c.is_mid_turn(&id, cx));
}

#[gpui::test]
async fn test_an_interrupted_turn_ends_in_estimated_idle_after_the_timer(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let (id, token) = c.start_live(cx).await;

    // `UserPromptSubmit`, then no `Stop`, and the output stops.
    c.deliver(event_report(&id, &token, 2, Event::PromptSubmitted), cx);
    assert_eq!(c.live_state(&id, cx), (ChatState::Running, Source::Exact));
    c.pass_idle_time(cx);
    assert_eq!(
        c.live_state(&id, cx),
        (ChatState::Idle, Source::Estimated(NO_TURN_END))
    );
    assert!(!c.is_mid_turn(&id, cx));

    // The next report makes the state exact again.
    c.deliver(event_report(&id, &token, 3, Event::PromptSubmitted), cx);
    assert_eq!(c.live_state(&id, cx), (ChatState::Running, Source::Exact));
    c.deliver(event_report(&id, &token, 4, Event::TurnDone), cx);
    assert_eq!(c.live_state(&id, cx), (ChatState::Idle, Source::Exact));
}

#[gpui::test]
async fn test_a_denied_permission_ends_through_keyboard_input_and_the_timer(
    cx: &mut TestAppContext,
) {
    let c = channels(cx).await;
    let (id, token) = c.start_live(cx).await;
    c.deliver(event_report(&id, &token, 2, Event::PromptSubmitted), cx);
    c.deliver(
        event_report(&id, &token, 3, needs_input("permission_prompt")),
        cx,
    );
    assert_eq!(c.live_state(&id, cx), (ChatState::Waiting, Source::Exact));

    // The timer never ends exact waiting: a permission prompt waits silently.
    c.pass_idle_time(cx);
    assert_eq!(c.live_state(&id, cx), (ChatState::Waiting, Source::Exact));

    // Kyle answers: keyboard input leaves exact waiting for estimated running.
    c.h.type_line(&id, "no", cx);
    c.h.wait_content(&id, "got no", cx).await;
    c.wait_live_state(&id, ChatState::Running, Source::Estimated(NO_TURN_END), cx)
        .await;

    // The denial ends the turn without a report: the timer ends it.
    c.pass_idle_time(cx);
    assert_eq!(
        c.live_state(&id, cx),
        (ChatState::Idle, Source::Estimated(NO_TURN_END))
    );
}

#[gpui::test]
async fn test_idle_prompt_ends_running_and_waiting_as_estimated_idle(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let (id, token) = c.start_live(cx).await;

    c.deliver(event_report(&id, &token, 2, Event::PromptSubmitted), cx);
    c.deliver(event_report(&id, &token, 3, needs_input("idle_prompt")), cx);
    assert_eq!(
        c.live_state(&id, cx),
        (ChatState::Idle, Source::Estimated(NO_TURN_END))
    );

    c.deliver(
        event_report(&id, &token, 4, needs_input("permission_prompt")),
        cx,
    );
    assert_eq!(c.live_state(&id, cx), (ChatState::Waiting, Source::Exact));
    c.deliver(event_report(&id, &token, 5, needs_input("idle_prompt")), cx);
    assert_eq!(
        c.live_state(&id, cx),
        (ChatState::Idle, Source::Estimated(NO_TURN_END))
    );

    // From idle, `idle_prompt` is exact.
    c.deliver(event_report(&id, &token, 6, needs_input("idle_prompt")), cx);
    assert_eq!(c.live_state(&id, cx), (ChatState::Idle, Source::Exact));
}

#[gpui::test]
async fn test_stale_and_unchanged_reports_change_nothing_and_tell_nothing(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let (id, token) = c.start_live(cx).await;
    c.deliver(event_report(&id, &token, 10, Event::PromptSubmitted), cx);
    assert_eq!(c.live_state(&id, cx), (ChatState::Running, Source::Exact));
    let events = c.count_events(&id, cx);

    // An older report is dropped silently.
    c.deliver(event_report(&id, &token, 9, Event::TurnDone), cx);
    cx.run_until_parked();
    assert_eq!(c.live_state(&id, cx), (ChatState::Running, Source::Exact));
    assert_eq!(c.last_report_at(&id, cx), Some(10));
    assert_eq!(*events.borrow(), 0);

    // A report that only advances the last report time is not a state change.
    c.deliver(
        event_report(
            &id,
            &token,
            11,
            Event::SessionStart {
                source: SessionSource::Compact,
            },
        ),
        cx,
    );
    c.deliver(event_report(&id, &token, 12, Event::ToolDone), cx);
    c.deliver(event_report(&id, &token, 13, Event::PromptSubmitted), cx);
    cx.run_until_parked();
    assert_eq!(c.live_state(&id, cx), (ChatState::Running, Source::Exact));
    assert_eq!(c.last_report_at(&id, cx), Some(13));
    assert_eq!(*events.borrow(), 0);

    // A real change is told once.
    c.deliver(event_report(&id, &token, 14, Event::TurnDone), cx);
    cx.run_until_parked();
    assert_eq!(c.live_state(&id, cx), (ChatState::Idle, Source::Exact));
    assert_eq!(*events.borrow(), 1);
}

#[gpui::test]
async fn test_one_chats_report_never_changes_another_chats_state(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let (a, token_a) = c.start_live(cx).await;
    let (b, token_b) = c.start_live(cx).await;
    assert_ne!(token_a, token_b);

    // Through the server, chat A's token is refused for chat B.
    assert_eq!(
        c.send(&event_report(&b, &token_a, 2, Event::PromptSubmitted), cx)
            .await,
        refused("bad token")
    );
    // Delivered to the runtime anyway, it is dropped there too.
    c.deliver(event_report(&b, &token_a, 3, Event::PromptSubmitted), cx);
    assert_eq!(c.live_state(&a, cx), (ChatState::Idle, Source::Exact));
    assert_eq!(c.live_state(&b, cx), (ChatState::Idle, Source::Exact));
    assert_eq!(c.last_report_at(&b, cx), Some(1));

    // Chat A's own report changes chat A only.
    c.deliver(event_report(&a, &token_a, 4, Event::PromptSubmitted), cx);
    assert_eq!(c.live_state(&a, cx), (ChatState::Running, Source::Exact));
    assert_eq!(c.live_state(&b, cx), (ChatState::Idle, Source::Exact));
    c.deliver(
        event_report(&b, &token_b, 5, needs_input("permission_prompt")),
        cx,
    );
    assert_eq!(c.live_state(&a, cx), (ChatState::Running, Source::Exact));
    assert_eq!(c.live_state(&b, cx), (ChatState::Waiting, Source::Exact));
}

#[gpui::test]
async fn test_each_exact_change_writes_once_and_a_burst_writes_once(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    c.h.runtime
        .update(cx, |runtime, _| runtime.set_active_debounce(Duration::ZERO));
    let (id, token) = c.start_live(cx).await;
    let steps = [
        (Event::PromptSubmitted, ChatState::Running),
        (needs_input("permission_prompt"), ChatState::Waiting),
        (Event::ToolDone, ChatState::Running),
        (Event::TurnDone, ChatState::Idle),
    ];
    // Each change is a write; a report that changes nothing is none.
    let mut writes = active_writes(&c.h, cx);
    for (at, (event, state)) in (2..).zip(steps) {
        c.deliver(event_report(&id, &token, at, event), cx);
        c.wait_live_state(&id, state, Source::Exact, cx).await;
        writes += 1;
        assert_eq!(active_writes(&c.h, cx), writes, "{state}");
    }
    c.deliver(event_report(&id, &token, 6, Event::TurnDone), cx);
    assert_eq!(active_writes(&c.h, cx), writes, "no change, no write");

    // With a debounce, a burst of reports is one write at once (the window has passed on the
    // runtime's clock) and one more when the window closes.
    let debounce = Duration::from_secs(60);
    c.h.runtime.update(cx, |runtime, cx| {
        runtime.set_active_debounce(debounce);
        runtime.advance_clock(debounce, cx)
    });
    let before = active_writes(&c.h, cx);
    for (at, (event, _)) in (7..).zip([
        (Event::PromptSubmitted, ()),
        (Event::ToolDone, ()),
        (Event::TurnDone, ()),
    ]) {
        c.deliver(event_report(&id, &token, at, event), cx);
    }
    cx.run_until_parked();
    assert_eq!(active_writes(&c.h, cx) - before, 1, "the first change writes");
    cx.executor().advance_clock(debounce);
    cx.run_until_parked();
    assert_eq!(
        active_writes(&c.h, cx) - before,
        2,
        "the rest of the burst is one write when the window closes"
    );
}

#[gpui::test]
async fn test_a_runtime_dropped_without_quit_leaves_the_last_exact_change(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    c.h.runtime
        .update(cx, |runtime, _| runtime.set_active_debounce(Duration::ZERO));
    let (id, token) = c.start_live(cx).await;
    let old = age_record(&c.h, &id);
    let before = active_writes(&c.h, cx);
    c.deliver(event_report(&id, &token, 2, Event::PromptSubmitted), cx);
    c.wait_live_state(&id, ChatState::Running, Source::Exact, cx)
        .await;
    wait_for(cx, "the record to change", WITHIN, |_| {
        record_active(&c.h, &id) != old
    })
    .await;
    let written = record_active(&c.h, &id);
    // No quit, no close: the record holds the last change, the way a crash leaves it.
    cx.run_until_parked();
    assert_eq!(
        record_active(&c.h, &id),
        written,
        "nothing wrote after the change"
    );
    assert_eq!(active_writes(&c.h, cx), before + 1);
}

impl Channels {
    fn record_session(&self, id: &str) -> Option<String> {
        read_record(&record_path(&self.h.scope, id))
            .unwrap()
            .session_id
    }

    fn live_session(&self, id: &str, cx: &mut TestAppContext) -> Option<String> {
        self.h.runtime.read_with(cx, |runtime, _| {
            runtime
                .live(id)
                .and_then(|chat| chat.session_id().map(str::to_string))
        })
    }

    /// The values that follow `flag` in the logged Claude arguments, in order.
    fn flag_values(&self, flag: &str) -> Vec<String> {
        let args = self.h.logged("claude", "arg");
        args.windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
            .collect()
    }

    /// The token of `tokens` the server accepts for `chat`, found by sending each a `session_start`
    /// report naming `session`. Exactly one is accepted.
    async fn own_token(
        &self,
        chat: &str,
        tokens: &[String],
        session: &str,
        cx: &mut TestAppContext,
    ) -> String {
        let mut accepted = Vec::new();
        for token in tokens {
            if self
                .send(&session_report(chat, token, 1, session), cx)
                .await
                == OK
            {
                accepted.push(token.clone());
            }
        }
        assert_eq!(accepted.len(), 1, "one token per chat");
        accepted.pop().unwrap()
    }

    fn cleanups(&self, cx: &mut TestAppContext) -> usize {
        self.h
            .runtime
            .read_with(cx, |runtime, _| runtime.stats().cleanups)
    }

    /// Closes the chat and waits until the store ran everything queued before (the cleanup too),
    /// by queueing a write of `keep`'s record after it.
    async fn close_and_settle(&self, id: &str, keep: &str, cx: &mut TestAppContext) {
        assert!(
            self.h
                .runtime
                .update(cx, |runtime, cx| runtime.close(id, cx))
        );
        let path = record_path(&self.h.scope, keep);
        let write = self
            .h
            .store
            .update(cx, |store, cx| store.update(path, |_| Ok(()), cx));
        write.await.unwrap();
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn test_two_claude_chats_started_at_once_each_resume_their_own_id(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let first = c.request(Agent::Claude, "2.1.319");
    let second = c.request(Agent::Claude, "2.1.319");
    assert_eq!(first.record.cwd, second.record.cwd, "the same directory");
    let (a, b) = (first.record.id.clone(), second.record.id.clone());
    let mut resumes = [first.clone(), second.clone()];

    // Both launches start before either finished.
    let projects = vec![c.h.project.clone()];
    let (launch_a, launch_b) = c.h.runtime.update(cx, |runtime, cx| {
        (
            runtime.launch(first, &projects, cx),
            runtime.launch(second, &projects, cx),
        )
    });
    launch_a.await.unwrap();
    launch_b.await.unwrap();
    wait_for(cx, "both fakes to start", STARTUP, |_| {
        c.starts("claude") == 2
    })
    .await;
    c.h.wait_content(&a, "ready", cx).await;
    c.h.wait_content(&b, "ready", cx).await;

    // Each id was chosen before launch and given on the command line: nothing is captured.
    let session_a = c.record_session(&a).unwrap();
    let session_b = c.record_session(&b).unwrap();
    assert_ne!(session_a, session_b);
    let mut given = c.flag_values("--session-id");
    given.sort();
    let mut expected = vec![session_a.clone(), session_b.clone()];
    expected.sort();
    assert_eq!(given, expected);
    assert_eq!(
        c.h.runtime
            .read_with(cx, |runtime, _| runtime.stats().captures),
        0
    );

    // Each chat's own report confirms its own id.
    let tokens = c.env("BONGOS_CHAT_TOKEN");
    assert_eq!(tokens.len(), 2);
    let token_a = c.own_token(&a, &tokens, &session_a, cx).await;
    let token_b = c.own_token(&b, &tokens, &session_b, cx).await;
    assert_ne!(token_a, token_b);
    // A report carrying the other chat's token never moves an id across.
    c.deliver(session_report(&b, &token_a, 2, &session_a), cx);
    c.deliver(session_report(&a, &token_b, 2, &session_b), cx);
    cx.run_until_parked();
    assert_eq!(c.live_session(&a, cx), Some(session_a.clone()));
    assert_eq!(c.live_session(&b, cx), Some(session_b.clone()));

    // Prompted, so both are kept; then each resumes its own id.
    for (id, token) in [(&a, &token_a), (&b, &token_b)] {
        c.deliver(event_report(id, token, 3, Event::PromptSubmitted), cx);
    }
    c.close_and_settle(&a, &a, cx).await;
    c.close_and_settle(&b, &b, cx).await;
    assert_eq!(c.cleanups(cx), 0);
    for (resume, session) in resumes.iter_mut().zip([&session_a, &session_b]) {
        resume.kind = LaunchKind::Resume;
        resume.record = read_record(&resume.record_path).unwrap();
        assert_eq!(resume.record.session_id.as_ref(), Some(session));
        c.start(resume.clone(), cx).await;
        assert_eq!(c.flag_values("--resume").last(), Some(session));
    }
}

#[gpui::test]
async fn test_a_fork_fallback_records_the_reported_session_id(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let mut request = c.request(Agent::Claude, "2.1.319");
    request.kind = LaunchKind::Fork {
        source_session: "0b0c0d0e-1111-4222-8333-444455556666".to_string(),
        native: true,
    };
    // The fallback that forks without `--session-id`: the CLI picks the id.
    request.force = vec![Fallback::CaptureSessionId];
    let id = c.start(request, cx).await;
    assert!(c.flag_values("--session-id").is_empty());
    assert_eq!(
        c.h.logged("claude", "arg")
            .iter()
            .filter(|arg| *arg == "--fork-session")
            .count(),
        1
    );
    let chosen = c.record_session(&id).unwrap();
    let token = c.env("BONGOS_CHAT_TOKEN").pop().unwrap();

    // Another chat's or an earlier launch's report changes nothing.
    let stray = "99999999-aaaa-4bbb-8ccc-dddddddddddd";
    c.deliver(session_report(&id, &"0".repeat(32), 1, stray), cx);
    cx.run_until_parked();
    assert_eq!(c.live_session(&id, cx), Some(chosen.clone()));

    let reported = "12345678-9abc-4def-8123-456789abcdef";
    assert_ne!(chosen, reported);
    assert_eq!(
        c.send(&session_report(&id, &token, 2, reported), cx).await,
        OK
    );
    assert_eq!(c.live_session(&id, cx).as_deref(), Some(reported));
    wait_for(cx, "the reported id in the record", STARTUP, |_| {
        c.record_session(&id).as_deref() == Some(reported)
    })
    .await;
    assert_eq!(
        c.h.runtime
            .read_with(cx, |runtime, _| runtime.stats().captures),
        0
    );
}

#[gpui::test]
async fn test_a_live_chat_with_no_prompt_report_is_cleaned_up_without_reading_files(
    cx: &mut TestAppContext,
) {
    let c = channels(cx).await;
    assert!(!c.h.home.join(".claude").exists());
    let (id, _) = c.start_live(cx).await;
    let (other, _) = c.start_live(cx).await;
    c.close_and_settle(&id, &other, cx).await;
    assert!(!record_path(&c.h.scope, &id).exists());
    assert_eq!(c.cleanups(cx), 1);
    assert_eq!(
        crate::cleanup::storage_reads(&c.h.home),
        0,
        "no file was read"
    );
    assert!(!c.h.home.join(".claude").exists());
}

#[gpui::test]
async fn test_a_live_chat_with_a_prompt_report_is_kept(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    for event in [Event::PromptSubmitted, Event::TurnDone] {
        let (id, token) = c.start_live(cx).await;
        c.deliver(event_report(&id, &token, 2, event), cx);
        assert!(c.h.runtime.read_with(cx, |runtime, _| {
            runtime.live(&id).unwrap().prompt_reported()
        }));
        c.close_and_settle(&id, &id, cx).await;
        assert!(record_path(&c.h.scope, &id).exists());
    }
    assert_eq!(c.cleanups(cx), 0);
    assert_eq!(crate::cleanup::storage_reads(&c.h.home), 0);
}

#[gpui::test]
async fn test_a_resumed_live_chat_without_a_prompt_report_keeps_the_file_check(
    cx: &mut TestAppContext,
) {
    let c = channels(cx).await;
    let request = c.request(Agent::Claude, "2.1.319");
    let mut resume = request.clone();
    let id = c.start(request, cx).await;
    let token = c.env("BONGOS_CHAT_TOKEN").pop().unwrap();
    assert_eq!(c.report(&id, &token, 1, cx).await, OK);
    c.deliver(event_report(&id, &token, 2, Event::PromptSubmitted), cx);
    c.close_and_settle(&id, &id, cx).await;

    // A resume of a chat whose record does not say prompted: the conversation is in the CLI's
    // files, not in this run's reports.
    let path = record_path(&c.h.scope, &id);
    resume.kind = LaunchKind::Resume;
    resume.record = read_record(&path).unwrap();
    assert!(!resume.record.prompted);
    assert!(resume.record.session_id.is_some());
    c.start(resume, cx).await;
    let token = c.env("BONGOS_CHAT_TOKEN").pop().unwrap();
    assert_eq!(c.report(&id, &token, 1, cx).await, OK);
    wait_for(cx, "the channel to go live", STARTUP, |cx| {
        c.channel(&id, cx) == Some(ChannelStatus::Live)
    })
    .await;

    // No prompt in this run, but the conversation may be from before: the files decide, and with
    // no Claude directory to look in, the record stays.
    c.close_and_settle(&id, &id, cx).await;
    assert!(path.exists());
    assert_eq!(c.cleanups(cx), 0);
    assert!(crate::cleanup::storage_reads(&c.h.home) > 0);
}

impl Channels {
    /// The text the estimated tag's tooltip shows for the chat, `None` when its state is exact.
    fn estimate_reason(&self, id: &str, cx: &mut TestAppContext) -> Option<String> {
        self.h.runtime.read_with(cx, |runtime, _| {
            runtime.live(id).and_then(|chat| chat.estimate_reason())
        })
    }
}

#[gpui::test]
async fn test_the_estimated_tag_names_each_reason(cx: &mut TestAppContext) {
    let c = channels(cx).await;

    // A launch whose channel waits for its first report.
    let pending = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;
    assert_eq!(c.channel(&pending, cx), Some(ChannelStatus::Pending));
    assert_eq!(
        c.estimate_reason(&pending, cx).as_deref(),
        Some("No report from Claude yet")
    );

    // A live channel: exact has no reason, an unreported turn end has one.
    let (live, token) = c.start_live(cx).await;
    assert_eq!(c.estimate_reason(&live, cx), None);
    c.deliver(event_report(&live, &token, 2, Event::PromptSubmitted), cx);
    assert_eq!(c.estimate_reason(&live, cx), None);
    c.pass_idle_time(cx);
    assert_eq!(
        c.estimate_reason(&live, cx).as_deref(),
        Some("No turn end reported (interrupted or denied)")
    );

    // An agent with no channel, and an old tool version.
    let codex = c.start(c.request(Agent::Codex, ""), cx).await;
    assert_eq!(
        c.estimate_reason(&codex, cx).as_deref(),
        Some("Codex does not report its state yet")
    );
    let old = c.start(c.request(Agent::Claude, "2.1.300"), cx).await;
    assert_eq!(
        c.estimate_reason(&old, cx).as_deref(),
        Some("Claude 2.1.300 is older than the first version with hooks bongos knows (2.1.319)")
    );

    // A reporter path that cannot be quoted.
    cx.update(|cx| {
        ChannelConfig {
            reporter: vec!["it's".to_string()],
        }
        .set_global(cx);
    });
    let unquotable = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;
    assert_eq!(
        c.estimate_reason(&unquotable, cx).as_deref(),
        Some("The bongos path cannot be quoted")
    );

    // Channels off, for a running chat and for a new one.
    set_channels(cx, false);
    cx.run_until_parked();
    assert_eq!(c.estimate_reason(&live, cx).as_deref(), Some(REPORTS_OFF));
    let off = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;
    assert_eq!(c.estimate_reason(&off, cx).as_deref(), Some(REPORTS_OFF));
}
