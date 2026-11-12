use std::cell::RefCell;
use std::rc::Rc;

use editor::Editor;
use gpui::{Entity, TestAppContext, VisualTestContext, WindowHandle, point};
use project::{Fs as _, Project, ProjectPath};
use text::Point;
use workspace::{
    AppState, MinibufferRouting, MultiWorkspace, SaveIntent, Workspace, WorkspaceHandle as _,
};

use crate::actions::PopGlobalMark;

/// Where the active pane is: file, caret row and vertical scroll.
type Location = (ProjectPath, u32, f64);

/// A workspace on a project of three 40-line files, `one.txt`, `two.txt` and `three.txt`, none
/// open yet.
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
        let text = (1..=40)
            .map(|ix| format!("{name} {ix}\n"))
            .collect::<String>();
        fake.insert_file(format!("/root/{name}.txt"), text.into_bytes())
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

async fn open(
    workspace: &Entity<Workspace>,
    path: &ProjectPath,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(path.clone(), None, true, window, cx)
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

fn pop_global_mark(cx: &mut VisualTestContext) {
    cx.dispatch_action(PopGlobalMark);
    cx.run_until_parked();
}

async fn go_back(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.go_back(workspace.active_pane().downgrade(), window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
}

async fn go_forward(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.go_forward(workspace.active_pane().downgrade(), window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
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

/// One, then two moved to row 15 and scrolled, then three moved to row 30. The history ends
/// with two at row 15 and three at row 0. Returns where two was left and where three is.
async fn visit_three_files(
    workspace: &Entity<Workspace>,
    paths: &[ProjectPath; 3],
    cx: &mut VisualTestContext,
) -> (Location, Location) {
    open(workspace, &paths[0], cx).await;
    let two = open(workspace, &paths[1], cx).await;
    select_row(&two, 15, cx);
    scroll_to(&two, 8., cx);
    let left_in_two = location(workspace, cx);
    let three = open(workspace, &paths[2], cx).await;
    select_row(&three, 30, cx);
    (left_in_two, location(workspace, cx))
}

#[gpui::test]
async fn pop_global_mark_returns_to_the_previous_file_at_its_cursor_and_scroll(
    cx: &mut TestAppContext,
) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let (left_in_two, _) = visit_three_files(&workspace, &paths, &mut cx).await;
    assert_eq!(left_in_two, (paths[1].clone(), 15, 8.));

    pop_global_mark(&mut cx);
    assert_eq!(location(&workspace, &mut cx), left_in_two);
}

#[gpui::test]
async fn pop_global_mark_skips_same_file_entries_without_consuming_them(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    visit_three_files(&workspace, &paths, &mut cx).await;

    pop_global_mark(&mut cx);
    assert_eq!(location(&workspace, &mut cx).0, paths[1]);
    // Three's row-0 entry, newer than two's, is still the newest one back.
    go_back(&workspace, &mut cx).await;
    assert_eq!(location(&workspace, &mut cx), (paths[2].clone(), 0, 0.));
}

#[gpui::test]
async fn go_forward_after_pop_global_mark_returns(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let (left_in_two, in_three) = visit_three_files(&workspace, &paths, &mut cx).await;

    pop_global_mark(&mut cx);
    assert_eq!(location(&workspace, &mut cx), left_in_two);
    go_forward(&workspace, &mut cx).await;
    assert_eq!(file_and_row(&workspace, &mut cx), (in_three.0, in_three.1));
}

#[gpui::test]
async fn pop_global_mark_reopens_a_closed_file(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    select_row(&one, 20, &mut cx);
    let left_in_one = location(&workspace, &mut cx);
    let one_id = one.entity_id();
    drop(one);
    open(&workspace, &paths[1], &mut cx).await;
    let pane = workspace.read_with(&cx, |workspace, _| workspace.active_pane().clone());
    pane.update_in(&mut cx, |pane, window, cx| {
        pane.close_item_by_id(one_id, SaveIntent::Close, window, cx)
    })
    .await
    .unwrap();
    cx.run_until_parked();
    assert_eq!(pane.read_with(&cx, |pane, _| pane.items_len()), 1);

    pop_global_mark(&mut cx);
    assert_eq!(
        file_and_row(&workspace, &mut cx),
        (left_in_one.0, left_in_one.1)
    );
    assert_eq!(pane.read_with(&cx, |pane, _| pane.items_len()), 2);
}

#[gpui::test]
async fn pop_global_mark_with_no_other_file_says_so(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let one = open(&workspace, &paths[0], &mut cx).await;
    // Only entries in this file are in the history.
    select_row(&one, 20, &mut cx);
    let here = location(&workspace, &mut cx);
    let messages = echo(&mut cx);

    pop_global_mark(&mut cx);
    assert_eq!(location(&workspace, &mut cx), here);
    assert_eq!(
        *messages.borrow(),
        vec!["No earlier location in another file"]
    );
    // The same-file entry is still there for go-back.
    go_back(&workspace, &mut cx).await;
    assert_eq!(location(&workspace, &mut cx), (paths[0].clone(), 0, 0.));
}
