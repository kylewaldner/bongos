//! Tests of the link, unlink and open commands over a real work store in a temporary directory
//! and a fake Linear: what one link writes, what the other two send, and that nothing reaches
//! Linear for an unlinked task, while the sync is off, or without a token.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bongos_linear_model::api::{
    ApiError, Issue, IssueRef, IssueSummary, LinearApi, StateKind, WorkflowState,
};
use bongos_linear_model::fake::{Call, FakeLinear};
use bongos_linear_model::ident::parse_identifier;
use bongos_linear_model::subtickets::unoffered;
use bongos_work::{Focus, WorkDirs, WorkStore};
use bongos_work_model::commands::default_todos;
use bongos_work_model::store::WorkTree;
use bongos_work_model::{
    Extra, Kind, LinearState, ProjectFile, ProposalRecord, ProposalState, TaskFile, layout,
    serialize_project, serialize_task,
};
use db::AppDatabase;
use fs::RealFs;
use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
use project::Project;
use settings::SettingsStore;
use tempfile::TempDir;
use workspace::notifications::NotificationId;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::LinearToast;
use crate::actions::LastToast;
use crate::actions_tests::{dispatch, set_enabled_in, toast_shown};
use crate::link::{self, Choice, LINK_FIRST, TYPED_ROW};
use crate::sync::{self, ReadState};
use crate::token::{self, NO_TOKEN_MESSAGE};

const SECRET: &str = "lin_api_SECRETTOKEN123";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn leaf_file(id: &str, goal: &str) -> TaskFile {
    let mut file = TaskFile::new(goal, Kind::PrShip);
    file.id = id.to_string();
    file.todos = default_todos(Kind::PrShip);
    file
}

fn group_file(id: &str, goal: &str) -> TaskFile {
    let mut file = TaskFile::new(goal, Kind::PrShip);
    file.id = id.to_string();
    file
}

fn synced(file: &mut TaskFile, identifier: &str) {
    file.linear = Some(identifier.to_string());
    file.linear_state = Some(LinearState {
        issue_id: format!("uuid-{identifier}"),
        team_id: "team".to_string(),
        url: format!("https://linear.app/acme/issue/{identifier}"),
        ..LinearState::default()
    });
}

fn applied(file: &mut TaskFile, number: u32) {
    file.proposals.push(ProposalRecord {
        number,
        state: ProposalState::Applied,
        at: "2026-11-09T10:00:00Z".parse().unwrap(),
        by: "kyle".to_string(),
        hash: format!("hash-{number}"),
        extra: Extra::new(),
    });
}

fn put(dir: &Path, file: &TaskFile) {
    write(&layout::task_file(dir), &serialize_task(file).unwrap());
}

/// Project `p`: the leaves `alpha` (l1, unlinked), `beta` (l2, synced to SEQ-5) and `hand` (l4,
/// a hand-set SEQ-9), the group `phase` (g1, unlinked) with an applied proposal of its own and
/// one on its leaf `inner` (l3), and the group `stream` (g2, synced to SEQ-100) with the leaf
/// `sub` (l5).
fn seed(root: &Path) {
    let project = layout::project_dir(root, "p");
    write(
        &layout::project_file(&project),
        &serialize_project(&ProjectFile::new("P")).unwrap(),
    );
    put(&project, &group_file("top", "P"));
    put(
        &layout::child_dir(&project, "alpha"),
        &leaf_file("l1", "Alpha"),
    );
    let mut beta = leaf_file("l2", "Beta");
    synced(&mut beta, "SEQ-5");
    put(&layout::child_dir(&project, "beta"), &beta);
    let mut hand = leaf_file("l4", "Hand");
    hand.linear = Some("SEQ-9".to_string());
    put(&layout::child_dir(&project, "hand"), &hand);

    let phase = layout::child_dir(&project, "phase");
    let mut group = group_file("g1", "Phase");
    applied(&mut group, 1);
    put(&phase, &group);
    let mut inner = leaf_file("l3", "Inner");
    applied(&mut inner, 2);
    put(&layout::child_dir(&phase, "inner"), &inner);

    let stream = layout::child_dir(&project, "stream");
    let mut group = group_file("g2", "Stream");
    synced(&mut group, "SEQ-100");
    put(&stream, &group);
    put(&layout::child_dir(&stream, "sub"), &leaf_file("l5", "Sub"));
}

pub(crate) fn state(id: &str, kind: StateKind) -> WorkflowState {
    WorkflowState {
        id: id.to_string(),
        name: id.to_string(),
        kind,
        position: 1.0,
    }
}

fn issue(number: u32) -> Issue {
    Issue {
        id: format!("uuid-SEQ-{number}"),
        identifier: format!("SEQ-{number}"),
        title: format!("Ticket {number}"),
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
    fake: FakeLinear,
    app: TestAppContext,
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

/// A store over the seeded tree, a workspace with the sync's actions registered, a fake client
/// and, when `token`, a token (`SECRET`) in the environment. Tickets 5, 100, 101 and 121 exist.
async fn env(
    cx: &mut TestAppContext,
    linear_on: bool,
    token: bool,
) -> (Env, &mut VisualTestContext) {
    cx.executor().allow_parking();
    let app = cx.clone();
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    seed(&root);
    let fake = FakeLinear::new("acme");
    fake.add_team("team", "SEQ", vec![state("backlog", StateKind::Backlog)]);
    for number in [5, 100, 101, 121] {
        fake.add_issue(issue(number));
    }
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        cx.set_global(AppDatabase::test_new());
        app_state
    });
    cx.update(|cx| {
        bongos_work::init(
            Arc::new(RealFs::new(None, cx.background_executor().clone())),
            WorkDirs { root },
            cx,
        );
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
    let store = cx.update(|cx| WorkStore::global(cx).unwrap());
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    workspace.update_in(cx, |workspace, window, cx| {
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
    set_enabled_in(cx, linear_on);
    (
        Env {
            workspace,
            store,
            fake,
            app,
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

    fn file(&self, slug: &str, cx: &mut VisualTestContext) -> TaskFile {
        let tree = self.tree(cx);
        tree.tasks()
            .into_iter()
            .find(|task| task.slug == slug)
            .unwrap_or_else(|| panic!("no task {slug}"))
            .file
            .clone()
            .unwrap()
    }

    fn writes(&self, cx: &mut VisualTestContext) -> usize {
        self.store.read_with(cx, |store, _| store.stats().writes)
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

    fn requests_allowed(&self, cx: &mut VisualTestContext) -> u64 {
        let linear = cx.update(|_, cx| sync::global(cx).unwrap());
        linear.read_with(cx, |linear, cx| linear.snapshot(cx).requests_allowed)
    }

    /// Starts the link of the task `id` to `issue`, as the confirm does after Kyle answers yes,
    /// and waits for the write.
    async fn link(&self, id: &str, issue: &Issue, cx: &mut VisualTestContext) {
        let before = self.writes(cx);
        self.clear_toast(cx);
        let (id, issue) = (id.to_string(), issue.clone());
        self.workspace.update(cx, |workspace, cx| {
            link::finish_link(workspace, id, issue, cx)
        });
        cx.run_until_parked();
        wait_until(cx, "the link", |cx| {
            self.writes(cx) > before || self.last_toast(cx) != ""
        })
        .await;
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn link_writes_link_state_and_goal_in_one_write(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    let writes = env.writes(cx);
    env.link("l1", &issue(121), cx).await;
    assert_eq!(env.writes(cx), writes + 1, "one write");

    let file = env.file("alpha", cx);
    assert_eq!(file.linear.as_deref(), Some("SEQ-121"));
    let state = file.linear_state.expect("the link record");
    assert_eq!(state.issue_id, "uuid-SEQ-121");
    assert_eq!(state.team_id, "team");
    assert_eq!(state.url, "https://linear.app/acme/issue/SEQ-121");
    assert_eq!(state.description, "Description 121");
    assert!(state.offered.is_empty(), "a leaf seeds nothing");
    assert_eq!(file.goal, "Ticket 121", "the task takes Linear's title");
    let last = file.history.last().unwrap();
    assert_eq!(last.event, "linked to SEQ-121");
    assert_eq!(file.history.len(), 1, "one history entry");
    assert!(
        env.fake.calls().is_empty(),
        "writing the link reads nothing"
    );
    assert_eq!(env.last_toast(cx), "Linked to SEQ-121");
    // The other tasks are untouched.
    assert!(env.file("beta", cx).history.is_empty());
    assert_eq!(env.file("hand", cx).linear.as_deref(), Some("SEQ-9"));
    assert!(env.file("hand", cx).linear_state.is_none());
}

#[gpui::test]
async fn a_group_link_seeds_offered_with_the_applied_records(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    env.link("g1", &issue(120), cx).await;
    let file = env.file("phase", cx);
    let state = file.linear_state.expect("the link record");
    let mut offered: Vec<(String, u32, String)> = state
        .offered
        .iter()
        .map(|offered| (offered.root.clone(), offered.number, offered.hash.clone()))
        .collect();
    offered.sort();
    assert_eq!(
        offered,
        vec![
            ("g1".to_string(), 1, "hash-1".to_string()),
            ("l3".to_string(), 2, "hash-2".to_string()),
        ]
    );
    let tree = env.tree(cx);
    assert!(
        unoffered(&tree).iter().all(|offer| offer.group_id != "g1"),
        "nothing is offered for the records that existed at the link"
    );
}

#[gpui::test]
async fn a_ticket_linked_elsewhere_is_refused_with_no_write(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    let tree = env.tree(cx);
    let reason = link::check(&tree, "l1", &issue(5)).unwrap_err();
    assert!(reason.starts_with("SEQ-5 is linked to "), "{reason}");
    assert!(reason.contains("beta"), "{reason}");

    let writes = env.writes(cx);
    let before = env.file("alpha", cx);
    env.link("l1", &issue(5), cx).await;
    assert_eq!(env.writes(cx), writes, "nothing is written");
    assert_eq!(env.file("alpha", cx), before);
    assert!(env.last_toast(cx).starts_with("SEQ-5 is linked to "));
    // A hand-set link counts too.
    assert!(link::check(&tree, "l1", &issue(9)).is_err());
    // An unlinked ticket is fine, also for a group whose ticket is a subticket of a linked group's.
    assert!(link::check(&tree, "l1", &issue(121)).is_ok());
    let mut child = issue(101);
    child.parent = Some(issue(100).summary());
    assert!(link::check(&tree, "g1", &child).is_ok());
}

#[gpui::test]
async fn unlink_keeps_the_goal_and_sends_nothing(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    env.focus("l2", cx);
    let writes = env.writes(cx);
    dispatch("task::UnlinkFromLinear", cx);
    wait_until(cx, "the unlink", |cx| env.writes(cx) > writes).await;

    let file = env.file("beta", cx);
    assert_eq!(file.linear, None);
    assert_eq!(file.linear_state, None);
    assert_eq!(file.goal, "Beta", "the goal stays");
    assert_eq!(file.history.last().unwrap().event, "unlinked from SEQ-5");
    assert_eq!(env.writes(cx), writes + 1);
    assert!(env.fake.calls().is_empty());
    assert_eq!(env.requests_allowed(cx), 0);
}

#[gpui::test]
async fn open_sends_nothing_and_a_hand_set_link_toasts(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    env.focus("l4", cx);
    dispatch("task::OpenInLinear", cx);
    assert_eq!(env.app.opened_url(), None, "a hand-set link has no URL");
    assert_eq!(env.last_toast(cx), LINK_FIRST);
    assert_eq!(LINK_FIRST, "link the task first (task: link to linear)");
    assert!(toast_shown(&env.workspace, cx));

    env.clear_toast(cx);
    env.focus("l1", cx);
    dispatch("task::OpenInLinear", cx);
    assert_eq!(env.app.opened_url(), None);
    assert_eq!(env.last_toast(cx), LINK_FIRST);

    env.focus("l2", cx);
    dispatch("task::OpenInLinear", cx);
    assert_eq!(
        env.app.opened_url().as_deref(),
        Some("https://linear.app/acme/issue/SEQ-5")
    );

    assert!(env.fake.calls().is_empty());
    assert_eq!(env.requests_allowed(cx), 0);
}

#[gpui::test]
async fn unlinked_tasks_and_a_disabled_sync_send_no_request(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    // Unlink and open on an unlinked task and on a hand-set link: refused, nothing sent, nothing
    // written.
    let writes = env.writes(cx);
    for id in ["l1", "l4"] {
        env.focus(id, cx);
        for name in ["task::UnlinkFromLinear", "task::OpenInLinear"] {
            dispatch(name, cx);
        }
    }
    assert_eq!(env.writes(cx), writes);
    assert!(env.fake.calls().is_empty());
    assert_eq!(env.requests_allowed(cx), 0);

    // While off, all three only say so.
    set_enabled_in(cx, false);
    env.focus("l2", cx);
    for name in [
        "task::LinkToLinear",
        "task::UnlinkFromLinear",
        "task::OpenInLinear",
    ] {
        env.clear_toast(cx);
        dispatch(name, cx);
        assert_eq!(env.last_toast(cx), crate::actions::OFF_MESSAGE, "{name}");
    }
    assert_eq!(env.writes(cx), writes);
    assert_eq!(env.app.opened_url(), None);
    assert!(env.fake.calls().is_empty());
    assert_eq!(env.requests_allowed(cx), 0);
    assert!(env.file("beta", cx).linear_state.is_some());
}

#[gpui::test]
async fn link_without_a_token_toasts_and_sends_nothing(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, false).await;
    env.focus("l1", cx);
    dispatch("task::LinkToLinear", cx);
    assert_eq!(env.last_toast(cx), NO_TOKEN_MESSAGE);
    assert!(env.fake.calls().is_empty());
    assert_eq!(env.requests_allowed(cx), 0);
}

#[gpui::test]
async fn link_reads_the_recent_tickets_and_a_linked_groups_ticket(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    // `alpha` has no linked ancestor: only the recent tickets are read.
    env.focus("l1", cx);
    dispatch("task::LinkToLinear", cx);
    wait_until(cx, "the recent read", |_| !env.fake.calls().is_empty()).await;
    cx.run_until_parked();
    assert_eq!(env.fake.calls(), vec![Call::Recent(20)]);
    assert_eq!(env.requests_allowed(cx), 1);
    assert_eq!(env.writes(cx), 0, "the picker writes nothing");

    // `sub` is under the group linked to SEQ-100: its ticket is read too.
    env.fake.clear_calls();
    env.focus("l5", cx);
    dispatch("task::LinkToLinear", cx);
    wait_until(cx, "the reads", |_| env.fake.calls().len() == 2).await;
    assert_eq!(
        env.fake.calls(),
        vec![Call::Recent(20), Call::Issues(vec!["SEQ-100".to_string()])]
    );

    // A task that is already linked is refused before anything is read.
    env.fake.clear_calls();
    env.clear_toast(cx);
    env.focus("l2", cx);
    dispatch("task::LinkToLinear", cx);
    assert_eq!(
        env.last_toast(cx),
        "beta is already linked to SEQ-5; unlink it first"
    );
    assert!(env.fake.calls().is_empty());
}

#[gpui::test]
async fn the_token_is_in_no_toast(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    let mut toasts = Vec::new();
    // A failed read shows Linear's error.
    env.fake.fail_next(ApiError::Unauthorized);
    env.focus("l1", cx);
    dispatch("task::LinkToLinear", cx);
    wait_until(cx, "the failed read", |cx| env.last_toast(cx) != "").await;
    toasts.push(env.last_toast(cx));
    assert!(toasts[0].starts_with("Could not list recent Linear tickets"));
    for (name, id) in [
        ("task::UnlinkFromLinear", "l1"),
        ("task::OpenInLinear", "l4"),
        ("task::LinkToLinear", "l2"),
    ] {
        env.clear_toast(cx);
        env.focus(id, cx);
        dispatch(name, cx);
        toasts.push(env.last_toast(cx));
    }
    env.clear_toast(cx);
    env.link("l1", &issue(5), cx).await;
    toasts.push(env.last_toast(cx));
    env.clear_toast(cx);
    env.link("l1", &issue(121), cx).await;
    toasts.push(env.last_toast(cx));
    for toast in &toasts {
        assert!(
            !toast.contains(SECRET) && !toast.contains("lin_api"),
            "{toast}"
        );
    }
    // Nor in the dump or the state of the sync.
    let linear = cx.update(|_, cx| sync::global(cx).unwrap());
    let snapshot = linear.read_with(cx, |linear, cx| linear.snapshot(cx));
    assert!(!crate::dump::render(&snapshot).contains(SECRET));
    assert!(
        snapshot
            .reads
            .iter()
            .all(|(_, state)| *state != ReadState::NotFound)
    );
}

fn summary(number: u32) -> IssueSummary {
    issue(number).summary()
}

#[gpui::test]
async fn the_picker_lists_subtickets_then_recent_tickets(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true, true).await;
    let tree = env.tree(cx);
    // SEQ-5 is linked to `beta`; SEQ-101 is a subticket, SEQ-102 too, and 101 is also recent.
    let subtickets = [summary(5), summary(101), summary(102)];
    let recent = [summary(101), summary(5), summary(121)];
    let rows = link::choices(&tree, "l1", &subtickets, &recent);
    let labels: Vec<&str> = rows.iter().map(|(_, label)| label.as_str()).collect();
    assert_eq!(labels[0], TYPED_ROW);
    assert!(rows[0].0 == Choice::Typed);
    let identifiers: Vec<&str> = rows
        .iter()
        .filter_map(|(choice, _)| match choice {
            Choice::Ticket(ticket) => Some(ticket.identifier.as_str()),
            Choice::Typed => None,
        })
        .collect();
    // The linked subticket is left out of the subtickets; a recent ticket is listed once.
    assert_eq!(identifiers, vec!["SEQ-101", "SEQ-102", "SEQ-5", "SEQ-121"]);
    assert!(labels[1].contains("subticket"));
    assert!(!labels[3].contains("subticket"));
}

#[test]
fn a_typed_ticket_is_an_identifier_or_a_url() {
    let seq = || IssueRef::Identifier(parse_identifier("SEQ-121").unwrap());
    for text in [
        "SEQ-121",
        "seq-121",
        " SEQ-121 ",
        "https://linear.app/acme/issue/SEQ-121/proration-engine",
    ] {
        assert_eq!(link::parse_typed(text), Some(seq()), "{text}");
    }
    assert_eq!(link::parse_typed("proration"), None);
}

#[test]
fn the_confirm_names_the_ticket_and_what_happens() {
    let prompt = link::confirm_prompt(
        "Billing",
        &Issue {
            title: "Proration engine".into(),
            ..issue(121)
        },
    );
    assert!(prompt.starts_with("Link Billing to SEQ-121 'Proration engine'? "));
    assert!(prompt.contains("move SEQ-121 to started when its plan begins"));
    assert!(prompt.contains("ask before cancelling it"));
}
