//! Where `task: start` puts worktrees: the repository's `worktree_root`, the default root, the
//! central directory, the fallback when a parent is not writable, the exclude lines, and the
//! promise that worktrees made under `.bongos-worktrees` by older versions stay untouched.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use bongos_work_model::commands::StartedWorktree;
use bongos_work_model::store::{WorkTree, load};
use bongos_work_model::{Kind, layout, serialize_task};
use gpui::TestAppContext;
use tempfile::TempDir;
use workspace::notifications::NotificationId;

use crate::Repository;
use crate::command_tests::{Input, wait_until};
use crate::store_tests::{project_toml, write};
use crate::worktree_tests::{
    central, exclude_entries, git, leaf, refusal, set_repository_root, setup, test_layout,
    test_repository,
};
use crate::worktrees::{
    EXCLUDE_LINE, Layout, Plan, StartedToast, WORKTREES_DIR, inside_line, plan, started_message,
};

/// A project `name` on the repository `repository`, with its top group and the leaves given as
/// `(slug, id)`.
fn project(root: &Path, name: &str, repository: &str, leaves: &[(&str, &str)]) {
    let dir = layout::project_dir(root, name);
    write(
        &layout::project_file(&dir),
        &format!("schema = 1\ntitle = \"{name}\"\nrepositories = [\"{repository}\"]\n"),
    );
    leaf(&dir, &format!("top-{name}"), Kind::PrShip, |file| {
        file.todos.clear()
    });
    for (slug, id) in leaves {
        leaf(&layout::child_dir(&dir, slug), id, Kind::PrShip, |_| {});
    }
}

fn loaded(root: &Path) -> WorkTree {
    let tree = load(root);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    tree
}

/// The worktree path and new branch `task: start` plans for `id`.
fn planned(
    tree: &WorkTree,
    id: &str,
    repositories: &[Repository],
    layout: &Layout,
) -> (PathBuf, String) {
    match plan(tree, id, repositories, layout) {
        Ok(Plan::Ready(request)) => (
            request.worktree().to_path_buf(),
            request.new_branch_name().unwrap().to_string(),
        ),
        Ok(_) => panic!("expected a ready plan"),
        Err(message) => panic!("refused: {message}"),
    }
}

/// Records `worktree` and `branch` on the task `id` at `dir`, as a start would.
fn record(dir: &Path, id: &str, worktree: &Path, branch: &str) {
    let worktree = worktree.to_string_lossy().into_owned();
    leaf(dir, id, Kind::PrShip, |file| {
        file.worktree = Some(worktree);
        file.branch = Some(branch.to_string());
    });
}

#[test]
fn a_repository_root_wins_and_another_repository_is_unaffected() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("work");
    project(&root, "invoicing", "invoicing", &[("inner", "i1")]);
    project(&root, "api", "api", &[("probe", "p1")]);
    let tree = loaded(&root);
    let repositories = [
        test_repository("invoicing", "/src/invoicing", Some("{repo}-worktrees")),
        test_repository("api", "/src/api", None),
    ];
    let mut layout = test_layout(Path::new("/data"));

    assert_eq!(
        planned(&tree, "i1", &repositories, &layout).0,
        PathBuf::from("/src/invoicing-worktrees/inner")
    );
    assert_eq!(
        planned(&tree, "p1", &repositories, &layout).0,
        PathBuf::from("/data/worktrees/api/probe")
    );

    // A default root moves the repository without its own root only.
    layout.default_root = Some("~/trees/{repo}".to_string());
    assert_eq!(
        planned(&tree, "i1", &repositories, &layout).0,
        PathBuf::from("/src/invoicing-worktrees/inner")
    );
    assert_eq!(
        planned(&tree, "p1", &repositories, &layout).0,
        PathBuf::from("/home/u/trees/api/probe")
    );
}

#[test]
fn two_projects_on_one_repository_share_the_root_with_distinct_paths_and_branches() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("work");
    let long = "reconcile-the-ledger-exports-with-the-quarterly-statements-again";
    project(
        &root,
        "invoicing",
        "invoicing",
        &[("backfill", "b1"), (long, "x1")],
    );
    project(
        &root,
        "billing",
        "invoicing",
        &[("backfill", "b2"), (long, "x2")],
    );
    let repositories = [test_repository("invoicing", "/src/invoicing", None)];
    let layout = test_layout(Path::new("/data"));

    // The first project's tasks start as they are.
    let tree = loaded(&root);
    let (backfill, branch) = planned(&tree, "b1", &repositories, &layout);
    assert_eq!(
        backfill,
        PathBuf::from("/data/worktrees/invoicing/backfill")
    );
    assert_eq!(branch, "backfill");
    let (long_path, long_branch) = planned(&tree, "x1", &repositories, &layout);
    assert_eq!(long_branch, long, "the branch keeps the full slug");
    let invoicing = layout::project_dir(&root, "invoicing");
    record(
        &layout::child_dir(&invoicing, "backfill"),
        "b1",
        &backfill,
        &branch,
    );
    record(
        &layout::child_dir(&invoicing, long),
        "x1",
        &long_path,
        &long_branch,
    );

    // The second project's get `-2` in the folder and the branch on a clash. A shortened slug
    // already differs by its id suffix, and its branch still gets `-2`.
    let tree = loaded(&root);
    let (path, branch) = planned(&tree, "b2", &repositories, &layout);
    assert_eq!(path, PathBuf::from("/data/worktrees/invoicing/backfill-2"));
    assert_eq!(branch, "backfill-2");
    let (path, branch) = planned(&tree, "x2", &repositories, &layout);
    assert_ne!(path, long_path);
    assert_eq!(path.parent(), long_path.parent());
    assert_eq!(branch, format!("{long}-2"));
}

#[test]
fn a_worktree_root_in_project_toml_changes_nothing() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("work");
    project(&root, "invoicing", "invoicing", &[("inner", "i1")]);
    write(
        &layout::project_file(&layout::project_dir(&root, "invoicing")),
        &format!(
            "{}worktree_root = \"/elsewhere/{{slug}}\"\n",
            project_toml("Invoicing")
        ),
    );
    let tree = loaded(&root);
    let repositories = [test_repository("invoicing", "/src/invoicing", None)];
    assert_eq!(
        planned(&tree, "i1", &repositories, &test_layout(Path::new("/data"))).0,
        PathBuf::from("/data/worktrees/invoicing/inner")
    );
}

#[test]
fn a_too_long_root_refuses_with_the_setting_name_and_makes_nothing() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("work");
    project(&root, "invoicing", "invoicing", &[("inner", "i1")]);
    let tree = loaded(&root);
    let long = temp.path().join("x".repeat(200));
    let long_text = long.to_string_lossy().into_owned();
    let layout = test_layout(&temp.path().join("data"));

    let repositories = [test_repository(
        "invoicing",
        "/src/invoicing",
        Some(&long_text),
    )];
    let message = refusal(plan(&tree, "i1", &repositories, &layout));
    assert!(
        message.contains("longer than 200 characters")
            && message.ends_with("shorten work.repositories[invoicing].worktree_root"),
        "{message}"
    );

    let repositories = [test_repository("invoicing", "/src/invoicing", None)];
    let layout = Layout {
        default_root: Some(long_text),
        ..layout
    };
    let message = refusal(plan(&tree, "i1", &repositories, &layout));
    assert!(
        message.ends_with("shorten work.default_worktree_root"),
        "{message}"
    );
    assert!(!long.exists() && !temp.path().join("data").exists());

    // An unknown token names its setting too.
    let repositories = [test_repository(
        "invoicing",
        "/src/invoicing",
        Some("/trees/{user}"),
    )];
    let message = refusal(plan(&tree, "i1", &repositories, &layout));
    assert!(
        message.starts_with("work.repositories[invoicing].worktree_root: "),
        "{message}"
    );
}

#[test]
fn only_a_worktree_inside_the_checkout_gets_a_line() {
    let repository = Path::new("/src/invoicing");
    assert_eq!(
        inside_line(repository, &repository.join(".trees/backfill")),
        Some("/.trees/".to_string())
    );
    assert_eq!(
        inside_line(repository, Path::new("/src/invoicing-worktrees/backfill")),
        None
    );
    assert_eq!(
        inside_line(repository, Path::new("/data/worktrees/invoicing/backfill")),
        None
    );
}

#[test]
fn the_fallback_toast_says_where_and_why() {
    let started = StartedWorktree {
        branch: "backfill".to_string(),
        worktree: "/data/worktrees/invoicing/backfill".to_string(),
        fallback: Some("/src/invoicing-worktrees".to_string()),
        ..Default::default()
    };
    assert_eq!(
        started_message(&started),
        "Worktree created in the central directory; /src/invoicing-worktrees is not writable."
    );
}

#[gpui::test]
async fn a_template_inside_the_checkout_adds_its_own_exclude_line(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let template = setup
        .repository
        .join(".trees")
        .to_string_lossy()
        .into_owned();
    set_repository_root(cx, &setup.repository, None, Some(&template));
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    let worktree = setup.repository.join(".trees").join("backfill");
    assert_eq!(
        harness.file(cx, &["backfill"]).worktree,
        Some(worktree.to_string_lossy().into_owned())
    );
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "backfill");
    assert_eq!(
        exclude_entries(&setup.real.join(".git")),
        [EXCLUDE_LINE, "/.trees/"]
    );
    assert_eq!(git(&setup.real, &["status", "--porcelain"]), "");
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Whether a file can be made in `dir` (a privileged user ignores the mode bits).
fn writable(dir: &Path) -> bool {
    let probe = dir.join("probe");
    let made = std::fs::write(&probe, "").is_ok();
    std::fs::remove_file(&probe).ok();
    made
}

#[gpui::test]
async fn an_unwritable_parent_falls_back_to_the_central_directory_once(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let sibling = setup
        .repository
        .parent()
        .unwrap()
        .join("invoicing-worktrees");
    std::fs::create_dir_all(&sibling).unwrap();
    set_mode(&sibling, 0o555);
    if writable(&sibling) {
        set_mode(&sibling, 0o755);
        eprintln!("skipped: the mode bits do not stop this user from writing");
        return;
    }
    set_repository_root(cx, &setup.repository, None, Some("{repo}-worktrees"));

    harness.clear_toasts(cx);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    let file = harness.file(cx, &["backfill"]);
    let worktree = central(harness, "backfill");
    assert_eq!(file.worktree, Some(worktree.to_string_lossy().into_owned()));
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "backfill");
    let created: Vec<&str> = file
        .history
        .iter()
        .map(|entry| entry.event.as_str())
        .filter(|event| event.starts_with("worktree created"))
        .collect();
    assert_eq!(created, ["worktree created in the central directory"]);
    let detail = file.history.last().unwrap().detail.clone().unwrap();
    assert!(
        detail.ends_with(&format!("{} is not writable", sibling.display())),
        "{detail}"
    );
    assert!(!harness.has_toast(cx), "no refusal");
    harness.workspace.read_with(cx, |workspace, _| {
        assert!(
            workspace
                .notification_ids()
                .contains(&NotificationId::unique::<StartedToast>())
        )
    });
    assert_eq!(std::fs::read_dir(&sibling).unwrap().count(), 0);

    // With the central parent unwritable too, the start refuses and records nothing.
    let central_parent = worktree.parent().unwrap().to_path_buf();
    set_mode(&central_parent, 0o555);
    let writes = harness.writes(cx);
    harness.clear_toasts(cx);
    harness
        .run(cx, "task::Start", &[Input::Pick("alpha")])
        .await;
    wait_until(cx, "the refusal", |cx| harness.has_toast(cx)).await;
    assert_eq!(harness.writes(cx), writes);
    assert_eq!(harness.file(cx, &["phase", "alpha"]).worktree, None);
    assert_eq!(git(&setup.real, &["branch", "--list", "alpha"]), "");
    set_mode(&central_parent, 0o755);
    set_mode(&sibling, 0o755);
}

/// The bytes of everything an old worktree is made of: its `.git` file, a file in it, git's
/// record of it and the commit of its branch.
fn old_worktree_bytes(real: &Path, worktree: &Path) -> Vec<Vec<u8>> {
    vec![
        std::fs::read(worktree.join(".git")).unwrap(),
        std::fs::read(worktree.join("notes.txt")).unwrap(),
        std::fs::read(real.join(".git/worktrees/backfill/gitdir")).unwrap(),
        git(real, &["rev-parse", "refs/heads/backfill"]).into_bytes(),
        git(worktree, &["status", "--porcelain"]).into_bytes(),
    ]
}

#[gpui::test]
async fn a_worktree_under_bongos_worktrees_stays_where_it_is(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let real = &setup.real;

    // An older version started backfill under `.bongos-worktrees` and excluded that directory.
    let old = real.join(WORKTREES_DIR).join("backfill");
    git(
        real,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "backfill",
            old.to_str().unwrap(),
            "main",
        ],
    );
    write(&old.join("notes.txt"), "work in progress");
    write(
        &real.join(".git/info/exclude"),
        &format!("{EXCLUDE_LINE}\n"),
    );
    let recorded = setup
        .repository
        .join(WORKTREES_DIR)
        .join("backfill")
        .to_string_lossy()
        .into_owned();
    let mut backfill = harness.file(cx, &["backfill"]);
    backfill.worktree = Some(recorded.clone());
    backfill.branch = Some("backfill".to_string());
    write(
        &layout::task_file(&harness.task_dir(&["backfill"])),
        &serialize_task(&backfill).unwrap(),
    );
    wait_until(cx, "the recorded worktree to load", |cx| {
        harness.file(cx, &["backfill"]).worktree.is_some()
    })
    .await;
    let before = old_worktree_bytes(real, &old);

    // Starting it again reports the old worktree and makes nothing.
    harness.clear_toasts(cx);
    let writes = harness.writes(cx);
    harness
        .run(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    wait_until(cx, "the report", |cx| harness.has_toast(cx)).await;
    assert_eq!(harness.writes(cx), writes);

    // A new root, then another task: the new worktree goes there and the old one is untouched.
    set_repository_root(cx, &setup.repository, None, Some("{repo}-trees"));
    harness.clear_toasts(cx);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("alpha")])
        .await;
    let alpha = setup
        .repository
        .parent()
        .unwrap()
        .join("invoicing-trees")
        .join("alpha");
    assert_eq!(
        harness.file(cx, &["phase", "alpha"]).worktree,
        Some(alpha.to_string_lossy().into_owned())
    );
    assert_eq!(old_worktree_bytes(real, &old), before);
    assert_eq!(harness.file(cx, &["backfill"]).worktree, Some(recorded));
    assert_eq!(exclude_entries(&real.join(".git")), [EXCLUDE_LINE]);
    assert_eq!(git(real, &["status", "--porcelain"]), "");
}
