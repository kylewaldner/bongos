use std::fs;
use std::path::Path;

use tempfile::TempDir;

use crate::*;

fn resolve_in(root: &TempDir, chat_type: &str, agent: Agent) -> Result<ResolvedType> {
    resolve(
        chat_type,
        agent,
        root.path(),
        &root.path().join("claude-home"),
    )
}

fn seeded() -> TempDir {
    let root = TempDir::new().unwrap();
    seed(root.path()).unwrap();
    root
}

fn write_skill(skills_root: &Path, name: &str, text: &str) {
    let dir = skills_root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("SKILL.md"), text).unwrap();
}

#[test]
fn five_types_are_shipped() {
    let names: Vec<_> = SHIPPED_TYPES.iter().map(|shipped| shipped.name).collect();
    assert_eq!(names, ["plan", "implement", "review", "thinking", "free"]);
}

#[test]
fn every_shipped_type_parses_with_the_prd_values() {
    // (type, layout, skills, task_context, review_context, thinking)
    let expected = [
        ("plan", Layout::ChatOnly, vec![], false, false, false),
        ("free", Layout::ChatOnly, vec![], false, false, false),
        ("thinking", Layout::Dual, vec![], false, false, true),
        (
            "implement",
            Layout::Dual,
            vec!["model-routing"],
            true,
            false,
            false,
        ),
        ("review", Layout::Dual, vec![], false, true, false),
    ];
    let root = seeded();
    for (name, layout, skills, task_context, review_context, thinking) in expected {
        let resolved = resolve_in(&root, name, Agent::Claude).unwrap();
        let config = &resolved.config;
        assert_eq!(config.schema, 1, "{name}");
        assert!(!config.description.is_empty(), "{name}");
        assert_eq!(config.layout, layout, "{name}");
        assert_eq!(config.skills, skills, "{name}");
        assert_eq!(config.task_context, task_context, "{name}");
        assert_eq!(config.review_context, review_context, "{name}");
        assert_eq!(config.thinking, thinking, "{name}");
        assert!(config.artifacts, "{name}");
        assert!(config.extra.is_empty(), "{name}");
        assert_eq!(resolved.context, "", "{name}");
        assert_eq!(resolved.agent_context, None, "{name}");
        assert_eq!(resolved.overrides, AgentOverrides::default(), "{name}");
    }
}

#[test]
fn type_config_defaults_and_unknown_keys() {
    let config = parse_type_config("schema = 1\nfuture = \"x\"\n", None).unwrap();
    assert_eq!(config.layout, Layout::Dual);
    assert!(config.skills.is_empty());
    assert!(config.artifacts);
    assert!(!config.task_context);
    assert_eq!(
        config.extra.get("future").and_then(|v| v.as_str()),
        Some("x")
    );
}

#[test]
fn type_config_errors_name_the_file_and_line() {
    let path = Path::new("/types/plan/type.toml");
    let error = parse_type_config("schema = 1\nlayout = \"sideways\"\n", Some(path)).unwrap_err();
    assert_eq!(error.path.as_deref(), Some(path));
    assert_eq!(error.line, Some(2));
    assert!(matches!(error.kind, ErrorKind::Syntax(_)));

    let error = parse_type_config("description = \"x\"\n", None).unwrap_err();
    assert!(matches!(
        error.kind,
        ErrorKind::UnsupportedSchema { found: None }
    ));
    let error = parse_type_config("schema = 2\n", None).unwrap_err();
    assert!(matches!(
        error.kind,
        ErrorKind::UnsupportedSchema { found: Some(_) }
    ));
}

#[test]
fn seed_creates_the_five_directories_with_their_files() {
    let root = TempDir::new().unwrap();
    let types_root = root.path().join("nested").join("chat-types");
    let written = seed(&types_root).unwrap();
    assert_eq!(written, ["plan", "implement", "review", "thinking", "free"]);
    for shipped in SHIPPED_TYPES {
        for (file, text) in shipped.files {
            let on_disk = fs::read_to_string(types_root.join(shipped.name).join(file)).unwrap();
            assert_eq!(&on_disk, text, "{}/{file}", shipped.name);
        }
    }
    // No temporary files are left behind.
    for entry in fs::read_dir(types_root.join("implement")).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(name == "type.toml" || name == "context.md", "{name}");
    }
}

#[test]
fn seed_twice_writes_nothing_the_second_time() {
    let root = seeded();
    assert!(seed(root.path()).unwrap().is_empty());
}

#[test]
fn seed_never_overwrites_an_edited_file() {
    let root = seeded();
    let context = root.path().join("implement").join("context.md");
    let type_file = root.path().join("plan").join("type.toml");
    fs::write(&context, "Use the repo's test script.\n").unwrap();
    fs::write(&type_file, "schema = 1\nlayout = \"dual\"\n").unwrap();

    seed(root.path()).unwrap();

    assert_eq!(
        fs::read_to_string(&context).unwrap(),
        "Use the repo's test script.\n"
    );
    assert_eq!(
        fs::read_to_string(&type_file).unwrap(),
        "schema = 1\nlayout = \"dual\"\n"
    );
}

#[test]
fn seed_never_touches_a_file_the_owner_added() {
    let root = seeded();
    let claude_md = root.path().join("implement").join("claude.md");
    let mine = root.path().join("mine");
    fs::write(&claude_md, "Claude only.\n").unwrap();
    fs::create_dir_all(&mine).unwrap();
    fs::write(mine.join("type.toml"), "schema = 1\n").unwrap();

    let written = seed(root.path()).unwrap();

    assert!(written.is_empty());
    assert_eq!(fs::read_to_string(&claude_md).unwrap(), "Claude only.\n");
    assert_eq!(
        fs::read_to_string(mine.join("type.toml")).unwrap(),
        "schema = 1\n"
    );
    assert!(!mine.join("context.md").exists());
}

#[test]
fn seed_leaves_a_directory_missing_one_shipped_file_alone() {
    let root = seeded();
    let review = root.path().join("review");
    fs::remove_file(review.join("context.md")).unwrap();
    let edited = root.path().join("free").join("type.toml");
    fs::write(&edited, "schema = 1\n").unwrap();
    fs::remove_dir_all(root.path().join("thinking")).unwrap();

    let written = seed(root.path()).unwrap();

    // The directory exists, so the missing file is not recreated; a missing directory is.
    assert_eq!(written, ["thinking"]);
    assert!(!review.join("context.md").exists());
    assert!(review.join("type.toml").exists());
    assert!(root.path().join("thinking").join("context.md").exists());
    assert_eq!(fs::read_to_string(&edited).unwrap(), "schema = 1\n");
}

#[test]
fn resolve_reads_an_edited_context_from_disk_every_time() {
    let root = seeded();
    assert_eq!(
        resolve_in(&root, "implement", Agent::Claude)
            .unwrap()
            .context,
        ""
    );

    fs::write(
        root.path().join("implement").join("context.md"),
        "Be brief.\n",
    )
    .unwrap();
    let after = resolve_in(&root, "implement", Agent::Claude).unwrap();
    assert_eq!(after.context, "Be brief.\n");
    // Only the edited type changes.
    assert_eq!(
        resolve_in(&root, "plan", Agent::Claude).unwrap().context,
        ""
    );
}

#[test]
fn resolve_without_a_context_file_gives_an_empty_context() {
    let root = seeded();
    fs::remove_file(root.path().join("plan").join("context.md")).unwrap();
    assert_eq!(resolve_in(&root, "plan", Agent::Codex).unwrap().context, "");
}

#[test]
fn resolve_picks_the_agents_own_files() {
    let root = seeded();
    let dir = root.path().join("implement");
    fs::write(dir.join("claude.md"), "For Claude.\n").unwrap();
    fs::write(dir.join("codex.md"), "For Codex.\n").unwrap();
    fs::write(
        dir.join("claude.toml"),
        "extra_args = [\"--verbose\"]\nmodel = \"claude-opus-5-5\"\n",
    )
    .unwrap();

    let claude = resolve_in(&root, "implement", Agent::Claude).unwrap();
    assert_eq!(claude.agent_context.as_deref(), Some("For Claude.\n"));
    assert_eq!(claude.overrides.extra_args, ["--verbose"]);
    assert_eq!(claude.overrides.model.as_deref(), Some("claude-opus-5-5"));

    let codex = resolve_in(&root, "implement", Agent::Codex).unwrap();
    assert_eq!(codex.agent_context.as_deref(), Some("For Codex.\n"));
    assert_eq!(codex.overrides, AgentOverrides::default());
}

#[test]
fn skill_lookup_prefers_the_types_root_over_claude_home() {
    let root = seeded();
    let claude_home = root.path().join("claude-home");
    write_skill(
        &root.path().join(SKILLS_DIR),
        "model-routing",
        "from the types root",
    );
    write_skill(
        &claude_home.join("skills"),
        "model-routing",
        "from claude home",
    );

    let resolved = resolve("implement", Agent::Claude, root.path(), &claude_home).unwrap();
    assert_eq!(resolved.skills.len(), 1);
    assert_eq!(resolved.skills[0].name, "model-routing");
    assert_eq!(resolved.skills[0].text, "from the types root");
    assert_eq!(
        resolved.skills[0].dir,
        root.path().join(SKILLS_DIR).join("model-routing")
    );
    assert!(resolved.missing_skills.is_empty());

    // Without the first, the second is used.
    fs::remove_dir_all(root.path().join(SKILLS_DIR)).unwrap();
    let resolved = resolve("implement", Agent::Claude, root.path(), &claude_home).unwrap();
    assert_eq!(resolved.skills[0].text, "from claude home");
    assert_eq!(
        resolved.skills[0].dir,
        claude_home.join("skills").join("model-routing")
    );
}

#[test]
fn a_skill_directory_without_skill_md_does_not_match() {
    let root = seeded();
    let claude_home = root.path().join("claude-home");
    fs::create_dir_all(root.path().join(SKILLS_DIR).join("model-routing")).unwrap();
    write_skill(
        &claude_home.join("skills"),
        "model-routing",
        "from claude home",
    );

    let resolved = resolve("implement", Agent::Claude, root.path(), &claude_home).unwrap();
    assert_eq!(resolved.skills[0].text, "from claude home");
}

#[test]
fn a_missing_skill_is_reported_and_does_not_fail_the_resolve() {
    let root = seeded();
    let resolved = resolve_in(&root, "implement", Agent::Claude).unwrap();
    assert!(resolved.skills.is_empty());
    assert_eq!(resolved.missing_skills, ["model-routing"]);
    assert_eq!(resolved.config.skills, ["model-routing"]);
}

#[test]
fn skills_keep_the_listed_order_and_unsafe_names_are_missing() {
    let root = seeded();
    write_skill(&root.path().join(SKILLS_DIR), "b", "B");
    write_skill(&root.path().join(SKILLS_DIR), "a", "A");
    // A file outside the skills directory must not be reachable through a name.
    fs::write(root.path().join("SKILL.md"), "outside").unwrap();
    fs::write(
        root.path().join("plan").join("type.toml"),
        "schema = 1\nskills = [\"b\", \"nope\", \"a\", \"..\", \"../plan\", \"\"]\n",
    )
    .unwrap();

    let resolved = resolve_in(&root, "plan", Agent::Claude).unwrap();
    let found: Vec<_> = resolved
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert_eq!(found, ["b", "a"]);
    assert_eq!(resolved.missing_skills, ["nope", "..", "../plan", ""]);
}

#[test]
fn a_type_the_owner_added_resolves() {
    let root = seeded();
    let dir = root.path().join("spike");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("type.toml"),
        "schema = 1\nlayout = \"chat-only\"\n",
    )
    .unwrap();
    let resolved = resolve_in(&root, "spike", Agent::Codex).unwrap();
    assert_eq!(resolved.name, "spike");
    assert_eq!(resolved.config.layout, Layout::ChatOnly);
}

#[test]
fn resolve_errors() {
    let root = seeded();
    let error = resolve_in(&root, "nothing", Agent::Claude).unwrap_err();
    assert!(matches!(error.kind, ErrorKind::NoSuchType(_)));
    for name in ["", "..", "../plan", "a/b", SKILLS_DIR, ".hidden"] {
        let error = resolve_in(&root, name, Agent::Claude).unwrap_err();
        assert!(matches!(error.kind, ErrorKind::InvalidName(_)), "{name}");
    }

    let dir = root.path().join("plan");
    fs::remove_file(dir.join("type.toml")).unwrap();
    let error = resolve_in(&root, "plan", Agent::Claude).unwrap_err();
    assert!(matches!(error.kind, ErrorKind::Io(_)));
    assert_eq!(error.path, Some(dir.join("type.toml")));

    fs::write(dir.join("type.toml"), "schema = 1\nskills = 3\n").unwrap();
    let error = resolve_in(&root, "plan", Agent::Claude).unwrap_err();
    assert!(matches!(error.kind, ErrorKind::Syntax(_)));
    assert_eq!(error.path, Some(dir.join("type.toml")));
}

#[test]
fn an_invalid_agent_file_fails_only_that_agent() {
    let root = seeded();
    let dir = root.path().join("plan");
    fs::write(dir.join("codex.toml"), "extra_args = 4\n").unwrap();
    assert!(resolve_in(&root, "plan", Agent::Claude).is_ok());
    let error = resolve_in(&root, "plan", Agent::Codex).unwrap_err();
    assert_eq!(error.path, Some(dir.join("codex.toml")));
}

#[test]
fn a_broken_skill_is_missing_not_an_error() {
    let root = seeded();
    let claude_home = root.path().join("claude-home");
    let skills = root.path().join(SKILLS_DIR);
    // A plain file where the skill directory should be (NotADirectory).
    fs::create_dir_all(&skills).unwrap();
    fs::write(skills.join("model-routing"), "not a directory").unwrap();
    let resolved = resolve("implement", Agent::Claude, root.path(), &claude_home).unwrap();
    assert!(resolved.skills.is_empty());
    assert_eq!(resolved.missing_skills, ["model-routing"]);

    // A SKILL.md that is a directory (IsADirectory), then invalid UTF-8; the next root is tried.
    fs::remove_file(skills.join("model-routing")).unwrap();
    fs::create_dir_all(skills.join("model-routing").join("SKILL.md")).unwrap();
    let resolved = resolve("implement", Agent::Claude, root.path(), &claude_home).unwrap();
    assert_eq!(resolved.missing_skills, ["model-routing"]);

    fs::remove_dir_all(skills.join("model-routing")).unwrap();
    fs::create_dir_all(skills.join("model-routing")).unwrap();
    fs::write(skills.join("model-routing").join("SKILL.md"), [0xff, 0xfe]).unwrap();
    write_skill(
        &claude_home.join("skills"),
        "model-routing",
        "from claude home",
    );
    let resolved = resolve("implement", Agent::Claude, root.path(), &claude_home).unwrap();
    assert_eq!(resolved.skills[0].text, "from claude home");
    assert!(resolved.missing_skills.is_empty());
}
