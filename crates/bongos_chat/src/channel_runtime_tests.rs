//! Launches with a chat channel: the runtime against a real report server in a short temporary
//! directory and the fake CLIs of `test-fixtures/` in a real PTY. Reports are written to the
//! server's socket from helper threads with the token the fake logged from its environment, as an
//! agent's hook would. Parking is allowed; waits poll on the executor's timers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bongos_chat_model::channel::report::{
    self, AgentName, ChatId, Event, Report, SessionSource, Token,
};
use bongos_chat_model::{
    Agent, ChannelOff, ClaudeAdapter, Fallback, LaunchKind, read_record, record_path,
};
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use settings::{ChatAgentContent, SettingsStore};
use tempfile::TempDir;

use crate::channel_server::ChannelServer;
use crate::channel_server_tests::{OK, exchange, inputs, refused, temp};
use crate::runtime_tests::{Harness, STARTUP, fixture, harness, wait_for, write};
use crate::{
    CLAUDE_SETTINGS_FILE, ChannelConfig, ChannelStatus, ChatState, LaunchRequest, REPORTS_OFF,
    Source,
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

struct Channels {
    h: Harness,
    _socket_temp: TempDir,
    server: Entity<ChannelServer>,
}

/// The runtime harness with `chats.channels` on, a running global report server whose reports go
/// to the runtime, and a reporter of `["true"]`.
async fn channels(cx: &mut TestAppContext) -> Channels {
    let h = harness(cx, true, 60, &[]).await;
    set_channels(cx, true);
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
        let line = report_line(chat, token, at);
        let socket = self.socket(cx);
        let reply = exchange(&socket, line).await;
        cx.run_until_parked();
        reply
    }

    fn channel(&self, id: &str, cx: &mut TestAppContext) -> Option<ChannelStatus> {
        self.h.runtime.read_with(cx, |runtime, _| {
            runtime.live(id).map(|chat| chat.channel().clone())
        })
    }

    fn last_report_at(&self, id: &str, cx: &mut TestAppContext) -> Option<u64> {
        self.h.runtime.read_with(cx, |runtime, _| {
            runtime.live(id).and_then(|chat| chat.last_report_at())
        })
    }
}

fn report_of(chat: &str, token: &str, at: u64) -> Report {
    Report {
        chat: ChatId::new(chat).unwrap(),
        token: Token::new(token).unwrap(),
        at,
        agent: AgentName::claude(),
        event: Event::SessionStart {
            source: SessionSource::Startup,
        },
        session: None,
    }
}

fn report_line(chat: &str, token: &str, at: u64) -> Vec<u8> {
    report::render(&report_of(chat, token, at))
        .unwrap()
        .into_bytes()
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
    let source = c.h.runtime.read_with(cx, |runtime, _| {
        runtime.live(&id).unwrap().state_source().clone()
    });
    assert_eq!(
        source,
        Source::Estimated("No report from claude yet".into())
    );

    // Another token is refused and changes nothing.
    let other = "0".repeat(32);
    assert_eq!(c.report(&id, &other, 5, cx).await, refused("bad token"));
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Pending));

    // The launch's own token is accepted, and the report is recorded without changing the state.
    let state = c.h.state(&id, cx);
    assert_eq!(c.report(&id, token, 7, cx).await, OK);
    wait_for(cx, "the channel to go live", STARTUP, |cx| {
        c.channel(&id, cx) == Some(ChannelStatus::Live)
    })
    .await;
    assert_eq!(c.last_report_at(&id, cx), Some(7));
    assert_eq!(c.h.state(&id, cx), state);
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
    let reason = "Started without reports (the CLI exited when given them)".to_string();
    assert_eq!(c.channel(&id, cx), Some(ChannelStatus::Off(reason)));
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
    cx.executor().timer(Duration::from_millis(300)).await;
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
    assert_eq!(
        c.channel(&id, cx),
        Some(ChannelStatus::Off(REPORTS_OFF.to_string()))
    );
    let (source, has_token) = c.h.runtime.read_with(cx, |runtime, _| {
        let chat = runtime.live(&id).unwrap();
        (chat.state_source().clone(), chat.has_token())
    });
    assert_eq!(source, Source::Estimated(REPORTS_OFF.to_string()));
    assert!(!has_token);
    assert_eq!(c.h.state(&id, cx), state, "the state is kept");
    assert!(!c.server.read_with(cx, |server, _| server.is_running()));

    // On again: a new server, which does not know the chat; the chat stays off until it launches
    // again.
    set_channels(cx, true);
    cx.run_until_parked();
    assert_eq!(c.report(&id, &token, 2, cx).await, refused("unknown chat"));
    assert_eq!(
        c.channel(&id, cx),
        Some(ChannelStatus::Off(REPORTS_OFF.to_string()))
    );
}

#[gpui::test]
async fn test_no_channel_for_codex_an_old_claude_or_channels_off(cx: &mut TestAppContext) {
    let c = channels(cx).await;
    let codex = c.start(c.request(Agent::Codex, ""), cx).await;
    assert_eq!(
        c.channel(&codex, cx),
        Some(ChannelStatus::Off(
            "Codex does not report its state yet".to_string()
        ))
    );
    let old = c.start(c.request(Agent::Claude, "2.1.300"), cx).await;
    assert_eq!(
        c.channel(&old, cx),
        Some(ChannelStatus::Off(
            "Claude 2.1.300 is older than the first version with hooks bongos knows (2.1.319)"
                .to_string()
        ))
    );
    set_channels(cx, false);
    let off = c.start(c.request(Agent::Claude, "2.1.319"), cx).await;
    assert_eq!(
        c.channel(&off, cx),
        Some(ChannelStatus::Off(REPORTS_OFF.to_string()))
    );
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
