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
        let mut expected = ENV_NAMES.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert!(spec.env.values().all(|value| value == "v"));
    }
    assert_eq!(ENV_NAMES.len(), 11);
}
