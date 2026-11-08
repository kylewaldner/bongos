//! The top level of the Work panel: freshness order, fading, the Stale and Closed sections, the
//! filters, what survives a new panel, and the graph.

use bongos_work_model::{Edge, Kind, UnblockPoint, layout, serialize_task};
use chrono::TimeDelta;
use db::AppDatabase;
use db::kvp::KeyValueStore;
use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};

use crate::command_tests::{Harness, Input, dispatch, group_toml, harness, leaf_toml, wait_until};
use crate::filters::{Filters, Saved, ViewMode};
use crate::focus::KVP_NAMESPACE;
use crate::panel::WorkPanel;
use crate::panel_tests::{breadcrumb, has, lines, next_row, open_panel, press};
use crate::store_tests::{project_toml, write};
use crate::view::Key;
use crate::{Focus, focus};

/// A second project `other` with one leaf, written after the seed so it is the fresher one.
async fn with_other_project(harness: &Harness, cx: &mut VisualTestContext) {
    let project = layout::project_dir(&harness.root, "other");
    write(&layout::project_file(&project), &project_toml("Other"));
    write(&layout::task_file(&project), &group_toml("g5", "Other"));
    let leaf = layout::child_dir(&project, "chore");
    write(
        &layout::task_file(&leaf),
        &leaf_toml("l5", "Chore", Kind::PrShip),
    );
    wait_until(cx, "the store to see the other project", |cx| {
        harness.tree(cx).task_by_id("l5").is_some()
    })
    .await;
}

fn skew(panel: &Entity<WorkPanel>, by: TimeDelta, cx: &mut VisualTestContext) {
    panel.update(cx, |panel, cx| panel.skew_clock(by, cx));
}

fn faded(panel: &Entity<WorkPanel>, cx: &mut VisualTestContext) -> Vec<String> {
    panel.read_with(cx, |panel, _| panel.faded_titles())
}

/// The index of the first line that starts with `text`.
fn at(lines: &[String], text: &str) -> usize {
    lines
        .iter()
        .position(|line| line.starts_with(text))
        .unwrap_or_else(|| panic!("no line starts with {text:?}: {lines:?}"))
}

#[gpui::test]
async fn a_quiet_task_fades_in_place_then_goes_stale_at_the_top_level(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    with_other_project(&harness, cx).await;
    let panel = open_panel(&harness, cx).await;

    // Fresh: the newest first, nothing dimmed.
    let top = lines(&panel, cx);
    assert!(at(&top, "Other") < at(&top, "Invoicing"), "{top:?}");
    assert!(faded(&panel, cx).is_empty());
    skew(&panel, TimeDelta::hours(1), cx);
    assert!(faded(&panel, cx).is_empty());

    // 1.5 days idle: both fade and keep their places.
    skew(&panel, TimeDelta::hours(36) + TimeDelta::minutes(1), cx);
    let top = lines(&panel, cx);
    assert!(at(&top, "Other") < at(&top, "Invoicing"), "{top:?}");
    assert_eq!(faded(&panel, cx), ["Other", "Invoicing"]);
    assert!(!has(&top, "Stale"), "{top:?}");

    // 3 days idle: both move to the folded Stale section.
    skew(&panel, TimeDelta::days(3) + TimeDelta::minutes(1), cx);
    let top = lines(&panel, cx);
    assert_eq!(top, ["Stale (2)  folded"]);
    dispatch(cx, "work::ShowStale");
    let top = lines(&panel, cx);
    assert!(at(&top, "Stale (2)") < at(&top, "Other"), "{top:?}");
    assert!(at(&top, "Other") < at(&top, "Invoicing"), "{top:?}");
    assert!(!has(&top, "folded"), "{top:?}");

    // Inside a group nothing moves: a quiet task only fades.
    next_row(cx);
    next_row(cx);
    press(cx, menu::Confirm);
    assert_eq!(breadcrumb(&panel, cx), "Work > Invoicing");
    let level = lines(&panel, cx);
    assert!(at(&level, "Backfill") < at(&level, "Phase"), "{level:?}");
    assert!(!has(&level, "Stale"), "{level:?}");
    assert_eq!(faded(&panel, cx), ["Backfill", "Phase"]);
    // The way back folds it again.
    dispatch(cx, "work::Up");
    dispatch(cx, "work::ShowStale");
    assert_eq!(lines(&panel, cx), ["Stale (2)  folded"]);
}

#[gpui::test]
async fn creating_a_task_moves_it_and_its_ancestors_to_the_top(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    with_other_project(&harness, cx).await;
    let panel = open_panel(&harness, cx).await;
    let top = lines(&panel, cx);
    assert!(at(&top, "Other") < at(&top, "Invoicing"), "{top:?}");

    // A task two levels down brings its project to the top.
    harness
        .run_and_wait(
            cx,
            "task::NewChild",
            &[Input::Pick("phase"), Input::Text("Gamma")],
        )
        .await;
    wait_until(cx, "the project to move up", |cx| {
        let top = lines(&panel, cx);
        top.iter().position(|line| line.starts_with("Invoicing"))
            < top.iter().position(|line| line.starts_with("Other"))
    })
    .await;

    // A new one-off puts its activity above everything.
    harness
        .run_and_wait(cx, "task::New", &[Input::Text("Fresh idea")])
        .await;
    wait_until(cx, "the activity to appear", |cx| {
        has(&lines(&panel, cx), "general")
    })
    .await;
    let top = lines(&panel, cx);
    assert!(at(&top, "general") < at(&top, "Invoicing"), "{top:?}");
    assert!(at(&top, "Invoicing") < at(&top, "Other"), "{top:?}");
    assert!(has(&top, "one-offs"), "{top:?}");
}

#[gpui::test]
async fn closed_is_folded_and_one_keypress_away(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .run_and_wait(cx, "task::New", &[Input::Text("Spec")])
        .await;
    harness
        .run_and_wait(cx, "task::Close", &[Input::Pick("Spec")])
        .await;
    let panel = open_panel(&harness, cx).await;
    wait_until(cx, "the closed section", |cx| {
        has(&lines(&panel, cx), "Closed (1)")
    })
    .await;
    let top = lines(&panel, cx);
    assert!(
        has(&top, "Closed (1)  folded") && !has(&top, "Spec"),
        "{top:?}"
    );

    // One action opens it, and the same one folds it.
    dispatch(cx, "work::ShowClosed");
    let top = lines(&panel, cx);
    assert!(has(&top, "Spec") && !has(&top, "folded"), "{top:?}");
    assert!(at(&top, "Invoicing") < at(&top, "Closed"), "{top:?}");
    dispatch(cx, "work::ShowClosed");
    assert!(!has(&lines(&panel, cx), "Spec"));

    // RET on the heading does the same.
    next_row(cx);
    assert!(
        panel
            .read_with(cx, |panel, _| panel.view_text())
            .lines()
            .any(|line| line.starts_with('>') && line.contains("Closed"))
    );
    press(cx, menu::Confirm);
    assert!(has(&lines(&panel, cx), "Spec"));
}

#[gpui::test]
async fn the_filters_narrow_every_level(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .run_and_wait(cx, "task::New", &[Input::Text("Spec")])
        .await;
    let panel = open_panel(&harness, cx).await;
    wait_until(cx, "the one-off", |cx| has(&lines(&panel, cx), "general")).await;
    let top = lines(&panel, cx);
    assert!(has(&top, "Invoicing") && has(&top, "general"), "{top:?}");

    harness
        .run(cx, "work::FilterKind", &[Input::Pick("spike")])
        .await;
    let top = lines(&panel, cx);
    assert!(has(&top, "general") && !has(&top, "Invoicing"), "{top:?}");
    harness
        .run(cx, "work::FilterKind", &[Input::Pick("Any kind")])
        .await;
    assert!(has(&lines(&panel, cx), "Invoicing"));

    // The todo filter reaches down into groups: only the leaves with that current todo remain.
    harness
        .run(cx, "work::FilterTodo", &[Input::Pick("implement")])
        .await;
    let top = lines(&panel, cx);
    assert!(has(&top, "Invoicing") && !has(&top, "general"), "{top:?}");
    press(cx, menu::Confirm);
    let level = lines(&panel, cx);
    assert!(has(&level, "Backfill") && has(&level, "Phase"), "{level:?}");
    harness
        .run(cx, "work::FilterTodo", &[Input::Pick("Any todo")])
        .await;
    assert!(panel.read_with(cx, |panel, _| *panel.filters() == Filters::default()));
}

/// A new panel in the window, as another workspace would make one.
fn another_panel(harness: &Harness, cx: &mut VisualTestContext) -> Entity<WorkPanel> {
    harness.workspace.update_in(cx, |workspace, window, cx| {
        let weak = cx.entity().downgrade();
        let fs = workspace.app_state().fs.clone();
        cx.new(|cx| WorkPanel::new(weak, fs, window, cx))
    })
}

#[gpui::test]
async fn the_drill_path_and_filters_survive_a_new_panel(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let panel = open_panel(&harness, cx).await;
    press(cx, menu::Confirm);
    next_row(cx);
    press(cx, menu::Confirm);
    assert_eq!(breadcrumb(&panel, cx), "Work > Invoicing > Phase");
    dispatch(cx, "work::ToggleRow");
    harness
        .run(cx, "work::FilterKind", &[Input::Pick("pr-ship")])
        .await;
    harness
        .run(cx, "work::FilterTodo", &[Input::Pick("implement")])
        .await;
    dispatch(cx, "work::ShowStale");
    dispatch(cx, "work::ShowClosed");
    dispatch(cx, "work::Graph");
    let state = panel.read_with(cx, |panel, _| panel.state());
    assert_eq!(state.path.len(), 2);
    assert_eq!(state.expanded.len(), 1);
    assert_eq!(state.view, ViewMode::Graph);
    assert!(state.filters.show_stale && state.filters.show_closed);
    assert!(state.filters.todo.is_some() && state.filters.kind.is_some());

    // The values are written as they change.
    wait_until(cx, "the values to be saved", |cx| {
        cx.update(|_, cx| Saved::read(cx)) == state
    })
    .await;

    // A panel made later starts where the first one is.
    let second = another_panel(&harness, cx);
    wait_until(cx, "the second panel to show the tree", |cx| {
        second.read_with(cx, |panel, _| !panel.lines().is_empty())
    })
    .await;
    assert_eq!(breadcrumb(&second, cx), "Work > Invoicing > Phase");
    assert_eq!(second.read_with(cx, |panel, _| panel.state()), state);
    assert!(second.read_with(cx, |panel, _| panel.shows_graph()));
    assert!(has(&lines(&second, cx), "| alpha"));
}

#[gpui::test]
async fn ids_that_no_longer_exist_are_dropped_silently(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let db = cx.update(|_, cx| {
        cx.set_global(AppDatabase::test_new());
        KeyValueStore::global(cx)
    });
    let scoped = db.scoped(KVP_NAMESPACE);
    // The project is there; the group under it is not.
    scoped
        .write(
            "drill_path".to_string(),
            r#"["g1","gone","l2"]"#.to_string(),
        )
        .await
        .unwrap();
    scoped
        .write("expanded".to_string(), r#"["l1","gone"]"#.to_string())
        .await
        .unwrap();
    scoped
        .write("view".to_string(), r#""graph""#.to_string())
        .await
        .unwrap();
    let panel = open_panel(&harness, cx).await;
    assert_eq!(breadcrumb(&panel, cx), "Work > Invoicing");
    let state = panel.read_with(cx, |panel, _| panel.state());
    assert_eq!(state.path, [Key::Task("g1".to_string())]);
    assert_eq!(state.expanded, [Key::Task("l1".to_string())]);
    assert!(!harness.has_toast(cx));
    // What was dropped is written back.
    wait_until(cx, "the cleaned path to be saved", |cx| {
        cx.update(|_, cx| Saved::read(cx)).path == state.path
    })
    .await;
}

#[gpui::test]
async fn the_graph_draws_the_group_and_ret_on_a_node_goes_there(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    cx.update(|_, cx| {
        cx.set_global(AppDatabase::test_new());
        focus::init(cx);
    });
    // `phase` goes after `backfill`.
    let phase = harness.task_dir(&["phase"]);
    let mut file = bongos_work_model::read_task(&layout::task_file(&phase)).unwrap();
    file.after.push(Edge {
        task: "backfill".to_string(),
        unblock: UnblockPoint::Merged,
        extra: Default::default(),
    });
    write(&layout::task_file(&phase), &serialize_task(&file).unwrap());
    wait_until(cx, "the edge", |cx| {
        harness
            .tree(cx)
            .task_by_id("g2")
            .and_then(|task| task.file.as_ref().map(|file| !file.after.is_empty()))
            .unwrap_or(false)
    })
    .await;
    let panel = open_panel(&harness, cx).await;

    // The top level has no graph.
    harness.clear_toasts(cx);
    dispatch(cx, "work::Graph");
    assert!(harness.has_toast(cx));
    assert!(!panel.read_with(cx, |panel, _| panel.shows_graph()));

    press(cx, menu::Confirm);
    dispatch(cx, "work::Graph");
    assert!(panel.read_with(cx, |panel, _| panel.shows_graph()));
    let graph = lines(&panel, cx);
    assert!(has(&graph, "| backfill  Ready"), "{graph:?}");
    assert!(has(&graph, "| phase  Blocked"), "{graph:?}");
    assert!(has(&graph, "v  after (merged) from backfill"), "{graph:?}");
    assert!(graph.iter().any(|line| line.starts_with("+-")), "{graph:?}");
    assert!(!has(&graph, "Alpha"), "{graph:?}");
    let text = panel.read_with(cx, |panel, _| panel.view_text());
    assert!(
        text.lines().any(|line| line.starts_with(">| backfill")),
        "{text}"
    );

    // RET on a group's box goes into it, and the graph follows.
    next_row(cx);
    press(cx, menu::Confirm);
    assert_eq!(breadcrumb(&panel, cx), "Work > Invoicing > Phase");
    let inner = lines(&panel, cx);
    assert!(has(&inner, "| alpha") && has(&inner, "| beta"), "{inner:?}");

    // RET on a leaf's box focuses it.
    press(cx, menu::Confirm);
    wait_until(cx, "the focus", |cx| {
        cx.update(|_, cx| Focus::global(cx).unwrap().read(cx).task_id() == Some("l2"))
    })
    .await;

    // The same toggle lists the group again (focus moved the window to another workspace, which
    // has no commands registered, so the panel is asked directly).
    panel.update(cx, |panel, cx| panel.toggle_graph(cx));
    assert!(!panel.read_with(cx, |panel, _| panel.shows_graph()));
    assert!(has(&lines(&panel, cx), "Alpha"));
}

#[gpui::test]
async fn the_new_actions_refuse_with_a_toast_without_a_panel(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    for name in [
        "work::FilterTodo",
        "work::FilterKind",
        "work::ShowStale",
        "work::ShowClosed",
        "work::Graph",
    ] {
        harness.clear_toasts(cx);
        dispatch(cx, name);
        assert!(harness.has_toast(cx), "{name} showed no toast");
        assert!(harness.picker(cx).is_none(), "{name} opened a picker");
    }
}
