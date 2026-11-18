//! Capabilities parsing, versions, fallback labels and the helpers the adapter tests share.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::*;

pub(crate) const CLAUDE_HELP: &str = include_str!("fixtures/help/claude-2.1.319.txt");
pub(crate) const CODEX_HELP: &str = include_str!("fixtures/help/codex-unverified.txt");
pub(crate) const CLAUDE_VERSION: &str = "2.1.319 (Claude Code)";
pub(crate) const SESSION: &str = "7f1c2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f";
pub(crate) const SOURCE: &str = "11111111-2222-4333-8444-555555555555";
pub(crate) const RUNTIME: &str = "/rt/chats/0190aaaa";
/// A path that only a shared artifact could carry; no argv may hold it.
pub(crate) const ARTIFACT: &str = "/work/tasks/billing/notes/secret-design.md";

/// The fixture without the named lines, so a flag or subcommand is "missing".
pub(crate) fn help_without(help: &str, drop: &[&str]) -> String {
    help.lines()
        .filter(|line| !drop.contains(&line.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn claude_caps(drop: &[&str]) -> Capabilities {
    ClaudeAdapter::new("claude")
        .capabilities_from_help(CLAUDE_VERSION, &help_without(CLAUDE_HELP, drop))
}

pub(crate) fn codex_caps(drop: &[&str]) -> Capabilities {
    CodexAdapter::new("codex").capabilities_from_help("0.0.0", &help_without(CODEX_HELP, drop))
}

pub(crate) fn files() -> GeneratedFiles {
    GeneratedFiles {
        system_path: PathBuf::from(format!("{RUNTIME}/system.md")),
        system_text: format!("SYSTEM-TEXT with {ARTIFACT}"),
        settings_path: Some(PathBuf::from(format!("{RUNTIME}/claude-settings.json"))),
        plugin_dir: Some(PathBuf::from(format!("{RUNTIME}/plugin"))),
        has_skills: true,
    }
}

pub(crate) fn plan(agent: Agent, kind: LaunchKind, session_id: Option<&str>) -> LaunchPlan {
    let mut chat = ChatRecord::new(agent, "implement", PathBuf::from("/repos/billing/wt"));
    chat.name = "Billing proration".to_string();
    chat.session_id = session_id.map(str::to_string);
    let env = [
        ("BONGOS_CHAT_ID", "0190aaaa"),
        ("BONGOS_WORKTREE", "/repos/billing/wt"),
        ("ANTHROPIC_API_KEY", "should-never-pass"),
        ("HOME", "/home/kyle"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect::<BTreeMap<_, _>>();
    LaunchPlan {
        kind,
        chat,
        files: files(),
        model: Some("haiku".to_string()),
        extra_args: vec!["--verbose".to_string()],
        env,
        force: Vec::new(),
        channel: None,
    }
}

/// A token value no generated text or argument may hold.
pub(crate) const TOKEN_MARKER: &str = "tok3nMARKERzz9q";
pub(crate) const SOCKET: &str = "/run/bongos/chat.sock";

pub(crate) fn reporter(program: &str) -> Vec<String> {
    strings(&[program, "--chat-report"])
}

pub(crate) fn channel_of(program: &str) -> ChannelPlan {
    ChannelPlan::new(
        &reporter(program),
        PathBuf::from(SOCKET),
        channel::report::Token::new(TOKEN_MARKER).unwrap(),
    )
    .unwrap()
}

/// `plan` with the channel of `/opt/bongos/bongos`.
pub(crate) fn channel_plan(agent: Agent, kind: LaunchKind, session_id: Option<&str>) -> LaunchPlan {
    let mut plan = plan(agent, kind, session_id);
    plan.channel = Some(channel_of("/opt/bongos/bongos"));
    plan
}

#[test]
fn the_channel_quotes_every_reporter_word_and_hides_its_token() {
    let plan = channel_of("/opt/my bongos/bongos");
    assert_eq!(
        plan.reporter_command(),
        "'/opt/my bongos/bongos' '--chat-report'"
    );
    assert_eq!(plan.socket(), std::path::Path::new(SOCKET));
    assert!(!format!("{plan:?}").contains(TOKEN_MARKER));
    let env = plan.env("0190aaaa");
    assert_eq!(env[CHANNEL_SOCKET_ENV], SOCKET);
    assert_eq!(env["BONGOS_CHAT_ID"], "0190aaaa");
    assert_eq!(env[CHANNEL_TOKEN_ENV], TOKEN_MARKER);
    for name in env.keys() {
        assert!(ENV_NAMES.contains(&name.as_str()), "{name}");
    }
}

#[test]
fn an_unquotable_reporter_turns_the_channel_off() {
    for program in ["/opt/it's/bongos", "/opt/a\nb/bongos", ""] {
        let mut force = Vec::new();
        let plan = plan_channel(
            &reporter(program),
            PathBuf::from(SOCKET),
            channel::report::Token::new(TOKEN_MARKER).unwrap(),
            &mut force,
        );
        assert!(plan.is_none(), "{program:?}");
        assert_eq!(
            force,
            vec![Fallback::ChannelOff(ChannelOff::UnquotablePath)]
        );
        assert_eq!(force[0].label(), "the bongos path cannot be quoted");
    }
    let mut force = Vec::new();
    assert!(
        plan_channel(
            &[],
            PathBuf::from(SOCKET),
            channel::report::Token::new(TOKEN_MARKER).unwrap(),
            &mut force,
        )
        .is_none()
    );
    assert_eq!(
        force,
        vec![Fallback::ChannelOff(ChannelOff::UnquotablePath)]
    );
}

#[test]
fn the_codex_line_ignores_a_channel() {
    let kinds = [
        LaunchKind::New,
        LaunchKind::Resume,
        LaunchKind::Fork {
            source_session: SOURCE.to_string(),
            native: true,
        },
    ];
    for kind in kinds {
        for session in [Some(SESSION), None] {
            let adapter = CodexAdapter::new("codex");
            let without =
                adapter.launch(&plan(Agent::Codex, kind.clone(), session), &codex_caps(&[]));
            let with = adapter.launch(
                &channel_plan(Agent::Codex, kind.clone(), session),
                &codex_caps(&[]),
            );
            assert_eq!(with, without, "{kind:?} {session:?}");
            assert!(!with.channel);
            assert!(!with.env.contains_key(CHANNEL_TOKEN_ENV));
        }
    }
}

#[test]
fn a_plain_env_never_passes_the_channel_names() {
    let mut plan = plan(Agent::Claude, LaunchKind::New, Some(SESSION));
    plan.env
        .insert(CHANNEL_TOKEN_ENV.to_string(), TOKEN_MARKER.to_string());
    plan.env
        .insert(CHANNEL_SOCKET_ENV.to_string(), SOCKET.to_string());
    for spec in [
        ClaudeAdapter::new("claude").launch(&plan, &claude_caps(&[])),
        CodexAdapter::new("codex").launch(&plan, &codex_caps(&[])),
    ] {
        assert!(!spec.env.contains_key(CHANNEL_TOKEN_ENV));
        assert!(!spec.env.contains_key(CHANNEL_SOCKET_ENV));
        assert!(!spec.channel);
    }
}

pub(crate) fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

#[test]
fn help_gives_option_lines_and_command_words() {
    let caps = claude_caps(&[]);
    for flag in [
        "--session-id",
        "--resume",
        "--fork-session",
        "--name",
        "--settings",
        "-n",
        "--plugin-dir",
    ] {
        assert!(caps.has_flag(flag), "{flag}");
    }
    for name in ["auth", "mcp", "doctor"] {
        assert!(caps.has_subcommand(name), "{name}");
    }
    assert_eq!(caps.version(), "2.1.319");
    assert!(!caps.has_flag("--nonsense"));
}

#[test]
fn prose_and_option_descriptions_do_not_count() {
    let help = "Usage: tool [options] [command]\n\
        This text mentions --prose-flag and the word status\n\
        \x20 -x, --extra <value>   Description naming --not-a-flag, and more\n\
        \x20 --model=<name>  A model\n\
        Commands:\n\
        \x20 login  Log in\n\
        \x20 sync [options]  Sync things\n\
        Some prose line here\n";
    let caps = Capabilities::from_help("1.0.0", help, &[]);
    assert!(caps.has_flag("-x") && caps.has_flag("--extra") && caps.has_flag("--model"));
    assert!(!caps.has_flag("--prose-flag") && !caps.has_flag("--not-a-flag"));
    assert!(caps.has_subcommand("login") && caps.has_subcommand("sync"));
    assert!(!caps.has_subcommand("some") && !caps.has_subcommand("this"));
}

#[test]
fn a_hidden_flag_is_present_from_its_minimum_version() {
    // The fixture omits `--append-system-prompt-file` on purpose; `--help` hides it.
    assert!(!CLAUDE_HELP.contains("--append-system-prompt-file"));
    let adapter = ClaudeAdapter::new("claude");
    let at = adapter.capabilities_from_help("2.1.319 (Claude Code)", CLAUDE_HELP);
    assert!(at.has_flag("--append-system-prompt-file"));
    let later = adapter.capabilities_from_help("2.2.0", CLAUDE_HELP);
    assert!(later.has_flag("--append-system-prompt-file"));
    let earlier = adapter.capabilities_from_help("2.1.318", CLAUDE_HELP);
    assert!(!earlier.has_flag("--append-system-prompt-file"));
    let unknown = adapter.capabilities_from_help("not a version", CLAUDE_HELP);
    assert!(!unknown.has_flag("--append-system-prompt-file"));
}

#[test]
fn versions_compare_by_number() {
    assert!(version_at_least("2.1.319", "2.1.319"));
    assert!(version_at_least("2.10.0", "2.9.9"));
    assert!(version_at_least("v2.2.0", "2.1.319"));
    assert!(!version_at_least("2.1.99", "2.1.319"));
    assert!(!version_at_least("", "2.1.319"));
}

const TABLE: VersionTable = VersionTable::new(&[
    VersionRow {
        name: "--old",
        min_version: "1.2.0",
        verified: true,
    },
    VersionRow {
        name: "--new",
        min_version: "1.10.0",
        verified: false,
    },
    VersionRow {
        name: "thing",
        min_version: "1.2.0",
        verified: true,
    },
    VersionRow {
        name: "thing",
        min_version: "1.5.0",
        verified: true,
    },
]);

fn table_has(version: Option<&str>) -> Vec<&'static str> {
    let caps = Capabilities::from_version(version, &TABLE);
    ["--old", "--new", "thing", "absent"]
        .into_iter()
        .filter(|name| caps.has(name))
        .collect()
}

#[test]
fn a_version_table_gives_rows_at_or_below_the_version() {
    assert_eq!(table_has(Some("1.1.9")), Vec::<&str>::new());
    assert_eq!(table_has(Some("1.2.0")), ["--old", "thing"]);
    assert_eq!(table_has(Some("1.4.9")), ["--old", "thing"]);
    assert_eq!(table_has(Some("1.9.9")), ["--old", "thing"]);
    assert_eq!(table_has(Some("1.10.0")), ["--old", "--new", "thing"]);
    // Newer than every row keeps every row.
    assert_eq!(table_has(Some("7.0.0")), ["--old", "--new", "thing"]);
}

#[test]
fn a_version_table_reads_the_versions_the_parser_reads() {
    for version in [
        "v1.10.0",
        "1.10.0 (Some Tool)",
        "  1.10.0\n",
        "1.10.0.",
        "1.10.0.4",
    ] {
        assert_eq!(
            table_has(Some(version)),
            ["--old", "--new", "thing"],
            "{version}"
        );
    }
    // `1.2` reads as 1.2.0.
    assert_eq!(table_has(Some("1.2")), ["--old", "thing"]);
    assert_eq!(table_has(Some("1.1")), Vec::<&str>::new());
}

#[test]
fn an_unreadable_version_gives_no_capabilities() {
    for version in [
        None,
        Some(""),
        Some("   "),
        Some("abc"),
        Some("not a version"),
        Some("1"),
        Some("1.x.3"),
        Some("1.10.0-beta"),
    ] {
        let caps = Capabilities::from_version(version, &TABLE);
        assert!(!caps.has("--old") && !caps.has_flag("--old"), "{version:?}");
        assert!(!caps.has("thing"), "{version:?}");
    }
    assert_eq!(
        Capabilities::from_version(None, &TABLE),
        Capabilities::default()
    );
    assert_eq!(Capabilities::from_version(Some(""), &TABLE).version(), "");
}

#[test]
fn flag_rows_answer_has_flag_and_other_rows_do_not() {
    let caps = Capabilities::from_version(Some("1.10.0"), &TABLE);
    assert!(caps.has_flag("--old") && caps.has_flag("--new"));
    assert!(caps.has("thing") && !caps.has_flag("thing") && !caps.has_subcommand("thing"));
    assert_eq!(caps.version(), "1.10.0");
    let older = Capabilities::from_version(Some("v1.3.0 (Tool)"), &TABLE);
    assert_eq!(older.version(), "1.3.0");
    assert!(older.has_flag("--old") && !older.has_flag("--new"));
}

#[test]
fn from_help_capabilities_hold_no_table_rows() {
    let caps = claude_caps(&[]);
    assert!(!caps.has("--settings") && caps.has_flag("--settings"));
}

#[test]
fn every_fallback_has_a_label() {
    let all = [
        Fallback::SystemPromptAsText,
        Fallback::SystemPromptUnavailable,
        Fallback::ContextInSystemPrompt,
        Fallback::SkillsAsText,
        Fallback::NameOmitted,
        Fallback::ModelOmitted,
        Fallback::CaptureSessionId,
        Fallback::ForkUnsupported,
        Fallback::RestartNoSession,
        Fallback::ResumedLatest,
        Fallback::ContextAsFirstMessage,
    ];
    let labels: std::collections::BTreeSet<_> = all.iter().map(|f| f.label()).collect();
    assert_eq!(labels.len(), all.len());
    assert_eq!(
        Fallback::ResumedLatest.label(),
        "resumed the latest Codex session in this directory"
    );
    assert_eq!(
        Fallback::ContextAsFirstMessage.label(),
        "context sent as first message"
    );
}

#[test]
fn adapter_for_picks_the_agent() {
    assert_eq!(adapter_for(Agent::Claude, "claude").agent(), Agent::Claude);
    assert_eq!(adapter_for(Agent::Codex, "codex").agent(), Agent::Codex);
}

#[test]
fn the_environment_holds_only_the_d10_names() {
    let mut env: BTreeMap<String, String> = ENV_NAMES
        .iter()
        .map(|name| (name.to_string(), "v".to_string()))
        .collect();
    env.insert("ANTHROPIC_API_KEY".to_string(), "secret".to_string());
    env.insert("OPENAI_API_KEY".to_string(), "secret".to_string());
    env.insert("BONGOS_SECRET".to_string(), "secret".to_string());
    for agent in [Agent::Claude, Agent::Codex] {
        let mut plan = plan(agent, LaunchKind::New, Some(SESSION));
        plan.env = env.clone();
        let caps = if agent == Agent::Claude {
            claude_caps(&[])
        } else {
            codex_caps(&[])
        };
        let spec = adapter_for(agent, "x").launch(&plan, &caps);
        let names: Vec<_> = spec.env.keys().map(String::as_str).collect();
        // The channel's names come only from a channel plan, never from the plain env.
        let mut expected: Vec<_> = ENV_NAMES
            .iter()
            .copied()
            .filter(|name| ![CHANNEL_SOCKET_ENV, CHANNEL_TOKEN_ENV].contains(name))
            .collect();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert!(spec.env.values().all(|value| value == "v"));
    }
    assert_eq!(ENV_NAMES.len(), 13);
    assert!(ENV_NAMES.contains(&CHANNEL_SOCKET_ENV) && ENV_NAMES.contains(&CHANNEL_TOKEN_ENV));
}
