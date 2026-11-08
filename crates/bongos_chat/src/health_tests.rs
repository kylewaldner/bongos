//! `ChatHealth` against the fake CLIs in `test-fixtures/`, which are real programs: the probes run
//! as real processes, so these tests allow parking, as the store tests do.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bongos_chat_model::Agent;
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use settings::{ChatAgentContent, Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::health::{Probe, run_probe};
use crate::{
    ChatHealth, ChatHealthEvent, ChatsSettings, Health, HealthState, find_program, fix_logged_out,
    fix_not_found,
};

/// A short timeout for the tests that expect a probe to hang.
const SHORT_TIMEOUT: Duration = Duration::from_millis(400);

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test-fixtures")
        .join(name)
}

fn command_of(path: &Path) -> String {
    path.to_str().unwrap().to_string()
}

fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

fn set_settings(cx: &mut TestAppContext, enabled: bool, claude: &str, codex: &str) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                let chats = content.chats.get_or_insert_default();
                chats.enabled = Some(enabled);
                let agents = chats.agents.get_or_insert_default();
                agents.claude = Some(ChatAgentContent {
                    command: Some(claude.to_string()),
                    extra_args: None,
                });
                agents.codex = Some(ChatAgentContent {
                    command: Some(codex.to_string()),
                    extra_args: None,
                });
            });
        });
    });
}

struct Setup {
    health: Entity<ChatHealth>,
    /// The `HealthChanged` events seen.
    events: Arc<Mutex<usize>>,
    _subscription: gpui::Subscription,
}

fn setup(
    cx: &mut TestAppContext,
    enabled: bool,
    claude: &str,
    codex: &str,
    env: BTreeMap<String, String>,
) -> Setup {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        ChatsSettings::register(cx);
    });
    set_settings(cx, enabled, claude, codex);
    let health = cx.new(|cx| ChatHealth::with_timeout(env, SHORT_TIMEOUT, cx));
    let events = Arc::new(Mutex::new(0));
    let subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(&health, move |_, event: &ChatHealthEvent, _| {
            assert_eq!(*event, ChatHealthEvent::HealthChanged);
            *events.lock().unwrap() += 1;
        })
    });
    Setup {
        health,
        events,
        _subscription: subscription,
    }
}

/// Lets real time pass (the fakes are real processes) and runs what that woke. The test clock is
/// not used for waiting: it only moves when a test moves it.
fn pump(cx: &mut TestAppContext, millis: u64) {
    std::thread::sleep(Duration::from_millis(millis));
    cx.run_until_parked();
}

/// Waits until `rounds` probe rounds were started and none is running.
async fn wait_round(cx: &mut TestAppContext, health: &Entity<ChatHealth>, rounds: usize) {
    for _ in 0..200 {
        let done = health.read_with(cx, |health, _| {
            health.stats().rounds >= rounds && !health.is_probing()
        });
        if done {
            return;
        }
        pump(cx, 20);
    }
    panic!("timed out waiting for probe round {rounds}");
}

fn health_of(cx: &mut TestAppContext, health: &Entity<ChatHealth>, agent: Agent) -> Health {
    health.read_with(cx, |health, _| health.health(agent).clone())
}

fn calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn fakes() -> (String, String) {
    (
        command_of(&fixture("fake-claude")),
        command_of(&fixture("fake-codex")),
    )
}

#[gpui::test]
async fn test_both_clis_ready(cx: &mut TestAppContext) {
    let (claude, codex) = fakes();
    let setup = setup(cx, true, &claude, &codex, BTreeMap::new());
    wait_round(cx, &setup.health, 1).await;

    let claude = health_of(cx, &setup.health, Agent::Claude);
    assert_eq!(
        claude.state,
        HealthState::Ready {
            version: "2.1.319".to_string(),
            login_checked: true
        }
    );
    assert_eq!(claude.fix, "");
    assert!(claude.is_offered());
    let caps = claude.caps.expect("capabilities read from the fake help");
    assert!(caps.has_flag("--session-id"));
    assert!(caps.has_subcommand("auth"));

    let codex = health_of(cx, &setup.health, Agent::Codex);
    assert_eq!(
        codex.state,
        HealthState::Ready {
            version: "0.46.0".to_string(),
            login_checked: true
        }
    );
    assert!(codex.caps.unwrap().has_subcommand("fork"));
    assert_eq!(*setup.events.lock().unwrap(), 1);
}

#[gpui::test]
async fn test_program_not_found(cx: &mut TestAppContext) {
    let setup = setup(
        cx,
        true,
        "bongos-no-such-cli-claude",
        "/no/such/dir/codex",
        BTreeMap::new(),
    );
    wait_round(cx, &setup.health, 1).await;

    let claude = health_of(cx, &setup.health, Agent::Claude);
    assert_eq!(claude.state, HealthState::NotFound);
    assert_eq!(
        claude.fix,
        "bongos-no-such-cli-claude was not found on PATH; install Claude Code or set \
         chats.agents.claude.command"
    );
    assert!(!claude.is_offered());
    assert!(claude.caps.is_none());

    let codex = health_of(cx, &setup.health, Agent::Codex);
    assert_eq!(codex.state, HealthState::NotFound);
    assert_eq!(
        codex.fix,
        "/no/such/dir/codex was not found; fix chats.agents.codex.command"
    );
    setup.health.read_with(cx, |health, _| {
        assert_eq!(health.offered().count(), 0);
    });
}

#[gpui::test]
async fn test_logged_out(cx: &mut TestAppContext) {
    let (claude, codex) = fakes();
    let setup = setup(
        cx,
        true,
        &claude,
        &codex,
        env_of(&[("FAKE_CLI_LOGGED_OUT", "1")]),
    );
    wait_round(cx, &setup.health, 1).await;

    let claude_health = health_of(cx, &setup.health, Agent::Claude);
    assert_eq!(claude_health.state, HealthState::LoggedOut);
    assert_eq!(claude_health.fix, fix_logged_out(Agent::Claude, &claude));
    assert!(!claude_health.is_offered());
    assert!(claude_health.caps.is_some());
    let codex_health = health_of(cx, &setup.health, Agent::Codex);
    assert_eq!(codex_health.state, HealthState::LoggedOut);
    assert_eq!(
        codex_health.fix,
        format!("Run `{codex} login` in a terminal and log in")
    );
}

#[gpui::test]
async fn test_unchecked_before_a_probe_and_without_a_login_probe(cx: &mut TestAppContext) {
    let (claude, codex) = fakes();
    // Never probed: chats are off.
    let off = setup(cx, false, &claude, &codex, BTreeMap::new());
    let before = health_of(cx, &off.health, Agent::Claude);
    assert_eq!(before.state, HealthState::Unchecked);
    assert_eq!(before.fix, "Claude Code not checked yet");
    assert!(before.is_offered());
    drop(off);

    // The CLI has no offline login probe: ready, with a note.
    let on = setup(
        cx,
        true,
        &claude,
        &codex,
        env_of(&[("FAKE_CLI_NO_AUTH", "1")]),
    );
    wait_round(cx, &on.health, 1).await;
    let claude_health = health_of(cx, &on.health, Agent::Claude);
    assert_eq!(
        claude_health.state,
        HealthState::Ready {
            version: "2.1.319".to_string(),
            login_checked: false
        }
    );
    assert_eq!(claude_health.fix, "Claude Code login not checked");
    assert!(claude_health.is_offered());
}

#[gpui::test]
async fn test_hanging_version_probe_is_unchecked(cx: &mut TestAppContext) {
    let (claude, codex) = fakes();
    let setup = setup(
        cx,
        true,
        &claude,
        &codex,
        env_of(&[("FAKE_CLI_HANG", "all")]),
    );
    // Both `--version` calls are running and their timers are set: move the clock past them.
    cx.run_until_parked();
    cx.executor().advance_clock(SHORT_TIMEOUT * 2);
    wait_round(cx, &setup.health, 1).await;

    let claude_health = health_of(cx, &setup.health, Agent::Claude);
    assert_eq!(claude_health.state, HealthState::Unchecked);
    assert!(claude_health.fix.contains("did not answer"));
    assert!(claude_health.is_offered());
    assert!(claude_health.caps.is_none());
    assert_eq!(
        health_of(cx, &setup.health, Agent::Codex).state,
        HealthState::Unchecked
    );
}

#[gpui::test]
async fn test_a_hanging_probe_times_out_and_is_killed(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let program = fixture("fake-claude");
    let env = env_of(&[("FAKE_CLI_HANG", "all")]);
    let executor = cx.executor();
    let hung = cx.background_spawn({
        let executor = executor.clone();
        async move {
            run_probe(
                &program,
                &["--version".to_string()],
                &env,
                &executor,
                SHORT_TIMEOUT,
            )
            .await
        }
    });
    cx.run_until_parked();
    executor.advance_clock(SHORT_TIMEOUT * 2);
    let started = Instant::now();
    let hung = hung.await;
    assert!(matches!(hung, Probe::TimedOut), "{hung:?}");
    // Well before the fake's 30 s sleep ends.
    assert!(started.elapsed() < Duration::from_secs(10));

    // The same program answers when it does not hang.
    let answered = run_probe(
        &fixture("fake-claude"),
        &["--version".to_string()],
        &BTreeMap::new(),
        &executor,
        Duration::from_secs(20),
    )
    .await;
    assert!(
        matches!(&answered, Probe::Done { status: 0, text } if text.contains("2.1.319")),
        "{answered:?}"
    );
}

#[gpui::test]
async fn test_nothing_probes_while_disabled(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let log = temp.path().join("calls.log");
    let (claude, codex) = fakes();
    let env = env_of(&[("FAKE_CLI_LOG", log.to_str().unwrap())]);
    let setup = setup(cx, false, &claude, &codex, env.clone());

    setup.health.update(cx, |health, cx| {
        health.refresh(cx);
        health.refresh_now(cx);
        health.set_env(
            env_of(&[("FAKE_CLI_LOG", log.to_str().unwrap()), ("X", "1")]),
            cx,
        );
    });
    pump(cx, 500);
    setup.health.read_with(cx, |health, _| {
        assert_eq!(health.stats().rounds, 0);
        assert!(!health.is_probing());
    });
    assert!(!log.exists(), "a CLI was run while chats were off");
    assert_eq!(*setup.events.lock().unwrap(), 0);

    // Turning chats on probes; turning them off again resets the health and probes no more.
    set_settings(cx, true, &claude, &codex);
    wait_round(cx, &setup.health, 1).await;
    assert!(!calls(&log).is_empty());
    assert!(matches!(
        health_of(cx, &setup.health, Agent::Claude).state,
        HealthState::Ready { .. }
    ));

    set_settings(cx, false, &claude, &codex);
    let seen = calls(&log).len();
    assert_eq!(
        health_of(cx, &setup.health, Agent::Claude).state,
        HealthState::Unchecked
    );
    setup.health.update(cx, |health, cx| health.refresh_now(cx));
    pump(cx, 500);
    assert_eq!(calls(&log).len(), seen);
    setup.health.read_with(cx, |health, _| {
        assert_eq!(health.stats().rounds, 1);
    });
}

#[gpui::test]
async fn test_capabilities_are_cached_per_version(cx: &mut TestAppContext) {
    let temp = TempDir::new().unwrap();
    let log = temp.path().join("calls.log");
    let (claude, codex) = fakes();
    let base = [("FAKE_CLI_LOG", log.to_str().unwrap())];
    let setup = setup(cx, true, &claude, &codex, env_of(&base));
    wait_round(cx, &setup.health, 1).await;
    let help_calls = |log: &Path, program: &str| {
        calls(log)
            .iter()
            .filter(|call| call.as_str() == format!("{program} --help"))
            .count()
    };
    assert_eq!(help_calls(&log, "claude"), 1);
    assert_eq!(*setup.events.lock().unwrap(), 1);

    // Asked again within a minute: nothing runs.
    setup.health.update(cx, |health, cx| health.refresh(cx));
    pump(cx, 200);
    setup.health.read_with(cx, |health, _| {
        assert_eq!(health.stats().rounds, 1);
    });

    // Forced, same version: `--version` runs again, `--help` does not, and nothing changed.
    setup.health.update(cx, |health, cx| health.refresh_now(cx));
    wait_round(cx, &setup.health, 2).await;
    assert_eq!(help_calls(&log, "claude"), 1);
    assert_eq!(help_calls(&log, "codex"), 1);
    assert_eq!(
        calls(&log)
            .iter()
            .filter(|call| call.as_str() == "claude --version")
            .count(),
        2
    );
    assert_eq!(*setup.events.lock().unwrap(), 1);

    // An updated CLI reports a new version: its help is read again and the health changes.
    let mut updated = env_of(&base);
    updated.insert("FAKE_CLI_VERSION".to_string(), "2.2.0".to_string());
    setup
        .health
        .update(cx, |health, cx| health.set_env(updated, cx));
    wait_round(cx, &setup.health, 3).await;
    assert_eq!(help_calls(&log, "claude"), 2);
    assert_eq!(
        health_of(cx, &setup.health, Agent::Claude).state,
        HealthState::Ready {
            version: "2.2.0".to_string(),
            login_checked: true
        }
    );
    assert_eq!(*setup.events.lock().unwrap(), 2);
}

#[gpui::test]
async fn test_changing_a_command_probes_again(cx: &mut TestAppContext) {
    let (claude, codex) = fakes();
    let setup = setup(
        cx,
        true,
        "bongos-no-such-cli-claude",
        &codex,
        BTreeMap::new(),
    );
    wait_round(cx, &setup.health, 1).await;
    assert_eq!(
        health_of(cx, &setup.health, Agent::Claude).state,
        HealthState::NotFound
    );

    set_settings(cx, true, &claude, &codex);
    wait_round(cx, &setup.health, 2).await;
    assert!(matches!(
        health_of(cx, &setup.health, Agent::Claude).state,
        HealthState::Ready { .. }
    ));
}

#[test]
fn test_find_program_uses_the_given_path() {
    let dir = fixture("");
    let env = env_of(&[("PATH", dir.to_str().unwrap())]);
    assert_eq!(
        find_program("fake-claude", &env),
        Some(dir.join("fake-claude"))
    );
    assert_eq!(find_program("fake-nothing", &env), None);
    assert_eq!(
        find_program(fixture("fake-codex").to_str().unwrap(), &BTreeMap::new()),
        Some(fixture("fake-codex"))
    );
    assert_eq!(find_program("  ", &env), None);
    // A file that is not executable is not a program.
    assert_eq!(
        find_program("fake-claude", &env_of(&[("PATH", "/nonexistent")])),
        None
    );
}

#[test]
fn test_fix_texts() {
    assert_eq!(
        fix_not_found(Agent::Claude, "claude"),
        "claude was not found on PATH; install Claude Code or set chats.agents.claude.command"
    );
    assert_eq!(
        fix_not_found(Agent::Codex, "codex"),
        "codex was not found on PATH; install Codex or set chats.agents.codex.command"
    );
    assert_eq!(
        fix_logged_out(Agent::Claude, "claude"),
        "Run `claude` in a terminal and log in"
    );
    assert_eq!(
        fix_logged_out(Agent::Codex, "codex"),
        "Run `codex login` in a terminal and log in"
    );
}
