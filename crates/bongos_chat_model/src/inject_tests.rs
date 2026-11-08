use std::fs;
use std::path::{Path, PathBuf};

use bongos_work_model::{Kind, SessionContext, SharedArtifact, TaskFile, serialize_task};
use tempfile::TempDir;

use crate::*;

const SKILL_TEXT: &str = "SKILL-TEXT-MARKER route the work to cheaper models";
const TASK_DIR: &str = "/work/projects/invoicing/tasks/backfill";

fn artifact(path: &str, description: &str) -> SharedArtifact {
    SharedArtifact {
        path: PathBuf::from(path),
        description: description.to_string(),
    }
}

fn synthetic_context(artifacts: Vec<SharedArtifact>) -> SessionContext {
    SessionContext {
        goal: "Backfill usage events".to_string(),
        task_toml: Path::new(TASK_DIR).join("task.toml"),
        task_dir: PathBuf::from(TASK_DIR),
        worktree: Some(PathBuf::from("/repos/invoicing/wt/backfill")),
        parent_goal: Some("Invoicing".to_string()),
        artifacts,
    }
}

fn facts() -> TaskFacts {
    TaskFacts {
        project_title: Some("Invoicing".to_string()),
        project_dir: Some(PathBuf::from("/work/projects/invoicing")),
        notes_dir: Some(PathBuf::from(
            "/work/projects/invoicing/tasks/backfill/notes",
        )),
        deps: vec![
            ("Create the events table".to_string(), "done".to_string()),
            ("Review the schema".to_string(), "blocked".to_string()),
        ],
        pr_links: vec!["https://example.com/pull/7".to_string()],
    }
}

fn types_root() -> TempDir {
    let root = TempDir::new().unwrap();
    seed(root.path()).unwrap();
    let skill = root.path().join("skills/model-routing");
    fs::create_dir_all(&skill).unwrap();
    fs::write(skill.join("SKILL.md"), SKILL_TEXT).unwrap();
    root
}

fn resolved(root: &TempDir, chat_type: &str, agent: Agent) -> ResolvedType {
    resolve(
        chat_type,
        agent,
        root.path(),
        &root.path().join("claude-home"),
    )
    .unwrap()
}

fn record(agent: Agent, chat_type: &str) -> ChatRecord {
    let mut record = ChatRecord::new(
        agent,
        chat_type,
        PathBuf::from("/repos/invoicing/wt/backfill"),
    );
    record.id = "0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000".to_string();
    record
}

struct Fixture {
    context: SessionContext,
    facts: TaskFacts,
    record: ChatRecord,
    resolved: ResolvedType,
    handoff: Option<HandoffParent>,
    skills_as_plugin: bool,
}

impl Fixture {
    fn new(root: &TempDir, chat_type: &str, agent: Agent, artifacts: Vec<SharedArtifact>) -> Self {
        Self {
            context: synthetic_context(artifacts),
            facts: facts(),
            record: record(agent, chat_type),
            resolved: resolved(root, chat_type, agent),
            handoff: None,
            skills_as_plugin: false,
        }
    }

    fn input(&self) -> RenderInput<'_> {
        RenderInput {
            context: &self.context,
            facts: &self.facts,
            record: &self.record,
            resolved: &self.resolved,
            scope_dir: Path::new(TASK_DIR),
            task_id: Some("0190task-id"),
            record_path: Path::new("/work/projects/invoicing/tasks/backfill/chats/c.toml"),
            context_path: Path::new("/home/kyle/.local/share/bongos/chats/c/context.md"),
            bongos_root: Some(Path::new("/home/kyle/.bongos")),
            skills_as_plugin: self.skills_as_plugin,
            handoff: self.handoff.as_ref(),
        }
    }

    /// Everything a launch generates, as one list of strings.
    fn everything(&self) -> Vec<String> {
        let input = self.input();
        let mut all = vec![
            render_context(&input),
            render_system(&input),
            render_codex_instructions(&input),
            render_settings(input.context_path).unwrap(),
        ];
        all.extend(render_env(&input).into_values());
        if let Some(parent) = &self.handoff {
            all.push(handoff_note(parent));
        }
        all
    }
}

#[test]
fn plan_and_free_are_bare() {
    let root = types_root();
    for chat_type in ["plan", "free"] {
        let fixture = Fixture::new(
            &root,
            chat_type,
            Agent::Claude,
            vec![artifact("/t/a.md", "A")],
        );
        let context = render_context(&fixture.input());
        assert!(
            context.contains("Goal: Backfill usage events"),
            "{chat_type}"
        );
        assert!(context.contains("/t/a.md: A"), "{chat_type}");
        assert!(!context.contains("Notes directory"), "{chat_type}");
        assert!(!context.contains("Dependencies"), "{chat_type}");
        assert!(!context.contains("Create the events table"), "{chat_type}");
        assert!(!context.contains("pull/7"), "{chat_type}");
        assert!(!context.contains("thinking mode"), "{chat_type}");
        assert!(
            !render_system(&fixture.input()).contains("SKILL-TEXT"),
            "{chat_type}"
        );
    }
}

#[test]
fn implement_has_task_context_and_skill_text() {
    let root = types_root();
    let fixture = Fixture::new(&root, "implement", Agent::Claude, vec![]);
    let context = render_context(&fixture.input());
    assert!(context.contains("Task file: /work/projects/invoicing/tasks/backfill/task.toml"));
    assert!(context.contains("Task directory: /work/projects/invoicing/tasks/backfill"));
    assert!(context.contains("Worktree: /repos/invoicing/wt/backfill"));
    assert!(context.contains("Group goal: Invoicing"));
    assert!(context.contains("Project: Invoicing"));
    assert!(context.contains("Project directory: /work/projects/invoicing"));
    assert!(context.contains("Notes directory: /work/projects/invoicing/tasks/backfill/notes"));
    assert!(context.contains("- Create the events table (done)"));
    assert!(context.contains("- Review the schema (blocked)"));
    assert!(!context.contains("pull/7"));

    let system = render_system(&fixture.input());
    assert!(system.contains(SKILL_TEXT));

    let mut as_plugin = Fixture::new(&root, "implement", Agent::Claude, vec![]);
    as_plugin.skills_as_plugin = true;
    assert!(!render_system(&as_plugin.input()).contains(SKILL_TEXT));
}

#[test]
fn review_has_pr_links_and_no_task_context() {
    let root = types_root();
    let fixture = Fixture::new(&root, "review", Agent::Claude, vec![]);
    let context = render_context(&fixture.input());
    assert!(context.contains("- https://example.com/pull/7"));
    assert!(!context.contains("Notes directory"));
    assert!(!context.contains("Create the events table"));
}

#[test]
fn thinking_type_adds_the_thinking_line() {
    let root = types_root();
    let thinking = Fixture::new(&root, "thinking", Agent::Claude, vec![]);
    assert!(render_context(&thinking.input()).contains("This task is in thinking mode"));
    let implement = Fixture::new(&root, "implement", Agent::Claude, vec![]);
    assert!(!render_context(&implement.input()).contains("thinking mode"));
}

#[test]
fn an_empty_list_says_so() {
    let root = types_root();
    let fixture = Fixture::new(&root, "implement", Agent::Claude, vec![]);
    let context = render_context(&fixture.input());
    assert!(context.contains("## Shared artifacts"));
    assert!(context.contains("None are shared with this chat."));
}

#[test]
fn the_prompts_file_is_mentioned_only_when_shared() {
    let root = types_root();
    let prompts = format!("{TASK_DIR}/prompts.txt");

    let shared = Fixture::new(
        &root,
        "implement",
        Agent::Claude,
        vec![
            artifact(&prompts, "What Kyle asked"),
            artifact("/t/a.md", ""),
        ],
    );
    let context = render_context(&shared.input());
    assert!(context.contains(&format!("Prompts file: {prompts}")));

    let not_shared = Fixture::new(
        &root,
        "implement",
        Agent::Claude,
        vec![artifact("/t/a.md", "")],
    );
    let context = render_context(&not_shared.input());
    assert!(!context.contains("prompts.txt"));
    assert!(!context.contains("Prompts file"));

    // A record that points at another prompts file is checked against that path.
    let mut other = Fixture::new(
        &root,
        "implement",
        Agent::Claude,
        vec![artifact(&prompts, "")],
    );
    other.record.prompts_file = Some(PathBuf::from("/elsewhere/prompts.txt"));
    assert!(!render_context(&other.input()).contains("Prompts file"));
}

#[test]
fn codex_instructions_hold_the_context_and_claude_system_does_not() {
    let root = types_root();
    let claude = Fixture::new(
        &root,
        "implement",
        Agent::Claude,
        vec![artifact("/t/a.md", "A")],
    );
    let codex = Fixture::new(
        &root,
        "implement",
        Agent::Codex,
        vec![artifact("/t/a.md", "A")],
    );
    let context = render_context(&claude.input());
    assert!(!render_system(&claude.input()).contains("Goal: Backfill"));
    let instructions = render_codex_instructions(&codex.input());
    assert!(instructions.contains(render_context(&codex.input()).trim_end()));
    assert!(instructions.contains(SKILL_TEXT));
    assert!(context.contains("/t/a.md: A"));
}

#[test]
fn the_agent_override_follows_the_type_context() {
    let root = types_root();
    fs::write(
        root.path().join("implement/context.md"),
        "TYPE-CONTEXT-MARKER\n",
    )
    .unwrap();
    fs::write(
        root.path().join("implement/claude.md"),
        "CLAUDE-ONLY-OVERRIDE",
    )
    .unwrap();
    let claude = Fixture::new(&root, "implement", Agent::Claude, vec![]);
    let codex = Fixture::new(&root, "implement", Agent::Codex, vec![]);
    let system = render_system(&claude.input());
    assert!(system.contains("CLAUDE-ONLY-OVERRIDE"));
    let type_context = system.find("TYPE-CONTEXT-MARKER").unwrap();
    assert!(type_context < system.find("CLAUDE-ONLY-OVERRIDE").unwrap());
    assert!(!render_codex_instructions(&codex.input()).contains("CLAUDE-ONLY-OVERRIDE"));
}

#[test]
fn a_description_on_many_lines_stays_one_list_item() {
    let root = types_root();
    let fixture = Fixture::new(
        &root,
        "plan",
        Agent::Claude,
        vec![artifact("/t/a.md", "first\nsecond\n\n- injected item")],
    );
    let context = render_context(&fixture.input());
    assert!(context.contains("- /t/a.md: first second - injected item\n"));
}

#[test]
fn handoff_note_names_the_parent_and_its_transcript_only() {
    let parent = HandoffParent {
        name: "implement claude 2026-11-08".to_string(),
        chat_type: "implement".to_string(),
        agent: "claude".to_string(),
        transcript: Some(PathBuf::from("/home/kyle/.claude/projects/x/abc.jsonl")),
    };
    let note = handoff_note(&parent);
    assert!(note.contains("implement claude 2026-11-08"));
    assert!(note.contains("implement chat run with claude"));
    assert!(note.contains("/home/kyle/.claude/projects/x/abc.jsonl"));

    let missing = HandoffParent {
        transcript: None,
        ..parent.clone()
    };
    assert!(handoff_note(&missing).contains("transcript could not be found"));

    let root = types_root();
    let mut fixture = Fixture::new(&root, "implement", Agent::Claude, vec![]);
    fixture.handoff = Some(parent);
    assert!(render_system(&fixture.input()).contains("# Handoff"));
    let plain = Fixture::new(&root, "implement", Agent::Claude, vec![]);
    assert!(!render_system(&plain.input()).contains("# Handoff"));
}

#[test]
fn the_hook_command_quotes_the_path() {
    let settings = render_settings(Path::new("/home/kyle/my chats/c d/context.md")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&settings).unwrap();
    let hook = &value["hooks"]["SessionStart"][0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    assert_eq!(hook["command"], "cat '/home/kyle/my chats/c d/context.md'");

    // Characters JSON must escape stay valid JSON.
    let settings = render_settings(Path::new("/tmp/a\"b\\c$d`e/context.md")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(
        value["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "cat '/tmp/a\"b\\c$d`e/context.md'"
    );
}

#[test]
fn the_hook_refuses_a_path_it_cannot_quote() {
    for path in [
        "/tmp/it's/context.md",
        "/tmp/a\nb/context.md",
        "/tmp/a\0b/context.md",
        "/tmp/a\rb/context.md",
        "",
    ] {
        let error = render_settings(Path::new(path)).unwrap_err();
        assert!(matches!(error.kind, ErrorKind::Unquotable(_)), "{path:?}");
        assert!(shell_quote(Path::new(path)).is_err(), "{path:?}");
    }
    assert_eq!(shell_quote(Path::new("/a b/c")).unwrap(), "'/a b/c'");
}

#[test]
fn the_environment_holds_the_ids_and_paths_and_nothing_else() {
    let root = types_root();
    let fixture = Fixture::new(
        &root,
        "implement",
        Agent::Codex,
        vec![artifact("/t/a.md", "A")],
    );
    let env = render_env(&fixture.input());
    let expected = [
        ("BONGOS_CHAT_ID", "0190aaaa-bbbb-7ccc-8ddd-eeeeffff0000"),
        ("BONGOS_CHAT_TYPE", "implement"),
        ("BONGOS_CHAT_AGENT", "codex"),
        (
            "BONGOS_CHAT_RECORD",
            "/work/projects/invoicing/tasks/backfill/chats/c.toml",
        ),
        (
            "BONGOS_CHAT_CONTEXT",
            "/home/kyle/.local/share/bongos/chats/c/context.md",
        ),
        ("BONGOS_TASK_ID", "0190task-id"),
        ("BONGOS_TASK_DIR", TASK_DIR),
        (
            "BONGOS_TASK_FILE",
            "/work/projects/invoicing/tasks/backfill/task.toml",
        ),
        ("BONGOS_PROJECT_DIR", "/work/projects/invoicing"),
        ("BONGOS_WORKTREE", "/repos/invoicing/wt/backfill"),
        ("BONGOS_ROOT", "/home/kyle/.bongos"),
    ];
    let actual: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut expected = expected.to_vec();
    expected.sort();
    assert_eq!(actual, expected);

    // Variables that do not apply are left out.
    let mut bare = Fixture::new(&root, "free", Agent::Claude, vec![]);
    bare.facts.project_dir = None;
    let mut input = bare.input();
    input.task_id = None;
    input.bongos_root = None;
    let env = render_env(&input);
    for name in ["BONGOS_TASK_ID", "BONGOS_PROJECT_DIR", "BONGOS_ROOT"] {
        assert!(!env.contains_key(name), "{name}");
    }
    assert!(env.contains_key("BONGOS_CHAT_ID"));
}

#[test]
fn the_digest_of_the_given_list_is_stable_and_order_free() {
    let list = vec![artifact("/t/a.md", "A"), artifact("/t/b.md", "B")];
    let mut reversed = list.clone();
    reversed.reverse();
    assert_eq!(context_digest(&list), context_digest(&reversed));
    assert_eq!(context_digest(&list), context_digest(&list.clone()));
    assert_ne!(context_digest(&list), context_digest(&list[..1]));
    let root = types_root();
    let fixture = Fixture::new(&root, "plan", Agent::Claude, list.clone());
    assert_eq!(
        context_digest(&fixture.context.artifacts),
        context_digest(&list)
    );
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

const SECRETS: [(&str, &str); 4] = [
    ("secret-root.md", "Root-level compensation numbers"),
    ("notes/salary-review.md", "Salary review notes"),
    ("prompts.txt", "The private prompts Kyle typed"),
    ("hidden/layoff-plan.md", "Layoff planning draft"),
];

/// A real task directory with one shared file and the four human-only ones in `SECRETS`.
fn task_on_disk(temp: &TempDir, break_manifest: bool) -> PathBuf {
    let dir = temp.path().join("work/projects/invoicing/tasks/backfill");
    let mut task = TaskFile::new("Backfill usage events", Kind::PrShip);
    task.worktree = Some("/repos/invoicing/wt/backfill".to_string());
    write(&dir.join("task.toml"), &serialize_task(&task).unwrap());
    write(&dir.join("docs/design.md"), "# design\n");
    let mut manifest = String::from(
        "schema = 1\n\n[[artifact]]\npath = \"docs/design.md\"\ndescription = \"The backfill design\"\nvisibility = \"shared\"\n",
    );
    for (path, description) in SECRETS {
        write(&dir.join(path), "private\n");
        manifest.push_str(&format!(
            "\n[[artifact]]\npath = \"{path}\"\ndescription = \"{description}\"\nvisibility = \"human-only\"\n"
        ));
    }
    if break_manifest {
        manifest.push_str("\n[[artifact]\n");
    }
    write(&dir.join("artifacts.toml"), &manifest);
    dir
}

fn every_output_for(root: &TempDir, task_dir: &Path, context: SessionContext) -> Vec<String> {
    let mut all = Vec::new();
    for (chat_type, agent) in [
        ("plan", Agent::Claude),
        ("free", Agent::Codex),
        ("implement", Agent::Claude),
        ("implement", Agent::Codex),
        ("review", Agent::Claude),
        ("thinking", Agent::Codex),
    ] {
        let mut fixture = Fixture::new(root, chat_type, agent, vec![]);
        fixture.context = context.clone();
        // The record and the facts point at the scope directory the way a real launch does,
        // and the record names the human-only prompts file.
        fixture.record.prompts_file = Some(task_dir.join("prompts.txt"));
        fixture.handoff = Some(HandoffParent {
            name: "earlier chat".to_string(),
            chat_type: "implement".to_string(),
            agent: "claude".to_string(),
            transcript: Some(PathBuf::from("/home/kyle/.claude/projects/x/abc.jsonl")),
        });
        let input = RenderInput {
            scope_dir: task_dir,
            ..fixture.input()
        };
        all.push(render_context(&input));
        all.push(render_system(&input));
        all.push(render_codex_instructions(&input));
        all.push(render_settings(input.context_path).unwrap());
        all.extend(render_env(&input).into_values());
        all.push(handoff_note(input.handoff.unwrap()));
    }
    all
}

fn assert_no_secret(outputs: &[String]) {
    assert!(!outputs.is_empty());
    for output in outputs {
        for (path, description) in SECRETS {
            let file_name = Path::new(path).file_name().unwrap().to_str().unwrap();
            for needle in [path, file_name, description] {
                assert!(!output.contains(needle), "{needle} found in:\n{output}");
            }
        }
    }
}

#[test]
fn nothing_human_only_reaches_any_generated_output() {
    let temp = TempDir::new().unwrap();
    let task_dir = task_on_disk(&temp, false);
    let root = types_root();
    let context = bongos_work_model::session_context(&task_dir).unwrap();

    // The search is only meaningful if the shared file is found and the rest are left out.
    assert_eq!(context.artifacts.len(), 1);
    let outputs = every_output_for(&root, &task_dir, context);
    assert!(
        outputs
            .iter()
            .any(|output| output.contains("docs/design.md"))
    );
    assert!(
        outputs
            .iter()
            .any(|output| output.contains("The backfill design"))
    );
    assert_no_secret(&outputs);
}

#[test]
fn a_broken_manifest_lists_nothing() {
    let temp = TempDir::new().unwrap();
    let task_dir = task_on_disk(&temp, true);
    let root = types_root();
    let context = bongos_work_model::session_context(&task_dir).unwrap();
    assert!(context.artifacts.is_empty());
    let outputs = every_output_for(&root, &task_dir, context);
    assert_no_secret(&outputs);
    assert!(!outputs.iter().any(|output| output.contains("design.md")));
    assert!(
        outputs
            .iter()
            .any(|output| output.contains("None are shared with this chat."))
    );
}

#[test]
fn a_hidden_artifact_is_gone_from_the_next_render() {
    let temp = TempDir::new().unwrap();
    let task_dir = task_on_disk(&temp, false);
    let root = types_root();
    let before = bongos_work_model::session_context(&task_dir).unwrap();
    let before_digest = context_digest(&before.artifacts);

    let manifest = fs::read_to_string(task_dir.join("artifacts.toml")).unwrap();
    fs::write(
        task_dir.join("artifacts.toml"),
        manifest.replace("visibility = \"shared\"", "visibility = \"human-only\""),
    )
    .unwrap();
    let after = bongos_work_model::session_context(&task_dir).unwrap();
    assert!(after.artifacts.is_empty());
    assert_ne!(before_digest, context_digest(&after.artifacts));
    let outputs = every_output_for(&root, &task_dir, after);
    assert!(!outputs.iter().any(|output| output.contains("design.md")));
    assert_no_secret(&outputs);
}

#[test]
fn facts_carry_no_artifact() {
    // `TaskFacts` has exactly the five fields of the design; a new field must be reviewed against
    // the promise, so this destructuring fails to compile when one is added.
    let TaskFacts {
        project_title: _,
        project_dir: _,
        notes_dir: _,
        deps: _,
        pr_links: _,
    } = TaskFacts::default();
}
