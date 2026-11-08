//! The Work panel in a window: one level at a time, drilling in and up with the keys, the rows
//! the snapshot and the registries produce, expansion, and the panel following `work.enabled`.

use std::rc::Rc;

use bongos_work_model::commands::default_todos;
use bongos_work_model::{Kind, TaskFile, Todo, TodoState, layout, serialize_task};
use db::AppDatabase;
use settings::Settings as _;
use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext};

use crate::artifact_tests::with_artifacts;
use crate::command_tests::{Harness, Input, dispatch, harness, wait_until};
use crate::panel::{WorkPanel, init_panel};
use crate::registry::NeedsYouRegistry;
use crate::registry_tests::{FakeGates, gate, with_deep_leaf};
use crate::store_tests::{set_enabled, write};
use crate::{Focus, focus};

pub(crate) fn panel_of(harness: &Harness, cx: &mut VisualTestContext) -> Option<Entity<WorkPanel>> {
    harness
        .workspace
        .read_with(cx, |workspace, cx| workspace.panel::<WorkPanel>(cx))
}

/// Adds the panel to the harness window the way the app does, and waits for it to show the tree.
pub(crate) async fn open_panel(harness: &Harness, cx: &mut VisualTestContext) -> Entity<WorkPanel> {
    // A database of its own, so what one test saves never reaches another.
    cx.update(|_, cx| {
        if !cx.has_global::<AppDatabase>() {
            cx.set_global(AppDatabase::test_new());
        }
    });
    harness.workspace.update_in(cx, |workspace, window, cx| {
        init_panel(workspace, window, cx)
    });
    cx.run_until_parked();
    let panel = panel_of(&harness, cx).expect("the panel is added while enabled");
    wait_until(cx, "the panel to show the tree", |cx| {
        panel.read_with(cx, |panel, _| !panel.lines().is_empty())
    })
    .await;
    harness.workspace.update_in(cx, |workspace, window, cx| {
        workspace.focus_panel::<WorkPanel>(window, cx)
    });
    panel
}

pub(crate) fn lines(panel: &Entity<WorkPanel>, cx: &mut VisualTestContext) -> Vec<String> {
    panel.read_with(cx, |panel, _| panel.lines())
}

pub(crate) fn breadcrumb(panel: &Entity<WorkPanel>, cx: &mut VisualTestContext) -> String {
    panel.read_with(cx, |panel, _| panel.breadcrumb())
}

pub(crate) fn has(lines: &[String], text: &str) -> bool {
    lines.iter().any(|line| line.contains(text))
}

pub(crate) fn press(cx: &mut VisualTestContext, action: impl gpui::Action) {
    cx.dispatch_action(action);
    cx.run_until_parked();
}

pub(crate) fn next_row(cx: &mut VisualTestContext) {
    press(cx, menu::SelectNext);
}

#[gpui::test]
async fn ret_drills_in_and_up_returns(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let panel = open_panel(&harness, cx).await;

    // The top level lists the project and nothing below it.
    assert_eq!(breadcrumb(&panel, cx), "Work");
    let top = lines(&panel, cx);
    assert!(has(&top, "Invoicing"), "{top:?}");
    assert!(
        !has(&top, "Backfill") && !has(&top, "Phase") && !has(&top, "Alpha"),
        "{top:?}"
    );

    // RET on the project shows its children, one level only.
    press(cx, menu::Confirm);
    assert_eq!(breadcrumb(&panel, cx), "Work > Invoicing");
    let level = lines(&panel, cx);
    assert!(has(&level, "Backfill") && has(&level, "Phase"), "{level:?}");
    assert!(!has(&level, "Alpha") && !has(&level, "Beta"), "{level:?}");
    // The group row has its goal, the roll-up and no leaf details.
    assert!(has(&level, "0/2"), "{level:?}");

    // RET on the group `phase` shows its two leaves and nothing of the level above.
    next_row(cx);
    press(cx, menu::Confirm);
    assert_eq!(breadcrumb(&panel, cx), "Work > Invoicing > Phase");
    let inner = lines(&panel, cx);
    assert!(has(&inner, "Alpha") && has(&inner, "Beta"), "{inner:?}");
    assert!(!has(&inner, "Backfill"), "{inner:?}");

    // `work: up` goes back with the group selected.
    dispatch(cx, "work::Up");
    assert_eq!(breadcrumb(&panel, cx), "Work > Invoicing");
    let text = panel.read_with(cx, |panel, _| panel.view_text());
    assert!(
        text.lines()
            .any(|line| line.starts_with('>') && line.contains("Phase")),
        "{text}"
    );
    dispatch(cx, "work::Up");
    assert_eq!(breadcrumb(&panel, cx), "Work");
    // At the top there is nowhere further up.
    dispatch(cx, "work::Up");
    assert_eq!(breadcrumb(&panel, cx), "Work");
}

#[gpui::test]
async fn a_gate_three_deep_shows_on_the_top_row_and_drills_to_the_task(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    with_deep_leaf(&harness, cx).await;
    let panel = open_panel(&harness, cx).await;
    let source = Rc::new(FakeGates::default());
    source
        .0
        .borrow_mut()
        .push(gate("l4", "gate: approve the plan"));
    cx.update(|_, cx| NeedsYouRegistry::add_source("gates", source.clone(), cx));
    cx.run_until_parked();

    // The project row, the phase row and the deep row each say so; the leaf says why.
    assert!(has(&lines(&panel, cx), "1 needs you"));
    press(cx, menu::Confirm);
    let level = lines(&panel, cx);
    assert!(
        has(&level, "Phase") && has(&level, "1 needs you"),
        "{level:?}"
    );
    next_row(cx);
    press(cx, menu::Confirm);
    let level = lines(&panel, cx);
    assert!(
        has(&level, "Deep") && has(&level, "1 needs you"),
        "{level:?}"
    );
    // Alpha, Beta, then Deep.
    next_row(cx);
    next_row(cx);
    press(cx, menu::Confirm);
    assert_eq!(breadcrumb(&panel, cx), "Work > Invoicing > Phase > Deep");
    let level = lines(&panel, cx);
    assert!(
        has(&level, "Gate") && has(&level, "needs you: gate: approve the plan"),
        "{level:?}"
    );

    // Removing the source clears it everywhere.
    cx.update(|_, cx| NeedsYouRegistry::remove_source("gates", cx));
    cx.run_until_parked();
    assert!(!has(&lines(&panel, cx), "needs you"));
}

/// Adds a leaf `solo` (`l9`) with one todo to the project and waits for the store to see it.
async fn with_solo_leaf(harness: &Harness, cx: &mut VisualTestContext) {
    let mut file = TaskFile::new("Solo", Kind::Document);
    file.id = "l9".to_string();
    file.todos = vec![Todo {
        name: "write".to_string(),
        role: None,
        workflow: None,
        state: TodoState::Open,
        started: None,
        extra: Default::default(),
    }];
    write(
        &layout::task_file(&harness.task_dir(&["solo"])),
        &serialize_task(&file).unwrap(),
    );
    wait_until(cx, "the store to see the solo leaf", |cx| {
        harness.tree(cx).task_by_id("l9").is_some()
    })
    .await;
}

#[gpui::test]
async fn expanding_a_row_shows_todos_only_when_there_is_more_than_one(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    with_solo_leaf(&harness, cx).await;
    assert!(default_todos(Kind::PrShip).len() > 1);
    let panel = open_panel(&harness, cx).await;
    press(cx, menu::Confirm);
    // Backfill, Phase, Solo: the leaf row has its current todo in brackets.
    let level = lines(&panel, cx);
    assert!(has(&level, "[write]"), "{level:?}");

    // Backfill has several todos: expanding lists them.
    dispatch(cx, "work::ToggleRow");
    let expanded = lines(&panel, cx);
    assert!(
        has(&expanded, "Todos") && has(&expanded, "[ ] "),
        "{expanded:?}"
    );
    dispatch(cx, "work::ToggleRow");
    assert!(!has(&lines(&panel, cx), "Todos"));

    // Solo has one: expanding it shows no list.
    next_row(cx);
    next_row(cx);
    let text = panel.read_with(cx, |panel, _| panel.view_text());
    assert!(
        text.lines()
            .any(|line| line.starts_with('>') && line.contains("Solo")),
        "{text}"
    );
    dispatch(cx, "work::ToggleRow");
    let solo = lines(&panel, cx);
    assert!(!has(&solo, "Todos"), "{solo:?}");
    assert!(!has(&solo, "[ ] write"), "{solo:?}");
}

#[gpui::test]
async fn expanding_shows_the_artifacts_the_lock_and_the_guarantee(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    with_artifacts(&harness, cx).await;
    harness
        .run_and_wait(
            cx,
            "artifact::Hide",
            &[Input::Pick("backfill"), Input::Pick("salary")],
        )
        .await;
    let panel = open_panel(&harness, cx).await;
    press(cx, menu::Confirm);
    let level = lines(&panel, cx);
    assert!(has(&level, "2 artifacts, 1 locked"), "{level:?}");

    dispatch(cx, "work::ToggleRow");
    let expanded = lines(&panel, cx);
    assert!(has(&expanded, "Artifacts (2)"), "{expanded:?}");
    assert!(
        has(&expanded, bongos_work_model::artifacts::GUARANTEE),
        "{expanded:?}"
    );
    assert!(has(&expanded, "salary-review.md  locked"), "{expanded:?}");
    assert!(has(&expanded, "design.md  shared"), "{expanded:?}");
    // No session is open, so the line about new sessions is not there.
    assert!(
        !has(&expanded, "Changes apply to new sessions"),
        "{expanded:?}"
    );
}

#[gpui::test]
async fn edge_markers_sit_between_siblings(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    // `phase` goes after `backfill`.
    let phase = harness.task_dir(&["phase"]);
    let mut file = bongos_work_model::read_task(&layout::task_file(&phase)).unwrap();
    file.after.push(bongos_work_model::Edge {
        task: "backfill".to_string(),
        unblock: bongos_work_model::UnblockPoint::Merged,
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
    press(cx, menu::Confirm);
    let level = lines(&panel, cx);
    let marker = level
        .iter()
        .position(|line| line.contains("after Backfill"))
        .unwrap();
    let backfill = level
        .iter()
        .position(|line| line.contains("Backfill  "))
        .unwrap();
    let phase = level
        .iter()
        .position(|line| line.contains("Phase"))
        .unwrap();
    assert!(backfill < marker && marker < phase, "{level:?}");
}

#[gpui::test]
async fn ret_on_a_leaf_focuses_it(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    cx.update(|_, cx| {
        cx.set_global(AppDatabase::test_new());
        focus::init(cx);
    });
    let panel = open_panel(&harness, cx).await;
    press(cx, menu::Confirm);
    // `backfill` is the first row of the project's level and a leaf.
    press(cx, menu::Confirm);
    wait_until(cx, "the focus", |cx| {
        cx.update(|_, cx| Focus::global(cx).unwrap().read(cx).task_id() == Some("l1"))
    })
    .await;
    // The panel stayed on the same level.
    assert_eq!(breadcrumb(&panel, cx), "Work > Invoicing");
}

#[gpui::test]
async fn the_panel_is_absent_while_disabled_and_follows_the_setting(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, false, true).await;
    harness.workspace.update_in(cx, |workspace, window, cx| {
        init_panel(workspace, window, cx)
    });
    cx.run_until_parked();
    assert!(panel_of(&harness, cx).is_none());

    set_enabled(cx, true);
    cx.run_until_parked();
    assert!(panel_of(&harness, cx).is_some(), "added when enabled");

    set_enabled(cx, false);
    cx.run_until_parked();
    assert!(panel_of(&harness, cx).is_none());
}

#[gpui::test]
fn work_is_enabled_by_default(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
        crate::WorkSettings::register(cx);
        assert!(crate::WorkSettings::get_global(cx).enabled);
    });
}

#[gpui::test]
async fn a_default_workspace_has_the_work_panel(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let app_state = cx.update(|cx| {
        let app_state = workspace::AppState::test(cx);
        crate::WorkSettings::register(cx);
        if !cx.has_global::<AppDatabase>() {
            cx.set_global(AppDatabase::test_new());
        }
        app_state
    });
    let project = project::Project::test(app_state.fs.clone(), [], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        workspace::MultiWorkspace::test_new(project.clone(), window, cx)
    });
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        init_panel(workspace, window, cx)
    });
    cx.run_until_parked();
    let panel = workspace.read_with(cx, |workspace, cx| workspace.panel::<WorkPanel>(cx));
    assert!(panel.is_some(), "no setting was written, yet the panel exists");
}

#[gpui::test]
async fn the_panel_actions_dispatch_by_name(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let panel = open_panel(&harness, cx).await;
    harness.workspace.update_in(cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx)
    });
    let focused = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| panel.focus_handle(cx).contains_focused(window, cx))
    };
    assert!(!focused(cx));
    dispatch(cx, "work::TogglePanel");
    assert!(focused(cx));
    assert!(!harness.has_toast(cx));
    dispatch(cx, "work::ToggleRow");
    dispatch(cx, "work::Up");
    assert!(!harness.has_toast(cx));
}

#[cfg(debug_assertions)]
#[gpui::test]
async fn the_view_dump_writes_the_rows_of_the_level(cx: &mut TestAppContext) {
    use crate::panel::{DUMP_FILE, write_view};

    let (harness, cx) = harness(cx, true, true).await;
    let panel = open_panel(&harness, cx).await;
    press(cx, menu::Confirm);
    let logs = tempfile::TempDir::new().unwrap();
    let text = panel.read_with(cx, |panel, _| panel.view_text());
    let path = write_view(&text, logs.path()).unwrap();
    assert_eq!(path, logs.path().join(DUMP_FILE));
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.starts_with("Work > Invoicing\n"), "{written}");
    assert!(
        written.contains("Backfill") && written.contains("Phase"),
        "{written}"
    );
    assert!(!written.contains("Alpha"), "{written}");
}
