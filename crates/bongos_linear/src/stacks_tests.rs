//! Tests of the stacked-leaf notices and chip: the toast once per task per session, the open
//! parent PR warning, the chip, and that nothing is said before the first look or while the sync
//! is off. The tree fixture writes task files to a temporary directory and loads them.

use bongos_work::RowChipsRegistry;
use bongos_work_model::commands::default_todos;
use bongos_work_model::store::{WorkTree, load};
use bongos_work_model::{
    Kind, LinearState, ProjectFile, ResultKind, StackedOn, TaskFile, TaskResult, TodoRole,
    UnblockPoint, layout, serialize_project, serialize_task,
};
use gpui::TestAppContext;
use tempfile::TempDir;

use crate::link_tests::wait_until;
use crate::stacks::{CHIP, Notices, chip_for, stacked_notice};
use crate::status_tests::{add, begin, settle, start};

/// A leaf of the fixture. Its id is `id-<slug>`.
#[derive(Clone)]
pub(crate) struct Spec {
    slug: &'static str,
    goal: &'static str,
    identifier: Option<&'static str>,
    pending: bool,
    stacked_on: Option<&'static str>,
    begun: bool,
    pr: Option<&'static str>,
}

impl Spec {
    pub(crate) fn leaf(slug: &'static str, goal: &'static str) -> Self {
        Self {
            slug,
            goal,
            identifier: None,
            pending: false,
            stacked_on: None,
            begun: false,
            pr: None,
        }
    }

    pub(crate) fn linked(mut self, identifier: &'static str) -> Self {
        self.identifier = Some(identifier);
        self
    }

    pub(crate) fn pending(mut self) -> Self {
        self.pending = true;
        self
    }

    pub(crate) fn stacked_on(mut self, slug: &'static str) -> Self {
        self.stacked_on = Some(slug);
        self
    }

    pub(crate) fn begun(mut self) -> Self {
        self.begun = true;
        self
    }

    pub(crate) fn pr(mut self, url: &'static str) -> Self {
        self.pr = Some(url);
        self
    }

    fn file(&self) -> TaskFile {
        let mut file = TaskFile::new(self.goal, Kind::PrShip);
        file.goal = self.goal.to_string();
        file.id = id(self.slug);
        file.todos = default_todos(Kind::PrShip);
        if let Some(identifier) = self.identifier {
            file.linear = Some(identifier.to_string());
            file.linear_state = Some(LinearState {
                issue_id: format!("uuid-{identifier}"),
                team_id: "team".to_string(),
                url: format!("https://linear.app/acme/issue/{identifier}"),
                ..LinearState::default()
            });
        }
        if self.pending {
            file.linear_state = Some(LinearState {
                pending_create: Some("0196f1c2-0000-4000-8000-000000000000".to_string()),
                ..LinearState::default()
            });
        }
        file.stacked_on = self.stacked_on.map(|task| StackedOn {
            task: task.to_string(),
            unblock: UnblockPoint::Merged,
            extra: Default::default(),
        });
        if self.begun {
            let index = file
                .todos
                .iter()
                .position(|todo| todo.role == Some(TodoRole::Plans))
                .unwrap_or(0);
            file.todos[index].started = Some("2026-11-09T11:00:00Z".parse().unwrap());
        }
        file.result = self.pr.map(|url| TaskResult {
            kind: ResultKind::Pr,
            url: Some(url.to_string()),
            state: None,
            urls: Vec::new(),
            extra: Default::default(),
        });
        file
    }
}

pub(crate) fn id(slug: &str) -> String {
    format!("id-{slug}")
}

/// The tree of project `p` with `specs` as the leaves of its top group; the directory goes away
/// with the returned guard.
pub(crate) fn tree(specs: &[Spec]) -> (TempDir, WorkTree) {
    let temp = TempDir::new().unwrap();
    let project = layout::project_dir(temp.path(), "p");
    let write = |dir: &std::path::Path, text: String| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(layout::task_file(dir), text).unwrap();
    };
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        layout::project_file(&project),
        serialize_project(&ProjectFile::new("P")).unwrap(),
    )
    .unwrap();
    let mut top = TaskFile::new("P", Kind::PrShip);
    top.id = "top".to_string();
    write(&project, serialize_task(&top).unwrap());
    for spec in specs {
        write(
            &layout::child_dir(&project, spec.slug),
            serialize_task(&spec.file()).unwrap(),
        );
    }
    let tree = load(temp.path());
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    (temp, tree)
}

fn base() -> Vec<Spec> {
    vec![
        Spec::leaf("parent", "Parent").linked("SEQ-121"),
        Spec::leaf("child", "Child").linked("SEQ-123"),
    ]
}

fn stacked() -> Vec<Spec> {
    vec![
        Spec::leaf("parent", "Parent").linked("SEQ-121"),
        Spec::leaf("child", "Child")
            .linked("SEQ-123")
            .stacked_on("parent"),
    ]
}

#[test]
fn a_linked_stacked_leaf_has_the_chip() {
    let (_dir, tree) = tree(&[
        Spec::leaf("parent", "Parent").linked("SEQ-121"),
        Spec::leaf("child", "Child")
            .linked("SEQ-123")
            .stacked_on("parent"),
        Spec::leaf("unlinked", "Unlinked").stacked_on("parent"),
        Spec::leaf("pending", "Pending")
            .pending()
            .stacked_on("parent"),
    ]);
    assert_eq!(chip_for(&tree, &id("child")), Some(CHIP));
    assert_eq!(CHIP, "stacked: check Linear rule");
    assert_eq!(chip_for(&tree, &id("parent")), None, "not stacked");
    assert_eq!(chip_for(&tree, &id("unlinked")), None);
    assert_eq!(chip_for(&tree, &id("pending")), None);
}

#[test]
fn the_stacked_notice_names_the_parent_branch_and_the_rule() {
    let (_dir, tree) = tree(&stacked());
    assert_eq!(
        stacked_notice(&tree, &id("child")).as_deref(),
        Some(
            "SEQ-123 is stacked on SEQ-121; its PR targets seq-121-parent. Linear may mark \
             SEQ-123 Done when that PR merges into the parent branch. Add a 'no action' rule in \
             Linear's GitHub settings for target branches matching `^[a-z]+-[0-9]+-`. Bongos \
             writes no status for stacked leaves."
        )
    );
    assert_eq!(stacked_notice(&tree, &id("parent")), None);
}

#[test]
fn the_stacked_toast_fires_once_per_task_per_session() {
    let (_a, before) = tree(&base());
    let (_b, after) = tree(&stacked());
    let (_c, begun) = tree(&[
        Spec::leaf("parent", "Parent").linked("SEQ-121"),
        Spec::leaf("child", "Child")
            .linked("SEQ-123")
            .stacked_on("parent")
            .begun(),
    ]);
    let mut notices = Notices::default();
    assert!(
        notices.look(&before, true).is_empty(),
        "the first look only notes"
    );
    let gained = notices.look(&after, true);
    assert_eq!(gained.len(), 1);
    assert!(
        gained[0].starts_with("SEQ-123 is stacked on SEQ-121; its PR targets seq-121-parent."),
        "{gained:?}"
    );
    assert!(notices.look(&after, true).is_empty(), "no change");
    assert!(
        notices.look(&begun, true).is_empty(),
        "the start of a task already told about says nothing"
    );
    // Stacked off and on again in the same session: still once.
    assert!(notices.look(&before, true).is_empty());
    assert!(notices.look(&after, true).is_empty());
}

#[test]
fn a_task_stacked_before_the_session_is_told_about_when_it_starts() {
    let (_a, stacked_tree) = tree(&stacked());
    let (_b, begun) = tree(&[
        Spec::leaf("parent", "Parent").linked("SEQ-121"),
        Spec::leaf("child", "Child")
            .linked("SEQ-123")
            .stacked_on("parent")
            .begun(),
    ]);
    let mut notices = Notices::default();
    assert!(notices.look(&stacked_tree, true).is_empty());
    let started = notices.look(&begun, true);
    assert_eq!(started.len(), 1, "{started:?}");
    assert!(started[0].contains("SEQ-123 is stacked on SEQ-121"));
    assert!(notices.look(&begun, true).is_empty());
}

#[test]
fn an_open_parent_pr_warns_when_a_leaf_is_stacked_on_it() {
    let (_a, before) = tree(&[
        Spec::leaf("parent", "Parent")
            .linked("SEQ-121")
            .pr("https://github.com/acme/app/pull/42"),
        Spec::leaf("child", "Child").linked("SEQ-123"),
    ]);
    let (_b, after) = tree(&[
        Spec::leaf("parent", "Parent")
            .linked("SEQ-121")
            .pr("https://github.com/acme/app/pull/42"),
        Spec::leaf("child", "Child")
            .linked("SEQ-123")
            .stacked_on("parent"),
    ]);
    let mut notices = Notices::default();
    notices.look(&before, true);
    let messages = notices.look(&after, true);
    assert_eq!(messages.len(), 1, "one toast: {messages:?}");
    assert!(
        messages[0].contains(
            "PR https://github.com/acme/app/pull/42 for SEQ-121 is already open; add \
             `Closes SEQ-123` to its description"
        ),
        "{messages:?}"
    );
    assert!(messages[0].contains("SEQ-123 is stacked on SEQ-121"));
}

#[test]
fn a_parent_without_a_pr_has_no_warning() {
    let (_a, before) = tree(&base());
    let (_b, after) = tree(&stacked());
    let mut notices = Notices::default();
    notices.look(&before, true);
    let messages = notices.look(&after, true);
    assert_eq!(messages.len(), 1);
    assert!(!messages[0].contains("already open"), "{messages:?}");
}

#[test]
fn nothing_is_said_while_off_and_the_past_is_not_announced_later() {
    let (_a, before) = tree(&base());
    let (_b, after) = tree(&stacked());
    let mut notices = Notices::default();
    notices.look(&before, false);
    assert!(notices.look(&after, false).is_empty());
    assert!(
        notices.look(&after, true).is_empty(),
        "turning the sync on does not announce an edge made while it was off"
    );
}

#[gpui::test]
async fn the_watcher_toasts_once_and_the_row_shows_the_chip(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    cx.update(|_, cx| crate::stacks::connect(cx));
    env.clear_toast(cx);
    // `beta` (l2) is SEQ-5; the new leaf is stacked on it from the start.
    add(&env, None, "stk", "l12", Some(6), true, Some("beta"), cx).await;
    wait_until(cx, "the stacked toast", |cx| {
        env.last_toast(cx).contains("SEQ-6 is stacked on SEQ-5")
    })
    .await;
    assert!(
        env.last_toast(cx).contains("seq-5-beta"),
        "{}",
        env.last_toast(cx)
    );
    let chips = cx.update(|_, cx| RowChipsRegistry::chips("l12", cx));
    assert_eq!(
        chips
            .iter()
            .map(|chip| chip.to_string())
            .collect::<Vec<_>>(),
        [CHIP]
    );
    let chips = cx.update(|_, cx| RowChipsRegistry::chips("l2", cx));
    assert!(chips.is_empty(), "the parent is not stacked");

    env.clear_toast(cx);
    begin(&env, "l12", cx).await;
    settle(cx).await;
    assert_eq!(
        env.last_toast(cx),
        "",
        "the start says nothing the second time"
    );
}
