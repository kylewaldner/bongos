//! Tests of the refresh: its triggers, the 60 second window, coalescing, batches, the chips it
//! drives, the one write, and that nothing is sent without links, while off or without a token.
//! Over a real work store in a temporary directory and a fake Linear.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bongos_linear_model::api::{ApiError, Issue, LinearApi, StateKind, WorkflowState};
use bongos_linear_model::fake::{Call, FakeLinear};
use bongos_linear_model::sync::{self as model, refresh_diff};
use bongos_work::{Focus, RowChipsRegistry, WorkDirs, WorkStore};
use bongos_work_model::commands::{Context as WorkContext, default_todos};
use bongos_work_model::store::WorkTree;
use bongos_work_model::{
    Kind, LinearState, ProjectFile, TaskFile, layout, serialize_project, serialize_task,
};
use db::AppDatabase;
use fs::RealFs;
use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
use project::Project;
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::LinearToast;
use crate::actions::LastToast;
use crate::actions_tests::{dispatch, set_enabled_in, toast_shown};
use crate::refresh::{self, NO_LINKED, Refresher, WINDOW_MS, apply_changes};
use crate::sync::{self, LinearSync, ReadState};
use crate::token::{self, NO_TOKEN_MESSAGE};

const SECRET: &str = "lin_api_SECRETTOKEN123";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn leaf(id: &str, goal: &str) -> TaskFile {
    let mut file = TaskFile::new(goal, Kind::PrShip);
    file.id = id.to_string();
    file.todos = default_todos(Kind::PrShip);
    file
}

fn group(id: &str, goal: &str) -> TaskFile {
    let mut file = TaskFile::new(goal, Kind::PrShip);
    file.id = id.to_string();
    file
}

/// Links `file` to SEQ-`number` the way a link leaves it, with the description Linear has.
fn synced(file: &mut TaskFile, number: u32) {
    file.linear = Some(format!("SEQ-{number}"));
    file.linear_state = Some(LinearState {
        issue_id: format!("uuid-SEQ-{number}"),
        team_id: "team".to_string(),
        url: format!("https://linear.app/acme/issue/SEQ-{number}"),
        description: format!("Description {number}"),
        ..LinearState::default()
    });
}

fn put(dir: &Path, file: &TaskFile) {
    write(&layout::task_file(dir), &serialize_task(file).unwrap());
}

/// Project `p`: the leaves `alpha` (l1, never linked), `beta` (l2, SEQ-5, goal "Old beta") and
/// `gamma` (l3, SEQ-6, in step with Linear), `lost` (l4, SEQ-7, which Linear does not have) and
/// the group `stream` (g1, SEQ-100) with the leaf `sub` (l5, unlinked). With `linked` false no
/// task is linked.
fn seed(root: &Path, linked: bool) {
    let project = layout::project_dir(root, "p");
    write(
        &layout::project_file(&project),
        &serialize_project(&ProjectFile::new("P")).unwrap(),
    );
    put(&project, &group("top", "P"));
    let link = |file: &mut TaskFile, number| {
        if linked {
            synced(file, number)
        }
    };
    put(&layout::child_dir(&project, "alpha"), &leaf("l1", "Alpha"));
    let mut beta = leaf("l2", "Old beta");
    link(&mut beta, 5);
    put(&layout::child_dir(&project, "beta"), &beta);
    let mut gamma = leaf("l3", "Ticket 6");
    link(&mut gamma, 6);
    put(&layout::child_dir(&project, "gamma"), &gamma);
    let mut lost = leaf("l4", "Lost");
    link(&mut lost, 7);
    put(&layout::child_dir(&project, "lost"), &lost);
    let stream = layout::child_dir(&project, "stream");
    let mut stream_file = group("g1", "Stream");
    link(&mut stream_file, 100);
    put(&stream, &stream_file);
    put(&layout::child_dir(&stream, "sub"), &leaf("l5", "Sub"));
}

fn state(id: &str, kind: StateKind) -> WorkflowState {
    WorkflowState {
        id: id.to_string(),
        name: id.to_string(),
        kind,
        position: 1.0,
    }
}

fn issue(number: u32, title: &str) -> Issue {
    Issue {
        id: format!("uuid-SEQ-{number}"),
        identifier: format!("SEQ-{number}"),
        title: title.to_string(),
        description: format!("Description {number}"),
        url: format!("https://linear.app/acme/issue/SEQ-{number}"),
        team_id: "team".to_string(),
        state: state("backlog", StateKind::Backlog),
        parent: None,
        children: Vec::new(),
        children_truncated: false,
        blocks: Vec::new(),
    }
}

struct Env {
    workspace: Entity<Workspace>,
    store: Entity<WorkStore>,
    linear: Entity<LinearSync>,
    refresher: Entity<Refresher>,
    fake: FakeLinear,
    _temp: TempDir,
}

fn canonical(temp: &TempDir) -> PathBuf {
    std::fs::canonicalize(temp.path()).unwrap()
}

async fn wait_until(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    for _ in 0..200 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        cx.background_executor
            .timer(Duration::from_millis(50))
            .await;
    }
    panic!("timed out waiting for {what}");
}

/// A store over the seeded tree, a workspace with the work and sync actions, a fake client that
/// has tickets 5, 6 and 100 (the title of 5 changed, and 101 under 100 that no task links), and,
/// when `token`, a token in the environment. The work store and focus exist before the sync, as
/// in the app.
async fn env(
    cx: &mut TestAppContext,
    linked: bool,
    linear_on: bool,
    token: bool,
) -> (Env, &mut VisualTestContext) {
    env_with(cx, linked, Some(linear_on), token).await
}

/// [`env`] with `linear.enabled` left at its default when `linear_on` is `None`.
async fn env_with(
    cx: &mut TestAppContext,
    linked: bool,
    linear_on: Option<bool>,
    token: bool,
) -> (Env, &mut VisualTestContext) {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    seed(&root, linked);
    let fake = FakeLinear::new("acme");
    fake.add_team("team", "SEQ", vec![state("backlog", StateKind::Backlog)]);
    let mut five = issue(5, "New beta");
    five.description = "Fresh words".into();
    fake.add_issue(five);
    fake.add_issue(issue(6, "Ticket 6"));
    fake.add_issue(issue(100, "Stream"));
    let mut child = issue(101, "Child 101");
    child.parent = Some(issue(100, "Stream").summary());
    fake.add_issue(child);
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        cx.set_global(AppDatabase::test_new());
        app_state
    });
    cx.update(|cx| {
        bongos_work::init(
            Arc::new(RealFs::new(None, cx.background_executor().clone())),
            WorkDirs { root },
            cx,
        );
        crate::init(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(true);
            });
        });
        token::install_credentials_provider(
            Arc::new(crate::token_tests::FakeCredentials::default()),
            cx,
        );
        let env_token = token.then(|| SECRET.to_string());
        token::install_env(
            move |name| {
                (name == token::ENV_VAR)
                    .then(|| env_token.clone())
                    .flatten()
            },
            cx,
        );
        let client = fake.clone();
        token::install_client_factory(
            Arc::new(move |_| Arc::new(client.clone()) as Arc<dyn LinearApi>),
            cx,
        );
    });
    let (store, linear, refresher) = cx.update(|cx| {
        (
            WorkStore::global(cx).unwrap(),
            sync::global(cx).unwrap(),
            refresh::global(cx).unwrap(),
        )
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    workspace.update_in(cx, |workspace, window, cx| {
        bongos_work::register(workspace);
        crate::register(workspace);
        window.focus(&gpui::Focusable::focus_handle(workspace, cx), cx)
    });
    wait_until(cx, "the first load", |cx| {
        store.read_with(cx, |store, _| store.snapshot().is_some())
    })
    .await;
    // Let the watches settle before writing anything.
    cx.background_executor
        .timer(Duration::from_millis(1000))
        .await;
    if let Some(linear_on) = linear_on {
        set_enabled_in(cx, linear_on);
    }
    (
        Env {
            workspace,
            store,
            linear,
            refresher,
            fake,
            _temp: temp,
        },
        cx,
    )
}

impl Env {
    fn tree(&self, cx: &mut VisualTestContext) -> Arc<WorkTree> {
        self.store
            .read_with(cx, |store, _| store.snapshot().unwrap())
    }

    fn file(&self, id: &str, cx: &mut VisualTestContext) -> TaskFile {
        self.tree(cx)
            .task_by_id(id)
            .unwrap_or_else(|| panic!("no task {id}"))
            .file
            .clone()
            .unwrap()
    }

    fn writes(&self, cx: &mut VisualTestContext) -> usize {
        self.store.read_with(cx, |store, _| store.stats().writes)
    }

    fn calls(&self) -> Vec<Call> {
        self.fake.calls()
    }

    fn focus(&self, id: &str, cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            Focus::global(cx)
                .unwrap()
                .update(cx, |focus, cx| focus.set(Some(id.to_string()), cx))
        });
    }

    fn last_toast(&self, cx: &mut VisualTestContext) -> String {
        cx.update(|_, cx| {
            cx.try_global::<LastToast>()
                .map(|t| t.0.clone())
                .unwrap_or_default()
        })
    }

    fn clear_toast(&self, cx: &mut VisualTestContext) {
        self.workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&NotificationId::unique::<LinearToast>(), cx)
        });
        cx.update(|_, cx| cx.set_global(LastToast(String::new())));
    }

    fn read_state(&self, id: &str, cx: &mut VisualTestContext) -> Option<ReadState> {
        self.linear.read_with(cx, |linear, _| linear.read_state(id))
    }

    fn chips(&self, id: &str, cx: &mut VisualTestContext) -> Vec<String> {
        cx.update(|_, cx| {
            RowChipsRegistry::chips(id, cx)
                .into_iter()
                .map(|chip| chip.to_string())
                .collect()
        })
    }

    /// Lets the cycle that is running, if any, finish, writes included.
    async fn settle(&self, cx: &mut VisualTestContext) {
        wait_until(cx, "the refresh to finish", |cx| {
            !self
                .refresher
                .read_with(cx, |refresher, _| refresher.busy())
        })
        .await;
    }

    /// The Work view opens, as the panel and `task: focus` tell the observers.
    async fn open_view(&self, cx: &mut VisualTestContext) {
        self.refresher
            .update(cx, |refresher, cx| refresher.view_opened(cx));
        self.settle(cx).await;
    }

    /// `task: refresh from linear`.
    async fn command(&self, cx: &mut VisualTestContext) {
        dispatch("task::RefreshFromLinear", cx);
        self.settle(cx).await;
    }

    fn advance(&self, ms: u64, cx: &mut VisualTestContext) {
        self.refresher
            .update(cx, |refresher, _| refresher.advance_clock(ms));
    }
}

fn issues_call(identifiers: &[&str]) -> Call {
    Call::Issues(identifiers.iter().map(|i| i.to_string()).collect())
}

#[gpui::test]
async fn refresh_updates_a_changed_title_and_writes_nothing_when_unchanged(
    cx: &mut TestAppContext,
) {
    let (env, cx) = env(cx, true, true, true).await;
    env.focus("l2", cx);
    env.settle(cx).await;
    assert_eq!(env.calls(), vec![issues_call(&["SEQ-5"])]);
    let beta = env.file("l2", cx);
    assert_eq!(beta.goal, "New beta");
    let entry = beta
        .history
        .iter()
        .find(|entry| entry.event == "title from Linear")
        .unwrap();
    assert_eq!(entry.by, "linear");
    assert_eq!(
        entry.detail.as_deref(),
        Some("\"Old beta\" -> \"New beta\"")
    );
    assert_eq!(
        beta.linear_state.as_ref().unwrap().description,
        "Fresh words"
    );
    assert!(
        beta.history
            .iter()
            .any(|e| e.event == "description from Linear" && e.by == "linear")
    );
    assert_eq!(env.writes(cx), 1, "title and description in one write");

    // Again, ignoring the window: nothing is different, so nothing is written.
    let before = env.writes(cx);
    let history = beta.history.len();
    env.command(cx).await;
    assert_eq!(env.calls().len(), 2);
    assert_eq!(env.writes(cx), before);
    assert_eq!(env.file("l2", cx).history.len(), history);
    assert!(env.read_state("l2", cx).is_some());
}

#[gpui::test]
async fn opening_the_view_with_only_unlinked_tasks_makes_no_call(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, false, true, true).await;
    env.open_view(cx).await;
    env.focus("l1", cx);
    env.settle(cx).await;
    dispatch("task::RefreshFromLinear", cx);
    env.settle(cx).await;
    assert_eq!(env.last_toast(cx), NO_LINKED);
    env.clear_toast(cx);
    dispatch("task::Focus", cx);
    env.settle(cx).await;
    assert!(env.calls().is_empty());
    let allowed = env
        .linear
        .read_with(cx, |linear, cx| linear.snapshot(cx).requests_allowed);
    assert_eq!(allowed, 0);
}

/// Every task of the tree with its file, and what the sync adds to it: its context section, its
/// branch name and its row chips.
fn unlinked_view(
    env: &Env,
    cx: &mut VisualTestContext,
) -> Vec<(
    String,
    TaskFile,
    Option<String>,
    Option<String>,
    Vec<String>,
)> {
    let tree = env.tree(cx);
    let mut view = Vec::new();
    for id in ["l1", "l2", "l3", "l4", "g1", "l5"] {
        let task = tree.task_by_id(id).unwrap();
        let file = task.file.clone().unwrap();
        let section = crate::conventions::section_for(&tree, id);
        let branch = bongos_linear_model::conventions::branch_name(&file, "{slug}", &task.slug);
        let chips = env.chips(id, cx);
        view.push((task.slug.clone(), file, section, branch, chips));
    }
    view
}

#[gpui::test]
async fn a_default_workspace_leaves_unlinked_tasks_alone(cx: &mut TestAppContext) {
    // The default settings: `linear.enabled` is true, with a token, and no task is linked.
    let (on, cx) = env_with(cx, false, None, true).await;
    cx.update(|_, cx| assert!(crate::LinearSettings::get_global(cx).enabled));
    on.open_view(cx).await;
    for id in ["l1", "l2", "l3", "l4", "g1", "l5"] {
        on.focus(id, cx);
        on.settle(cx).await;
    }
    dispatch("task::RefreshFromLinear", cx);
    on.settle(cx).await;
    assert!(on.calls().is_empty(), "{:?}", on.calls());
    let allowed = on
        .linear
        .read_with(cx, |linear, cx| linear.snapshot(cx).requests_allowed);
    assert_eq!(allowed, 0);
    assert_eq!(on.writes(cx), 0, "nothing is written");
    let with_sync = unlinked_view(&on, cx);

    // The same tasks with the sync off: rows, branch names and context text are the same.
    set_enabled_in(cx, false);
    on.open_view(cx).await;
    on.focus("l1", cx);
    on.settle(cx).await;
    assert_eq!(unlinked_view(&on, cx), with_sync);
    for (slug, file, section, branch, chips) in &with_sync {
        assert_eq!(file.linear, None, "{slug}");
        assert_eq!(file.linear_state, None, "{slug}");
        assert_eq!(*section, None, "{slug}");
        assert_eq!(*branch, None, "{slug}");
        assert!(chips.is_empty(), "{slug}");
    }
}

#[gpui::test]
async fn nothing_is_sent_while_off(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, false, true).await;
    env.open_view(cx).await;
    env.focus("l2", cx);
    env.settle(cx).await;
    assert!(env.calls().is_empty(), "linear.enabled is false");
    assert_eq!(env.chips("l2", cx), Vec::<String>::new());
}

#[gpui::test]
async fn nothing_is_sent_without_a_token(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, false).await;
    env.open_view(cx).await;
    env.focus("l2", cx);
    env.settle(cx).await;
    assert!(env.calls().is_empty(), "no token");
    assert!(
        !toast_shown(&env.workspace, cx),
        "an automatic refresh is quiet"
    );
    assert_eq!(
        env.chips("l2", cx),
        vec!["linear unreachable".to_string()],
        "without a token the linked rows show the quiet unreachable state"
    );
    dispatch("task::RefreshFromLinear", cx);
    env.settle(cx).await;
    assert_eq!(env.last_toast(cx), NO_TOKEN_MESSAGE);
    assert!(env.calls().is_empty());
}

#[gpui::test]
async fn the_window_skips_recent_reads_and_the_command_ignores_it(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    env.open_view(cx).await;
    assert_eq!(
        env.calls(),
        vec![issues_call(&["SEQ-100", "SEQ-5", "SEQ-6", "SEQ-7"])]
    );
    env.open_view(cx).await;
    assert_eq!(
        env.calls().len(),
        1,
        "a second open within 60 s makes no call"
    );
    env.advance(WINDOW_MS - 1_000, cx);
    env.open_view(cx).await;
    assert_eq!(env.calls().len(), 1);
    env.advance(2_000, cx);
    env.open_view(cx).await;
    assert_eq!(env.calls().len(), 2, "after 60 s it reads again");
    env.command(cx).await;
    assert_eq!(env.calls().len(), 3, "the command ignores the window");
}

#[gpui::test]
async fn opening_reads_only_the_focused_tasks_project_or_its_linked_ancestors(
    cx: &mut TestAppContext,
) {
    let (env, cx) = env(cx, true, true, true).await;
    // A focused leaf under a linked group reads the leaf's linked ancestors, not its siblings.
    env.focus("l5", cx);
    env.settle(cx).await;
    assert_eq!(env.calls(), vec![issues_call(&["SEQ-100"])]);
    let tree = env.tree(cx);
    assert_eq!(refresh::focus_scope(&tree, "l5"), vec!["g1".to_string()]);
    assert_eq!(refresh::focus_scope(&tree, "l1"), Vec::<String>::new());
    assert_eq!(refresh::subtree_scope(&tree, "g1"), vec!["g1".to_string()]);
}

#[gpui::test]
async fn the_real_task_focus_command_refreshes_the_view(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    dispatch("task::Focus", cx);
    env.settle(cx).await;
    assert_eq!(env.calls().len(), 1);
}

#[gpui::test]
async fn triggers_during_a_cycle_coalesce_into_one_follow_up(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    env.refresher.update(cx, |refresher, cx| {
        refresher.request(vec!["l2".into()], true, cx);
        refresher.request(vec!["l3".into()], true, cx);
        refresher.request(vec!["l3".into(), "l4".into()], true, cx);
        refresher.request(vec!["l2".into()], false, cx);
    });
    env.settle(cx).await;
    assert_eq!(
        env.calls(),
        vec![issues_call(&["SEQ-5"]), issues_call(&["SEQ-6", "SEQ-7"])],
        "one cycle, then one follow-up that holds everything queued"
    );
}

#[gpui::test]
async fn batches_follow_the_complexity_budget_one_at_a_time(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    env.refresher
        .update(cx, |refresher, _| refresher.set_budget(200));
    env.refresher.update(cx, |refresher, cx| {
        refresher.request(vec!["l2".into(), "l3".into(), "l4".into()], true, cx)
    });
    env.settle(cx).await;
    assert_eq!(
        env.calls(),
        vec![
            issues_call(&["SEQ-5"]),
            issues_call(&["SEQ-6"]),
            issues_call(&["SEQ-7"])
        ]
    );
}

#[test]
fn batch_sizes_come_from_the_estimate() {
    let items: Vec<u32> = (0..130).collect();
    let all = refresh::batches(&items, bongos_linear_model::graphql::BATCH_BUDGET);
    assert!(all.len() > 1 && all.len() < 5, "{}", all.len());
    assert_eq!(all.concat(), items);
    assert_eq!(refresh::batches(&items[..3], 200).len(), 3);
    assert!(refresh::batches::<u32>(&[], 200).is_empty());
}

#[test]
fn the_window_is_sixty_seconds() {
    assert!(refresh::is_fresh(Some(1_000), 1_000));
    assert!(refresh::is_fresh(Some(1_000), 60_999));
    assert!(!refresh::is_fresh(Some(1_000), 61_000));
    assert!(!refresh::is_fresh(None, 1_000));
}

#[gpui::test]
async fn a_link_changed_between_read_and_write_is_skipped(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    let tree = env.tree(cx);
    let issues = vec![issue(5, "New beta")];
    let diff = refresh_diff(&tree, &["l2".to_string()], &issues);
    assert_eq!(diff.changes.len(), 1);

    // The task is linked to another ticket before the write.
    let other = issue(8, "Other");
    let unlink = env.store.update(cx, |store, cx| {
        store.write(
            WorkContext::kyle(),
            |tree, ctx| model::unlink(tree, ctx, "l2"),
            cx,
        )
    });
    unlink.await.unwrap();
    let relink = env.store.update(cx, |store, cx| {
        let other = other.clone();
        store.write(
            WorkContext::kyle(),
            move |tree, ctx| model::link(tree, ctx, "l2", &other),
            cx,
        )
    });
    relink.await.unwrap();
    wait_until(cx, "the relink", |cx| {
        env.file("l2", cx).linear.as_deref() == Some("SEQ-8")
    })
    .await;
    let writes = env.writes(cx);
    let task = cx.update(|_, cx| apply_changes(diff, cx));
    assert!(task.is_none(), "nothing is left to write for the old link");
    assert_eq!(env.writes(cx), writes);
    assert_eq!(
        env.file("l2", cx).goal,
        "Other",
        "the new link's title, not the old read's"
    );
}

#[gpui::test]
async fn chips_show_unreachable_rate_limited_not_found_and_suggestions(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    env.command(cx).await;
    // The read worked: tickets 5, 6 and 100 are fresh, 7 does not exist, 101 is a suggestion.
    assert_eq!(env.chips("l3", cx), Vec::<String>::new());
    assert_eq!(env.chips("l4", cx), vec!["linear: not found".to_string()]);
    assert_eq!(env.chips("g1", cx), vec!["1 suggestion".to_string()]);
    assert_eq!(env.chips("l1", cx), Vec::<String>::new());

    env.fake.fail_next(ApiError::Network("down".into()));
    env.command(cx).await;
    assert_eq!(env.chips("l3", cx), vec!["linear unreachable".to_string()]);
    assert_eq!(
        env.chips("g1", cx),
        vec!["linear unreachable".to_string(), "1 suggestion".to_string()]
    );

    env.fake.fail_next(ApiError::RateLimited { reset_at: None });
    env.command(cx).await;
    assert_eq!(env.chips("l3", cx), vec!["linear rate limited".to_string()]);

    // The next good read clears them.
    env.command(cx).await;
    assert_eq!(env.chips("l3", cx), Vec::<String>::new());

    // Off hides the chips.
    set_enabled_in(cx, false);
    assert_eq!(env.chips("l4", cx), Vec::<String>::new());
}

#[gpui::test]
async fn after_a_rate_limit_only_the_command_tries(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    env.fake.fail_next(ApiError::RateLimited { reset_at: None });
    env.command(cx).await;
    assert_eq!(env.calls().len(), 1);
    assert!(env.last_toast(cx).starts_with("Linear rate limit reached"));
    env.advance(WINDOW_MS + 1, cx);
    env.open_view(cx).await;
    env.focus("l2", cx);
    env.settle(cx).await;
    assert_eq!(
        env.calls().len(),
        1,
        "automatic refreshes stop while limited"
    );
    env.command(cx).await;
    assert_eq!(env.calls().len(), 2, "the command tries");
    env.advance(WINDOW_MS + 1, cx);
    env.open_view(cx).await;
    assert_eq!(env.calls().len(), 3, "a good read lifts the limit");
}

#[gpui::test]
async fn a_known_reset_time_lets_automatic_refreshes_try_again(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    let soon = crate::client::now_ms() + 5 * 60_000;
    env.fake.fail_next(ApiError::RateLimited {
        reset_at: Some(soon),
    });
    env.command(cx).await;
    assert!(
        env.last_toast(cx).contains("resets in 5 min"),
        "{}",
        env.last_toast(cx)
    );
    env.advance(WINDOW_MS + 1, cx);
    env.open_view(cx).await;
    assert_eq!(env.calls().len(), 1);
    env.advance(5 * 60_000, cx);
    env.open_view(cx).await;
    assert_eq!(env.calls().len(), 2);
}

#[gpui::test]
async fn a_rejected_token_toasts_once_and_stops_every_request(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    env.fake.fail_next(ApiError::Unauthorized);
    env.open_view(cx).await;
    cx.run_until_parked();
    assert_eq!(env.calls().len(), 1);
    assert_eq!(
        env.last_toast(cx),
        "Linear rejected the token; run linear: set token"
    );
    env.clear_toast(cx);
    env.advance(WINDOW_MS + 1, cx);
    env.open_view(cx).await;
    assert_eq!(env.calls().len(), 1);
    assert_eq!(env.last_toast(cx), "", "no second toast");
    // The explicit command is refused at the gate too, with the same words.
    env.command(cx).await;
    assert_eq!(env.calls().len(), 1);
    assert_eq!(
        env.last_toast(cx),
        "Linear rejected the token; run linear: set token"
    );
    // A token that is set again lifts it.
    env.linear.update(cx, |linear, _| {
        linear.accept_token(Arc::new(env.fake.clone()) as Arc<dyn LinearApi>)
    });
    env.command(cx).await;
    assert_eq!(env.calls().len(), 2);
}

#[gpui::test]
async fn no_toast_dump_or_chip_carries_the_token_or_error_text(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    env.fake
        .fail_next(ApiError::Server(format!("boom {SECRET} typed text")));
    env.command(cx).await;
    let toast = env.last_toast(cx);
    assert!(
        !toast.contains(SECRET) && !toast.contains("typed text"),
        "{toast}"
    );
    let dump = env
        .linear
        .read_with(cx, |linear, cx| crate::dump::render(&linear.snapshot(cx)));
    assert!(!dump.contains(SECRET));
    for id in ["l2", "l3", "g1"] {
        for chip in env.chips(id, cx) {
            assert!(!chip.contains(SECRET));
        }
    }
}

#[gpui::test]
async fn the_command_reads_the_tasks_under_the_task_at_point(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    env.focus("g1", cx);
    env.settle(cx).await;
    env.fake.clear_calls();
    env.command(cx).await;
    assert_eq!(env.calls(), vec![issues_call(&["SEQ-100"])]);
    env.focus("l1", cx);
    env.settle(cx).await;
    env.clear_toast(cx);
    env.command(cx).await;
    assert_eq!(env.calls().len(), 1, "an unlinked leaf has nothing to read");
    assert_eq!(env.last_toast(cx), NO_LINKED);
}

#[gpui::test]
async fn the_linked_set_follows_the_store(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true, true).await;
    let linked = |env: &Env, cx: &mut VisualTestContext| {
        env.linear
            .read_with(cx, |linear, cx| linear.snapshot(cx).linked)
    };
    assert_eq!(linked(&env, cx), vec!["g1", "l2", "l3", "l4"]);
    // A link written by the store, not through the command, is seen from its event.
    let ticket = issue(9, "Nine");
    let write = env.store.update(cx, |store, cx| {
        store.write(
            WorkContext::kyle(),
            move |tree, ctx| model::link(tree, ctx, "l1", &ticket),
            cx,
        )
    });
    write.await.unwrap();
    wait_until(cx, "the linked set", |cx| {
        linked(&env, cx).contains(&"l1".to_string())
    })
    .await;
    let write = env.store.update(cx, |store, cx| {
        store.write(
            WorkContext::kyle(),
            |tree, ctx| model::unlink(tree, ctx, "l3"),
            cx,
        )
    });
    write.await.unwrap();
    wait_until(cx, "the linked set", |cx| {
        !linked(&env, cx).contains(&"l3".to_string())
    })
    .await;
    assert!(
        env.linear
            .read_with(cx, |linear, _| linear.read_state("l3").is_none())
    );
}
