//! Tests of the search in buffer picker: its rows, the seed, the narrowing, the preview, what RET
//! moves and what the pane choices open.

use bongos_emacs::mark::mark_for;
use bongos_minibuffer::SessionHandle;
use bongos_window::test_support::split_center;
use editor::{Editor, SelectionEffects};
use gpui::{
    App, BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext,
    WindowHandle,
};
use language::Point;
use project::Project;
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace, Pane, Workspace, WorkspaceHandle as _};
use zed_actions::minibuffer;

use crate::lines::{HOST, LineRow, candidates, start, symbol_in};
use crate::test_support::{WorldSnapshot, world_snapshot};

/// The text of the file the tests search: 120 lines, a few of them with something to find.
fn content() -> String {
    (0..120)
        .map(|row| match row {
            5 => "let target = widget;".to_string(),
            70 => "    widget.render(target)".to_string(),
            71 => "widget_two".to_string(),
            _ => format!("    line {row}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A workspace on `/root` holding `notes.txt` with `text`, opened in the active pane.
async fn setup(
    cx: &mut TestAppContext,
    text: String,
) -> (Entity<Workspace>, Entity<Editor>, VisualTestContext) {
    let params = cx.update(|cx| {
        let params = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        crate::init(cx);
        params
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(vec![HOST.to_string()]);
            });
        });
    });
    params
        .fs
        .as_fake()
        .insert_tree("/root", serde_json::json!({ "notes.txt": text }))
        .await;
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("the window holds a workspace");
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let path = cx
        .read(|cx| workspace.file_project_paths(cx))
        .into_iter()
        .next()
        .expect("the file is in the worktree");
    let task = workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.open_path(path, None, true, window, cx)
    });
    task.await.expect("the file opens");
    cx.run_until_parked();
    let editor = active_editor(&workspace, &mut cx);
    (workspace, editor, cx)
}

fn active_editor(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Editor> {
    cx.read(|cx| workspace.read(cx).active_item_as::<Editor>(cx))
        .expect("the active item is an editor")
}

fn put_cursor(editor: &Entity<Editor>, row: u32, column: u32, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        let at = Point::new(row, column);
        editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
            selections.select_ranges([at..at]);
        });
    });
    cx.run_until_parked();
}

fn cursor(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Point {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        editor.selections.newest::<Point>(&snapshot).head()
    })
}

fn open_picker(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> SessionHandle {
    let handle = cx.update(|window, cx| start(workspace, window, cx));
    cx.run_until_parked();
    handle.expect("the picker opens")
}

fn type_input(handle: &SessionHandle, text: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| handle.set_input(text, window, cx));
    cx.run_until_parked();
}

fn selected_row(handle: &SessionHandle, cx: &mut VisualTestContext) -> Option<u32> {
    let value = cx.read(|cx| handle.selected(cx))?;
    value.downcast_ref::<LineRow>().map(|row| row.0)
}

fn select_next(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::SelectNext);
    cx.run_until_parked();
}

/// The rows of the first `count` entries, found by walking the selection down the list.
fn listed(handle: &SessionHandle, count: usize, cx: &mut VisualTestContext) -> Vec<u32> {
    let mut rows = Vec::new();
    for _ in 0..count {
        rows.extend(selected_row(handle, cx));
        select_next(cx);
    }
    rows
}

/// The world as a picker must leave it; a pane that takes the focus back when the picker closes
/// stamps its active item again, so the stamps are dropped.
fn snapshot(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> WorldSnapshot {
    let mut snapshot = cx.update(|_, cx: &mut App| world_snapshot(workspace, cx));
    for pane in &mut snapshot.panes {
        for (_, stamp) in &mut pane.activation_history {
            *stamp = 0;
        }
    }
    snapshot
}

/// The row at the middle of the editor's visible lines.
fn centered_row(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> f64 {
    editor.update_in(cx, |editor, _, cx| {
        let visible = editor
            .visible_line_count()
            .expect("the editor was laid out");
        editor.scroll_position(cx).y + visible / 2.
    })
}

fn slot_editor(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<Entity<Editor>> {
    cx.read(|cx| pane.read(cx).preview().cloned())?
        .downcast::<Editor>()
        .ok()
}

fn active_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
    cx.read(|cx| workspace.read(cx).active_pane().clone())
}

#[gpui::test]
async fn lines_every_line_is_listed_with_its_number(cx: &mut TestAppContext) {
    let text = content();
    let rows = candidates(&text);
    assert_eq!(rows.len(), 120);
    assert_eq!(rows[0].primary.as_ref(), "    line 0");
    assert_eq!(rows[0].secondary.as_deref(), Some("1"));
    assert_eq!(rows[5].primary.as_ref(), "let target = widget;");
    assert_eq!(rows[5].secondary.as_deref(), Some("6"));
    assert_eq!(rows[119].secondary.as_deref(), Some("120"));

    let (workspace, editor, mut cx) = setup(cx, text).await;
    let cx = &mut cx;
    // No symbol at the cursor: the input is empty and the cursor's line is selected first.
    put_cursor(&editor, 7, 0, cx);
    let handle = open_picker(&workspace, cx);
    assert_eq!(cx.read(|cx| handle.input(cx)).as_deref(), Some(""));
    assert_eq!(selected_row(&handle, cx), Some(7));
    assert_eq!(listed(&handle, 3, cx), [7, 8, 9]);
}

#[gpui::test]
async fn lines_input_is_seeded_with_the_symbol_at_point(cx: &mut TestAppContext) {
    assert_eq!(symbol_in("let target = widget;", 6), Some("target"));
    assert_eq!(symbol_in("let target = widget;", 10), Some("target"));
    assert_eq!(symbol_in("let target = widget;", 11), None);
    assert_eq!(symbol_in("", 0), None);

    let (workspace, editor, mut cx) = setup(cx, content()).await;
    let cx = &mut cx;
    put_cursor(&editor, 5, 6, cx);
    let handle = open_picker(&workspace, cx);
    assert_eq!(cx.read(|cx| handle.input(cx)).as_deref(), Some("target"));
    assert!(matches!(selected_row(&handle, cx), Some(5 | 70)));
}

#[gpui::test]
async fn lines_narrowing_keeps_the_matching_lines_in_order(cx: &mut TestAppContext) {
    let (workspace, _editor, mut cx) = setup(cx, content()).await;
    let cx = &mut cx;
    let handle = open_picker(&workspace, cx);
    type_input(&handle, "widget", cx);
    assert_eq!(listed(&handle, 3, cx), [5, 70, 71]);
    type_input(&handle, "widget target", cx);
    assert_eq!(listed(&handle, 2, cx), [5, 70]);
    type_input(&handle, "zzzz", cx);
    assert_eq!(selected_row(&handle, cx), None);
}

#[gpui::test]
async fn lines_preview_centers_the_line_and_cancel_leaves_everything_as_it_was(
    cx: &mut TestAppContext,
) {
    let (workspace, editor, mut cx) = setup(cx, content()).await;
    let cx = &mut cx;
    put_cursor(&editor, 2, 0, cx);
    let pane = active_pane(&workspace, cx);
    let before = snapshot(&workspace, cx);

    let handle = open_picker(&workspace, cx);
    type_input(&handle, "widget.render", cx);
    assert_eq!(selected_row(&handle, cx), Some(70));
    let transient = slot_editor(&pane, cx).expect("the preview is a transient editor");
    assert_ne!(transient.entity_id(), editor.entity_id());
    let center = centered_row(&transient, cx);
    assert!(
        (center - 70.).abs() <= 1.5,
        "row 70 is centered, center is {center}"
    );
    assert_eq!(snapshot(&workspace, cx).without_previews(), before);
    assert_eq!(cursor(&editor, cx), Point::new(2, 0));

    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(snapshot(&workspace, cx), before);
    assert!(slot_editor(&pane, cx).is_none());
}

#[gpui::test]
async fn lines_confirm_moves_to_the_first_non_blank_and_pushes_the_mark(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = setup(cx, content()).await;
    let cx = &mut cx;
    put_cursor(&editor, 2, 3, cx);
    let handle = open_picker(&workspace, cx);
    type_input(&handle, "widget.render", cx);
    assert_eq!(selected_row(&handle, cx), Some(70));
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();

    assert_eq!(cursor(&editor, cx), Point::new(70, 4));
    let center = centered_row(&editor, cx);
    assert!(
        (center - 70.).abs() <= 1.5,
        "row 70 is centered, center is {center}"
    );
    let mark = cx.update(|_, cx| {
        let anchor = mark_for(&editor, cx)?;
        let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
        Some(snapshot.summary_for_anchor::<Point>(&anchor))
    });
    assert_eq!(mark, Some(Point::new(2, 3)));
}

#[gpui::test]
async fn lines_other_pane_opens_the_line_there_and_the_origin_cursor_stays(
    cx: &mut TestAppContext,
) {
    let (workspace, editor, mut cx) = setup(cx, content()).await;
    let cx = &mut cx;
    put_cursor(&editor, 2, 0, cx);
    let panes = cx.update(|window, cx| split_center(&workspace, 2, window, cx));
    cx.update(|window, cx| window.focus(&panes[0].focus_handle(cx), cx));
    cx.run_until_parked();

    let handle = open_picker(&workspace, cx);
    type_input(&handle, "widget.render", cx);
    assert_eq!(selected_row(&handle, cx), Some(70));
    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    cx.simulate_keystrokes("o");
    cx.run_until_parked();

    let shown = cx
        .read(|cx| panes[1].read(cx).active_item())
        .and_then(|item| cx.read(|cx| item.act_as::<Editor>(cx)))
        .expect("the other pane shows an editor");
    assert_eq!(cursor(&shown, cx), Point::new(70, 4));
    assert_eq!(cursor(&editor, cx), Point::new(2, 0));
}

#[gpui::test]
async fn lines_a_hundred_thousand_lines_open_and_narrow(cx: &mut TestAppContext) {
    let text = (0..100_000)
        .map(|row| format!("line number {row}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(candidates(&text).len(), 100_000);

    let (workspace, _editor, mut cx) = setup(cx, text).await;
    let cx = &mut cx;
    let handle = open_picker(&workspace, cx);
    assert_eq!(selected_row(&handle, cx), Some(0));
    type_input(&handle, "number 99999", cx);
    assert_eq!(selected_row(&handle, cx), Some(99_999));
    type_input(&handle, "nomatchatall", cx);
    assert_eq!(selected_row(&handle, cx), None);
}
