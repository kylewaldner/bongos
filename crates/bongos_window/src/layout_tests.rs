use editor::Editor;
use gpui::{
    Action, Entity, EntityId, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use project::{Fs as _, Project, ProjectPath};
use workspace::{
    AppState, MultiWorkspace, Pane, Workspace, WorkspaceHandle as _, pane_group::Member,
};

use crate::actions::{BalancePanes, MoveBufferToPane, SwapPanes, SwitchWindow};
use crate::picker::is_picking;
use crate::test_support::{TreeSnapshot, add_editor, layout_snapshot, split_center};

/// A workspace in a multi-workspace window on a project of three files, `one.txt`, `two.txt`
/// and `three.txt`, none open yet.
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
        let text = (1..=10)
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

fn press(keys: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(keys);
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

fn active_id(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<EntityId> {
    cx.update(|_, cx| pane.read(cx).active_item().map(|item| item.item_id()))
}

fn item_ids(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Vec<EntityId> {
    cx.update(|_, cx| pane.read(cx).items().map(|item| item.item_id()).collect())
}

/// How many tabs are open in the center.
fn open_items(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    cx.update(|_, cx| {
        workspace
            .read(cx)
            .center_panes()
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

fn badges(panes: &[Entity<Pane>], cx: &mut VisualTestContext) -> Vec<Option<String>> {
    cx.update(|_, cx| {
        panes
            .iter()
            .map(|pane| pane.read(cx).badge().map(ToString::to_string))
            .collect()
    })
}

fn picking(cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| is_picking(window, cx))
}

fn root_flexes(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<f32> {
    match cx.update(|_, cx| layout_snapshot(workspace, cx)).tree {
        TreeSnapshot::Axis { flexes, .. } => flexes,
        TreeSnapshot::Pane(_) => Vec::new(),
    }
}

/// Three center panes side by side: the first shows `one.txt` then `two.txt` (current), the
/// second `three.txt`, the third a new editor. Focus is in the first.
async fn three_panes(
    workspace: &Entity<Workspace>,
    paths: &[ProjectPath; 3],
    cx: &mut VisualTestContext,
) -> Vec<Entity<Pane>> {
    let panes = cx.update(|window, cx| split_center(workspace, 3, window, cx));
    cx.run_until_parked();
    open_in(workspace, &panes[0], &paths[0], cx).await;
    open_in(workspace, &panes[0], &paths[1], cx).await;
    open_in(workspace, &panes[1], &paths[2], cx).await;
    cx.update(|window, cx| add_editor(workspace, &panes[2], window, cx));
    cx.run_until_parked();
    focus_active_item(&panes[0], cx);
    panes
}

#[gpui::test]
async fn test_balance_equalizes_resized_panes(cx: &mut TestAppContext) {
    let (workspace, _, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = cx.update(|window, cx| split_center(&workspace, 3, window, cx));
    cx.run_until_parked();
    cx.update(|_, cx| {
        let root = workspace.read(cx).center_group().root.clone();
        let Member::Axis(axis) = root else {
            panic!("the center is a split");
        };
        *axis.flexes.lock() = vec![0.4, 1.9, 0.7];
    });
    cx.run_until_parked();
    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    run(BalancePanes, cx);
    assert_eq!(root_flexes(&workspace, cx), vec![1.0; 3]);
    let widths = cx.update(|_, cx| {
        let workspace = workspace.read(cx);
        panes
            .iter()
            .filter_map(|pane| workspace.center_group().bounding_box_for_pane(pane))
            .map(|bounds| f32::from(bounds.size.width))
            .collect::<Vec<_>>()
    });
    assert_eq!(widths.len(), 3, "every pane is laid out");
    let min = widths.iter().copied().fold(f32::MAX, f32::min);
    let max = widths.iter().copied().fold(f32::MIN, f32::max);
    assert!(max - min < 8.0, "widths within a column: {widths:?}");
    assert_eq!(center_panes(&workspace, cx), panes);
    // Only the sizes changed.
    let after = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    match (before.tree, after.tree) {
        (TreeSnapshot::Axis { members: a, .. }, TreeSnapshot::Axis { members: b, .. }) => {
            assert_eq!(a, b)
        }
        _ => panic!("the center is a split"),
    }
}

#[gpui::test]
async fn test_swap_swaps_tabs_and_keeps_focus_in_place(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = cx.update(|window, cx| split_center(&workspace, 2, window, cx));
    cx.run_until_parked();
    open_in(&workspace, &panes[0], &paths[0], cx).await;
    open_in(&workspace, &panes[1], &paths[1], cx).await;
    open_in(&workspace, &panes[1], &paths[2], cx).await;
    focus_active_item(&panes[0], cx);
    let first = item_ids(&panes[0], cx);
    let second = item_ids(&panes[1], cx);
    let items = open_items(&workspace, cx);
    run(SwapPanes, cx);
    let after = center_panes(&workspace, cx);
    assert_eq!(after, vec![panes[1].clone(), panes[0].clone()]);
    assert_eq!(
        item_ids(&after[0], cx),
        second,
        "the tabs moved with their pane"
    );
    assert_eq!(item_ids(&after[1], cx), first);
    assert_eq!(active_path(&after[0], cx), Some(paths[2].clone()));
    assert_eq!(
        focused_pane(&workspace, cx),
        Some(after[0].clone()),
        "focus stays on the left"
    );
    assert_eq!(open_items(&workspace, cx), items);
}

#[gpui::test]
async fn test_swap_with_one_pane_changes_nothing(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let pane = center_panes(&workspace, cx).remove(0);
    open_in(&workspace, &pane, &paths[0], cx).await;
    open_in(&workspace, &pane, &paths[1], cx).await;
    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    run(SwapPanes, cx);
    assert_eq!(cx.update(|_, cx| layout_snapshot(&workspace, cx)), before);
    assert_eq!(focused_pane(&workspace, cx), Some(pane));
}

#[gpui::test]
async fn test_move_buffer_picker_shows_switch_window_letters(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = three_panes(&workspace, &paths, cx).await;
    run(SwitchWindow, cx);
    let switch_letters = badges(&panes, cx);
    press("z", cx);
    assert_eq!(badges(&panes, cx), vec![None; 3]);
    run(MoveBufferToPane, cx);
    assert!(picking(cx));
    assert_eq!(badges(&panes, cx), switch_letters);
    assert_eq!(
        switch_letters,
        vec![Some("a".into()), Some("s".into()), Some("d".into())]
    );
    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    press("z", cx);
    assert!(!picking(cx));
    assert_eq!(
        badges(&panes, cx),
        vec![None; 3],
        "a cancel clears the letters"
    );
    assert_eq!(cx.update(|_, cx| layout_snapshot(&workspace, cx)), before);
}

#[gpui::test]
async fn test_move_buffer_moves_active_item_and_focuses_destination(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = three_panes(&workspace, &paths, cx).await;
    let moved = active_id(&panes[0], cx).unwrap();
    let items = open_items(&workspace, cx);
    run(MoveBufferToPane, cx);
    press("d", cx);
    assert_eq!(
        badges(&panes, cx),
        vec![None; 3],
        "a pick clears the letters"
    );
    assert_eq!(center_panes(&workspace, cx), panes);
    assert_eq!(active_id(&panes[2], cx), Some(moved));
    assert_eq!(item_ids(&panes[2], cx).len(), 2);
    assert!(!item_ids(&panes[0], cx).contains(&moved));
    assert_eq!(focused_pane(&workspace, cx), Some(panes[2].clone()));
    assert_eq!(open_items(&workspace, cx), items);
}

#[gpui::test]
async fn test_move_buffer_source_shows_previous_item(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = three_panes(&workspace, &paths, cx).await;
    run(MoveBufferToPane, cx);
    press("s", cx);
    assert_eq!(active_path(&panes[1], cx), Some(paths[1].clone()));
    assert_eq!(active_path(&panes[0], cx), Some(paths[0].clone()));
    assert_eq!(item_ids(&panes[0], cx).len(), 1);
}

#[gpui::test]
async fn test_move_buffer_single_item_source_shows_other_buffer(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = cx.update(|window, cx| split_center(&workspace, 2, window, cx));
    cx.run_until_parked();
    open_in(&workspace, &panes[0], &paths[0], cx).await;
    open_in(&workspace, &panes[1], &paths[1], cx).await;
    focus_active_item(&panes[0], cx);
    let moved = active_id(&panes[0], cx).unwrap();
    let other = active_id(&panes[1], cx).unwrap();
    // With two targets the other pane is picked at once.
    run(MoveBufferToPane, cx);
    assert_eq!(center_panes(&workspace, cx), panes, "the source pane stays");
    assert_eq!(active_id(&panes[1], cx), Some(moved));
    assert_eq!(item_ids(&panes[1], cx), vec![other, moved]);
    assert_eq!(focused_pane(&workspace, cx), Some(panes[1].clone()));
    assert_eq!(active_path(&panes[0], cx), Some(paths[1].clone()));
    let clone = active_id(&panes[0], cx).unwrap();
    assert_ne!(clone, other, "the source shows a clone, not the same tab");
}

#[gpui::test]
async fn test_move_buffer_to_pane_with_same_file_keeps_its_editor(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = cx.update(|window, cx| split_center(&workspace, 2, window, cx));
    cx.run_until_parked();
    open_in(&workspace, &panes[1], &paths[0], cx).await;
    open_in(&workspace, &panes[0], &paths[1], cx).await;
    open_in(&workspace, &panes[0], &paths[0], cx).await;
    let existing = active_id(&panes[1], cx).unwrap();
    let moved = active_id(&panes[0], cx).unwrap();
    assert_ne!(existing, moved);
    let editor = cx.update(|_, cx| {
        panes[0]
            .read(cx)
            .active_item()
            .and_then(|item| item.act_as::<Editor>(cx))
            .unwrap()
    });
    cx.update(|window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
        editor.update(cx, |editor, cx| editor.insert("typed ", window, cx));
    });
    cx.run_until_parked();
    let text = cx.update(|_, cx| editor.read(cx).text(cx));
    assert!(text.starts_with("typed "));
    drop(editor);
    run(MoveBufferToPane, cx);
    assert_eq!(center_panes(&workspace, cx), panes);
    assert_eq!(item_ids(&panes[1], cx), vec![existing]);
    assert_eq!(focused_pane(&workspace, cx), Some(panes[1].clone()));
    let kept_text = cx.update(|window, cx| {
        let item = panes[1].read(cx).active_item().unwrap();
        assert!(item.item_focus_handle(cx).contains_focused(window, cx));
        item.act_as::<Editor>(cx).unwrap().read(cx).text(cx)
    });
    assert_eq!(kept_text, text, "no text is lost");
    assert_eq!(active_path(&panes[0], cx), Some(paths[1].clone()));
}

#[gpui::test]
async fn test_move_buffer_without_active_item_does_nothing(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = cx.update(|window, cx| split_center(&workspace, 3, window, cx));
    cx.run_until_parked();
    open_in(&workspace, &panes[1], &paths[0], cx).await;
    open_in(&workspace, &panes[2], &paths[1], cx).await;
    cx.update(|window, cx| window.focus(&panes[0].focus_handle(cx), cx));
    cx.run_until_parked();
    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    run(MoveBufferToPane, cx);
    assert!(!picking(cx));
    assert_eq!(badges(&panes, cx), vec![None; 3]);
    assert_eq!(cx.update(|_, cx| layout_snapshot(&workspace, cx)), before);
}

#[gpui::test]
async fn test_move_buffer_picking_current_pane_does_nothing(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = three_panes(&workspace, &paths, cx).await;
    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    run(MoveBufferToPane, cx);
    press("a", cx);
    assert_eq!(badges(&panes, cx), vec![None; 3]);
    assert_eq!(cx.update(|_, cx| layout_snapshot(&workspace, cx)), before);
    assert_eq!(focused_pane(&workspace, cx), Some(panes[0].clone()));
}

#[gpui::test]
async fn test_move_buffer_to_pane_closed_before_pick_is_dropped(cx: &mut TestAppContext) {
    let (workspace, paths, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = three_panes(&workspace, &paths, cx).await;
    run(MoveBufferToPane, cx);
    assert!(picking(cx));
    // The third pane closes with its only tab while the picker waits.
    cx.update(|window, cx| {
        let item = panes[2].read(cx).active_item().unwrap().item_id();
        panes[2].update(cx, |pane, cx| {
            pane.remove_item(item, false, true, window, cx)
        });
    });
    cx.run_until_parked();
    assert_eq!(center_panes(&workspace, cx), panes[..2].to_vec());
    assert!(picking(cx), "the picker still waits");
    let before = cx.update(|_, cx| layout_snapshot(&workspace, cx));
    press("d", cx);
    assert!(!picking(cx));
    assert_eq!(cx.update(|_, cx| layout_snapshot(&workspace, cx)), before);
    assert_eq!(active_path(&panes[0], cx), Some(paths[1].clone()));
}
