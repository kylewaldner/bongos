use bongos_emacs::prefix_arg::{Meaning, meaning};
use gpui::{Entity, TestAppContext, VisualTestContext};
use project::Project;
use workspace::{AppState, Pane, Workspace, dock::DockPosition};

use crate::actions::{IGNORED, READS};
use crate::targets::{letters, recent_other_item, targets};
use crate::test_support::{add_editor, add_panel, add_terminal, new_dock_pane, split_center};

async fn setup(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
    let params = cx.update(AppState::test);
    cx.update(|cx| {
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
    let project = Project::test(params.fs.clone(), [], cx).await;
    cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx))
}

fn current_targets(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Entity<Pane>> {
    cx.update(|window, cx| targets(workspace.read(cx), window, cx))
}

#[test]
fn letters_start_on_the_home_row_then_the_top_row_and_stop_at_nineteen() {
    assert_eq!(letters(0), Vec::<char>::new());
    assert_eq!(letters(3), vec!['a', 's', 'd']);
    assert_eq!(
        letters(19).iter().collect::<String>(),
        "asdfghjklqwertyuiop"
    );
    assert_eq!(letters(40), letters(19));
}

#[gpui::test]
async fn recent_other_item_skips_the_current_item(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let (first, second, older, newer, current) = cx.update(|window, cx| {
        let panes = split_center(&workspace, 2, window, cx);
        let older = add_editor(&workspace, &panes[0], window, cx);
        let newer = add_editor(&workspace, &panes[1], window, cx);
        let current = add_editor(&workspace, &panes[0], window, cx);
        (panes[0].clone(), panes[1].clone(), older, newer, current)
    });
    let chosen = cx.update(|_, cx| recent_other_item(workspace.read(cx), &first, cx));
    let chosen = chosen.expect("another item can be shown");
    assert_ne!(chosen.item_id(), current.entity_id());
    assert_ne!(chosen.item_id(), older.entity_id());
    assert_eq!(chosen.item_id(), newer.entity_id());

    // From the other pane, the most recent item elsewhere is the one just added to the first.
    let from_second = cx.update(|_, cx| recent_other_item(workspace.read(cx), &second, cx));
    assert_eq!(
        from_second.map(|item| item.item_id()),
        Some(current.entity_id())
    );
}

#[gpui::test]
async fn recent_other_item_falls_back_to_the_current_item_then_to_none(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let pane = cx.update(|_, cx| workspace.read(cx).active_pane().clone());
    let none = cx.update(|_, cx| recent_other_item(workspace.read(cx), &pane, cx));
    assert!(none.is_none());
    let only = cx.update(|window, cx| add_editor(&workspace, &pane, window, cx));
    let chosen = cx.update(|_, cx| recent_other_item(workspace.read(cx), &pane, cx));
    assert_eq!(chosen.map(|item| item.item_id()), Some(only.entity_id()));
}

#[gpui::test]
async fn controller_test_and_terminal_are_targets_in_that_order(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let (controller, test, terminal) = cx.update(|window, cx| {
        let panes = split_center(&workspace, 2, window, cx);
        add_editor(&workspace, &panes[0], window, cx);
        add_editor(&workspace, &panes[1], window, cx);
        let terminal_pane = new_dock_pane(&workspace, window, cx);
        add_terminal(&workspace, &terminal_pane, window, cx);
        add_panel(
            &workspace,
            DockPosition::Bottom,
            10,
            Some(terminal_pane.clone()),
            true,
            window,
            cx,
        );
        (panes[0].clone(), panes[1].clone(), terminal_pane)
    });
    assert_eq!(
        current_targets(&workspace, cx),
        vec![controller, test, terminal]
    );
}

#[gpui::test]
async fn the_left_dock_comes_first_and_the_bottom_dock_last(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let (left, center, right, bottom) = cx.update(|window, cx| {
        let center = workspace.read(cx).active_pane().clone();
        add_editor(&workspace, &center, window, cx);
        let mut docks = Vec::new();
        for (position, priority) in [
            (DockPosition::Bottom, 10),
            (DockPosition::Right, 11),
            (DockPosition::Left, 12),
        ] {
            let pane = new_dock_pane(&workspace, window, cx);
            add_editor(&workspace, &pane, window, cx);
            add_panel(
                &workspace,
                position,
                priority,
                Some(pane.clone()),
                true,
                window,
                cx,
            );
            docks.push(pane);
        }
        let left = docks.pop().unwrap();
        let right = docks.pop().unwrap();
        let bottom = docks.pop().unwrap();
        (left, center, right, bottom)
    });
    assert_eq!(
        current_targets(&workspace, cx),
        vec![left, center, right, bottom]
    );
}

#[gpui::test]
async fn a_work_panel_and_a_closed_dock_are_skipped(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let center = cx.update(|window, cx| {
        let center = workspace.read(cx).active_pane().clone();
        add_editor(&workspace, &center, window, cx);
        // A panel whose pane shows no editor or terminal, like the Work panel.
        let work_pane = new_dock_pane(&workspace, window, cx);
        add_panel(
            &workspace,
            DockPosition::Left,
            10,
            Some(work_pane),
            true,
            window,
            cx,
        );
        // A panel with no pane at all, like the project panel.
        add_panel(&workspace, DockPosition::Bottom, 11, None, true, window, cx);
        // A closed dock whose pane does show an editor.
        let hidden = new_dock_pane(&workspace, window, cx);
        add_editor(&workspace, &hidden, window, cx);
        add_panel(
            &workspace,
            DockPosition::Right,
            12,
            Some(hidden),
            false,
            window,
            cx,
        );
        center
    });
    assert_eq!(current_targets(&workspace, cx), vec![center]);
}

/// The real terminal panel and the Chats panel are too heavy to build in this crate's tests, so
/// test panels stand in: one whose pane shows a display-only terminal, one whose pane shows an
/// editor, as a Chats session's prompts pane does.
#[gpui::test]
async fn a_terminal_panel_and_a_chats_session_with_prompts_are_targets(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let (terminal, prompts) = cx.update(|window, cx| {
        let terminal = new_dock_pane(&workspace, window, cx);
        add_terminal(&workspace, &terminal, window, cx);
        add_panel(
            &workspace,
            DockPosition::Bottom,
            10,
            Some(terminal.clone()),
            true,
            window,
            cx,
        );
        let prompts = new_dock_pane(&workspace, window, cx);
        add_editor(&workspace, &prompts, window, cx);
        add_panel(
            &workspace,
            DockPosition::Right,
            11,
            Some(prompts.clone()),
            true,
            window,
            cx,
        );
        (terminal, prompts)
    });
    let found = current_targets(&workspace, cx);
    assert!(found.contains(&terminal));
    assert!(found.contains(&prompts));
}

#[gpui::test]
async fn a_zoomed_window_still_lists_every_pane(cx: &mut TestAppContext) {
    let (workspace, cx) = setup(cx).await;
    let panes = cx.update(|window, cx| {
        let panes = split_center(&workspace, 3, window, cx);
        for pane in &panes {
            add_editor(&workspace, pane, window, cx);
        }
        panes
    });
    cx.update(|window, cx| {
        panes[2].update(cx, |pane, cx| pane.zoom_in(&workspace::ZoomIn, window, cx));
    });
    cx.run_until_parked();
    assert!(cx.update(|_, cx| panes[2].read(cx).is_zoomed()));
    assert_eq!(current_targets(&workspace, cx), panes);
}

#[gpui::test]
async fn the_split_actions_read_the_prefix_argument_and_the_others_ignore_it(
    cx: &mut TestAppContext,
) {
    cx.update(|cx| {
        bongos_emacs::init(cx);
        crate::init(cx);
        assert_eq!(READS.len() + IGNORED.len(), 11);
        for name in READS {
            assert!(matches!(meaning(cx, name), Some(Meaning::Reads)), "{name}");
        }
        for name in IGNORED {
            assert!(
                matches!(meaning(cx, name), Some(Meaning::Ignored)),
                "{name}"
            );
        }
    });
}
