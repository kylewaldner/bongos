//! Tests of MCP discovery and the start-time checks: parsing `mcp list` text of both CLIs
//! (the Claude fixtures are recorded from Claude Code 2.1.322; the Codex ones follow the assumed
//! format), the configuration-file fallback, the cache, the lister that runs a fake CLI, and the
//! refusals that name a server.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bongos_workflow_model::definition::AgentKind;
use bongos_workflow_model::{FailureReason, NodeKey, ValidationContext, parse, validate};
use tempfile::TempDir;

use crate::mcp::{
    CACHE_TTL, Discovery, Lister, Location, McpCache, ProcessLister, Source, available,
    check_start, codex_disable_args, discover, parse_list, validation_servers,
};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fixtures");
const FAKE_CLAUDE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/test-fixtures/fake-claude-headless"
);
const FAKE_CODEX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test-fixtures/fake-codex-exec");

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(FIXTURES).join(name)).unwrap()
}

fn names(servers: &[&str]) -> BTreeSet<String> {
    servers.iter().map(|name| name.to_string()).collect()
}

fn list(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// A lister that answers with fixed text and counts its calls.
struct Fixed {
    text: Option<String>,
    calls: std::cell::Cell<u32>,
}

impl Fixed {
    fn new(text: Option<&str>) -> Self {
        Self {
            text: text.map(str::to_string),
            calls: std::cell::Cell::new(0),
        }
    }
}

impl Lister for Fixed {
    fn list(&self, _: AgentKind, _: &Path) -> Option<String> {
        self.calls.set(self.calls.get() + 1);
        self.text.clone()
    }
}

/// An isolated home with the fixture configuration files of both CLIs.
fn home_with_configs() -> (TempDir, Location) {
    let root = TempDir::new().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("work/proj");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    // The fixture names `/work/proj`; point it at this temp project.
    let config = fixture("claude-config.json").replace("/work/proj", &project.to_string_lossy());
    std::fs::write(home.join(".claude.json"), config).unwrap();
    std::fs::write(
        project.join(".mcp.json"),
        fixture("claude-project-mcp.json"),
    )
    .unwrap();
    std::fs::write(
        home.join(".codex/config.toml"),
        fixture("codex-config.toml"),
    )
    .unwrap();
    let location = Location::new(project, home);
    (root, location)
}

#[test]
fn claude_list_text_gives_names() {
    // Recorded: four servers that all failed to connect.
    let recorded = parse_list(AgentKind::Claude, &fixture("claude-mcp-list.txt")).unwrap();
    assert_eq!(recorded, names(&["alpha", "beta", "delta", "gamma"]));
    // The other status marks, and a name that holds a dash.
    let mixed = parse_list(AgentKind::Claude, &fixture("claude-mcp-list-mixed.txt")).unwrap();
    assert_eq!(mixed, names(&["broken", "github", "sentry"]));
    // No servers is an answer, not a failure.
    let empty = parse_list(AgentKind::Claude, &fixture("claude-mcp-list-empty.txt")).unwrap();
    assert!(empty.is_empty());
    // Text that is not a list is no answer.
    assert_eq!(parse_list(AgentKind::Claude, ""), None);
    assert_eq!(
        parse_list(AgentKind::Claude, "error: unknown command 'mcp'\n"),
        None
    );
}

#[test]
fn codex_list_text_gives_names_from_every_table() {
    let servers = parse_list(AgentKind::Codex, &fixture("codex-mcp-list.txt")).unwrap();
    // `linear` is disabled but configured, and the second table holds `sentry`.
    assert_eq!(servers, names(&["github", "linear", "sentry"]));
    let empty = parse_list(AgentKind::Codex, &fixture("codex-mcp-list-empty.txt")).unwrap();
    assert!(empty.is_empty());
    assert_eq!(parse_list(AgentKind::Codex, "warning: something\n"), None);
}

#[test]
fn discovery_prefers_the_list_and_falls_back_to_the_files() {
    let (_root, location) = home_with_configs();

    let from_list = discover(
        AgentKind::Claude,
        true,
        &Fixed::new(Some(&fixture("claude-mcp-list-mixed.txt"))),
        &location,
    );
    assert_eq!(from_list.source, Source::List);
    assert_eq!(from_list.servers, names(&["broken", "github", "sentry"]));

    // Claude: the user config, this project's entry (not another project's) and `.mcp.json`.
    for lister in [
        Fixed::new(None),
        Fixed::new(Some("error: unknown command\n")),
    ] {
        let from_files = discover(AgentKind::Claude, true, &lister, &location);
        assert_eq!(from_files.source, Source::Config);
        assert_eq!(
            from_files.servers,
            names(&["alpha", "beta", "gamma", "shared"])
        );
    }
    // A CLI without the `mcp` command is never asked.
    let never = Fixed::new(Some(&fixture("claude-mcp-list-mixed.txt")));
    let from_files = discover(AgentKind::Claude, false, &never, &location);
    assert_eq!(never.calls.get(), 0);
    assert_eq!(from_files.source, Source::Config);

    // Codex: the `[mcp_servers.*]` tables, nested tables not counted.
    let codex = discover(AgentKind::Codex, false, &Fixed::new(None), &location);
    assert_eq!(codex.source, Source::Config);
    assert_eq!(codex.servers, names(&["github", "sentry"]));

    // No files at all: nothing, from the files.
    let bare = Location::new(location.cwd.clone(), location.cwd.join("no-home"));
    let nothing = discover(AgentKind::Codex, false, &Fixed::new(None), &bare);
    assert!(nothing.servers.is_empty());
    let nothing = discover(AgentKind::Claude, false, &Fixed::new(None), &bare);
    assert_eq!(nothing.servers, names(&["shared"]));
}

#[test]
fn a_discovery_is_cached_for_sixty_seconds() {
    let (_root, location) = home_with_configs();
    let lister = Fixed::new(Some(&fixture("claude-mcp-list-mixed.txt")));
    let mut cache = McpCache::default();
    let start = Instant::now();
    let first = cache.discover(start, AgentKind::Claude, true, &lister, &location);
    cache.discover(
        start + Duration::from_secs(59),
        AgentKind::Claude,
        true,
        &lister,
        &location,
    );
    assert_eq!(lister.calls.get(), 1);
    // Another CLI has its own entry.
    cache.discover(start, AgentKind::Codex, true, &lister, &location);
    assert_eq!(lister.calls.get(), 2);
    // Sixty seconds on, it is read again.
    let later = start + CACHE_TTL;
    assert!(cache.get(AgentKind::Claude, &location.cwd, later).is_none());
    let again = cache.discover(later, AgentKind::Claude, true, &lister, &location);
    assert_eq!(lister.calls.get(), 3);
    assert_eq!(again, first);
    cache.clear();
    assert!(cache.get(AgentKind::Claude, &location.cwd, later).is_none());
}

#[test]
fn the_process_lister_runs_mcp_list_of_a_fake_cli() {
    let root = TempDir::new().unwrap();
    let lister = ProcessLister {
        programs: BTreeMap::from([
            (AgentKind::Claude, PathBuf::from(FAKE_CLAUDE)),
            (AgentKind::Codex, PathBuf::from(FAKE_CODEX)),
        ]),
        env: BTreeMap::from([
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            (
                "HOME".to_string(),
                root.path().to_string_lossy().into_owned(),
            ),
        ]),
        scratch: root.path().join("scratch"),
        timeout: Duration::from_secs(20),
    };
    let location = Location::new(root.path(), root.path());
    let claude = discover(AgentKind::Claude, true, &lister, &location);
    assert_eq!(claude.source, Source::List);
    assert_eq!(claude.servers, names(&["alpha", "beta", "delta", "gamma"]));
    let codex = discover(AgentKind::Codex, true, &lister, &location);
    assert_eq!(codex.source, Source::List);
    assert_eq!(codex.servers, names(&["github", "linear", "sentry"]));
    // The captured output is not left behind.
    assert_eq!(
        std::fs::read_dir(root.path().join("scratch"))
            .unwrap()
            .count(),
        0
    );

    // A program that is not there gives no list, and discovery falls back.
    let missing = ProcessLister {
        programs: BTreeMap::from([(AgentKind::Claude, root.path().join("no-claude"))]),
        ..lister
    };
    assert_eq!(missing.list(AgentKind::Claude, root.path()), None);
    assert_eq!(missing.list(AgentKind::Codex, root.path()), None);
    let fallback = discover(AgentKind::Claude, true, &missing, &location);
    assert_eq!(fallback.source, Source::Config);
}

#[test]
fn an_uninstalled_server_refuses_the_start_naming_it() {
    let key = NodeKey::new("triage", 1);
    let installed = names(&["github"]);
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let refusal = check_start(
            agent,
            Some(&key),
            &list(&["Read", "mcp__github__*", "mcp__sentry__*"]),
            &installed,
            &[],
        )
        .unwrap_err();
        assert_eq!(refusal.reason, FailureReason::McpServerUnavailable);
        let cli = crate::mcp::cli_name(agent);
        assert_eq!(
            refusal.detail,
            format!(
                "sentry: MCP server not installed for {cli}; install it with `{cli} mcp add`, \
                 or remove it from allowed_tools"
            )
        );
        assert_eq!(refusal.reason.label(), "mcp server unavailable");
        assert_eq!(refusal.node.as_ref(), Some(&key));
        // The installed one alone is fine.
        check_start(
            agent,
            Some(&key),
            &list(&["mcp__github__*"]),
            &installed,
            &[],
        )
        .unwrap();
    }
    // A bare server name is refused, as validation does.
    let bare = check_start(
        AgentKind::Claude,
        None,
        &list(&["mcp__github"]),
        &installed,
        &[],
    )
    .unwrap_err();
    assert!(
        bare.detail.contains("names a server, not a tool"),
        "{}",
        bare.detail
    );
}

#[test]
fn an_excluded_server_is_gone_for_both_clis() {
    let installed = names(&["github", "linear"]);
    let exclude = list(&["github"]);
    // It cannot be allowed, and validation sees it as not installed.
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        let refusal = check_start(
            agent,
            None,
            &list(&["mcp__github__*"]),
            &installed,
            &exclude,
        )
        .unwrap_err();
        assert!(
            refusal
                .detail
                .starts_with("github: excluded by workflows.mcp_exclude")
        );
    }
    let discovered = BTreeMap::from([
        (
            AgentKind::Claude,
            Discovery {
                servers: installed.clone(),
                source: Source::List,
            },
        ),
        (
            AgentKind::Codex,
            Discovery {
                servers: installed.clone(),
                source: Source::Config,
            },
        ),
    ]);
    let for_validation = validation_servers(&discovered, &exclude);
    assert_eq!(for_validation[&AgentKind::Claude], names(&["linear"]));
    assert_eq!(for_validation[&AgentKind::Codex], names(&["linear"]));
    assert_eq!(
        available(&discovered[&AgentKind::Codex], &exclude),
        names(&["linear"])
    );

    // On Codex it is disabled even though nothing names it; allowed servers stay on.
    assert_eq!(
        codex_disable_args(&installed, &list(&["mcp__linear__*"]), &exclude),
        list(&["-c", "mcp_servers.github.enabled=false"])
    );
    assert_eq!(
        codex_disable_args(&installed, &[], &[]),
        list(&[
            "-c",
            "mcp_servers.github.enabled=false",
            "-c",
            "mcp_servers.linear.enabled=false"
        ])
    );
    // A name that is not a plain key is quoted.
    assert_eq!(
        codex_disable_args(&names(&["my.server"]), &[], &[]),
        list(&["-c", "mcp_servers.\"my.server\".enabled=false"])
    );
}

#[test]
fn a_single_tool_entry_on_codex_is_rejected_at_validation_and_at_start() {
    let text = "schema = 1\nname = \"w\"\nstart = \"a\"\n\n[nodes.a]\nkind = \"agent\"\n\
                agent = \"codex\"\nprompt = \"a.md\"\n\
                allowed_tools = [\"mcp__linear__search\"]\n\
                output = { note = { type = \"text\" } }\nroutes = [{ to = \"done\" }]\n\n\
                [ends.done]\nkind = \"success\"\n";
    let definition = parse(text).unwrap();
    let discovered = BTreeMap::from([(
        AgentKind::Codex,
        Discovery {
            servers: names(&["linear"]),
            source: Source::List,
        },
    )]);
    let context = ValidationContext {
        mcp_servers: validation_servers(&discovered, &[]),
        dir_exists: true,
        files: names(&["a.md"]),
        ..ValidationContext::default()
    };
    let diagnostics = validate(&definition, &context);
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.is_error()
            && diagnostic
                .message
                .contains("codex limits MCP by server; use mcp__linear__*")),
        "{diagnostics:?}"
    );

    let refusal = check_start(
        AgentKind::Codex,
        None,
        &list(&["mcp__linear__search"]),
        &names(&["linear"]),
        &[],
    )
    .unwrap_err();
    assert!(
        refusal
            .detail
            .contains("codex limits MCP by server; use mcp__linear__*"),
        "{}",
        refusal.detail
    );
    // The same entry is fine on Claude, and the whole server is fine on Codex.
    check_start(
        AgentKind::Claude,
        None,
        &list(&["mcp__linear__search"]),
        &names(&["linear"]),
        &[],
    )
    .unwrap();
    check_start(
        AgentKind::Codex,
        None,
        &list(&["mcp__linear__*"]),
        &names(&["linear"]),
        &[],
    )
    .unwrap();
}

#[test]
fn a_malformed_exclude_name_refuses_the_start() {
    for agent in [AgentKind::Claude, AgentKind::Codex] {
        for bad in ["", "a b", "x__y\"", "a/b"] {
            let refusal =
                check_start(agent, None, &[], &BTreeSet::new(), &[bad.to_string()]).unwrap_err();
            assert_eq!(refusal.reason, FailureReason::McpServerUnavailable);
            assert!(
                refusal.detail.starts_with("workflows.mcp_exclude:"),
                "{bad}"
            );
        }
        assert!(check_start(agent, None, &[], &BTreeSet::new(), &list(&["ok-name_1.x"])).is_ok());
    }
}

#[test]
fn codex_does_not_disable_an_excluded_server_that_is_not_installed() {
    let installed = names(&["github"]);
    let args = codex_disable_args(&installed, &[], &list(&["ghost"]));
    assert_eq!(args, list(&["-c", "mcp_servers.github.enabled=false"]));
}
