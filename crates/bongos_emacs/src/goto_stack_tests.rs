use std::cell::RefCell;
use std::rc::Rc;

use editor::Editor;
use gpui::{
    Action as _, Entity, KeyBinding, TestAppContext, VisualTestContext, WindowHandle, point,
};
use project::{Fs as _, Project, ProjectPath, RemoveOptions};
use text::Point;
use workspace::{
    AppState, MinibufferRouting, MultiWorkspace, Pane, SaveIntent, SplitDirection, Workspace,
    WorkspaceHandle as _,
};

use crate::actions::{GotoStackBack, GotoStackForward, GotoStackPush, UniversalArgument};
use crate::goto_stack::{NO_EARLIER_LOCATION, NO_LATER_LOCATION};
use crate::prefix_arg;

/// Where the active pane is: file, caret row and vertical scroll.
type Location = (ProjectPath, u32, f64);

const ROWS: u32 = 40;

/// A workspace on a project of three 40-line files, `one.txt`, `two.txt` and `three.txt`, none
/// open yet. Line `n` (from 1) of `one.txt` reads `one n`.
async fn setup(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, [ProjectPath; 3], VisualTestContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    let fake = app_state.fs.as_fake();
    fake.create_dir("/root".as_ref()).await.unwrap();
    for name in ["one", "two", "three"] {
        fake.insert_file(format!("/root/{name}.txt"), lines(name, ROWS).into_bytes())
            .await;
    }
    let project = Project::test(app_state.fs.clone(), ["/root".as_ref()], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    let all = cx.read(|cx| workspace.file_project_paths(cx));
    let path = |name: &str| {
        all.iter()
            .find(|path| path.path.file_name() == Some(name))
            .unwrap()
            .clone()
    };
    let paths = [path("one.txt"), path("two.txt"), path("three.txt")];
    (workspace, paths, cx)
}

fn lines(name: &str, count: u32) -> String {
    (1..=count).map(|ix| format!("{name} {ix}\n")).collect()
}

async fn open(
    workspace: &Entity<Workspace>,
    path: &ProjectPath,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    open_in(workspace, None, path, cx).await
}

async fn open_in(
    workspace: &Entity<Workspace>,
    pane: Option<&Entity<Pane>>,
    path: &ProjectPath,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let pane = pane.map(|pane| pane.downgrade());
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(path.clone(), pane, true, window, cx)
        })
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();
    cx.run_until_parked();
    editor
}

/// Puts the caret at the start of `row`; a jump of ten rows or more records the old place.
fn select_row(editor: &Entity<Editor>, row: u32, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([Point::new(row, 0)..Point::new(row, 0)])
        })
    });
    cx.run_until_parked();
}

fn scroll_to(editor: &Entity<Editor>, y: f64, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.set_scroll_position(point(0., y), window, cx);
    });
}

fn edit(
    editor: &Entity<Editor>,
    range: std::ops::Range<Point>,
    text: &str,
    cx: &mut VisualTestContext,
) {
    editor.update(cx, |editor, cx| {
        editor.buffer().update(cx, |buffer, cx| {
            buffer.edit([(range, text)], None, cx);
        });
    });
    cx.run_until_parked();
}

fn location(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Location {
    workspace.update(cx, |workspace, cx| {
        let item = workspace.active_item(cx).unwrap();
        let path = item.project_path(cx).unwrap();
        let editor = item.downcast::<Editor>().unwrap();
        editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let row = editor.selections.newest_display(&display).head().row().0;
            (path, row, editor.scroll_position(cx).y)
        })
    })
}

fn file_and_row(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (ProjectPath, u32) {
    let (path, row, _) = location(workspace, cx);
    (path, row)
}

/// The caret of the active editor as a point, and the text of its line.
fn caret_and_line(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (Point, String) {
    workspace.update(cx, |workspace, cx| {
        let editor = workspace
            .active_item(cx)
            .unwrap()
            .downcast::<Editor>()
            .unwrap();
        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let caret = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            assert!(
                caret <= snapshot.max_point(),
                "caret {caret:?} is past the end"
            );
            let line = snapshot
                .text_for_range(
                    Point::new(caret.row, 0)
                        ..Point::new(
                            caret.row,
                            snapshot.line_len(multi_buffer::MultiBufferRow(caret.row)),
                        ),
                )
                .collect::<String>();
            (caret, line)
        })
    })
}

fn back(cx: &mut VisualTestContext) {
    cx.dispatch_action(GotoStackBack);
    cx.run_until_parked();
}

fn forward(cx: &mut VisualTestContext) {
    cx.dispatch_action(GotoStackForward);
    cx.run_until_parked();
}

fn push(cx: &mut VisualTestContext) {
    cx.dispatch_action(GotoStackPush);
    cx.run_until_parked();
}

fn active_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
    workspace.read_with(cx, |workspace, _| workspace.active_pane().clone())
}

/// The rows of `pane`'s backward stack, oldest first.
fn backward_rows(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Vec<Option<u32>> {
    pane.read_with(cx, |pane, cx| {
        let mut rows = Vec::new();
        pane.nav_history().last_backward_where(
            &mut |entry, _| {
                rows.push(entry.row);
                false
            },
            cx,
        );
        rows.reverse();
        rows
    })
}

fn echo(cx: &mut VisualTestContext) -> Rc<RefCell<Vec<String>>> {
    let messages = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });
    messages
}

async fn close(workspace: &Entity<Workspace>, editor: Entity<Editor>, cx: &mut VisualTestContext) {
    let id = editor.entity_id();
    drop(editor);
    let pane = active_pane(workspace, cx);
    pane.update_in(cx, |pane, window, cx| {
        pane.close_item_by_id(id, SaveIntent::Skip, window, cx)
    })
    .await
    .unwrap();
    cx.run_until_parked();
}

async fn delete_on_disk(workspace: &Entity<Workspace>, name: &str, cx: &mut VisualTestContext) {
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    fs.remove_file(format!("/root/{name}").as_ref(), RemoveOptions::default())
        .await
        .unwrap();
    cx.run_until_parked();
}

async fn write_on_disk(
    workspace: &Entity<Workspace>,
    name: &str,
    text: String,
    cx: &mut VisualTestContext,
) {
    let fs = workspace.read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    fs.as_fake()
        .insert_file(format!("/root/{name}"), text.into_bytes())
        .await;
    cx.run_until_parked();
}

#[gpui::test]
async fn back_and_forward_with_no_history_say_so_and_stay(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 5, &mut cx);
    let here = location(&workspace, &mut cx);
    let pane = active_pane(&workspace, &mut cx);
    assert!(backward_rows(&pane, &mut cx).is_empty());
    let messages = echo(&mut cx);

    back(&mut cx);
    assert_eq!(location(&workspace, &mut cx), here);
    forward(&mut cx);
    assert_eq!(location(&workspace, &mut cx), here);
    assert_eq!(
        *messages.borrow(),
        vec![NO_EARLIER_LOCATION, NO_LATER_LOCATION]
    );
    assert_eq!(NO_EARLIER_LOCATION, "No earlier location");
    assert_eq!(NO_LATER_LOCATION, "No later location");
}

#[gpui::test]
async fn back_and_forward_walk_the_history(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    let two = open(&workspace, &paths[1], &mut cx).await;
    select_row(&two, 3, &mut cx);
    let messages = echo(&mut cx);

    back(&mut cx);
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 20));
    back(&mut cx);
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 0));
    forward(&mut cx);
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 20));
    forward(&mut cx);
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[1].clone(), 3));
    assert!(messages.borrow().is_empty());
}

#[gpui::test]
async fn back_skips_a_file_deleted_on_disk(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    let two = open(&workspace, &paths[1], &mut cx).await;
    select_row(&two, 12, &mut cx);
    open(&workspace, &paths[2], &mut cx).await;
    // History: one 0, one 20, two 0, two 12; three is active.
    close(&workspace, one, &mut cx).await;
    close(&workspace, two, &mut cx).await;
    delete_on_disk(&workspace, "two.txt", &mut cx).await;

    back(&mut cx);
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 20));
}

#[gpui::test]
async fn a_failed_reopen_never_moves_the_caret(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    let two = open(&workspace, &paths[1], &mut cx).await;
    select_row(&two, 5, &mut cx);
    scroll_to(&two, 2., &mut cx);
    let here = location(&workspace, &mut cx);
    close(&workspace, one, &mut cx).await;
    delete_on_disk(&workspace, "one.txt", &mut cx).await;
    let pane = active_pane(&workspace, &mut cx);
    assert_eq!(backward_rows(&pane, &mut cx).len(), 1);

    back(&mut cx);
    assert_eq!(location(&workspace, &mut cx), here);
    assert_eq!(pane.read_with(&cx, |pane, _| pane.items_len()), 1);
    // The history records new places as usual afterwards.
    two.update(&mut cx, |editor, cx| editor.create_nav_history_entry(cx));
    cx.run_until_parked();
    assert_eq!(backward_rows(&pane, &mut cx), vec![Some(5)]);
    assert!(!pane.read_with(&cx, |pane, _| pane.can_navigate_forward()));
}

#[gpui::test]
async fn back_to_a_deleted_line_lands_on_the_nearest_surviving_line(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 30, &mut cx);
    select_row(&one, 5, &mut cx);
    // Rows 29 to 31 (lines "one 30" to "one 32") go.
    edit(&one, Point::new(29, 0)..Point::new(32, 0), "", &mut cx);

    back(&mut cx);
    assert_eq!(
        caret_and_line(&workspace, &mut cx),
        (Point::new(29, 0), "one 33".to_string())
    );
}

#[gpui::test]
async fn back_past_the_end_of_a_truncated_buffer_lands_at_the_end(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 30, &mut cx);
    select_row(&one, 5, &mut cx);
    edit(&one, Point::new(10, 0)..Point::new(ROWS, 0), "", &mut cx);

    back(&mut cx);
    assert_eq!(caret_and_line(&workspace, &mut cx).0, Point::new(10, 0));
}

#[gpui::test]
async fn back_after_edits_above_lands_on_the_same_text(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 30, &mut cx);
    select_row(&one, 5, &mut cx);
    edit(
        &one,
        Point::new(0, 0)..Point::new(0, 0),
        "a\nb\nc\n",
        &mut cx,
    );

    back(&mut cx);
    assert_eq!(
        caret_and_line(&workspace, &mut cx),
        (Point::new(33, 0), "one 31".to_string())
    );
}

#[gpui::test]
async fn back_to_a_closed_file_truncated_on_disk_lands_at_its_end(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 30, &mut cx);
    open(&workspace, &paths[1], &mut cx).await;
    close(&workspace, one, &mut cx).await;
    write_on_disk(&workspace, "one.txt", lines("one", 10), &mut cx).await;

    back(&mut cx);
    assert_eq!(location(&workspace, &mut cx).0, paths[0]);
    assert_eq!(caret_and_line(&workspace, &mut cx).0, Point::new(10, 0));
}

#[gpui::test]
async fn back_to_a_closed_file_edited_above_on_disk_keeps_the_saved_row(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 30, &mut cx);
    open(&workspace, &paths[1], &mut cx).await;
    close(&workspace, one, &mut cx).await;
    write_on_disk(
        &workspace,
        "one.txt",
        format!("a\nb\nc\n{}", lines("one", ROWS)),
        &mut cx,
    )
    .await;

    back(&mut cx);
    // The reopened buffer is a new one, so the saved row is used as it is.
    assert_eq!(location(&workspace, &mut cx).0, paths[0]);
    assert_eq!(
        caret_and_line(&workspace, &mut cx),
        (Point::new(30, 0), "one 28".to_string())
    );
}

#[gpui::test]
async fn back_reopens_a_closed_file_at_its_caret_and_scroll(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    scroll_to(&one, 12., &mut cx);
    let left_in_one = location(&workspace, &mut cx);
    open(&workspace, &paths[1], &mut cx).await;
    close(&workspace, one, &mut cx).await;
    let pane = active_pane(&workspace, &mut cx);
    assert_eq!(pane.read_with(&cx, |pane, _| pane.items_len()), 1);

    back(&mut cx);
    assert_eq!(location(&workspace, &mut cx), left_in_one);
    assert_eq!(pane.read_with(&cx, |pane, _| pane.items_len()), 2);
}

#[gpui::test]
async fn two_panes_push_and_walk_independently(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let left = active_pane(&workspace, &mut cx);
    let one = open_in(&workspace, Some(&left), &paths[0], &mut cx).await;
    select_row(&one, 4, &mut cx);
    push(&mut cx);
    select_row(&one, 6, &mut cx);

    let right = workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.split_pane(left.clone(), SplitDirection::Right, window, cx)
    });
    let two = open_in(&workspace, Some(&right), &paths[1], &mut cx).await;
    assert_eq!(active_pane(&workspace, &mut cx), right);
    select_row(&two, 7, &mut cx);
    push(&mut cx);
    select_row(&two, 9, &mut cx);
    assert_eq!(backward_rows(&left, &mut cx), vec![Some(4)]);
    assert_eq!(backward_rows(&right, &mut cx), vec![Some(7)]);

    back(&mut cx);
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[1].clone(), 7));
    let caret_in_one = one.update(&mut cx, |editor, cx| {
        editor
            .selections
            .newest::<Point>(&editor.display_snapshot(cx))
            .head()
    });
    assert_eq!(caret_in_one, Point::new(6, 0));
    assert_eq!(backward_rows(&left, &mut cx), vec![Some(4)]);

    left.update_in(&mut cx, |pane, window, cx| {
        pane.focus_active_item(window, cx)
    });
    cx.run_until_parked();
    assert_eq!(active_pane(&workspace, &mut cx), left);
    back(&mut cx);
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 4));
    let caret_in_two = two.update(&mut cx, |editor, cx| {
        editor
            .selections
            .newest::<Point>(&editor.display_snapshot(cx))
            .head()
    });
    assert_eq!(caret_in_two, Point::new(7, 0));
}

#[gpui::test]
async fn back_twice_before_a_reopen_finishes_ends_on_a_valid_entry(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    let two = open(&workspace, &paths[1], &mut cx).await;
    select_row(&two, 15, &mut cx);
    open(&workspace, &paths[2], &mut cx).await;
    // History: one 0, one 20, two 0, two 15; three is active.
    close(&workspace, one, &mut cx).await;
    close(&workspace, two, &mut cx).await;

    cx.update(|window, cx| {
        window.dispatch_action(GotoStackBack.boxed_clone(), cx);
        window.dispatch_action(GotoStackBack.boxed_clone(), cx);
    });
    cx.run_until_parked();
    let (path, row) = file_and_row(&workspace, &mut cx);
    assert!(
        [
            (paths[1].clone(), 15),
            (paths[1].clone(), 0),
            (paths[0].clone(), 20),
        ]
        .contains(&(path.clone(), row)),
        "ended on {path:?} row {row}"
    );
    caret_and_line(&workspace, &mut cx);

    // The history still walks after the race.
    back(&mut cx);
    forward(&mut cx);
    caret_and_line(&workspace, &mut cx);
}

#[gpui::test]
async fn push_records_the_caret_without_moving_it(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 5, &mut cx);
    let here = location(&workspace, &mut cx);
    let pane = active_pane(&workspace, &mut cx);
    assert!(backward_rows(&pane, &mut cx).is_empty());

    push(&mut cx);
    assert_eq!(location(&workspace, &mut cx), here);
    assert_eq!(backward_rows(&pane, &mut cx), vec![Some(5)]);

    // A push at the same place stays one entry.
    push(&mut cx);
    assert_eq!(backward_rows(&pane, &mut cx), vec![Some(5)]);

    // A push records even a place a caret move would not (fewer than ten rows away).
    select_row(&one, 8, &mut cx);
    push(&mut cx);
    assert_eq!(backward_rows(&pane, &mut cx), vec![Some(5), Some(8)]);
}

#[gpui::test]
async fn push_clears_the_forward_stack(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 5, &mut cx);
    push(&mut cx);
    select_row(&one, 25, &mut cx);
    back(&mut cx);
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 5));
    let pane = active_pane(&workspace, &mut cx);
    assert!(pane.read_with(&cx, |pane, _| pane.can_navigate_forward()));

    push(&mut cx);
    assert!(!pane.read_with(&cx, |pane, _| pane.can_navigate_forward()));
    let messages = echo(&mut cx);
    forward(&mut cx);
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 5));
    assert_eq!(*messages.borrow(), vec![NO_LATER_LOCATION]);
}

#[gpui::test]
async fn the_goto_stack_commands_ignore_the_prefix_argument(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    cx.update(|_, cx| {
        for name in [
            GotoStackBack::name_for_type(),
            GotoStackForward::name_for_type(),
            GotoStackPush::name_for_type(),
        ] {
            assert!(
                matches!(
                    prefix_arg::meaning(cx, name),
                    Some(prefix_arg::Meaning::Ignored)
                ),
                "{name}"
            );
        }
        cx.bind_keys([
            KeyBinding::new("ctrl-u", UniversalArgument, Some("Editor")),
            KeyBinding::new("f5", GotoStackBack, None),
            KeyBinding::new("f6", GotoStackForward, None),
            KeyBinding::new("f7", GotoStackPush, Some("Editor")),
        ]);
    });
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    select_row(&one, 35, &mut cx);
    let pane = active_pane(&workspace, &mut cx);
    let pending = |cx: &mut VisualTestContext| {
        cx.update(|window, cx| prefix_arg::pending(window, cx).is_some())
    };

    // C-u back goes back one entry, not four.
    cx.simulate_keystrokes("ctrl-u f5");
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 20));
    assert!(!pending(&mut cx));

    cx.simulate_keystrokes("ctrl-u f6");
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 35));
    assert!(!pending(&mut cx));

    cx.simulate_keystrokes("ctrl-u f7");
    assert_eq!(file_and_row(&workspace, &mut cx), (paths[0].clone(), 35));
    assert_eq!(
        backward_rows(&pane, &mut cx),
        vec![Some(0), Some(20), Some(35)]
    );
    assert!(!pending(&mut cx));
}
