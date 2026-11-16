//! The details of a task: the pure lines, the registry order, the tab opening once per task and
//! following the store, and the expanded row of the Work panel with a summary.

use std::rc::Rc;

use bongos_work_model::store::Task;
use bongos_work_model::{Edge, Kind, TaskFile, UnblockPoint, layout, serialize_task};
use gpui::{App, TestAppContext, VisualTestContext};

use crate::command_tests::{Harness, harness, wait_until};
use crate::detail::{
    DetailLine, LineKind, SectionFocus, SectionRow, SectionTone, TaskDetailView, TaskSection,
    TaskSectionSource, TaskSectionsRegistry, lines, plain, show_details, text_of,
};
use crate::panel_tests::{has, lines as panel_lines, next_row, open_panel, press};
use crate::store_tests::write;
use crate::task_detail;

fn text_has(lines: &[DetailLine], text: &str) -> bool {
    lines.iter().any(|line| line.text.contains(text))
}

struct Fake {
    title: &'static str,
    summary: Option<&'static str>,
    activated: std::cell::RefCell<Vec<String>>,
}

impl Fake {
    fn new(title: &'static str, summary: Option<&'static str>) -> Rc<Self> {
        Rc::new(Self {
            title,
            summary,
            activated: Default::default(),
        })
    }
}

impl TaskSectionSource for Fake {
    fn section(&self, task_id: &str, show_sources: bool, _: &App) -> Option<TaskSection> {
        (task_id == "l1").then(|| TaskSection {
            title: self.title.to_string(),
            header: format!("header of {} ({show_sources})", self.title),
            rows: vec![SectionRow {
                id: "row1".to_string(),
                text: format!("{} row", self.title),
                tone: SectionTone::Error,
                detail: Some("more".to_string()),
            }],
        })
    }

    fn summary(&self, task_id: &str, _: &App) -> Option<String> {
        self.summary.filter(|_| task_id == "l1").map(str::to_string)
    }

    fn activate(&self, _: &str, row_id: &str, _: &mut gpui::Window, _: &mut App) {
        self.activated.borrow_mut().push(row_id.to_string());
    }
}

fn backfill(harness: &Harness, cx: &mut VisualTestContext) -> Task {
    harness
        .tree(cx)
        .task_by_id("l1")
        .expect("seeded task")
        .clone()
}

#[gpui::test]
async fn lines_show_the_own_parts_with_no_source(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let mut task = backfill(&harness, cx);
    task.done_hold = Some("a check is missing".to_string());
    task.files.insert(
        "documents/plan.md".into(),
        std::time::SystemTime::UNIX_EPOCH,
    );
    let file = task.file.as_mut().unwrap();
    file.after.push(Edge {
        task: "phase".to_string(),
        unblock: UnblockPoint::Merged,
        extra: Default::default(),
    });
    task.broken_edges.push("ghost".to_string());
    let tree = harness.tree(cx);
    let out = lines(&task, &tree, &[]);
    for expected in [
        "Goal: Backfill",
        "Kind: pr-ship",
        "State: ",
        "held from Done: a check is missing",
        "documents/plan.md",
        "Todos (",
        "[ ] ",
        "Children (0)",
        "after phase",
        "broken edge: ghost",
    ] {
        assert!(text_has(&out, expected), "{expected} in {}", plain(&out));
    }
    assert!(!out.iter().any(|line| {
        matches!(
            line.kind,
            LineKind::SectionHeader(_) | LineKind::SectionRow { .. }
        )
    }));

    // A group lists its children, which open their own details.
    let project = harness.tree(cx).task_by_id("g1").unwrap().clone();
    let out = lines(&project, &tree, &[]);
    assert!(
        out.iter()
            .any(|line| line.kind == LineKind::Child("l1".to_string())),
        "{}",
        plain(&out)
    );
}

#[gpui::test]
async fn sections_sort_by_order_then_name(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let first = Fake::new("First", None);
    let second = Fake::new("Second", None);
    let third = Fake::new("Third", None);
    cx.update(|_, cx| {
        TaskSectionsRegistry::add_source("b", 5, second, cx);
        TaskSectionsRegistry::add_source("c", 5, third, cx);
        TaskSectionsRegistry::add_source("a", i32::MIN, first, cx);
    });
    let task = backfill(&harness, cx);
    let tree = harness.tree(cx);
    let sections = cx.update(|_, cx| TaskSectionsRegistry::sections("l1", false, cx));
    let out = lines(&task, &tree, &sections);
    let titles: Vec<&str> = out
        .iter()
        .filter(|line| matches!(line.kind, LineKind::SectionHeader(_)))
        .map(|line| line.text.as_str())
        .collect();
    assert_eq!(titles, ["First", "Second", "Third"]);
    let row = out
        .iter()
        .find(|line| matches!(line.kind, LineKind::SectionRow { .. }))
        .unwrap();
    assert_eq!(row.tone, SectionTone::Error);
}

fn views(harness: &Harness, cx: &mut VisualTestContext) -> Vec<gpui::Entity<TaskDetailView>> {
    harness.workspace.read_with(cx, |workspace, cx| {
        workspace.items_of_type::<TaskDetailView>(cx).collect()
    })
}

#[gpui::test]
async fn opening_twice_reuses_one_view_and_it_follows_the_store(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    for _ in 0..2 {
        harness.workspace.update_in(cx, |workspace, window, cx| {
            show_details("l1", None, workspace, window, cx)
        });
    }
    cx.run_until_parked();
    let open = views(&harness, cx);
    assert_eq!(open.len(), 1);
    let view = open[0].clone();
    assert!(view.read_with(cx, |view, _| text_has(view.lines(), "Goal: Backfill")));

    // A different task gets its own view.
    harness.workspace.update_in(cx, |workspace, window, cx| {
        show_details("l2", None, workspace, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(views(&harness, cx).len(), 2);

    // A store change rebuilds the lines.
    let mut file = TaskFile::new("Backfill renamed", Kind::PrShip);
    file.id = "l1".to_string();
    file.todos = bongos_work_model::commands::default_todos(Kind::PrShip);
    write(
        &layout::task_file(&harness.task_dir(&["backfill"])),
        &serialize_task(&file).unwrap(),
    );
    wait_until(cx, "the view to follow the store", |cx| {
        view.read_with(cx, |view, _| text_has(view.lines(), "Backfill renamed"))
    })
    .await;

    // A source registered later adds its section, and RET on its row activates it.
    let source = Fake::new("Checks", Some("Checks: not done"));
    cx.update(|_, cx| TaskSectionsRegistry::add_source("checks", i32::MIN, source.clone(), cx));
    cx.run_until_parked();
    harness.workspace.update_in(cx, |workspace, window, cx| {
        show_details(
            "l1",
            Some(SectionFocus {
                title: "Checks".to_string(),
            }),
            workspace,
            window,
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(views(&harness, cx).len(), 2);
    let selected = view.read_with(cx, |view, _| view.selected_line().cloned().unwrap());
    assert_eq!(selected.kind, LineKind::SectionHeader("checks".to_string()));
    press(cx, menu::SelectNext);
    press(cx, menu::Confirm);
    assert_eq!(*source.activated.borrow(), ["row1"]);

    // `s` toggles the sources flag handed to the source.
    press(cx, task_detail::ToggleSources);
    assert!(view.read_with(cx, |view, _| text_has(view.lines(), "(true)")));
    assert_eq!(text_of_flag(cx), true);
}

fn text_of_flag(cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| text_of("l1", true, cx).is_some_and(|text| text.contains("(true)")))
}

#[gpui::test]
async fn the_expanded_row_changes_only_by_the_summary_line(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let panel = open_panel(&harness, cx).await;
    press(cx, menu::Confirm);
    dispatch_toggle(cx);
    let plain_rows = panel_lines(&panel, cx);

    // A registry with no source changes nothing; a source adds one line.
    dispatch_toggle(cx);
    let source = Fake::new("Checks", Some("Checks: not done: 2 checks missing"));
    cx.update(|_, cx| TaskSectionsRegistry::add_source("checks", i32::MIN, source, cx));
    cx.run_until_parked();
    dispatch_toggle(cx);
    let with_summary = panel_lines(&panel, cx);
    assert!(has(&with_summary, "Checks: not done: 2 checks missing"));
    let without: Vec<String> = with_summary
        .iter()
        .filter(|line| !line.contains("Checks: not done"))
        .cloned()
        .collect();
    assert_eq!(without, plain_rows);

    // RET still drills in.
    dispatch_toggle(cx);
    next_row(cx);
    press(cx, menu::Confirm);
    cx.update(|_, cx| TaskSectionsRegistry::remove_source("checks", cx));
}

fn dispatch_toggle(cx: &mut VisualTestContext) {
    crate::command_tests::dispatch(cx, "work::ToggleRow");
}
