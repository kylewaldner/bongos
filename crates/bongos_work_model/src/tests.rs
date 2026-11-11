use std::fs;
use std::io::{self, Write};
use std::path::Path;

use chrono::NaiveDate;
use tempfile::TempDir;

use crate::files::{write_atomic, write_atomic_with};
use crate::layout::*;
use crate::slug::{oneoff_dir_name, slugify, unique_slug};
use crate::*;

const LEAF: &str = r#"
schema = 1
id = "0196f1c2-aaaa-7bbb-8ccc-ddddeeeeffff"
goal = "Backfill usage events"
kind = "pr-ship"
status = "done"
created = 2026-11-08T10:00:00Z
closed = 2026-11-09T12:00:00Z
repository = "invoicing"
merge_is_ship = false
default_visibility = "human-only"
activity = "support"
linear = "SEQ-412"
branch = "backfill-usage-events"
base = "main"
base_sha = "4be1"
rebase_ack_sha = "9ac0"
worktree = "/home/kyle/src/invoicing/.bongos-worktrees/backfill-usage-events"
ship_overridden = { at = 2026-11-09T12:00:00Z, parent_state = "review" }
after = [{ task = "schema-change", unblock = "merged" }]
stacked_on = { task = "backfill", unblock = "implemented" }

[[todos]]
name = "implement"
role = "produces-branch"
workflow = "implement"
state = "done"
started = 2026-11-08T10:05:00Z

[[todos]]
name = "ship"
role = "ships"
state = "open"

[result]
kind = "pr"
url = "https://github.com/acme/invoicing/pull/412"
state = "approved"

[[history]]
at = 2026-11-08T10:05:00Z
by = "kyle"
event = "todo done"
detail = "implement"
"#;

const GROUP: &str = r#"
schema = 1
id = "0196f1c2-0000-7000-8000-000000000001"
goal = "Billing"
kind = "other"
status = "open"
created = 2026-11-08T10:00:00Z
default_visibility = "shared"
"#;

fn leaf() -> TaskFile {
    parse_task(LEAF).expect("leaf parses")
}

#[test]
fn leaf_task_round_trips_with_every_field() {
    let task = leaf();
    assert_eq!(task.kind, Kind::PrShip);
    assert_eq!(task.status, Status::Done);
    assert_eq!(task.default_visibility, Visibility::HumanOnly);
    assert_eq!(task.after[0].unblock, UnblockPoint::Merged);
    assert_eq!(
        task.stacked_on.as_ref().unwrap().unblock,
        UnblockPoint::Implemented
    );
    assert_eq!(task.todos[0].role, Some(TodoRole::ProducesBranch));
    assert_eq!(task.todos[1].state, TodoState::Open);
    assert_eq!(task.result.as_ref().unwrap().kind, ResultKind::Pr);
    assert_eq!(
        task.result.as_ref().unwrap().state,
        Some(ResultState::Approved)
    );
    assert_eq!(task.history[0].detail.as_deref(), Some("implement"));
    assert!(task.extra.is_empty());

    let text = serialize_task(&task).unwrap();
    assert!(text.contains("created = 2026-11-08T10:00:00Z"), "{text}");
    assert!(text.contains("kind = \"pr-ship\""), "{text}");
    assert_eq!(parse_task(&text).unwrap(), task);
}

#[test]
fn group_task_round_trips_without_leaf_fields() {
    let group = parse_task(GROUP).unwrap();
    assert!(group.todos.is_empty() && group.result.is_none() && group.branch.is_none());
    assert!(group.worktree.is_none());
    let text = serialize_task(&group).unwrap();
    for absent in ["todos", "result", "branch", "worktree", "after", "history"] {
        assert!(!text.contains(absent), "{absent} written: {text}");
    }
    assert_eq!(parse_task(&text).unwrap(), group);
}

#[test]
fn new_task_has_a_v7_id_and_round_trips() {
    let task = TaskFile::new("Write the doc", Kind::Document);
    assert_eq!(task.id.len(), 36);
    assert_eq!(
        uuid::Uuid::parse_str(&task.id).unwrap().get_version_num(),
        7
    );
    assert_ne!(new_id(), new_id());
    let text = serialize_task(&task).unwrap();
    assert_eq!(parse_task(&text).unwrap(), task);
}

#[test]
fn project_round_trips() {
    let text = r#"
schema = 1
title = "Invoicing"
repositories = ["invoicing", "billing-api"]
linear = "SEQ"
archived = true

[docs_home]
kind = "notion"
page = "https://notion.so/abc"
"#;
    let project = parse_project(text).unwrap();
    assert_eq!(
        project.docs_home,
        Some(DocsHome::Notion {
            page: "https://notion.so/abc".into(),
            extra: Default::default()
        })
    );
    assert!(project.archived);
    let written = serialize_project(&project).unwrap();
    assert_eq!(parse_project(&written).unwrap(), project);

    let git = parse_project(
        "schema = 1\ntitle = \"P\"\ndocs_home = { kind = \"git\", path = \"docs\" }\n",
    )
    .unwrap();
    assert!(matches!(git.docs_home, Some(DocsHome::Git { ref path, .. }) if path == "docs"));
    assert!(!git.archived && git.repositories.is_empty());
    let written = serialize_project(&git).unwrap();
    assert!(written.contains("archived = false"), "{written}");
    assert_eq!(parse_project(&written).unwrap(), git);
}

#[test]
fn artifacts_round_trip() {
    let text = r#"
schema = 1

[[artifact]]
path = "notes/design.md"
description = "The design"
visibility = "shared"

[[artifact]]
path = "scratch.txt"
description = "Private scratch"
visibility = "human-only"
"#;
    let artifacts = parse_artifacts(text).unwrap();
    assert_eq!(artifacts.artifacts.len(), 2);
    assert_eq!(
        artifacts.artifacts[1].visibility,
        Some(Visibility::HumanOnly)
    );
    let written = serialize_artifacts(&artifacts).unwrap();
    assert_eq!(parse_artifacts(&written).unwrap(), artifacts);

    let empty = serialize_artifacts(&ArtifactsFile::default()).unwrap();
    assert_eq!(empty.trim(), "schema = 1");
    assert_eq!(parse_artifacts(&empty).unwrap(), ArtifactsFile::default());
}

#[test]
fn link_file_round_trips() {
    let link = parse_link("schema = 1\nurl = \"https://notion.so/x\"\ntitle = \"Spec\"\n").unwrap();
    assert_eq!(link, LinkFile::new("https://notion.so/x", "Spec"));
    assert_eq!(parse_link(&serialize_link(&link).unwrap()).unwrap(), link);
}

#[test]
fn unknown_keys_survive_a_read_modify_write() {
    let text = format!(
        "{LEAF}\n[[future]]\nname = \"x\"\n"
    )
    .replace(
        "schema = 1\n",
        "schema = 1\nfuture_flag = true\nfuture_when = 2026-01-02T03:04:05Z\nfuture_list = [1, 2]\n",
    )
    .replace(
        "state = \"open\"\n\n[result]",
        "state = \"open\"\ntodo_extra = { nested = \"yes\" }\n\n[result]",
    )
    .replace(
        "url = \"https://github.com/acme/invoicing/pull/412\"\n",
        "url = \"https://github.com/acme/invoicing/pull/412\"\nchecks = \"green\"\n",
    )
    .replace(
        "{ task = \"schema-change\", unblock = \"merged\" }",
        "{ task = \"schema-change\", unblock = \"merged\", why = \"data\" }",
    )
    .replace("detail = \"implement\"\n", "detail = \"implement\"\nrun = \"r1\"\n");
    let mut task = parse_task(&text).unwrap();
    assert_eq!(task.extra.len(), 4);
    assert_eq!(
        task.todos[1].extra["todo_extra"]["nested"].as_str(),
        Some("yes")
    );
    assert_eq!(
        task.result.as_ref().unwrap().extra["checks"].as_str(),
        Some("green")
    );
    assert_eq!(task.after[0].extra["why"].as_str(), Some("data"));
    assert_eq!(task.history[0].extra["run"].as_str(), Some("r1"));

    task.goal = "Renamed".into();
    task.status = Status::Open;
    let rewritten = parse_task(&serialize_task(&task).unwrap()).unwrap();
    assert_eq!(rewritten, task);
    assert_eq!(rewritten.extra["future_flag"].as_bool(), Some(true));
    assert!(rewritten.extra["future_when"].is_datetime());
    assert_eq!(rewritten.extra["future_list"].as_array().unwrap().len(), 2);
    assert_eq!(rewritten.extra["future"][0]["name"].as_str(), Some("x"));
    assert_eq!(
        rewritten.todos[1].extra["todo_extra"]["nested"].as_str(),
        Some("yes")
    );
    assert_eq!(rewritten.after[0].extra["why"].as_str(), Some("data"));
}

#[test]
fn unknown_keys_survive_in_project_artifacts_and_links() {
    let project = parse_project("schema = 1\ntitle = \"P\"\nowner = \"kyle\"\ndocs_home = { kind = \"git\", path = \"d\", mirror = \"m\" }\n").unwrap();
    let back = parse_project(&serialize_project(&project).unwrap()).unwrap();
    assert_eq!(back.extra["owner"].as_str(), Some("kyle"));
    assert!(
        matches!(back.docs_home, Some(DocsHome::Git { ref extra, .. }) if extra["mirror"].as_str() == Some("m"))
    );

    let artifacts = parse_artifacts("schema = 1\nnote = 1\n[[artifact]]\npath = \"a\"\ndescription = \"d\"\nvisibility = \"shared\"\ntag = \"t\"\n").unwrap();
    let back = parse_artifacts(&serialize_artifacts(&artifacts).unwrap()).unwrap();
    assert_eq!(back.extra["note"].as_integer(), Some(1));
    assert_eq!(back.artifacts[0].extra["tag"].as_str(), Some("t"));

    let link = parse_link("schema = 1\nurl = \"u\"\ntitle = \"t\"\nicon = \"i\"\n").unwrap();
    let back = parse_link(&serialize_link(&link).unwrap()).unwrap();
    assert_eq!(back.extra["icon"].as_str(), Some("i"));
}

#[test]
fn schema_other_than_one_is_rejected() {
    let error = parse_task(&GROUP.replace("schema = 1", "schema = 2")).unwrap_err();
    assert!(
        matches!(&error.kind, ErrorKind::UnsupportedSchema { found: Some(found) } if found == "2")
    );
    assert!(
        error.to_string().contains("unsupported schema 2"),
        "{error}"
    );

    let error = parse_project("title = \"P\"\n").unwrap_err();
    assert!(matches!(
        error.kind,
        ErrorKind::UnsupportedSchema { found: None }
    ));
    assert!(error.to_string().contains("missing"), "{error}");

    let mut task = parse_task(GROUP).unwrap();
    task.schema = 3;
    assert!(matches!(
        serialize_task(&task).unwrap_err().kind,
        ErrorKind::UnsupportedSchema { .. }
    ));
}

#[test]
fn read_errors_carry_path_line_and_reason() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("task.toml");
    fs::write(&path, "schema = 1\nid = \"x\"\ngoal = \nkind = \"other\"\n").unwrap();
    let error = read_task(&path).unwrap_err();
    assert_eq!(error.path.as_deref(), Some(path.as_path()));
    assert_eq!(error.line, Some(3));
    assert!(matches!(error.kind, ErrorKind::Syntax(_)));
    assert!(
        error
            .to_string()
            .starts_with(&format!("{}:3:", path.display())),
        "{error}"
    );

    // A well-formed file that breaks the model names the file too.
    fs::write(&path, "schema = 1\nid = \"x\"\ngoal = \"g\"\nkind = \"nope\"\nstatus = \"open\"\ncreated = 2026-11-08T10:00:00Z\n").unwrap();
    let error = read_task(&path).unwrap_err();
    assert_eq!(error.path.as_deref(), Some(path.as_path()));
    assert_eq!(error.line, Some(4));
    assert!(error.to_string().contains("nope"), "{error}");

    let missing = read_task(&directory.path().join("absent.toml")).unwrap_err();
    assert!(matches!(missing.kind, ErrorKind::Io(_)));
    assert!(missing.path.is_some());
}

#[test]
fn read_functions_read_each_file_kind() {
    let directory = TempDir::new().unwrap();
    let write = |name: &str, text: &str| {
        let path = directory.path().join(name);
        fs::write(&path, text).unwrap();
        path
    };
    assert_eq!(read_task(&write("t", LEAF)).unwrap(), leaf());
    assert_eq!(
        read_project(&write("p", "schema = 1\ntitle = \"P\"\n"))
            .unwrap()
            .title,
        "P"
    );
    assert!(
        read_artifacts(&write("a", "schema = 1\n"))
            .unwrap()
            .artifacts
            .is_empty()
    );
    assert_eq!(
        read_link(&write("l", "schema = 1\nurl = \"u\"\ntitle = \"t\"\n"))
            .unwrap()
            .url,
        "u"
    );
}

#[test]
fn slugify_rules() {
    assert_eq!(slugify("Backfill Usage Events"), "backfill-usage-events");
    assert_eq!(slugify("  --Hello,   World!!  "), "hello-world");
    assert_eq!(slugify("a__b..c//d"), "a-b-c-d");
    assert_eq!(slugify("Fix #412: the (big) bug"), "fix-412-the-big-bug");
    assert_eq!(slugify("v2.0"), "v2-0");
    assert_eq!(slugify(""), "task");
    assert_eq!(slugify("!!! ???"), "task");
    assert_eq!(slugify("日本語"), "task");
    assert_eq!(slugify("café au lait"), "caf-au-lait");
    assert_eq!(slugify("naïve résumé"), "na-ve-r-sum");

    let long = "a".repeat(60);
    assert_eq!(slugify(&long), "a".repeat(40));
    let cut = format!("{}-{}", "b".repeat(39), "c".repeat(10));
    assert_eq!(slugify(&cut), "b".repeat(39));
    let cut = format!("{} {}", "d".repeat(38), "e".repeat(10));
    assert_eq!(slugify(&cut), format!("{}-e", "d".repeat(38)));
}

#[test]
fn unique_slug_adds_numeric_suffixes() {
    let none: [&str; 0] = [];
    assert_eq!(unique_slug("fix", &none), "fix");
    assert_eq!(unique_slug("fix", &["other"]), "fix");
    assert_eq!(unique_slug("fix", &["fix"]), "fix-2");
    assert_eq!(unique_slug("fix", &["fix", "fix-2"]), "fix-3");
    assert_eq!(
        unique_slug("fix", &[String::from("fix"), String::from("fix-3")]),
        "fix-2"
    );

    let full = "f".repeat(40);
    let second = unique_slug(&full, std::slice::from_ref(&full));
    assert_eq!(second, format!("{}-2", "f".repeat(38)));
    assert_eq!(
        unique_slug(&full, &[full.clone(), second]),
        format!("{}-3", "f".repeat(38))
    );
}

#[test]
fn oneoff_dir_name_prefixes_the_date() {
    let date = NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
    assert_eq!(oneoff_dir_name(date, "fix-login"), "2026-11-08-fix-login");
}

#[test]
fn layout_paths() {
    let root = Path::new("/tmp/root");
    let project = project_dir(root, "invoicing");
    assert_eq!(project, Path::new("/tmp/root/projects/invoicing"));
    assert_eq!(
        project_file(&project),
        Path::new("/tmp/root/projects/invoicing/project.toml")
    );
    assert_eq!(
        task_file(&project),
        Path::new("/tmp/root/projects/invoicing/task.toml")
    );
    assert_eq!(
        plan_file(&project),
        Path::new("/tmp/root/projects/invoicing/plan.md")
    );
    assert_eq!(
        prompts_file(&project),
        Path::new("/tmp/root/projects/invoicing/prompts.txt")
    );
    assert_eq!(
        artifacts_file(&project),
        Path::new("/tmp/root/projects/invoicing/artifacts.toml")
    );
    let task = child_dir(&project, "backfill");
    assert_eq!(
        task,
        Path::new("/tmp/root/projects/invoicing/tasks/backfill")
    );
    assert_eq!(
        children_dir(&task),
        Path::new("/tmp/root/projects/invoicing/tasks/backfill/tasks")
    );
    assert_eq!(history_old_file(&task), task.join("history.old.toml"));
    assert_eq!(op_file(&project), project.join(".op.toml"));
    assert_eq!(chats_dir(&task), task.join("chats"));
    assert_eq!(runs_dir(&task), task.join("runs"));
    assert_eq!(projects_dir(root), Path::new("/tmp/root/projects"));

    assert_eq!(oneoffs_dir(root), Path::new("/tmp/root/oneoffs"));
    assert_eq!(
        activity_dir(root, "support"),
        Path::new("/tmp/root/oneoffs/support")
    );
    let oneoff = oneoff_dir(root, "support", "2026-11-08-fix-login");
    assert_eq!(
        oneoff,
        Path::new("/tmp/root/oneoffs/support/2026-11-08-fix-login")
    );
    assert_eq!(task_file(&oneoff), oneoff.join("task.toml"));
}

#[test]
fn reserved_names() {
    for name in [
        "task.toml",
        "project.toml",
        "artifacts.toml",
        "history.old.toml",
        ".op.toml",
        "tasks",
        "tasks/",
        "chats",
        "runs",
        "workflows",
        "workflows/",
        ".git",
        ".git/",
        ".task.toml.tmp-1234-AbCdEfGh",
        ".artifacts.toml.tmp-1-x",
        "..tmp-x",
    ] {
        assert!(is_reserved(name), "{name} should be reserved");
    }
    for name in [
        "notes",
        "plan.md",
        "prompts.txt",
        "design.link.toml",
        "task.toml.bak",
        ".tmp-x",
        "report.tmp-1",
        ".gitignore",
        "Task.toml",
    ] {
        assert!(!is_reserved(name), "{name} should not be reserved");
    }
}

fn names_in(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn write_atomic_creates_and_replaces_without_leaving_temp_files() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("task.toml");
    write_atomic(&path, b"first").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"first");
    write_atomic(&path, b"second, longer").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"second, longer");
    write_atomic(&path, b"").unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"");
    assert_eq!(names_in(directory.path()), ["task.toml"]);
}

#[test]
fn write_atomic_names_its_temp_file_and_keeps_the_mode() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TempDir::new().unwrap();
    let path = directory.path().join("task.toml");
    fs::write(&path, "old").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let mut seen = None;
    write_atomic_with(&path, |file| {
        seen = Some(names_in(directory.path()));
        file.write_all(b"new")
    })
    .unwrap();
    let seen = seen.unwrap();
    let temp = seen
        .iter()
        .find(|name| name.starts_with('.'))
        .expect("temp file exists");
    assert!(
        temp.starts_with(&format!(".task.toml.tmp-{}-", std::process::id())),
        "{temp}"
    );
    assert!(is_reserved(temp));
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[test]
fn failed_temp_write_leaves_the_old_file_and_no_temp_file() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("task.toml");
    fs::write(&path, "old").unwrap();
    let error = write_atomic_with(&path, |file| {
        file.write_all(b"partial")?;
        Err(io::Error::other("disk full"))
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "disk full");
    assert_eq!(fs::read(&path).unwrap(), b"old");
    assert_eq!(names_in(directory.path()), ["task.toml"]);
}

#[test]
fn write_atomic_into_a_missing_directory_fails_cleanly() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("absent").join("task.toml");
    assert!(write_atomic(&path, b"x").is_err());
    assert!(names_in(directory.path()).is_empty());
}

#[test]
fn existing_fixtures_serialize_without_the_new_keys() {
    for text in [LEAF, GROUP] {
        let task = parse_task(text).unwrap();
        assert_eq!(task.planned_result, None);
        assert!(task.proposals.is_empty());
        let written = serialize_task(&task).unwrap();
        for absent in ["planned_result", "proposals", "plans"] {
            assert!(!written.contains(absent), "{absent} written: {written}");
        }
        assert_eq!(parse_task(&written).unwrap(), task);
    }
    let project = parse_project("schema = 1\ntitle = \"P\"\n").unwrap();
    assert_eq!(project.auto_apply_proposals, None);
    let written = serialize_project(&project).unwrap();
    assert!(!written.contains("auto_apply_proposals"), "{written}");
}

#[test]
fn planned_result_proposals_and_the_plans_role_round_trip() {
    let leaf = LEAF.replacen(
        "schema = 1\n",
        "schema = 1\nplanned_result = \"a PR that backfills the events\"\n",
        1,
    );
    let text = format!(
        r#"{leaf}
[[proposals]]
number = 2
state = "applied"
at = 2026-11-09T12:00:00Z
by = "agent"
hash = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
future = "kept"

[[proposals]]
number = 3
state = "rejected"
at = 2026-11-09T13:00:00Z
by = "workflow:planner"
hash = "00"

[[todos]]
name = "plan"
role = "plans"
state = "open"
"#
    );
    let task = parse_task(&text).unwrap();
    assert_eq!(
        task.planned_result.as_deref(),
        Some("a PR that backfills the events")
    );
    assert_eq!(task.proposals.len(), 2);
    assert_eq!(task.proposals[0].number, 2);
    assert_eq!(task.proposals[0].state, ProposalState::Applied);
    assert_eq!(task.proposals[0].by, "agent");
    assert_eq!(
        task.proposals[0].extra.get("future").unwrap().as_str(),
        Some("kept")
    );
    assert_eq!(task.proposals[1].state, ProposalState::Rejected);
    assert_eq!(task.todos[2].role, Some(TodoRole::Plans));
    assert!(task.extra.is_empty());

    let written = serialize_task(&task).unwrap();
    assert!(written.contains("planned_result = "), "{written}");
    assert!(written.contains("[[proposals]]"), "{written}");
    assert!(written.contains("role = \"plans\""), "{written}");
    assert!(written.contains("state = \"applied\""), "{written}");
    assert_eq!(parse_task(&written).unwrap(), task);

    // Emptied again, the keys disappear.
    let mut task = task;
    task.planned_result = None;
    task.proposals.clear();
    let written = serialize_task(&task).unwrap();
    assert!(!written.contains("planned_result"), "{written}");
    assert!(!written.contains("proposals"), "{written}");
}

#[test]
fn auto_apply_proposals_round_trips_and_is_omitted_when_unset() {
    let project =
        parse_project("schema = 1\ntitle = \"P\"\nauto_apply_proposals = false\n").unwrap();
    assert_eq!(project.auto_apply_proposals, Some(false));
    let written = serialize_project(&project).unwrap();
    assert!(
        written.contains("auto_apply_proposals = false"),
        "{written}"
    );
    assert_eq!(parse_project(&written).unwrap(), project);

    let mut unset = project;
    unset.auto_apply_proposals = None;
    let written = serialize_project(&unset).unwrap();
    assert!(!written.contains("auto_apply_proposals"), "{written}");
}

#[test]
fn proposal_file_is_numbered_under_proposals() {
    let dir = Path::new("projects/invoicing/tasks/backfill");
    assert_eq!(PROPOSALS_DIR, "proposals");
    assert_eq!(
        proposal_file(dir, 2),
        dir.join("proposals").join("2.proposal.txt")
    );
    assert!(proposal_file(dir, 12).ends_with("proposals/12.proposal.txt"));
    // Not a reserved name, so it is listed like any artifact directory.
    assert!(!is_reserved(PROPOSALS_DIR));
}

fn linked_state() -> LinearState {
    LinearState {
        issue_id: "6f0c8a1e-2b9c-4c55-9d1a-0e7f3c2b8a41".to_string(),
        team_id: "0a1b".to_string(),
        url: "https://linear.app/sequence/issue/SEQ-121/proration-engine".to_string(),
        description: "Prorate\nby the day.".to_string(),
        started_sent: Some("2026-11-09T12:00:00Z".parse().unwrap()),
        cancel: Some(LinearCancel::Declined),
        pending_create: Some("3d2e".to_string()),
        dismissed: vec!["SEQ-127".to_string()],
        declined: vec!["0196f1c2-aaaa".to_string()],
        offered: vec![
            LinearOffered {
                root: "0196f1c2-bbbb".to_string(),
                number: 2,
                hash: "9f86d081".to_string(),
                extra: Extra::new(),
            },
            LinearOffered {
                root: "0196f1c2-cccc".to_string(),
                number: 3,
                hash: "00".to_string(),
                extra: Extra::new(),
            },
        ],
        extra: Extra::new(),
    }
}

#[test]
fn files_without_the_linear_state_serialize_without_it_and_are_stable() {
    let mut fixtures = vec![
        parse_task(LEAF).unwrap(),
        parse_task(GROUP).unwrap(),
        leaf(),
        crate::fixtures::pr_ship("a").file.unwrap(),
        TaskFile::new("plain", Kind::Other),
    ];
    // Files a command writes (feature 03's writers), with a proposal and history around.
    let tree = crate::fixtures::tree(vec![crate::fixtures::pr_ship("a")]);
    let change = crate::commands::new_task(
        &tree,
        &crate::fixtures::ctx(),
        crate::commands::Placement::Under(crate::fixtures::id_of(&tree, "a")),
        crate::commands::NewTask {
            goal: "Child".to_string(),
            ..Default::default()
        },
    );
    fixtures.extend(change.ok().and_then(|change| change.file().cloned()));
    for file in fixtures {
        assert_eq!(file.linear_state, None);
        let written = serialize_task(&file).unwrap();
        assert!(!written.contains("linear_state"), "{written}");
        assert_eq!(
            serialize_task(&parse_task(&written).unwrap()).unwrap(),
            written
        );
    }
}

#[test]
fn linear_state_round_trips_and_sits_between_proposals_and_history() {
    let mut file = parse_task(LEAF).unwrap();
    file.proposals.push(ProposalRecord {
        number: 1,
        state: ProposalState::Applied,
        at: "2026-11-09T12:00:00Z".parse().unwrap(),
        by: "agent".to_string(),
        hash: "00".to_string(),
        extra: Extra::new(),
    });
    file.linear_state = Some(linked_state());
    let written = serialize_task(&file).unwrap();
    for expected in [
        "[linear_state]",
        "[[linear_state.offered]]",
        "started_sent = 2026-11-09T12:00:00Z",
        "cancel = \"declined\"",
        "pending_create = \"3d2e\"",
        "dismissed = [\"SEQ-127\"]",
    ] {
        assert!(written.contains(expected), "{expected} missing: {written}");
    }
    let position = |needle: &str| written.find(needle).unwrap();
    assert!(position("[[proposals]]") < position("[linear_state]"));
    assert!(position("[[linear_state.offered]]") < position("[[history]]"));
    let parsed = parse_task(&written).unwrap();
    assert_eq!(parsed, file);
    assert_eq!(serialize_task(&parsed).unwrap(), written);
}

#[test]
fn linear_state_omits_empty_fields_and_keeps_unknown_keys() {
    let mut file = parse_task(GROUP).unwrap();
    file.linear = Some("SEQ-121".to_string());
    file.linear_state = Some(LinearState {
        issue_id: "6f0c".to_string(),
        ..LinearState::default()
    });
    let written = serialize_task(&file).unwrap();
    assert!(
        written.contains("[linear_state]\nissue_id = \"6f0c\"\n"),
        "{written}"
    );
    for absent in [
        "team_id",
        "url",
        "description",
        "started_sent",
        "cancel",
        "pending_create",
        "dismissed",
        "declined",
        "offered",
    ] {
        assert!(!written.contains(absent), "{absent} written: {written}");
    }
    let future = written.replace("[linear_state]\n", "[linear_state]\nfuture = \"kept\"\n");
    let parsed = parse_task(&future).unwrap();
    let state = parsed.linear_state.as_ref().unwrap();
    assert_eq!(state.extra.get("future").unwrap().as_str(), Some("kept"));
    assert!(parsed.extra.is_empty());
    assert!(
        serialize_task(&parsed)
            .unwrap()
            .contains("future = \"kept\"")
    );
}
