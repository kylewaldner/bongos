use editor::Editor;
use gpui::{Action, Axis, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle};
use project::{Fs as _, Project, ProjectPath};
use workspace::{AppState, MultiWorkspace, Pane, SplitDirection, Workspace, WorkspaceHandle as _};

use crate::actions::{
    DeleteWindow, SplitWindowHorizontallyInstead, SplitWindowVerticallyInstead,
    ToggleDeleteOtherWindows,
};
use crate::test_support::{TreeSnapshot, layout_snapshot, split_center};

/// A workspace in a multi-workspace window on a project of three 40-line files, `one.txt`,
/// `two.txt` and `three.txt`, none open yet.
async fn setup(
    cx: &mut TestAppContext,
) -> (Entity<Workspace>, [ProjectPath; 3], VisualTestContext) {
    let params = cx.update(|cx| {
        let params = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        bongos_emacs::init(cx);
        bongos_minibuffer::init(cx);
        crate::init(cx);
        params
    });
    let fake = params.fs.as_fake();
    fake.create_dir("/root".as_ref()).await.unwrap();
    for name in ["one", "two", "three"] {
        let text = (1..=40)
            .map(|ix| format!("{name} {ix}\n"))
            .collect::<String>();
        fake.insert_file(format!("/root/{name}.txt"), text.into_bytes())
            .await;
    }
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
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

/// Opens `path` in `pane`, focused, and waits for it.
async fn open_in(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    path: &ProjectPath,
    cx: &mut VisualTestContext,
) {
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path_preview(
                path.clone(),
                Some(pane.downgrade()),
                true,
                false,
                true,
                window,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();
}

fn run(action: impl Action, cx: &mut VisualTestContext) {
    cx.dispatch_action(action);
    cx.run_until_parked();
}

fn center_panes(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Entity<Pane>> {
    cx.update(|_, cx| workspace.read(cx).center_panes())
}

/// The center pane that holds focus.
fn focused_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Entity<Pane>> {
    cx.update(|window, cx| {
        workspace
            .read(cx)
            .center_panes()
            .into_iter()
            .find(|pane| pane.focus_handle(cx).contains_focused(window, cx))
    })
}

fn focus_active_item(pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let item = pane.read(cx).active_item().expect("an active item");
        window.focus(&item.item_focus_handle(cx), cx);
    });
    cx.run_until_parked();
}

/// The files shown by the tabs of `pane`, in tab order.
fn tab_paths(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Vec<ProjectPath> {
    cx.update(|_, cx| {
        pane.read(cx)
            .items()
            .filter_map(|item| item.project_path(cx))
            .collect()
    })
}

/// How many tabs are open in every pane of the window, docks included, and how many buffers the
/// project has open.
fn counts(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> (usize, usize) {
    cx.update(|_, cx| {
        let workspace = workspace.read(cx);
        let tabs = workspace
            .panes()
            .iter()
            .map(|pane| pane.read(cx).items_len())
            .sum();
        let buffers = workspace.project().read(cx).opened_buffers(cx).len();
        (tabs, buffers)
    })
}

/// The axis of the center's root split.
fn root_axis(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Axis> {
    match cx.update(|_, cx| layout_snapshot(workspace, cx)).tree {
        TreeSnapshot::Axis { axis, .. } => Some(axis),
        TreeSnapshot::Pane(_) => None,
    }
}

/// Three side-by-side center panes showing `one.txt`, `two.txt` and `three.txt` in that order.
async fn three_panes(
    workspace: &Entity<Workspace>,
    paths: &[ProjectPath; 3],
    cx: &mut VisualTestContext,
) -> Vec<Entity<Pane>> {
    let panes = cx.update(|window, cx| split_center(workspace, 3, window, cx));
    cx.run_until_parked();
    for (pane, path) in panes.iter().zip(paths) {
        open_in(workspace, pane, path, cx).await;
    }
    panes
}

#[gpui::test]
async fn test_two_stacked_panes_go_side_by_side_and_back(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let top = center_panes(&workspace, cx).remove(0);
    let bottom = workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(top.clone(), SplitDirection::Down, window, cx)
    });
    cx.run_until_parked();
    open_in(&workspace, &top, &paths[0], cx).await;
    open_in(&workspace, &bottom, &paths[1], cx).await;
    open_in(&workspace, &bottom, &paths[2], cx).await;
    assert_eq!(root_axis(&workspace, cx), Some(Axis::Vertical));
    let before = counts(&workspace, cx);

    run(SplitWindowHorizontallyInstead, cx);
    assert_eq!(root_axis(&workspace, cx), Some(Axis::Horizontal));
    assert_eq!(
        center_panes(&workspace, cx),
        vec![top.clone(), bottom.clone()]
    );
    assert_eq!(tab_paths(&top, cx), vec![paths[0].clone()]);
    assert_eq!(
        tab_paths(&bottom, cx),
        vec![paths[1].clone(), paths[2].clone()]
    );
    assert_eq!(counts(&workspace, cx), before);

    run(SplitWindowVerticallyInstead, cx);
    assert_eq!(root_axis(&workspace, cx), Some(Axis::Vertical));
    assert_eq!(
        center_panes(&workspace, cx),
        vec![top.clone(), bottom.clone()]
    );
    assert_eq!(tab_paths(&top, cx), vec![paths[0].clone()]);
    assert_eq!(
        tab_paths(&bottom, cx),
        vec![paths[1].clone(), paths[2].clone()]
    );
    assert_eq!(counts(&workspace, cx), before);
}

#[gpui::test]
async fn test_three_panes_keep_current_and_next(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = three_panes(&workspace, &paths, cx).await;
    focus_active_item(&panes[0], cx);
    let before = counts(&workspace, cx);
    run(SplitWindowVerticallyInstead, cx);
    assert_eq!(
        center_panes(&workspace, cx),
        vec![panes[0].clone(), panes[1].clone()]
    );
    assert_eq!(root_axis(&workspace, cx), Some(Axis::Vertical));
    assert_eq!(tab_paths(&panes[0], cx), vec![paths[0].clone()]);
    assert_eq!(
        tab_paths(&panes[1], cx),
        vec![paths[1].clone(), paths[2].clone()]
    );
    assert_eq!(counts(&workspace, cx), before);
}

#[gpui::test]
async fn test_no_tab_or_buffer_is_closed(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = three_panes(&workspace, &paths, cx).await;
    // Edit `three.txt`, in the pane that will be merged away.
    let editor = cx.update(|window, cx| {
        let editor = panes[2]
            .read(cx)
            .active_item()
            .and_then(|item| item.act_as::<Editor>(cx))
            .expect("an editor");
        editor.update(cx, |editor, cx| editor.insert("edited ", window, cx));
        editor
    });
    cx.run_until_parked();
    let text = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let editor = editor.read(cx);
            (editor.text(cx), editor.buffer().read(cx).is_dirty(cx))
        })
    };
    let edited = text(cx);
    assert!(edited.1, "the buffer is dirty");
    focus_active_item(&panes[0], cx);
    let before = counts(&workspace, cx);
    run(SplitWindowHorizontallyInstead, cx);
    assert_eq!(center_panes(&workspace, cx).len(), 2);
    assert_eq!(counts(&workspace, cx), before, "tabs and buffers");
    assert_eq!(text(cx), edited, "the dirty buffer is unchanged");
    let editor_id = editor.entity_id();
    let shown = cx.update(|_, cx| {
        panes[1]
            .read(cx)
            .items()
            .any(|item| item.item_id() == editor_id)
    });
    assert!(shown, "the edited tab moved to the next pane");
}

/// With one center pane, runs `action` and checks it split as `split-window-right` or
/// `split-window-below` would: the new pane follows along `axis`, shows a clone of `two.txt` and
/// has focus. Then deletes the new pane again, so the center is back to one pane.
fn check_one_pane_split(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    paths: &[ProjectPath; 3],
    action: impl Action,
    axis: Axis,
    cx: &mut VisualTestContext,
) {
    focus_active_item(pane, cx);
    let (tabs, buffers) = counts(workspace, cx);
    run(action, cx);
    let panes = center_panes(workspace, cx);
    assert_eq!(panes.len(), 2);
    assert_eq!(panes[0], *pane);
    assert_eq!(root_axis(workspace, cx), Some(axis));
    assert_eq!(tab_paths(pane, cx), paths.to_vec());
    assert_eq!(tab_paths(&panes[1], cx), vec![paths[1].clone()]);
    assert_eq!(
        counts(workspace, cx),
        (tabs + 1, buffers),
        "one cloned tab, no new buffer"
    );
    assert_eq!(focused_pane(workspace, cx), Some(panes[1].clone()));
    run(DeleteWindow, cx);
    assert_eq!(center_panes(workspace, cx), vec![pane.clone()]);
    assert_eq!(counts(workspace, cx), (tabs, buffers));
}

#[gpui::test]
async fn test_one_pane_splits(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let pane = center_panes(&workspace, cx).remove(0);
    for path in &paths {
        open_in(&workspace, &pane, path, cx).await;
    }
    check_one_pane_split(
        &workspace,
        &pane,
        &paths,
        SplitWindowHorizontallyInstead,
        Axis::Horizontal,
        cx,
    );
    check_one_pane_split(
        &workspace,
        &pane,
        &paths,
        SplitWindowVerticallyInstead,
        Axis::Vertical,
        cx,
    );
}

#[gpui::test]
async fn test_zoomed_window_unzooms_first(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = cx.update(|window, cx| split_center(&workspace, 2, window, cx));
    cx.run_until_parked();
    open_in(&workspace, &panes[0], &paths[0], cx).await;
    open_in(&workspace, &panes[1], &paths[1], cx).await;
    focus_active_item(&panes[0], cx);
    run(ToggleDeleteOtherWindows, cx);
    assert!(cx.update(|_, cx| workspace.read(cx).zoomed_item().is_some()));
    let before = counts(&workspace, cx);
    run(SplitWindowVerticallyInstead, cx);
    assert!(cx.update(|_, cx| workspace.read(cx).zoomed_item().is_none()));
    let zoomed = cx.update(|_, cx| {
        panes
            .iter()
            .map(|pane| pane.read(cx).is_zoomed())
            .collect::<Vec<_>>()
    });
    assert_eq!(zoomed, vec![false; 2]);
    assert_eq!(center_panes(&workspace, cx), panes);
    assert_eq!(root_axis(&workspace, cx), Some(Axis::Vertical));
    assert_eq!(counts(&workspace, cx), before);
}

#[gpui::test]
async fn test_focus_stays_on_current_pane(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = three_panes(&workspace, &paths, cx).await;
    // The last pane is current, so the next one wraps to the first.
    focus_active_item(&panes[2], cx);
    let before = counts(&workspace, cx);
    run(SplitWindowVerticallyInstead, cx);
    assert_eq!(
        center_panes(&workspace, cx),
        vec![panes[0].clone(), panes[2].clone()]
    );
    assert_eq!(root_axis(&workspace, cx), Some(Axis::Vertical));
    assert_eq!(
        tab_paths(&panes[0], cx),
        vec![paths[0].clone(), paths[1].clone()]
    );
    assert_eq!(tab_paths(&panes[2], cx), vec![paths[2].clone()]);
    assert_eq!(focused_pane(&workspace, cx), Some(panes[2].clone()));
    assert_eq!(
        cx.update(|_, cx| workspace.read(cx).active_pane().clone()),
        panes[2]
    );
    assert_eq!(counts(&workspace, cx), before);
}
