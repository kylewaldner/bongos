//! Tests of where a chosen row opens: resolving a pane choice without changing the workspace, and
//! splitting only when the choice is realized.

use std::{cell::RefCell, rc::Rc};

use bongos_minibuffer::{Candidate, Session, Source, open};
use bongos_window::{
    picker::is_picking,
    test_support::{add_editor, split_center},
};
use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use project::Project;
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace, Pane, SplitDirection, Workspace};
use zed_actions::minibuffer;

use crate::target::{OpenAt, PaneChoice, add_pane_choices, realize, resolve};

const HOST: &str = "navigation_target_test";

async fn setup(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
    let params = cx.update(|cx| {
        let params = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
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
    let project = Project::test(params.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("the window holds a workspace");
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    (workspace, cx)
}

/// `count` center panes in tree order, each showing an editor.
fn center_panes(
    workspace: &Entity<Workspace>,
    count: usize,
    cx: &mut VisualTestContext,
) -> Vec<Entity<Pane>> {
    let panes = cx.update(|window, cx| {
        let panes = split_center(workspace, count, window, cx);
        for pane in &panes {
            add_editor(workspace, pane, window, cx);
        }
        panes
    });
    cx.run_until_parked();
    panes
}

fn activate(workspace: &Entity<Workspace>, pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.focus(&pane.focus_handle(cx), cx));
    cx.run_until_parked();
    assert_eq!(&active(workspace, cx), pane);
}

fn active(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
    workspace.read_with(cx, |workspace, _| workspace.active_pane().clone())
}

fn pane_count(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> usize {
    workspace.read_with(cx, |workspace, _| workspace.center_panes().len())
}

fn resolved(
    workspace: &Entity<Workspace>,
    choice: &PaneChoice,
    cx: &mut VisualTestContext,
) -> crate::target::Resolved {
    workspace.read_with(cx, |workspace, cx| resolve(workspace, choice, cx))
}

fn realized(
    workspace: &Entity<Workspace>,
    resolved: crate::target::Resolved,
    cx: &mut VisualTestContext,
) -> Entity<Pane> {
    let pane = workspace.update_in(cx, |workspace, window, cx| {
        realize(workspace, resolved, window, cx)
    });
    cx.run_until_parked();
    pane
}

fn dedicate(pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    pane.update(cx, |pane, cx| pane.set_dedicated(true, cx));
}

fn expect_pane(resolved: &crate::target::Resolved, pane: &Entity<Pane>) {
    assert_eq!(&resolved.preview_pane, pane);
    match &resolved.open {
        OpenAt::Pane(open) => assert_eq!(open, pane),
        OpenAt::Split { .. } => panic!("expected an existing pane"),
    }
}

fn expect_split(
    resolved: &crate::target::Resolved,
    pane: &Entity<Pane>,
    direction: SplitDirection,
) {
    assert_eq!(&resolved.preview_pane, pane);
    match &resolved.open {
        OpenAt::Split {
            of,
            direction: found,
        } => {
            assert_eq!(of, pane);
            assert_eq!(*found, direction);
        }
        OpenAt::Pane(_) => panic!("expected a pending split"),
    }
}

#[gpui::test]
async fn target_default_is_the_active_pane_or_the_newest_other_one(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 3, cx);
    activate(&workspace, &panes[2], cx);
    activate(&workspace, &panes[0], cx);
    activate(&workspace, &panes[1], cx);

    let found = resolved(&workspace, &PaneChoice::Default, cx);
    expect_pane(&found, &panes[1]);

    dedicate(&panes[1], cx);
    let found = resolved(&workspace, &PaneChoice::Default, cx);
    expect_pane(&found, &panes[0]);
    assert_eq!(pane_count(&workspace, cx), 3);
    assert_eq!(&realized(&workspace, found, cx), &panes[0]);
    assert_eq!(pane_count(&workspace, cx), 3);
}

#[gpui::test]
async fn target_default_with_every_pane_dedicated_splits_right_only_when_realized(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[0], cx);
    for pane in &panes {
        dedicate(pane, cx);
    }

    let found = resolved(&workspace, &PaneChoice::Default, cx);
    expect_split(&found, &panes[0], SplitDirection::Right);
    assert_eq!(pane_count(&workspace, cx), 2);

    let pane = realized(&workspace, found, cx);
    assert_eq!(pane_count(&workspace, cx), 3);
    assert!(!panes.contains(&pane));
    let order = workspace.read_with(cx, |workspace, _| workspace.center_panes());
    assert_eq!(order, [panes[0].clone(), pane, panes[1].clone()]);
}

#[gpui::test]
async fn target_other_pane_is_the_next_center_pane_or_a_split(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let only = active(&workspace, cx);
    let found = resolved(&workspace, &PaneChoice::OtherPane, cx);
    expect_split(&found, &only, SplitDirection::Right);
    assert_eq!(pane_count(&workspace, cx), 1);
    let split = realized(&workspace, found, cx);
    assert_eq!(pane_count(&workspace, cx), 2);
    assert_ne!(split, only);

    let panes = center_panes(&workspace, 3, cx);
    activate(&workspace, &panes[1], cx);
    expect_pane(&resolved(&workspace, &PaneChoice::OtherPane, cx), &panes[2]);
    activate(&workspace, &panes[2], cx);
    let wrapped = resolved(&workspace, &PaneChoice::OtherPane, cx);
    expect_pane(&wrapped, &panes[0]);
    assert_eq!(&realized(&workspace, wrapped, cx), &panes[0]);
    assert_eq!(pane_count(&workspace, cx), 3);
}

#[gpui::test]
async fn target_split_choices_split_the_active_pane_when_realized(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[0], cx);

    let right = resolved(&workspace, &PaneChoice::SplitRight, cx);
    expect_split(&right, &panes[0], SplitDirection::Right);
    let down = resolved(&workspace, &PaneChoice::SplitDown, cx);
    expect_split(&down, &panes[0], SplitDirection::Down);
    assert_eq!(pane_count(&workspace, cx), 2);

    let below = realized(&workspace, down, cx);
    let beside = realized(&workspace, right, cx);
    assert_eq!(pane_count(&workspace, cx), 4);
    assert!(!panes.contains(&below) && !panes.contains(&beside));
    assert_ne!(below, beside);
}

type Chosen = Rc<RefCell<Vec<(String, Entity<Pane>)>>>;

fn choose(workspace: &Entity<Workspace>, chosen: &Chosen, keys: &str, cx: &mut VisualTestContext) {
    let weak = workspace.downgrade();
    let recorded = chosen.clone();
    workspace.update_in(cx, |workspace, window, cx| {
        let session =
            Session::completing_read(HOST, "Open: ").source(Source::Fixed(vec![Candidate::new(
                "row",
                "row".to_string(),
            )]));
        let session = add_pane_choices(session, weak, move |value, pane, _, _| {
            let row = value.downcast_ref::<String>().cloned().unwrap_or_default();
            recorded.borrow_mut().push((row, pane));
        });
        open(workspace, session, window, cx);
    });
    cx.run_until_parked();
    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    cx.simulate_keystrokes(keys);
    cx.run_until_parked();
}

#[gpui::test]
async fn target_pane_choices_hand_the_realized_pane_to_the_picker(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[0], cx);
    let chosen = Chosen::default();

    choose(&workspace, &chosen, "o", cx);
    assert_eq!(chosen.borrow().len(), 1);
    assert_eq!(chosen.borrow()[0].0, "row");
    assert_eq!(chosen.borrow()[0].1, panes[1]);
    assert_eq!(pane_count(&workspace, cx), 2);

    choose(&workspace, &chosen, "r", cx);
    choose(&workspace, &chosen, "d", cx);
    assert_eq!(chosen.borrow().len(), 3);
    assert_eq!(pane_count(&workspace, cx), 4);
    let new_panes: Vec<_> = chosen.borrow()[1..]
        .iter()
        .map(|(_, pane)| pane.clone())
        .collect();
    assert!(new_panes.iter().all(|pane| !panes.contains(pane)));
    assert_ne!(new_panes[0], new_panes[1]);

    // Pane by letter (p) starts the pane picker.
    choose(&workspace, &chosen, "p", cx);
    assert!(cx.update(|window, cx| is_picking(window, cx)));
    assert_eq!(chosen.borrow().len(), 3);
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
}

#[gpui::test]
async fn target_pane_by_letter_picks_a_pane_and_cancel_opens_nothing(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 3, cx);
    activate(&workspace, &panes[0], cx);
    let chosen = Chosen::default();

    choose(&workspace, &chosen, "p", cx);
    assert!(cx.update(|window, cx| is_picking(window, cx)));
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
    assert!(!cx.update(|window, cx| is_picking(window, cx)));
    assert!(chosen.borrow().is_empty());
    assert_eq!(pane_count(&workspace, cx), 3);

    choose(&workspace, &chosen, "p", cx);
    assert!(chosen.borrow().is_empty());
    cx.simulate_keystrokes("d");
    cx.run_until_parked();
    assert_eq!(chosen.borrow().len(), 1);
    assert_eq!(chosen.borrow()[0].1, panes[2]);
    assert_eq!(pane_count(&workspace, cx), 3);
}
