//! `task: start` against real git repositories in temporary directories, with the real git
//! binary: the branch and its base, the exclude line, refusals, and the setup command.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;

use bongos_work_model::commands::default_todos;
use bongos_work_model::store::load;
use bongos_work_model::worktree_path::{Configured, TemplateLayer};
use bongos_work_model::{
    Kind, LinearState, ResultKind, StackedOn, TaskFile, TaskResult, TodoState, UnblockPoint,
    layout, serialize_task,
};
use gpui::{App, Task, TestAppContext, UpdateGlobal as _, VisualTestContext, Window};
use parking_lot::Mutex;
use settings::{SettingsStore, WorkRepositoryContent};
use tempfile::TempDir;
use workspace::TerminalProvider;

use crate::Repository;
use crate::command_tests::{Harness, Input, dispatch, harness, wait_until};
use crate::store_tests::{project_toml, write};
use crate::worktrees::{
    EXCLUDE_LINE, Layout, Plan, Spot, WORKTREES_DIR, branch_name, common_git_dir,
    default_or_fallback, ensure_excluded, expand_home, plan, pull_request, remote_names,
    worktree_spot,
};

/// Runs git in `dir` with no user or system configuration, and returns its trimmed output.
// Blocking is fine here: the fixtures are made before, or between, the steps of a test.
#[allow(clippy::disallowed_methods)]
pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository on `main` with one commit.
pub(crate) fn init_repository(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "first"]);
}

fn exclude_lines(git_dir: &Path) -> usize {
    std::fs::read_to_string(git_dir.join("info").join("exclude"))
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == EXCLUDE_LINE)
        .count()
}

fn worktree_count(repository: &Path) -> usize {
    git(repository, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
}

/// Lists the repository `invoicing` (the seeded project's) at `path`.
fn set_repository(cx: &mut VisualTestContext, path: &Path, setup_command: Option<&str>) {
    set_repository_root(cx, path, setup_command, None);
}

/// Lists the repository `invoicing` at `path` with the worktree root `worktree_root`.
pub(crate) fn set_repository_root(
    cx: &mut VisualTestContext,
    path: &Path,
    setup_command: Option<&str>,
    worktree_root: Option<&str>,
) {
    let path = path.to_string_lossy().into_owned();
    let setup_command = setup_command.map(str::to_string);
    let worktree_root = worktree_root.map(str::to_string);
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().repositories =
                    Some(vec![WorkRepositoryContent {
                        name: "invoicing".to_string(),
                        path,
                        setup_command,
                        merge_is_ship: None,
                        deployments: None,
                        branch_template: None,
                        worktree_root,
                    }]);
            });
        });
    });
}

/// Where a task's worktree goes by default: the test store's central directory, under the
/// repository `invoicing`.
pub(crate) fn central(harness: &Harness, slug: &str) -> PathBuf {
    harness
        .root
        .with_file_name("central-worktrees")
        .join("invoicing")
        .join(slug)
}

/// The lines of the repository's `info/exclude` that are not comments.
pub(crate) fn exclude_entries(git_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(git_dir.join("info").join("exclude"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Records every task spawned in a terminal and reports it ended with the exit status it holds
/// (success by default).
#[derive(Clone, Default)]
pub(crate) struct Recorder(Arc<Mutex<Vec<::task::SpawnInTerminal>>>, ExitStatus);

impl TerminalProvider for Recorder {
    fn spawn(
        &self,
        task: ::task::SpawnInTerminal,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Option<anyhow::Result<ExitStatus>>> {
        self.0.lock().push(task);
        Task::ready(Some(Ok(self.1)))
    }
}

pub(crate) struct Setup {
    pub(crate) harness: Harness,
    /// The repository as the settings name it: a symbolic link to `real`.
    pub(crate) repository: PathBuf,
    pub(crate) real: PathBuf,
    pub(crate) terminal: Recorder,
    _temp: TempDir,
}

pub(crate) async fn setup<'a>(
    cx: &'a mut TestAppContext,
    setup_command: Option<&str>,
) -> (Setup, &'a mut VisualTestContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let temp = TempDir::new().unwrap();
    let real = std::fs::canonicalize(temp.path())
        .unwrap()
        .join("invoicing");
    init_repository(&real);
    let repository = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &repository).unwrap();
    set_repository(cx, &repository, setup_command);
    let terminal = Recorder::default();
    harness.workspace.update(cx, |workspace, _| {
        workspace.set_terminal_provider(terminal.clone())
    });
    (
        Setup {
            harness,
            repository,
            real,
            terminal,
            _temp: temp,
        },
        cx,
    )
}

#[gpui::test]
async fn start_cuts_the_branch_from_main_runs_the_setup_and_a_repeat_makes_nothing(
    cx: &mut TestAppContext,
) {
    let (setup, cx) = setup(cx, Some("make setup")).await;
    let harness = &setup.harness;
    let main = git(&setup.real, &["rev-parse", "main"]);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;

    let worktree = central(harness, "backfill");
    let file = harness.file(cx, &["backfill"]);
    assert_eq!(file.branch.as_deref(), Some("backfill"));
    assert_eq!(file.base.as_deref(), Some("main"));
    assert_eq!(file.base_sha.as_deref(), Some(main.as_str()));
    assert_eq!(file.worktree, Some(worktree.to_string_lossy().into_owned()));
    assert!(file.todos[0].started.is_some());
    assert_eq!(file.todos[0].state, TodoState::Open);
    assert!(file.todos[1..].iter().all(|todo| todo.started.is_none()));
    assert_eq!(file.history.last().unwrap().event, "worktree created");
    assert!(!harness.has_toast(cx), "no refusal");

    // The real repository has the branch, checked out at main's commit in the worktree.
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), main);
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "backfill");
    // The checkout gets only the legacy exclude line and no directory of worktrees.
    assert_eq!(exclude_entries(&setup.real.join(".git")), [EXCLUDE_LINE]);
    assert!(!setup.real.join(WORKTREES_DIR).exists());
    assert_eq!(git(&setup.real, &["status", "--porcelain"]), "");

    // The setup command ran in a terminal, in the worktree.
    wait_until(cx, "the setup command", |_| {
        !setup.terminal.0.lock().is_empty()
    })
    .await;
    {
        let spawned = setup.terminal.0.lock();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].command.as_deref(), Some("make setup"));
        assert_eq!(spawned[0].cwd.as_deref(), Some(worktree.as_path()));
    }

    // Starting it again reports the worktree and creates nothing.
    let writes = harness.writes(cx);
    harness.clear_toasts(cx);
    harness
        .run(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    wait_until(cx, "the report", |cx| harness.has_toast(cx)).await;
    assert_eq!(harness.writes(cx), writes);
    assert_eq!(worktree_count(&setup.real), 2);
    assert_eq!(setup.terminal.0.lock().len(), 1);
    assert_eq!(exclude_lines(&setup.real.join(".git")), 1);
}

#[gpui::test]
async fn a_stacked_task_is_refused_while_blocked_then_branches_from_its_parent(
    cx: &mut TestAppContext,
) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let beta_dir = harness.task_dir(&["phase", "beta"]);
    let mut beta = harness.file(cx, &["phase", "beta"]);
    beta.stacked_on = Some(StackedOn {
        task: "alpha".to_string(),
        unblock: UnblockPoint::Implemented,
        extra: Default::default(),
    });
    write(
        &layout::task_file(&beta_dir),
        &serialize_task(&beta).unwrap(),
    );
    wait_until(cx, "the edge to load", |cx| {
        harness.file(cx, &["phase", "beta"]).stacked_on.is_some()
    })
    .await;

    // Blocked: alpha has not reached Implemented.
    harness.clear_toasts(cx);
    let writes = harness.writes(cx);
    harness.run(cx, "task::Start", &[Input::Pick("beta")]).await;
    wait_until(cx, "the refusal", |cx| harness.has_toast(cx)).await;
    assert_eq!(harness.writes(cx), writes);
    assert_eq!(harness.file(cx, &["phase", "beta"]).worktree, None);
    assert!(!central(harness, "beta").exists());

    // Alpha starts from main, gains a commit and is implemented.
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("alpha")])
        .await;
    let alpha_worktree = central(harness, "alpha");
    git(
        &alpha_worktree,
        &["commit", "-q", "--allow-empty", "-m", "alpha work"],
    );
    let alpha_head = git(&setup.real, &["rev-parse", "alpha"]);
    assert_ne!(alpha_head, git(&setup.real, &["rev-parse", "main"]));
    harness
        .run_and_wait(cx, "task::Advance", &[Input::Pick("alpha")])
        .await;

    // Beta now branches from alpha's branch, at its head.
    harness.clear_toasts(cx);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("beta")])
        .await;
    let beta = harness.file(cx, &["phase", "beta"]);
    assert_eq!(beta.branch.as_deref(), Some("beta"));
    assert_eq!(beta.base.as_deref(), Some("alpha"));
    assert_eq!(beta.base_sha.as_deref(), Some(alpha_head.as_str()));
    let beta_worktree = central(harness, "beta");
    assert_eq!(git(&beta_worktree, &["rev-parse", "HEAD"]), alpha_head);
    assert!(!harness.has_toast(cx));
}

#[gpui::test]
async fn an_occupied_path_or_a_taken_branch_is_refused_and_left_alone(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, Some("make setup")).await;
    let harness = &setup.harness;
    let occupied = central(harness, "backfill");
    write(&occupied.join("notes.txt"), "keep me");
    git(&setup.real, &["branch", "alpha"]);

    for slug in ["backfill", "alpha"] {
        harness.clear_toasts(cx);
        let writes = harness.writes(cx);
        dispatch(cx, "task::Start");
        harness.answer(cx, &Input::Pick(slug)).await;
        wait_until(cx, "the refusal", |cx| harness.has_toast(cx)).await;
        assert_eq!(harness.writes(cx), writes, "{slug}");
    }
    assert_eq!(
        std::fs::read_to_string(occupied.join("notes.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(harness.file(cx, &["backfill"]).worktree, None);
    assert_eq!(harness.file(cx, &["phase", "alpha"]).worktree, None);
    assert_eq!(worktree_count(&setup.real), 1);
    assert_eq!(
        git(&setup.real, &["branch", "--list", "backfill"]),
        "",
        "no branch was made"
    );
    assert!(setup.terminal.0.lock().is_empty());
}

#[test]
fn the_exclude_line_is_added_once_also_through_a_linked_worktree() {
    let temp = TempDir::new().unwrap();
    let main = std::fs::canonicalize(temp.path()).unwrap().join("main");
    init_repository(&main);
    let git_dir = main.join(".git");
    std::fs::write(git_dir.join("info").join("exclude"), "*.log").unwrap();

    ensure_excluded(&main, EXCLUDE_LINE).unwrap();
    ensure_excluded(&main, EXCLUDE_LINE).unwrap();
    let text = std::fs::read_to_string(git_dir.join("info").join("exclude")).unwrap();
    assert_eq!(text, format!("*.log\n{EXCLUDE_LINE}\n"));

    // A repository that is itself a linked worktree writes to the common dir, creating `info/`.
    std::fs::remove_dir_all(git_dir.join("info")).unwrap();
    let linked = main.parent().unwrap().join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::canonicalize(common_git_dir(&linked).unwrap()).unwrap(),
        git_dir
    );
    ensure_excluded(&linked, EXCLUDE_LINE).unwrap();
    ensure_excluded(&linked, EXCLUDE_LINE).unwrap();
    assert_eq!(exclude_lines(&git_dir), 1);
    assert_eq!(exclude_lines(&git_dir.join("worktrees").join("linked")), 0);
    assert!(!linked.join(".gitignore").exists() && !main.join(".gitignore").exists());
}

#[test]
fn branch_names_paths_and_pull_numbers() {
    assert_eq!(branch_name("{slug}", "backfill", None), "backfill");
    assert_eq!(
        branch_name("person/{slug}", "backfill", Some("INV-7")),
        "person/backfill"
    );
    assert_eq!(
        branch_name("{linear}-{slug}", "backfill", Some("inv-7")),
        "inv-7-backfill"
    );
    assert_eq!(branch_name("{linear}-{slug}", "backfill", None), "backfill");
    assert_eq!(
        branch_name("person/{linear}/{slug}", "backfill", None),
        "person/backfill"
    );
    assert_eq!(branch_name("{linear}", "backfill", None), "backfill");

    let layout = test_layout(Path::new("/data"));
    let repository = test_repository("invoicing", "/src/invoicing", None);
    let spot = Spot {
        template: None,
        repository: &repository,
        layout: &layout,
    };
    let held = [PathBuf::from("/data/worktrees/invoicing/backfill")];
    assert_eq!(
        worktree_spot(&spot, "backfill", "l1", &held).unwrap(),
        (PathBuf::from("/data/worktrees/invoicing/backfill-2"), 2)
    );
    assert_eq!(
        worktree_spot(&spot, "alpha", "l2", &held).unwrap(),
        (PathBuf::from("/data/worktrees/invoicing/alpha"), 1)
    );

    let pull = pull_request;
    assert_eq!(
        pull("https://github.com/o/r/pull/12"),
        Some(("o/r".to_string(), 12))
    );
    assert_eq!(
        pull("https://github.com/o/r/pull/12/files"),
        Some(("o/r".to_string(), 12))
    );
    assert_eq!(pull("https://example.com/o/r/issues/12"), None);
    assert_eq!(pull("https://github.com/pull/12"), None);

    assert!(remote_names(
        "https://github.com/Acme/Invoicing.git",
        "acme/invoicing"
    ));
    assert!(remote_names(
        "git@github.com:acme/invoicing.git",
        "acme/invoicing"
    ));
    assert!(remote_names(
        "/srv/git/acme/invoicing.git/",
        "acme/invoicing"
    ));
    assert!(!remote_names(
        "https://github.com/acme/invoicing-old",
        "acme/invoicing"
    ));
    assert!(!remote_names(
        "https://github.com/notacme/invoicing",
        "acme/invoicing"
    ));
    assert!(!remote_names(
        "https://github.com/acme/elsewhere",
        "acme/invoicing"
    ));
}

#[test]
fn home_is_expanded_only_at_the_start() {
    let home = util::paths::home_dir();
    assert_eq!(expand_home("~"), *home);
    assert_eq!(expand_home("~/src/invoicing"), home.join("src/invoicing"));
    assert_eq!(
        expand_home("/srv/invoicing"),
        PathBuf::from("/srv/invoicing")
    );
    assert_eq!(expand_home("~person/src"), PathBuf::from("~person/src"));
    assert_eq!(expand_home("src/~/x"), PathBuf::from("src/~/x"));
}

#[test]
fn the_base_falls_back_to_main_when_git_finds_no_default_branch() {
    assert_eq!(default_or_fallback(Ok(None)), "main");
    assert_eq!(default_or_fallback(Err(anyhow::anyhow!("no git"))), "main");
    assert_eq!(default_or_fallback(Ok(Some("trunk".into()))), "trunk");
}

#[test]
fn a_path_held_through_a_symbolic_link_counts_as_held() {
    let temp = TempDir::new().unwrap();
    let real = std::fs::canonicalize(temp.path())
        .unwrap()
        .join("invoicing");
    std::fs::create_dir_all(&real).unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // Neither `trees` nor the worktree exists yet; the link still resolves.
    let held = [link.join("trees").join("backfill")];
    let layout = test_layout(&temp.path().join("data"));
    for root in [&real, &link] {
        let template = root.join("trees").to_string_lossy().into_owned();
        let repository = test_repository("invoicing", "/src/invoicing", Some(&template));
        let spot = Spot {
            template: Some(Configured {
                text: &template,
                layer: TemplateLayer::Own,
            }),
            repository: &repository,
            layout: &layout,
        };
        let (path, number) = worktree_spot(&spot, "backfill", "l1", &held).unwrap();
        assert_eq!(number, 2);
        assert_eq!(path, root.join("trees").join("backfill-2"));
    }
}

/// A layout with no default root, the central directory `<data>/worktrees` and the home `/home/u`.
pub(crate) fn test_layout(data: &Path) -> Layout {
    Layout {
        default_root: None,
        home: PathBuf::from("/home/u"),
        central: data.join("worktrees"),
    }
}

pub(crate) fn test_repository(name: &str, path: &str, worktree_root: Option<&str>) -> Repository {
    Repository {
        name: name.to_string(),
        path: path.to_string(),
        setup_command: None,
        merge_is_ship: true,
        deployments: true,
        branch_template: "{slug}".to_string(),
        worktree_root: worktree_root.map(str::to_string),
    }
}

pub(crate) fn leaf(dir: &Path, id: &str, kind: Kind, edit: impl FnOnce(&mut TaskFile)) {
    let mut file = TaskFile::new(id, kind);
    file.id = id.to_string();
    file.todos = default_todos(kind);
    edit(&mut file);
    write(&layout::task_file(dir), &serialize_task(&file).unwrap());
}

pub(crate) fn refusal(planned: Result<Plan, String>) -> String {
    match planned {
        Err(message) => message,
        Ok(_) => panic!("expected a refusal"),
    }
}

#[test]
fn documents_others_groups_and_gone_worktrees_are_refused() {
    let temp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("work");
    let project = layout::project_dir(&root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    leaf(&project, "top", Kind::PrShip, |file| file.todos.clear());
    let group = layout::child_dir(&project, "group");
    leaf(&group, "group", Kind::PrShip, |file| file.todos.clear());
    leaf(
        &layout::child_dir(&group, "inner"),
        "inner",
        Kind::PrShip,
        |_| {},
    );
    leaf(
        &layout::child_dir(&project, "doc"),
        "doc",
        Kind::Document,
        |_| {},
    );
    leaf(
        &layout::child_dir(&project, "misc"),
        "misc",
        Kind::Other,
        |_| {},
    );
    let gone = root.join("nowhere").to_string_lossy().into_owned();
    leaf(
        &layout::child_dir(&project, "gone"),
        "gone",
        Kind::PrShip,
        |file| {
            file.worktree = Some(gone.clone());
            file.branch = Some("gone".to_string());
        },
    );
    leaf(
        &layout::child_dir(&project, "lost"),
        "lost",
        Kind::PrShip,
        |file| {
            file.worktree = Some(gone.clone());
        },
    );
    let tree = load(&root);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let repositories = [Repository {
        name: "invoicing".to_string(),
        path: root.join("repository").to_string_lossy().into_owned(),
        setup_command: None,
        merge_is_ship: true,
        deployments: true,
        branch_template: "{slug}".to_string(),
        worktree_root: None,
    }];
    let layout = test_layout(&root.join("data"));
    let refused = |id: &str| refusal(plan(&tree, id, &repositories, &layout));

    assert_eq!(
        refused("doc"),
        "doc is of kind document; only pr-ship, pr-merge, spike and babysit tasks get a worktree"
    );
    assert_eq!(
        refused("misc"),
        "misc is of kind other; only pr-ship, pr-merge, spike and babysit tasks get a worktree"
    );
    assert_eq!(
        refused("group"),
        "group is a group; only a leaf task gets a worktree"
    );
    assert!(refused("top").ends_with("is a group; only a leaf task gets a worktree"));
    let gone_message = refused("gone");
    assert!(
        gone_message.contains(&format!("git worktree add {gone} gone")),
        "{gone_message}"
    );
    assert!(
        refused("lost").contains("Remove the `worktree` line from its task.toml"),
        "says what to do"
    );
    // A pr-ship leaf is planned.
    assert!(plan(&tree, "inner", &repositories, &layout).is_ok());
}

/// How a link sits on a task: nothing, a hand-set identifier, a synced link, or a create in flight.
#[derive(Clone, Copy)]
enum Link {
    None,
    Hand(&'static str),
    Synced(&'static str),
    Pending,
}

fn linked(file: &mut TaskFile, link: Link) {
    match link {
        Link::None => {}
        Link::Hand(id) => file.linear = Some(id.to_string()),
        Link::Synced(id) => {
            file.linear = Some(id.to_string());
            file.linear_state = Some(LinearState::default());
        }
        Link::Pending => {
            file.linear_state = Some(LinearState {
                pending_create: Some("key".to_string()),
                ..Default::default()
            });
        }
    }
}

/// The branch `task: start` would cut for the leaf `engine` under the group `phase`, with `group`
/// linked as given and `engine` linked as given, under `template`.
fn started_branch(group: Link, engine: Link, template: &str) -> String {
    let temp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("work");
    let project = layout::project_dir(&root, "invoicing");
    write(&layout::project_file(&project), &project_toml("Invoicing"));
    let phase = layout::child_dir(&project, "phase");
    leaf(&phase, "phase", Kind::PrShip, |file| {
        file.todos.clear();
        linked(file, group);
    });
    leaf(
        &layout::child_dir(&phase, "engine"),
        "engine",
        Kind::PrShip,
        |file| linked(file, engine),
    );
    let tree = load(&root);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    let repositories = [Repository {
        name: "invoicing".to_string(),
        path: root.join("repository").to_string_lossy().into_owned(),
        setup_command: None,
        merge_is_ship: true,
        deployments: true,
        branch_template: template.to_string(),
        worktree_root: None,
    }];
    match plan(
        &tree,
        "engine",
        &repositories,
        &test_layout(&root.join("data")),
    ) {
        Ok(Plan::Ready(request)) => request.new_branch_name().unwrap().to_string(),
        _ => panic!("expected a ready plan"),
    }
}

#[test]
fn branches_with_no_link_or_a_hand_set_one_are_the_ones_they_always_were() {
    for template in [
        "{slug}",
        "person/{slug}",
        "{linear}-{slug}",
        "person/{linear}/{slug}",
    ] {
        assert_eq!(
            started_branch(Link::None, Link::None, template),
            branch_name(template, "engine", None)
        );
        // A hand-set link on the leaf, and on the group above it (the ancestor fallback), expand
        // `{linear}` and never prefix a template without it.
        for link in [
            (Link::Hand("INV-7"), Link::None),
            (Link::None, Link::Hand("INV-7")),
        ] {
            assert_eq!(
                started_branch(link.0, link.1, template),
                branch_name(template, "engine", Some("INV-7"))
            );
        }
    }
    assert_eq!(
        started_branch(Link::Hand("INV-7"), Link::None, "{linear}-{slug}"),
        "INV-7-engine"
    );
    // A pending create has no identifier yet and no branch rule.
    assert_eq!(
        started_branch(Link::None, Link::Pending, "person/{slug}"),
        "person/engine"
    );
    assert_eq!(
        started_branch(Link::Hand("INV-7"), Link::Pending, "{linear}-{slug}"),
        "INV-7-engine"
    );
}

#[test]
fn a_synced_leaf_gets_its_identifier_in_the_branch() {
    // The shared vector, with the leaf's slug in place of its `proration-engine`.
    for (identifier, template, _, expected) in bongos_work_model::rules::LINEAR_BRANCH_VECTOR {
        assert_eq!(
            started_branch(Link::None, Link::Synced(identifier), template),
            expected.replace("proration-engine", "engine")
        );
    }
    assert_eq!(
        started_branch(Link::None, Link::Synced("SEQ-121"), "{slug}"),
        "seq-121-engine"
    );
    assert_eq!(
        started_branch(Link::None, Link::Synced("SEQ-121"), "person/{slug}"),
        "person/seq-121-engine"
    );
}

#[test]
fn a_synced_group_is_skipped_in_the_ancestor_fallback() {
    // The group's identifier never reaches a leaf's branch, with or without `{linear}`.
    assert_eq!(
        started_branch(Link::Synced("SEQ-9"), Link::None, "{slug}"),
        "engine"
    );
    assert_eq!(
        started_branch(Link::Synced("SEQ-9"), Link::None, "{linear}-{slug}"),
        "engine"
    );
    assert_eq!(
        started_branch(Link::Synced("SEQ-9"), Link::Pending, "person/{slug}"),
        "person/engine"
    );
    // The leaf's own synced link wins over its group's.
    assert_eq!(
        started_branch(Link::Synced("SEQ-9"), Link::Synced("SEQ-121"), "{slug}"),
        "seq-121-engine"
    );
}

#[gpui::test]
async fn a_failing_setup_command_shows_a_toast_and_keeps_the_worktree(cx: &mut TestAppContext) {
    use std::os::unix::process::ExitStatusExt as _;

    let (setup, cx) = setup(cx, Some("false")).await;
    let harness = &setup.harness;
    let failing = Recorder(Default::default(), ExitStatus::from_raw(1 << 8));
    harness.workspace.update(cx, |workspace, _| {
        workspace.set_terminal_provider(failing.clone())
    });
    harness.clear_toasts(cx);
    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    wait_until(cx, "the failure toast", |cx| harness.has_toast(cx)).await;
    assert_eq!(failing.0.lock().len(), 1);
    let worktree = central(harness, "backfill");
    assert!(worktree.join(".git").exists(), "the worktree stays");
    assert_eq!(
        harness.file(cx, &["backfill"]).worktree,
        Some(worktree.to_string_lossy().into_owned())
    );
    assert_eq!(worktree_count(&setup.real), 2);
}

#[gpui::test]
async fn an_unrecorded_worktree_is_adopted_with_the_merge_base_as_its_base(
    cx: &mut TestAppContext,
) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let cut_from = git(&setup.real, &["rev-parse", "main"]);
    let worktree = central(harness, "backfill");
    git(
        &setup.real,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "backfill",
            worktree.to_str().unwrap(),
            "main",
        ],
    );
    git(
        &setup.real,
        &["commit", "-q", "--allow-empty", "-m", "main moves on"],
    );

    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    let file = harness.file(cx, &["backfill"]);
    assert_eq!(file.branch.as_deref(), Some("backfill"));
    assert_eq!(file.base.as_deref(), Some("main"));
    assert_eq!(file.base_sha.as_deref(), Some(cut_from.as_str()));
    assert!(file.worktree.is_some());
    assert_eq!(worktree_count(&setup.real), 2);
    assert!(!harness.has_toast(cx));
}

#[gpui::test]
async fn babysit_fetches_the_pr_head_from_the_remote_that_names_its_repository(
    cx: &mut TestAppContext,
) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let real = &setup.real;
    let remotes = real.parent().unwrap().join("remotes");
    let mirror = remotes.join("acme").join("invoicing.git");
    let other = remotes.join("someone").join("else.git");
    for bare in [&mirror, &other] {
        std::fs::create_dir_all(bare).unwrap();
        git(bare, &["init", "-q", "--bare"]);
    }
    // `origin` is not the PR's repository and has no PR refs; `mirror` is and has one.
    git(real, &["remote", "add", "origin", other.to_str().unwrap()]);
    git(real, &["remote", "add", "mirror", mirror.to_str().unwrap()]);
    git(real, &["checkout", "-q", "-b", "feature"]);
    git(real, &["commit", "-q", "--allow-empty", "-m", "pr work"]);
    let head = git(real, &["rev-parse", "HEAD"]);
    git(real, &["push", "-q", "mirror", "feature:refs/pull/1/head"]);
    git(real, &["checkout", "-q", "main"]);
    git(real, &["branch", "-q", "-D", "feature"]);

    let watch_dir = harness.task_dir(&["watch"]);
    leaf(&watch_dir, "b1", Kind::Babysit, |file| {
        file.goal = "Watch the PRs".to_string();
        file.result = Some(TaskResult {
            kind: ResultKind::Prs,
            url: None,
            state: None,
            urls: vec![
                "https://github.com/acme/elsewhere/pull/2".to_string(),
                "https://github.com/acme/invoicing/pull/1".to_string(),
            ],
            extra: Default::default(),
        });
    });
    wait_until(cx, "the babysit task to load", |cx| {
        harness.tree(cx).task_by_id("b1").is_some()
    })
    .await;

    // No remote points at acme/elsewhere: refused, nothing fetched or created.
    harness.clear_toasts(cx);
    let writes = harness.writes(cx);
    harness
        .run(
            cx,
            "task::Start",
            &[Input::Pick("watch"), Input::Pick("elsewhere")],
        )
        .await;
    wait_until(cx, "the refusal", |cx| harness.has_toast(cx)).await;
    assert_eq!(harness.writes(cx), writes);
    assert_eq!(git(real, &["branch", "--list", "pr-2"]), "");
    assert!(!central(harness, "watch").exists());

    // acme/invoicing is `mirror`: its pull/1/head becomes `pr-1`, checked out in the worktree.
    harness.clear_toasts(cx);
    harness
        .run_and_wait(
            cx,
            "task::Start",
            &[Input::Pick("watch"), Input::Pick("invoicing/pull/1")],
        )
        .await;
    let file = harness.file(cx, &["watch"]);
    let worktree = central(harness, "watch");
    assert_eq!(file.branch.as_deref(), Some("pr-1"));
    assert_eq!(file.base, None);
    assert_eq!(file.base_sha, None);
    assert_eq!(file.worktree, Some(worktree.to_string_lossy().into_owned()));
    assert_eq!(git(real, &["rev-parse", "pr-1"]), head);
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "pr-1");
    assert_eq!(exclude_lines(&real.join(".git")), 1);
    assert!(!harness.has_toast(cx));
}
