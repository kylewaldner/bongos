//! Proposals written later and their automatic apply (D13), and the "needs you" source, over a
//! temporary work tree read by the real store and watched by its real watcher (the harness of the
//! view's tests). The chats a proposal names are given by [`TestChats`].

#![cfg(unix)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use bongos_builder_model::LiveSet;
use bongos_work::{LiveModes, LiveModesRegistry, LiveModesSource, WorkStore};
use gpui::{App, TestAppContext, UpdateGlobal as _, VisualTestContext};
use settings::SettingsStore;

use crate::actions::LastToast;
use crate::auto_apply::toggled_message;
use crate::later::{ChatOwner, FailAutoApply, Later, Status, TestChats, agent_line, finish_apply};
use crate::propose::{is_untouched, prefilled};
use crate::view_tests::{
    Harness, dispatch, harness, set_settings_in, snapshot, task_toml, wait_for_in, write,
};

/// A new leaf under `billing`, by `by`.
fn helper(number: u32, by: &str, why: &str) -> String {
    format!(
        "proposal {number}\nfor b\n{by}\nwhy {why}\n\ntask billing: Billing | id b\n  \
         task helper: Rounding helper\n    result a helper in code\n"
    )
}

fn engine_dir(h: &Harness) -> PathBuf {
    h.billing.join("tasks/engine")
}

fn file_of(dir: &Path, number: u32) -> PathBuf {
    dir.join(format!("proposals/{number}.proposal.txt"))
}

/// The chats `(id, task, name)`.
fn set_chats(cx: &mut VisualTestContext, chats: &[(&str, &str, &str)]) {
    let chats = chats
        .iter()
        .map(|(id, task, name)| {
            (
                id.to_string(),
                ChatOwner {
                    task_id: task.to_string(),
                    name: name.to_string(),
                },
            )
        })
        .collect();
    cx.update(|_, cx| cx.set_global(TestChats(chats)));
}

fn attempts(cx: &mut VisualTestContext) -> usize {
    cx.update(|_, cx| cx.global::<Later>().attempts)
}

fn status(cx: &mut VisualTestContext, path: &Path) -> Option<Status> {
    cx.update(|_, cx| {
        cx.global::<Later>()
            .entries
            .get(path)
            .map(|entry| entry.status.clone())
    })
}

fn reasons(cx: &mut VisualTestContext, id: &str) -> Vec<String> {
    cx.update(|_, cx| {
        WorkStore::global(cx)
            .unwrap()
            .read(cx)
            .needs_you_reasons(id)
            .to_vec()
    })
}

fn needs_you(cx: &mut VisualTestContext, id: &str) -> usize {
    cx.update(|_, cx| {
        WorkStore::global(cx)
            .unwrap()
            .read(cx)
            .rollup(id)
            .map_or(0, |rollup| rollup.needs_you)
    })
}

fn last_toast(cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| {
        cx.try_global::<LastToast>()
            .map(|toast| toast.0.clone())
            .unwrap_or_default()
    })
}

/// The history details of `id` by the agent.
fn agent_history(cx: &mut VisualTestContext, id: &str) -> Vec<String> {
    snapshot(cx)
        .task_by_id(id)
        .and_then(|task| task.file.clone())
        .map(|file| {
            file.history
                .into_iter()
                .filter(|entry| entry.by == "agent")
                .filter_map(|entry| entry.detail)
                .collect()
        })
        .unwrap_or_default()
}

fn has_goal(cx: &mut VisualTestContext, goal: &str) -> bool {
    snapshot(cx)
        .tasks()
        .into_iter()
        .any(|task| task.goal() == goal)
}

/// Waits until the scan saw the file at `path` as it is on disk and nothing runs.
async fn settled(cx: &mut VisualTestContext, path: &Path) {
    let what = format!("the scan of {}", path.display());
    wait_for_in(cx, &what, |cx| {
        let mtime = std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok();
        cx.update(|_, cx| {
            let later = cx.global::<Later>();
            later.is_idle()
                && later
                    .entries
                    .get(path)
                    .is_some_and(|entry| Some(entry.mtime) == mtime)
        })
    })
    .await;
}

async fn wait_reason(cx: &mut VisualTestContext, id: &str, reason: &str) {
    wait_for_in(cx, reason, |cx| reasons(cx, id).iter().any(|r| r == reason)).await;
}

/// Chats open on the tasks in the set, as the chats' live-modes source would report them.
#[derive(Clone, Default)]
struct FakeLive(Rc<RefCell<BTreeSet<String>>>);

impl LiveModesSource for FakeLive {
    fn live_modes(&self, task_id: &str, _: &App) -> LiveModes {
        LiveModes {
            open_chats: usize::from(self.0.borrow().contains(task_id)),
            run: None,
        }
    }
}

fn set_global_auto_apply(cx: &mut VisualTestContext, on: bool) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_builder.get_or_insert_default().auto_apply = Some(on);
            });
        });
    });
}

async fn toggle(cx: &mut VisualTestContext, on: bool) {
    let expected = toggled_message("acme", on);
    dispatch(cx, "task::ToggleAutoApply");
    wait_for_in(cx, &expected, |cx| last_toast(cx) == expected).await;
}

#[test]
fn the_agent_line_names_the_chat_and_the_why() {
    assert_eq!(
        agent_line(
            2,
            "proposal 2 applied: added 1 task",
            "helper chat",
            Some(" more ")
        ),
        "agent applied proposal 2: added 1 task (chat helper chat): more"
    );
    assert_eq!(
        agent_line(3, "proposal 3 applied: added 1 task", "c", Some("")),
        "agent applied proposal 3: added 1 task (chat c)"
    );
}

#[gpui::test]
async fn an_in_subtree_agent_proposal_applies_once_and_says_so(cx: &mut TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[]).await;
    set_chats(cx, &[("c1", "b", "helper chat")]);
    let path = h.file(2);
    let text = helper(2, "by chat c1", "rounding needs a helper");
    write(&path, &text);
    wait_for_in(cx, "the auto-apply toast", |cx| {
        last_toast(cx).starts_with("agent applied proposal 2: ")
    })
    .await;
    let toast = last_toast(cx);
    assert!(
        toast.ends_with("(chat helper chat): rounding needs a helper"),
        "{toast}"
    );
    wait_for_in(cx, "the new task", |cx| has_goal(cx, "Rounding helper")).await;
    let history = agent_history(cx, "b");
    assert_eq!(history.len(), 1, "{history:?}");
    assert!(history[0].starts_with(&toast), "{history:?}");
    settled(cx, &path).await;
    assert_eq!(status(cx, &path), Some(Status::Settled));
    assert!(reasons(cx, "b").is_empty(), "{:?}", reasons(cx, "b"));
    assert_eq!(attempts(cx), 1);

    // The same bytes written again change the mtime only: nothing is applied again.
    write(&path, &text);
    settled(cx, &path).await;
    // The builder's own write (task.toml and project.toml) does not trigger anything either.
    toggle(cx, false).await;
    settled(cx, &path).await;
    assert_eq!(attempts(cx), 1);
    assert_eq!(agent_history(cx, "b").len(), 1);
    let helpers = snapshot(cx)
        .tasks()
        .into_iter()
        .filter(|task| task.goal() == "Rounding helper")
        .count();
    assert_eq!(helpers, 1);
}

#[gpui::test]
async fn proposals_by_others_or_outside_the_subtree_wait_on_every_ancestor(
    cx: &mut TestAppContext,
) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[]).await;
    set_chats(
        cx,
        &[("cs", "s", "schema chat"), ("cp", "p", "proration chat")],
    );
    let engine = engine_dir(&h);
    let rates = engine.join("tasks/rates");
    let proration = engine.join("tasks/proration");
    // `proration` is being planned.
    write(
        &proration.join("task.toml"),
        &task_toml(
            "p",
            "Proration",
            &[
                ("plan", Some("plans")),
                ("implement", Some("produces-branch")),
                ("ship", Some("ships")),
            ],
            "",
        ),
    );
    wait_for_in(cx, "the plan todo", |cx| {
        snapshot(cx)
            .task_by_id("p")
            .and_then(|task| task.file.clone())
            .is_some_and(|file| file.todos.len() == 3)
    })
    .await;

    // A chat on `schema` proposes for its sibling `engine`.
    write(
        &file_of(&engine, 1),
        "proposal 1\nfor e\nby chat cs\nwhy\n\ntask engine: Engine | id e\n  \
         task tiers: Tiers\n    result tiers in code\n",
    );
    // Kyle proposes for `billing`.
    write(&h.file(1), &helper(1, "by kyle", "mine"));
    // A workflow proposes for `rates`.
    write(
        &file_of(&rates, 1),
        "proposal 1\nfor r\nby workflow ship\nwhy\n\ntask rates: Rates | id r\n  \
         todo implement | role produces-branch\n  todo ship | role ships\n  todo extra\n",
    );
    // The plan chat of `proration` proposes for it.
    write(
        &file_of(&proration, 1),
        "proposal 1\nfor p\nby chat cp\nwhy\n\ntask proration: Proration | id p\n  \
         todo plan | role plans\n  todo implement | role produces-branch\n  \
         todo ship | role ships\n  todo extra\n",
    );
    for id in ["e", "b", "r", "p"] {
        wait_reason(cx, id, "proposal 1 pending").await;
    }
    for path in [
        file_of(&engine, 1),
        h.file(1),
        file_of(&rates, 1),
        file_of(&proration, 1),
    ] {
        settled(cx, &path).await;
        assert_eq!(status(cx, &path), Some(Status::Pending), "{path:?}");
    }
    assert_eq!(attempts(cx), 0);
    assert!(!has_goal(cx, "Tiers"));
    assert!(!has_goal(cx, "Rounding helper"));
    // Every ancestor counts them: `engine` its own and its two leaves', the top all four.
    assert_eq!(needs_you(cx, "r"), 1);
    assert_eq!(needs_you(cx, "e"), 3);
    assert_eq!(needs_you(cx, "b"), 4);
    assert_eq!(needs_you(cx, "top"), 4);
}

#[gpui::test]
async fn a_proposal_with_a_drop_or_a_kept_line_waits(cx: &mut TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[]).await;
    set_chats(cx, &[("c1", "b", "billing chat")]);
    let live = FakeLive::default();
    live.0.borrow_mut().insert("s".to_string());
    cx.update(|_, cx| LiveModesRegistry::add_source("test live", Rc::new(live.clone()), cx));
    cx.run_until_parked();

    write(
        &h.file(1),
        "proposal 1\nfor b\nby chat c1\nwhy\n\ntask billing: Billing | id b\n  \
         task engine: Engine | id e\n    task rates: Rates | id r | drop\n",
    );
    // Splitting `schema`, whose chat runs, keeps it as it is: a kept line.
    write(
        &h.file(2),
        "proposal 2\nfor b\nby chat c1\nwhy\n\ntask billing: Billing | id b\n  \
         task schema: Schema | id s\n    task cols: Columns | keeps work\n      \
         result a PR adding columns\n      todo implement | role produces-branch\n      \
         todo ship | role ships\n    task docs: Column docs\n      result a PR with the docs\n",
    );
    wait_reason(cx, "b", "proposal 1 pending").await;
    wait_reason(cx, "b", "proposal 2 pending").await;
    settled(cx, &h.file(1)).await;
    settled(cx, &h.file(2)).await;
    assert_eq!(attempts(cx), 0);
    assert!(!has_goal(cx, "Column docs"));
    let rates = snapshot(cx)
        .task_by_id("r")
        .and_then(|task| task.file.clone());
    assert_eq!(
        rates.map(|file| file.status),
        Some(bongos_work::model::Status::Open)
    );
}

#[gpui::test]
async fn the_setting_and_the_project_switch_make_every_proposal_wait(cx: &mut TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[]).await;
    set_chats(cx, &[("c1", "b", "billing chat")]);

    set_global_auto_apply(cx, false);
    write(&h.file(1), &helper(1, "by chat c1", "one"));
    wait_reason(cx, "b", "proposal 1 pending").await;

    // The setting back on, and the project's switch off (it wins).
    set_global_auto_apply(cx, true);
    toggle(cx, false).await;
    write(
        &h.file(2),
        &helper(2, "by chat c1", "two").replace("helper: Rounding helper", "other: Other"),
    );
    wait_reason(cx, "b", "proposal 2 pending").await;
    settled(cx, &h.file(1)).await;
    settled(cx, &h.file(2)).await;
    // Proposal 1 waited: turning the setting on later does not apply it.
    assert_eq!(attempts(cx), 0);
    assert!(!has_goal(cx, "Rounding helper"));
    assert!(!has_goal(cx, "Other"));
    assert_eq!(
        reasons(cx, "b"),
        [
            "proposal 1 pending".to_string(),
            "proposal 2 pending".to_string()
        ]
    );
    // The roll-up counts tasks: `billing` once on every ancestor.
    assert_eq!(needs_you(cx, "top"), 1);

    // While the builder is off nothing is classified or reported.
    set_settings_in(cx, false);
    wait_for_in(cx, "no needs you", |cx| needs_you(cx, "top") == 0).await;
    write(&h.file(3), &helper(3, "by chat c1", "three"));
    cx.run_until_parked();
    assert_eq!(needs_you(cx, "top"), 0);
    assert_eq!(attempts(cx), 0);
}

#[gpui::test]
async fn a_failed_auto_apply_is_not_retried_until_the_file_changes(cx: &mut TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[]).await;
    set_chats(cx, &[("c1", "b", "billing chat")]);
    cx.update(|_, cx| cx.set_global(FailAutoApply(Some("the disk said no".to_string()))));
    let path = h.file(1);
    let text = helper(1, "by chat c1", "first");
    write(&path, &text);
    wait_reason(cx, "b", "proposal 1 pending: the disk said no").await;
    settled(cx, &path).await;
    assert_eq!(attempts(cx), 1);

    // The failure is gone, but neither the same bytes nor a tree change retries it.
    cx.update(|_, cx| cx.set_global(FailAutoApply(None)));
    write(&path, &text);
    settled(cx, &path).await;
    toggle(cx, false).await;
    toggle(cx, true).await;
    settled(cx, &path).await;
    assert_eq!(attempts(cx), 1);
    assert_eq!(
        reasons(cx, "b"),
        ["proposal 1 pending: the disk said no".to_string()]
    );
    assert!(!has_goal(cx, "Rounding helper"));

    // An edit is judged again.
    write(&path, &helper(1, "by chat c1", "second"));
    wait_for_in(cx, "the auto-apply", |cx| has_goal(cx, "Rounding helper")).await;
    settled(cx, &path).await;
    assert_eq!(attempts(cx), 2);
    assert!(reasons(cx, "b").is_empty());
    let toast = last_toast(cx);
    assert!(
        toast.starts_with("agent applied proposal 1: ")
            && toast.ends_with("(chat billing chat): second"),
        "{toast}"
    );
}

#[gpui::test]
async fn a_refused_second_run_on_applied_content_is_settled_not_failed(cx: &mut TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[]).await;
    set_chats(cx, &[("c1", "b", "helper chat")]);
    let path = h.file(2);
    write(&path, &helper(2, "by chat c1", "rounding needs a helper"));
    wait_for_in(cx, "the new task", |cx| has_goal(cx, "Rounding helper")).await;
    settled(cx, &path).await;
    assert_eq!(status(cx, &path), Some(Status::Settled));

    // The file was queued twice: the second run is refused, and the tree records the hash.
    let refused = "not applied on its own: it is not pending".to_string();
    cx.update(|_, cx| finish_apply(path.clone(), Err(refused.clone()), cx));
    assert_eq!(status(cx, &path), Some(Status::Settled));

    // A refusal on content the tree does not record is still a failure.
    cx.update(|_, cx| {
        cx.global_mut::<Later>()
            .judged
            .insert(path.clone(), "another hash".to_string());
        finish_apply(path.clone(), Err(refused.clone()), cx);
    });
    assert_eq!(status(cx, &path), Some(Status::Failed(refused)));
}

#[gpui::test]
async fn an_untouched_pre_filled_file_raises_nothing(cx: &mut TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[]).await;
    set_chats(cx, &[("c1", "b", "billing chat")]);
    let tree = snapshot(cx);
    let printed = bongos_builder_model::print(&tree, "b", &LiveSet::default()).unwrap();
    let text = prefilled(&printed, 1, "by chat c1");
    assert!(is_untouched(&text, &tree, &LiveSet::default()));
    write(&h.file(1), &text);
    settled(cx, &h.file(1)).await;
    assert_eq!(status(cx, &h.file(1)), Some(Status::Untouched));
    assert_eq!(needs_you(cx, "top"), 0);
    assert_eq!(attempts(cx), 0);
}
