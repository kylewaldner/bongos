use editor::Editor;
use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle, point};
use project::{Fs as _, Project, ProjectPath};
use workspace::{
    AppState, MultiWorkspace, Pane, SaveIntent, Workspace, WorkspaceHandle as _,
    dock::{DockPosition, test::TestPanel},
    pane_group::Member,
};

use crate::actions::ToggleDeleteOtherWindows;
use crate::test_support::{
    add_editor, add_panel, add_terminal, layout_snapshot, new_dock_pane, split_center,
};

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

/// `count` center panes, each showing a new editor, with focus in the first one.
fn panes_with_editors(
    workspace: &Entity<Workspace>,
    count: usize,
    cx: &mut VisualTestContext,
) -> (Vec<Entity<Pane>>, Vec<Entity<Editor>>) {
    let result = cx.update(|window, cx| {
        let panes = split_center(workspace, count, window, cx);
        let editors: Vec<_> = panes
            .iter()
            .map(|pane| add_editor(workspace, pane, window, cx))
            .collect();
        window.focus(&editors[0].focus_handle(cx), cx);
        (panes, editors)
    });
    cx.run_until_parked();
    result
}

fn focus_editor(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    cx.run_until_parked();
}

fn toggle(cx: &mut VisualTestContext) {
    cx.dispatch_action(ToggleDeleteOtherWindows);
    cx.run_until_parked();
}

fn is_zoomed(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| workspace.read(cx).zoomed_item().is_some())
}

fn zoomed_panes(panes: &[Entity<Pane>], cx: &mut VisualTestContext) -> Vec<bool> {
    cx.update(|_, cx| panes.iter().map(|pane| pane.read(cx).is_zoomed()).collect())
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

/// Opens `path` in `pane` and waits for it.
async fn open_in(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    path: &ProjectPath,
    allow_preview: bool,
    cx: &mut VisualTestContext,
) {
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path_preview(
                path.clone(),
                Some(pane.downgrade()),
                true,
                allow_preview,
                true,
                window,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();
}

#[gpui::test]
async fn three_panes_show_one_pane_and_close_none(cx: &mut TestAppContext) {
    let (workspace, _, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 3, cx);
    focus_editor(&editors[1], cx);
    let items = open_items(&workspace, cx);
    assert_eq!(items, 3);
    toggle(cx);
    assert!(is_zoomed(&workspace, cx));
    assert_eq!(zoomed_panes(&panes, cx), vec![false, true, false]);
    assert_eq!(cx.update(|_, cx| workspace.read(cx).center_panes()), panes);
    assert_eq!(open_items(&workspace, cx), items);
    for (editor, pane) in editors.iter().zip(&panes) {
        let ids = cx.update(|_, cx| {
            pane.read(cx)
                .items()
                .map(|item| item.item_id())
                .collect::<Vec<_>>()
        });
        assert_eq!(ids, vec![editor.entity_id()]);
    }
}

#[gpui::test]
async fn a_second_press_restores_the_layout_exactly(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 3, cx);
    // The first pane gets a pinned tab, a normal file tab and a preview tab.
    cx.update(|_, cx| panes[0].update(cx, |pane, _| pane.set_pinned_count(1)));
    open_in(&workspace, &panes[0], &paths[0], false, cx).await;
    open_in(&workspace, &panes[0], &paths[1], true, cx).await;
    open_in(&workspace, &panes[0], &paths[0], false, cx).await;
    let file_editor = cx.update(|_, cx| {
        panes[0]
            .read(cx)
            .items()
            .find_map(|item| {
                item.act_as::<Editor>(cx)
                    .filter(|editor| *editor != editors[0])
            })
            .expect("a file editor")
    });
    file_editor.update_in(cx, |editor, window, cx| {
        editor.set_scroll_position(point(0., 5.), window, cx)
    });
    cx.update(|_, cx| {
        let root = workspace.read(cx).center_group().root.clone();
        let Member::Axis(axis) = root else {
            panic!("the three panes share one axis")
        };
        *axis.flexes.lock() = vec![0.5, 1.0, 1.5];
    });
    focus_editor(&editors[1], cx);

    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    let items = open_items(&workspace, cx);
    // The snapshot has something to lose in every field.
    let crate::test_support::TreeSnapshot::Axis {
        members, flexes, ..
    } = &before.tree
    else {
        panic!("an axis")
    };
    assert_eq!(flexes, &vec![0.5, 1.0, 1.5]);
    let crate::test_support::TreeSnapshot::Pane(first) = &members[0] else {
        panic!("a pane")
    };
    assert_eq!(first.pinned_count, 1);
    assert!(first.preview_item.is_some());
    assert!(first.nav_history.0 > 0);
    assert!(first.scroll_positions.contains(&Some(point(0., 5.))));

    toggle(cx);
    assert!(is_zoomed(&workspace, cx));
    assert_eq!(zoomed_panes(&panes, cx), vec![false, true, false]);
    assert_eq!(open_items(&workspace, cx), items);
    toggle(cx);
    assert!(!is_zoomed(&workspace, cx));
    assert_eq!(zoomed_panes(&panes, cx), vec![false; 3]);
    assert_eq!(open_items(&workspace, cx), items);
    assert_eq!(cx.update(|_, cx| layout_snapshot(&workspace, cx)), before);
}

#[gpui::test]
async fn one_pane_does_nothing(cx: &mut TestAppContext) {
    let (workspace, _, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, _) = panes_with_editors(&workspace, 1, cx);
    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    toggle(cx);
    assert!(!is_zoomed(&workspace, cx));
    assert_eq!(zoomed_panes(&panes, cx), vec![false]);
    assert_eq!(cx.update(|_, cx| layout_snapshot(&workspace, cx)), before);
}

/// A center pane with an editor, and a bottom dock whose pane shows a display-only terminal.
fn editor_and_terminal_dock(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (Entity<Pane>, Entity<Pane>, Entity<TestPanel>) {
    let result = cx.update(|window, cx| {
        let center = workspace.read(cx).active_pane().clone();
        let editor = add_editor(workspace, &center, window, cx);
        let terminal_pane = new_dock_pane(workspace, window, cx);
        add_terminal(workspace, &terminal_pane, window, cx);
        let panel = add_panel(
            workspace,
            DockPosition::Bottom,
            10,
            Some(terminal_pane.clone()),
            true,
            window,
            cx,
        );
        window.focus(&editor.focus_handle(cx), cx);
        (center, terminal_pane, panel)
    });
    cx.run_until_parked();
    result
}

fn panel_zoomed(panel: &Entity<TestPanel>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| panel.read(cx).zoomed)
}

#[gpui::test]
async fn a_focused_terminal_dock_zooms_through_its_panel_and_a_second_press_unzooms(
    cx: &mut TestAppContext,
) {
    let (workspace, _, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (center, _, panel) = editor_and_terminal_dock(&workspace, cx);
    let items = open_items(&workspace, cx);
    cx.update(|window, cx| {
        let handle = panel.read(cx).focus_handle.clone();
        window.focus(&handle, cx)
    });
    cx.run_until_parked();
    assert!(!panel_zoomed(&panel, cx));
    toggle(cx);
    assert!(panel_zoomed(&panel, cx));
    assert!(is_zoomed(&workspace, cx));
    assert_eq!(zoomed_panes(std::slice::from_ref(&center), cx), vec![false]);
    toggle(cx);
    assert!(!panel_zoomed(&panel, cx));
    assert!(!is_zoomed(&workspace, cx));
    assert_eq!(open_items(&workspace, cx), items);
}

#[gpui::test]
async fn an_empty_center_pane_is_not_zoomed(cx: &mut TestAppContext) {
    let (workspace, _, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (center, _, panel) = editor_and_terminal_dock(&workspace, cx);
    let empty = cx.update(|window, cx| {
        let empty = workspace.update(cx, |workspace, cx| {
            workspace.split_pane(center.clone(), workspace::SplitDirection::Right, window, cx)
        });
        window.focus(&empty.focus_handle(cx), cx);
        empty
    });
    cx.run_until_parked();
    toggle(cx);
    assert!(!is_zoomed(&workspace, cx));
    assert!(!panel_zoomed(&panel, cx));
    assert_eq!(zoomed_panes(&[center, empty], cx), vec![false, false]);
}

#[gpui::test]
async fn a_file_opened_while_zoomed_lands_in_the_zoomed_pane_and_stays_zoomed(
    cx: &mut TestAppContext,
) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 3, cx);
    focus_editor(&editors[2], cx);
    toggle(cx);
    assert_eq!(zoomed_panes(&panes, cx), vec![false, false, true]);
    let counts = |cx: &mut VisualTestContext| -> Vec<usize> {
        cx.update(|_, cx| panes.iter().map(|pane| pane.read(cx).items_len()).collect())
    };
    assert_eq!(counts(cx), vec![1, 1, 1]);
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path_preview(paths[0].clone(), None, true, true, true, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert_eq!(counts(cx), vec![1, 1, 2]);
    assert!(is_zoomed(&workspace, cx));
    assert_eq!(zoomed_panes(&panes, cx), vec![false, false, true]);
    let opened = cx.update(|_, cx| {
        panes[2]
            .read(cx)
            .active_item()
            .and_then(|item| item.project_path(cx))
    });
    assert_eq!(opened, Some(paths[0].clone()));
}

#[gpui::test]
async fn a_hidden_pane_whose_last_tab_closes_is_gone_after_unzooming(cx: &mut TestAppContext) {
    let (workspace, _, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 3, cx);
    focus_editor(&editors[2], cx);
    toggle(cx);
    assert!(is_zoomed(&workspace, cx));
    let id = editors[0].entity_id();
    panes[0]
        .update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(id, SaveIntent::Skip, window, cx)
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert!(is_zoomed(&workspace, cx), "closing a hidden tab unzoomed");
    toggle(cx);
    assert!(!is_zoomed(&workspace, cx));
    let remaining = cx.update(|_, cx| workspace.read(cx).center_panes());
    assert_eq!(remaining, vec![panes[1].clone(), panes[2].clone()]);
    assert_eq!(zoomed_panes(&remaining, cx), vec![false, false]);
}

#[gpui::test]
async fn focusing_a_hidden_pane_unzooms(cx: &mut TestAppContext) {
    let (workspace, _, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let (panes, editors) = panes_with_editors(&workspace, 3, cx);
    focus_editor(&editors[2], cx);
    toggle(cx);
    assert!(is_zoomed(&workspace, cx));
    focus_editor(&editors[0], cx);
    assert!(!is_zoomed(&workspace, cx));
    assert_eq!(zoomed_panes(&panes, cx), vec![false; 3]);
}
