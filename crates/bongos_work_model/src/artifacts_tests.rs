//! Artifacts and the shared set (D22). The promise: a human-only artifact's path and description
//! never reach what a chat session is told. Every check searches the serialized context, and
//! first shows that the same search finds a shared artifact.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::artifacts::{merge, read_task_artifacts, session_context, shared_artifacts};
use crate::commands::{
    self, Context, NewProject, NewTask, Outcome, artifact_hide, artifact_hide_all, artifact_link,
    artifact_share, artifact_share_all, set_default_visibility,
};
use crate::layout;
use crate::store::{WorkTree, load};
use crate::*;

const SECRET_PATH: &str = "notes/salary-review.md";
const SECRET_DESCRIPTION: &str = "Compensation numbers for the team";
const PUBLIC_PATH: &str = "docs/design.md";
const PUBLIC_DESCRIPTION: &str = "The backfill design";
const WORKTREE: &str = "/repos/invoicing/.bongos-worktrees/backfill";

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn ctx() -> Context {
    Context::new("kyle", "2026-11-08T12:00:00Z".parse().unwrap())
}

fn task_toml(id: &str, goal: &str, edit: impl FnOnce(&mut TaskFile)) -> String {
    let mut file = TaskFile::new(goal, Kind::PrShip);
    file.id = id.to_string();
    edit(&mut file);
    serialize_task(&file).unwrap()
}

/// A project `invoicing` (`g1`) holding the leaf `backfill` (`l1`) with a worktree, a shared and
/// described `docs/design.md`, a described `notes/salary-review.md`, a `chats/` transcript and a
/// `plan.md`. Returns the root and the leaf's directory.
fn seed(temp: &TempDir) -> (PathBuf, PathBuf) {
    let root = temp.path().join("work");
    let project = layout::project_dir(&root, "invoicing");
    write(
        &layout::project_file(&project),
        "schema = 1\ntitle = \"Invoicing\"\n",
    );
    write(
        &layout::task_file(&project),
        &task_toml("g1", "Invoicing", |_| {}),
    );
    let backfill = layout::child_dir(&project, "backfill");
    write(
        &layout::task_file(&backfill),
        &task_toml("l1", "Backfill usage events", |file| {
            file.todos = commands::default_todos(Kind::PrShip);
            file.worktree = Some(WORKTREE.to_string());
        }),
    );
    write(&backfill.join(PUBLIC_PATH), "# design\n");
    write(&backfill.join(SECRET_PATH), "# numbers\n");
    write(&backfill.join("plan.md"), "# plan\n");
    write(&backfill.join("chats/one.md"), "a transcript\n");
    write(
        &layout::artifacts_file(&backfill),
        &format!(
            "schema = 1\n\n[[artifact]]\npath = \"{PUBLIC_PATH}\"\ndescription = \"{PUBLIC_DESCRIPTION}\"\n\n\
             [[artifact]]\npath = \"{SECRET_PATH}\"\ndescription = \"{SECRET_DESCRIPTION}\"\n"
        ),
    );
    (root, backfill)
}

/// Runs `command` on the tree under `root` as loaded now and writes its change.
fn run(root: &Path, command: impl FnOnce(&WorkTree) -> Outcome) -> commands::Change {
    let tree = load(root);
    let change = command(&tree).expect("the command should be accepted");
    commands::apply(root, &change).unwrap();
    change
}

/// The context and the shared list of `dir`, serialized as a chat session would receive them.
fn serialized(dir: &Path) -> String {
    let context = session_context(dir).unwrap();
    let list = shared_artifacts(dir);
    format!(
        "{}\n{}",
        serde_json::to_string(&context).unwrap(),
        serde_json::to_string(&list).unwrap()
    )
}

fn shared_paths(dir: &Path) -> Vec<PathBuf> {
    shared_artifacts(dir)
        .into_iter()
        .map(|artifact| artifact.path)
        .collect()
}

#[test]
fn a_hidden_artifact_never_appears_in_the_shared_list_or_the_session_context() {
    let temp = TempDir::new().unwrap();
    let (root, backfill) = seed(&temp);

    // Before hiding, the search finds both, so a miss below means the artifact is left out.
    let before = serialized(&backfill);
    for needle in [
        SECRET_PATH,
        SECRET_DESCRIPTION,
        PUBLIC_PATH,
        PUBLIC_DESCRIPTION,
    ] {
        assert!(
            before.contains(needle),
            "{needle} should be found in {before}"
        );
    }

    let change = run(&root, |tree| artifact_hide(tree, &ctx(), "l1", SECRET_PATH));
    // Only `artifacts.toml` was written.
    assert_eq!(
        change
            .writes
            .iter()
            .map(|write| write.path())
            .collect::<Vec<_>>(),
        [layout::artifacts_file(&backfill)]
    );

    let after = serialized(&backfill);
    for needle in [PUBLIC_PATH, PUBLIC_DESCRIPTION] {
        assert!(
            after.contains(needle),
            "{needle} should be found in {after}"
        );
    }
    for needle in [SECRET_PATH, SECRET_DESCRIPTION, "salary"] {
        assert!(!after.contains(needle), "{needle} leaked into {after}");
    }
    // The manifest still holds the description, for the human.
    let manifest = read_artifacts(&layout::artifacts_file(&backfill)).unwrap();
    let secret = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.path == SECRET_PATH)
        .unwrap();
    assert_eq!(secret.visibility, Some(Visibility::HumanOnly));
    assert_eq!(secret.description, SECRET_DESCRIPTION);

    let context = session_context(&backfill).unwrap();
    assert_eq!(context.goal, "Backfill usage events");
    assert_eq!(context.parent_goal.as_deref(), Some("Invoicing"));
    assert_eq!(context.worktree, Some(PathBuf::from(WORKTREE)));
    assert_eq!(context.task_toml, layout::task_file(&backfill));

    // Sharing it again brings it back.
    run(&root, |tree| {
        artifact_share(tree, &ctx(), "l1", SECRET_PATH)
    });
    assert!(serialized(&backfill).contains(SECRET_DESCRIPTION));
}

#[test]
fn hide_all_leaves_only_the_goal_and_task_paths() {
    let temp = TempDir::new().unwrap();
    let (root, backfill) = seed(&temp);
    let before = serialized(&backfill);
    assert!(before.contains("plan.md") && before.contains(PUBLIC_PATH));

    run(&root, |tree| artifact_hide_all(tree, &ctx(), "l1"));

    let context = session_context(&backfill).unwrap();
    assert!(context.artifacts.is_empty());
    assert!(shared_artifacts(&backfill).is_empty());
    let expected = serde_json::json!({
        "goal": "Backfill usage events",
        "task_toml": layout::task_file(&backfill),
        "task_dir": backfill,
        "worktree": WORKTREE,
        "parent_goal": "Invoicing",
        "artifacts": [],
    });
    assert_eq!(serde_json::to_value(&context).unwrap(), expected);
    let after = serialized(&backfill);
    for needle in [
        "plan.md",
        "docs",
        "design",
        SECRET_PATH,
        PUBLIC_DESCRIPTION,
        SECRET_DESCRIPTION,
    ] {
        assert!(!after.contains(needle), "{needle} leaked into {after}");
    }
    // A second hide all has nothing to do.
    let tree = load(&root);
    assert!(artifact_hide_all(&tree, &ctx(), "l1").is_err());
}

#[test]
fn share_all_round_trips() {
    let temp = TempDir::new().unwrap();
    let (root, backfill) = seed(&temp);
    let original = shared_artifacts(&backfill);
    assert_eq!(original.len(), 3, "{original:?}");

    run(&root, |tree| artifact_hide_all(tree, &ctx(), "l1"));
    assert!(shared_artifacts(&backfill).is_empty());
    run(&root, |tree| artifact_share_all(tree, &ctx(), "l1"));

    assert_eq!(shared_artifacts(&backfill), original);
    let manifest = read_artifacts(&layout::artifacts_file(&backfill)).unwrap();
    let paths: Vec<&str> = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect();
    assert_eq!(paths, [PUBLIC_PATH, SECRET_PATH, "plan.md"]);
    // `task.toml` was never touched: no history entry.
    let file = read_task(&layout::task_file(&backfill)).unwrap();
    assert!(file.history.is_empty());
}

#[test]
fn a_new_file_takes_the_default_visibility_and_a_gone_file_is_dropped() {
    let temp = TempDir::new().unwrap();
    let (root, backfill) = seed(&temp);
    write(&backfill.join("notes/findings.md"), "found\n");
    fs::remove_file(backfill.join(PUBLIC_PATH)).unwrap();

    let list = read_task_artifacts(&backfill).unwrap();
    let findings = list
        .artifacts
        .iter()
        .find(|artifact| artifact.path == "notes/findings.md")
        .unwrap();
    assert_eq!(findings.visibility, Some(Visibility::Shared));
    assert_eq!(findings.description, "");
    assert!(
        list.artifacts
            .iter()
            .all(|artifact| artifact.path != PUBLIC_PATH)
    );
    assert!(shared_paths(&backfill).contains(&backfill.join("notes/findings.md")));

    // The next write adds the new file to the manifest and drops the gone one.
    run(&root, |tree| artifact_hide(tree, &ctx(), "l1", "plan.md"));
    let manifest = read_artifacts(&layout::artifacts_file(&backfill)).unwrap();
    let paths: Vec<&str> = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect();
    assert_eq!(paths, ["notes/findings.md", SECRET_PATH, "plan.md"]);

    // Reserved files never count.
    assert!(
        manifest
            .artifacts
            .iter()
            .all(|artifact| !artifact.path.starts_with("chats") && artifact.path != "task.toml")
    );
}

#[test]
fn a_human_only_default_hides_files_an_agent_adds() {
    let temp = TempDir::new().unwrap();
    let (root, backfill) = seed(&temp);
    // A write pins the visibility of every file in the manifest.
    run(&root, |tree| artifact_hide(tree, &ctx(), "l1", "plan.md"));
    let change = run(&root, |tree| {
        set_default_visibility(tree, &ctx(), "l1", Visibility::HumanOnly)
    });
    assert_eq!(change.entry.event, "default visibility set");
    assert_eq!(
        read_task(&layout::task_file(&backfill))
            .unwrap()
            .default_visibility,
        Visibility::HumanOnly
    );

    // An agent writes a file; it is hidden without anyone touching the manifest.
    write(&backfill.join("notes/agent-scratch.md"), "secret scratch\n");
    let context = serialized(&backfill);
    assert!(context.contains(PUBLIC_PATH), "{context}");
    assert!(!context.contains("agent-scratch"), "{context}");
    // Files the manifest lists with a visibility keep it.
    assert!(context.contains(SECRET_PATH), "{context}");

    let tree = load(&root);
    assert!(set_default_visibility(&tree, &ctx(), "l1", Visibility::HumanOnly).is_err());
}

#[test]
fn a_hidden_file_with_a_backslash_in_its_name_stays_hidden() {
    let temp = TempDir::new().unwrap();
    let (root, backfill) = seed(&temp);
    // One file name holding a backslash, not `payroll/secret.md`.
    let name = "notes/payroll\\secret.md";
    write(&backfill.join(name), "numbers\n");
    assert!(serialized(&backfill).contains("secret.md"));

    run(&root, |tree| artifact_hide(tree, &ctx(), "l1", name));
    let after = serialized(&backfill);
    assert!(after.contains(PUBLIC_PATH), "{after}");
    for needle in ["secret.md", "payroll"] {
        assert!(!after.contains(needle), "{needle} leaked into {after}");
    }
    let manifest = read_artifacts(&layout::artifacts_file(&backfill)).unwrap();
    assert!(manifest.artifacts.iter().any(
        |artifact| artifact.path == name && artifact.visibility == Some(Visibility::HumanOnly)
    ));
}

#[test]
fn an_entry_without_a_visibility_follows_the_default() {
    let temp = TempDir::new().unwrap();
    let (root, backfill) = seed(&temp);
    // No visibility, and a misspelled key that is kept but means nothing.
    write(
        &layout::artifacts_file(&backfill),
        &format!(
            "schema = 1\n\n[[artifact]]\npath = \"{PUBLIC_PATH}\"\ndescription = \"{PUBLIC_DESCRIPTION}\"\n\n\
             [[artifact]]\npath = \"{SECRET_PATH}\"\ndescription = \"{SECRET_DESCRIPTION}\"\n\
             visiblity = \"shared\"\n"
        ),
    );
    // Under the shared default both are told, so the search below would find them.
    let shared = serialized(&backfill);
    for needle in [
        PUBLIC_PATH,
        PUBLIC_DESCRIPTION,
        SECRET_PATH,
        SECRET_DESCRIPTION,
    ] {
        assert!(
            shared.contains(needle),
            "{needle} should be found in {shared}"
        );
    }

    run(&root, |tree| {
        set_default_visibility(tree, &ctx(), "l1", Visibility::HumanOnly)
    });
    let hidden = serialized(&backfill);
    for needle in [
        PUBLIC_PATH,
        PUBLIC_DESCRIPTION,
        SECRET_PATH,
        SECRET_DESCRIPTION,
    ] {
        assert!(!hidden.contains(needle), "{needle} leaked into {hidden}");
    }
    // A write pins the resolved visibility and keeps the unknown key.
    run(&root, |tree| artifact_share(tree, &ctx(), "l1", "plan.md"));
    let manifest = read_artifacts(&layout::artifacts_file(&backfill)).unwrap();
    let secret = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.path == SECRET_PATH)
        .unwrap();
    assert_eq!(secret.visibility, Some(Visibility::HumanOnly));
    assert!(secret.extra.contains_key("visiblity"));
    assert!(!serialized(&backfill).contains(SECRET_PATH));
}

#[test]
fn a_manifest_that_does_not_parse_shares_nothing() {
    let temp = TempDir::new().unwrap();
    let (_, backfill) = seed(&temp);
    write(
        &layout::artifacts_file(&backfill),
        "schema = 1\n[[artifact]\npath = \"docs/design.md\"\n",
    );
    assert!(shared_artifacts(&backfill).is_empty());
    let context = serialized(&backfill);
    assert!(!context.contains("design") && !context.contains("salary"));
}

#[test]
fn a_users_own_git_directory_is_never_an_artifact() {
    let temp = TempDir::new().unwrap();
    let (_root, backfill) = seed(&temp);
    write(&backfill.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&backfill.join(".git/config"), "[core]\n");
    let file = read_task_artifacts(&backfill).unwrap();
    assert!(file.artifacts.iter().all(|a| !a.path.starts_with(".git")));
    assert!(shared_paths(&backfill).iter().all(|p| {
        p.strip_prefix(&backfill)
            .unwrap_or(p)
            .components()
            .all(|c| c.as_os_str() != ".git")
    }));
}

#[test]
fn every_new_task_and_project_gets_a_shared_prompts_file() {
    let temp = TempDir::new().unwrap();
    let (root, backfill) = seed(&temp);
    let child = run(&root, |tree| {
        commands::new_child(
            tree,
            &ctx(),
            "g1",
            NewTask {
                goal: "Proration".to_string(),
                ..NewTask::default()
            },
        )
    });
    assert!(layout::prompts_file(&child.dir).is_file());
    let project = run(&root, |tree| {
        commands::project_new(
            tree,
            &ctx(),
            NewProject {
                title: "Payments".to_string(),
                repositories: Vec::new(),
            },
        )
    });
    assert!(layout::prompts_file(&project.dir).is_file());
    assert_eq!(
        shared_paths(&project.dir),
        [layout::prompts_file(&project.dir)]
    );

    // An edit of an existing task creates none, and a split child under a human-only default
    // still shares its prompts.
    run(&root, |tree| commands::todo_skip(tree, &ctx(), "l1", None));
    assert!(!layout::prompts_file(&backfill).exists());
    run(&root, |tree| {
        set_default_visibility(tree, &ctx(), "l1", Visibility::HumanOnly)
    });
    let split = run(&root, |tree| {
        commands::split(tree, &ctx(), "l1", Some("Part one"))
    });
    let split_child = split
        .writes
        .iter()
        .find_map(|write| match write {
            commands::Write::Task { dir, file } if file.goal == "Part one" => Some(dir.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        shared_paths(&split_child),
        [layout::prompts_file(&split_child)]
    );
}

#[test]
fn a_link_is_an_artifact_file() {
    let temp = TempDir::new().unwrap();
    let (root, backfill) = seed(&temp);
    let change = run(&root, |tree| {
        artifact_link(
            tree,
            &ctx(),
            "l1",
            "https://notion.so/acme/Spec-123",
            "Billing spec",
        )
    });
    let path = backfill.join("billing-spec.link.toml");
    assert_eq!(change.writes[0].path(), path);
    let link = read_link(&path).unwrap();
    assert_eq!(link.url, "https://notion.so/acme/Spec-123");
    assert_eq!(link.title, "Billing spec");
    assert!(shared_paths(&backfill).contains(&path));

    // A second link with the same title gets its own file.
    let second = run(&root, |tree| {
        artifact_link(tree, &ctx(), "l1", "https://example.com", "Billing spec")
    });
    assert_eq!(
        second.writes[0].path(),
        backfill.join("billing-spec-2.link.toml")
    );
    let tree = load(&root);
    assert!(artifact_link(&tree, &ctx(), "l1", " ", "x").is_err());
}

#[test]
fn merge_keeps_the_strictest_duplicate_and_normalizes_paths() {
    let manifest = parse_artifacts(
        "schema = 1\n\
         [[artifact]]\npath = \"./notes//a.md\"\ndescription = \"first\"\n\
         [[artifact]]\npath = \"notes/a.md\"\nvisibility = \"human-only\"\n\
         [[artifact]]\npath = \"gone.md\"\n",
    )
    .unwrap();
    let merged = merge(
        &["notes/a.md".to_string(), "prompts.txt".to_string()],
        Some(&manifest),
        Visibility::HumanOnly,
    );
    assert_eq!(merged.artifacts.len(), 2);
    assert_eq!(merged.artifacts[0].path, "notes/a.md");
    assert_eq!(merged.artifacts[0].description, "first");
    assert_eq!(merged.artifacts[0].visibility, Some(Visibility::HumanOnly));
    assert_eq!(merged.artifacts[1].visibility, Some(Visibility::Shared));
}
