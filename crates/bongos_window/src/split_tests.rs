use bongos_emacs::actions::UniversalArgument;
use bongos_emacs::prefix_arg;
use bongos_minibuffer::minibuffer_for_window;
use gpui::{
    Action, Axis, Entity, Focusable as _, KeyBinding, TestAppContext, VisualTestContext,
    WindowHandle,
};
use project::{Fs as _, Project, ProjectPath};
use workspace::{
    AppState, MultiWorkspace, Pane, Workspace, WorkspaceHandle as _, dock::DockPosition,
};

use crate::actions::{DeleteWindow, SplitWindowBelow, SplitWindowRight, ToggleDeleteOtherWindows};
use crate::test_support::{
    TreeSnapshot, add_editor, add_panel, add_terminal, layout_snapshot, new_dock_pane, split_center,
};

/// A workspace in a multi-workspace window on a project of three 40-line files, `one.txt`,
/// `two.txt` and `three.txt`, none open yet. `ctrl-u` starts a prefix argument in an editor and
/// `f1` splits below.
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
        cx.bind_keys([
            KeyBinding::new("ctrl-u", UniversalArgument, Some("Editor")),
            KeyBinding::new("f1", SplitWindowBelow, None),
        ]);
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

fn active_path(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<ProjectPath> {
    cx.update(|_, cx| {
        pane.read(cx)
            .active_item()
            .and_then(|item| item.project_path(cx))
    })
}

/// How many tabs are open in every pane of the window, docks included.
fn open_items(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    cx.update(|_, cx| {
        workspace
            .read(cx)
            .panes()
            .iter()
            .map(|pane| pane.read(cx).items_len())
            .sum()
    })
}

fn focus_active_item(pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let item = pane.read(cx).active_item().expect("an active item");
        window.focus(&item.item_focus_handle(cx), cx);
    });
    cx.run_until_parked();
}

/// The axis of the center's root split.
fn root_axis(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<Axis> {
    match cx.update(|_, cx| layout_snapshot(workspace, cx)).tree {
        TreeSnapshot::Axis { axis, .. } => Some(axis),
        TreeSnapshot::Pane(_) => None,
    }
}

/// The echo area's log, newest first.
fn echoed(cx: &mut VisualTestContext) -> Vec<String> {
    let strip = cx
        .update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
        .expect("the window has a strip");
    strip.read_with(cx, |strip, _| {
        strip
            .messages()
            .into_iter()
            .map(|line| line.text.to_string())
            .collect()
    })
}

/// One center pane with `one.txt`, `two.txt` and `three.txt` opened in that order, so
/// `three.txt` is current and `two.txt` the most recent other buffer.
async fn three_buffers(
    workspace: &Entity<Workspace>,
    paths: &[ProjectPath; 3],
    cx: &mut VisualTestContext,
) -> Entity<Pane> {
    let pane = center_panes(workspace, cx).remove(0);
    for path in paths {
        open_in(workspace, &pane, path, cx).await;
    }
    pane
}

/// Splits in `direction`, and checks that the new pane follows the original along `axis`, shows
/// `two.txt` and has focus, and that the original still shows `three.txt`.
async fn check_split(action: impl Action, axis: Axis, cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let pane = three_buffers(&workspace, &paths, cx).await;
    run(action, cx);
    let panes = center_panes(&workspace, cx);
    assert_eq!(panes.len(), 2);
    assert_eq!(panes[0], pane);
    assert_eq!(root_axis(&workspace, cx), Some(axis));
    assert_eq!(active_path(&panes[1], cx), Some(paths[1].clone()));
    assert_eq!(active_path(&pane, cx), Some(paths[2].clone()));
    assert_eq!(focused_pane(&workspace, cx), Some(panes[1].clone()));
}

#[gpui::test]
async fn test_split_below_shows_recent_other_buffer(cx: &mut TestAppContext) {
    check_split(SplitWindowBelow, Axis::Vertical, cx).await;
}

#[gpui::test]
async fn test_split_right_shows_recent_other_buffer(cx: &mut TestAppContext) {
    check_split(SplitWindowRight, Axis::Horizontal, cx).await;
}

#[gpui::test]
async fn test_split_with_one_buffer_shows_it(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let pane = center_panes(&workspace, cx).remove(0);
    open_in(&workspace, &pane, &paths[0], cx).await;
    run(SplitWindowRight, cx);
    let panes = center_panes(&workspace, cx);
    assert_eq!(panes.len(), 2);
    assert_eq!(active_path(&panes[0], cx), Some(paths[0].clone()));
    assert_eq!(active_path(&panes[1], cx), Some(paths[0].clone()));
    let ids = cx.update(|_, cx| {
        panes
            .iter()
            .map(|pane| pane.read(cx).active_item().map(|item| item.item_id()))
            .collect::<Vec<_>>()
    });
    assert_ne!(
        ids[0], ids[1],
        "the new pane shows a clone, not the same tab"
    );
}

#[gpui::test]
async fn test_split_with_prefix_argument_keeps_focus(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let pane = three_buffers(&workspace, &paths, cx).await;
    cx.simulate_keystrokes("ctrl-u f1");
    cx.run_until_parked();
    let panes = center_panes(&workspace, cx);
    assert_eq!(panes.len(), 2);
    assert_eq!(root_axis(&workspace, cx), Some(Axis::Vertical));
    assert_eq!(active_path(&panes[1], cx), Some(paths[1].clone()));
    assert_eq!(focused_pane(&workspace, cx), Some(pane.clone()));
    assert_eq!(
        cx.update(|_, cx| workspace.read(cx).active_pane().clone()),
        pane
    );
    let left = cx.update(|window, cx| {
        (
            prefix_arg::pending(window, cx),
            prefix_arg::take(window, cx),
        )
    });
    assert_eq!(left, (None, None), "the argument is taken once");
}

#[gpui::test]
async fn test_split_while_zoomed_unzooms_first(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let pane = three_buffers(&workspace, &paths, cx).await;
    let other = cx.update(|window, cx| {
        let other = workspace.update(cx, |workspace, cx| {
            workspace.split_pane(pane.clone(), workspace::SplitDirection::Right, window, cx)
        });
        add_editor(&workspace, &other, window, cx);
        other
    });
    cx.run_until_parked();
    focus_active_item(&pane, cx);
    run(ToggleDeleteOtherWindows, cx);
    assert!(cx.update(|_, cx| workspace.read(cx).zoomed_item().is_some()));
    run(SplitWindowBelow, cx);
    assert!(cx.update(|_, cx| workspace.read(cx).zoomed_item().is_none()));
    let panes = center_panes(&workspace, cx);
    assert_eq!(panes.len(), 3);
    let zoomed = cx.update(|_, cx| {
        panes
            .iter()
            .map(|pane| pane.read(cx).is_zoomed())
            .collect::<Vec<_>>()
    });
    assert_eq!(zoomed, vec![false; 3]);
    assert_eq!(panes[0], pane);
    assert_eq!(panes[2], other);
    assert_eq!(focused_pane(&workspace, cx), Some(panes[1].clone()));
}

#[gpui::test]
async fn test_delete_window_on_sole_pane_changes_nothing(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let pane = three_buffers(&workspace, &paths, cx).await;
    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    run(DeleteWindow, cx);
    assert_eq!(echoed(cx), vec!["Sole pane".to_string()]);
    assert_eq!(cx.update(|_, cx| layout_snapshot(&workspace, cx)), before);
    assert_eq!(center_panes(&workspace, cx), vec![pane.clone()]);
    assert_eq!(focused_pane(&workspace, cx), Some(pane));
}

#[gpui::test]
async fn test_delete_window_moves_tabs_to_sibling_and_ends_dedication(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = cx.update(|window, cx| split_center(&workspace, 2, window, cx));
    cx.run_until_parked();
    open_in(&workspace, &panes[0], &paths[2], cx).await;
    open_in(&workspace, &panes[1], &paths[0], cx).await;
    open_in(&workspace, &panes[1], &paths[1], cx).await;
    cx.update(|_, cx| panes[1].update(cx, |pane, cx| pane.set_dedicated(true, cx)));
    assert_eq!(focused_pane(&workspace, cx), Some(panes[1].clone()));
    let items = open_items(&workspace, cx);
    run(DeleteWindow, cx);
    assert_eq!(center_panes(&workspace, cx), vec![panes[0].clone()]);
    assert_eq!(open_items(&workspace, cx), items);
    let moved = cx.update(|_, cx| {
        panes[0]
            .read(cx)
            .items()
            .filter_map(|item| item.project_path(cx))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        moved,
        vec![paths[2].clone(), paths[0].clone(), paths[1].clone()]
    );
    assert_eq!(focused_pane(&workspace, cx), Some(panes[0].clone()));
    let dedicated = cx.update(|_, cx| {
        workspace
            .read(cx)
            .panes()
            .iter()
            .chain(&panes)
            .any(|pane| pane.read(cx).is_dedicated())
    });
    assert!(!dedicated, "no pane is dedicated");
}

#[gpui::test]
async fn test_delete_window_in_dock_closes_dock(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let center = center_panes(&workspace, cx).remove(0);
    open_in(&workspace, &center, &paths[0], cx).await;
    cx.update(|window, cx| {
        let terminal_pane = new_dock_pane(&workspace, window, cx);
        add_terminal(&workspace, &terminal_pane, window, cx);
        let panel = add_panel(
            &workspace,
            DockPosition::Bottom,
            10,
            Some(terminal_pane),
            true,
            window,
            cx,
        );
        let handle = panel.read(cx).focus_handle.clone();
        window.focus(&handle, cx);
    });
    cx.run_until_parked();
    let dock_open = |cx: &mut VisualTestContext| {
        cx.update(|_, cx| workspace.read(cx).bottom_dock().read(cx).is_open())
    };
    assert!(dock_open(cx));
    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    let items = open_items(&workspace, cx);
    run(DeleteWindow, cx);
    assert!(!dock_open(cx));
    assert_eq!(cx.update(|_, cx| layout_snapshot(&workspace, cx)), before);
    assert_eq!(open_items(&workspace, cx), items);
    assert_eq!(echoed(cx), Vec::<String>::new());
}
