//! Tests of the buffer switcher: the rows and their order, narrowing, the preview while a row is
//! highlighted, and what confirming does.

use bongos_minibuffer::{Candidate, SessionHandle};
use bongos_window::test_support::{add_terminal, split_center};
use gpui::{
    Action as _, App, AppContext as _, BorrowAppContext as _, Context, Entity, EntityId,
    EventEmitter, FocusHandle, Focusable, IntoElement, NoAction, Pixels, Render, Styled as _,
    TestAppContext, VisualTestContext, Window, WindowHandle, div, px,
};
use project::Project;
use settings::SettingsStore;
use terminal_view::TerminalView;
use ui::{IconName, ParentElement as _};
use workspace::{
    AppState, MultiWorkspace, Pane, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    item::test::TestItem,
};
use zed_actions::minibuffer;

use crate::actions::SwitchBuffer;
use crate::switcher::{HOST, SwitchTarget, candidates, start};
use crate::test_support::{WorldSnapshot, world_snapshot};

async fn setup(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
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

/// Adds an item titled `title` to `pane` as its active item and focuses the pane.
fn add_item(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    title: &'static str,
    dirty: bool,
    cx: &mut VisualTestContext,
) -> Entity<TestItem> {
    let item = cx.update(|_, cx| {
        cx.new(|cx| {
            let mut item = TestItem::new(cx);
            item.tab_descriptions = Some(vec![title]);
            item.is_dirty = dirty;
            item
        })
    });
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item(
            pane.clone(),
            Box::new(item.clone()),
            None,
            true,
            true,
            window,
            cx,
        );
    });
    cx.run_until_parked();
    item
}

/// A dock panel that draws its pane, as the terminal panel does, so the focus given to an item
/// of the pane stays in the window's tree.
struct PanePanel {
    focus_handle: FocusHandle,
    pane: Entity<Pane>,
}

impl EventEmitter<PanelEvent> for PanePanel {}

impl Focusable for PanePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PanePanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.pane.clone())
    }
}

impl Panel for PanePanel {
    fn persistent_name() -> &'static str {
        "NavigationTestPanel"
    }

    fn panel_key() -> &'static str {
        "NavigationTestPanel"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Bottom
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Bottom
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(200.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        None
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        None
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        NoAction.boxed_clone()
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.pane.clone())
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

/// A bottom dock, closed, whose only pane shows a terminal.
fn closed_terminal_dock(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> (Entity<Pane>, Entity<TerminalView>) {
    let made = cx.update(|window, cx| {
        let (clock, project) = workspace.read_with(cx, |workspace, _| {
            (
                workspace.pane_history_timestamp(),
                workspace.project().clone(),
            )
        });
        let weak = workspace.downgrade();
        let pane = cx.new(|cx| {
            Pane::new(
                weak,
                project,
                clock,
                None,
                NoAction.boxed_clone(),
                false,
                window,
                cx,
            )
        });
        let panel = cx.new(|cx| PanePanel {
            focus_handle: cx.focus_handle(),
            pane: pane.clone(),
        });
        workspace.update(cx, |workspace, cx| workspace.add_panel(panel, window, cx));
        let terminal = add_terminal(workspace, &pane, window, cx);
        (pane, terminal)
    });
    cx.run_until_parked();
    made
}

fn center_panes(
    workspace: &Entity<Workspace>,
    count: usize,
    cx: &mut VisualTestContext,
) -> Vec<Entity<Pane>> {
    cx.update(|window, cx| split_center(workspace, count, window, cx))
}

fn rows(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Candidate> {
    cx.read(|cx| candidates(workspace.read(cx), cx))
}

fn item_of(candidate: &Candidate) -> EntityId {
    candidate
        .value
        .downcast_ref::<SwitchTarget>()
        .expect("a row stands for a switch target")
        .item
}

fn ids(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<EntityId> {
    rows(workspace, cx).iter().map(item_of).collect()
}

fn open_switcher(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> SessionHandle {
    let handle = cx.update(|window, cx| start(workspace, window, cx));
    cx.run_until_parked();
    handle.expect("the switcher opens")
}

fn selected_item(handle: &SessionHandle, cx: &mut VisualTestContext) -> Option<EntityId> {
    cx.read(|cx| {
        handle.selected(cx).and_then(|value| {
            value
                .downcast_ref::<SwitchTarget>()
                .map(|target| target.item)
        })
    })
}

fn confirm(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
}

fn select_next(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::SelectNext);
    cx.run_until_parked();
}

fn active_item(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<EntityId> {
    cx.read(|cx| pane.read(cx).active_item().map(|item| item.item_id()))
}

fn active_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
    cx.read(|cx| workspace.read(cx).active_pane().clone())
}

fn slot(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<EntityId> {
    cx.read(|cx| pane.read(cx).preview().map(|view| view.entity_id()))
}

/// The world as a picker must leave it. A pane that takes the focus back when the picker closes
/// stamps its active item again, so the stamps are dropped here and the order they give is
/// compared through `ids`.
fn snapshot(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> WorldSnapshot {
    let mut snapshot = cx.update(|_, cx: &mut App| world_snapshot(workspace, cx));
    for pane in &mut snapshot.panes {
        for (_, stamp) in &mut pane.activation_history {
            *stamp = 0;
        }
    }
    snapshot
}

#[gpui::test]
async fn switcher_lists_items_of_every_pane_most_recent_first_with_the_current_one_last(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    let (_, terminal) = closed_terminal_dock(&workspace, cx);
    let left_old = add_item(&workspace, &panes[0], "Old.kt", false, cx);
    let right = add_item(&workspace, &panes[1], "Right.kt", false, cx);
    let left_current = add_item(&workspace, &panes[0], "Current.kt", false, cx);

    assert_eq!(
        ids(&workspace, cx),
        [
            right.entity_id(),
            left_old.entity_id(),
            terminal.entity_id(),
            left_current.entity_id(),
        ],
        "the previous item is first, the current one last"
    );
}

#[gpui::test]
async fn switcher_rows_name_the_pane_and_mark_dirty_items(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    let (_, terminal) = closed_terminal_dock(&workspace, cx);
    add_item(&workspace, &panes[0], "Dirty.kt", true, cx);
    add_item(&workspace, &panes[1], "Clean.kt", false, cx);

    let rows = rows(&workspace, cx);
    let row = |title: &str| {
        rows.iter()
            .find(|row| row.primary.as_ref() == title)
            .unwrap_or_else(|| panic!("a row titled {title}"))
    };
    assert_eq!(row("Dirty.kt").kind.as_deref(), Some("● left"));
    assert_eq!(row("Clean.kt").kind.as_deref(), Some("right"));
    let terminal_row = rows
        .iter()
        .find(|row| item_of(row) == terminal.entity_id())
        .expect("a row for the terminal");
    assert_eq!(terminal_row.kind.as_deref(), Some("dock: terminal"));
    assert_eq!(terminal_row.secondary.as_deref(), Some("terminal"));
}

#[gpui::test]
async fn switcher_narrowing_brt_finds_the_billing_run_test(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    let billing = add_item(&workspace, &pane, "BillingRunTest.kt", false, cx);
    add_item(&workspace, &pane, "BuildReport.kt", false, cx);
    add_item(&workspace, &pane, "README.md", false, cx);
    add_item(&workspace, &pane, "Current.kt", false, cx);

    let handle = open_switcher(&workspace, cx);
    cx.update(|window, cx| handle.set_input("brt", window, cx));
    cx.run_until_parked();
    assert_eq!(selected_item(&handle, cx), Some(billing.entity_id()));
}

#[gpui::test]
async fn switcher_highlight_previews_in_the_holding_pane_and_cancel_leaves_the_world_as_it_was(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    let hidden = add_item(&workspace, &panes[0], "Hidden.kt", false, cx);
    add_item(&workspace, &panes[0], "Shown.kt", false, cx);
    add_item(&workspace, &panes[1], "Current.kt", false, cx);
    let before = snapshot(&workspace, cx);
    let order_before = ids(&workspace, cx);

    let handle = open_switcher(&workspace, cx);
    select_next(cx);
    assert_eq!(selected_item(&handle, cx), Some(hidden.entity_id()));
    assert_eq!(slot(&panes[0], cx), Some(hidden.entity_id()));
    assert_eq!(slot(&panes[1], cx), None);
    assert_eq!(
        snapshot(&workspace, cx).without_previews(),
        before,
        "previewing changes nothing else"
    );

    cx.dispatch_action(minibuffer::Cancel);
    cx.run_until_parked();
    assert_eq!(slot(&panes[0], cx), None);
    assert_eq!(snapshot(&workspace, cx), before);
    assert_eq!(ids(&workspace, cx), order_before);
}

#[gpui::test]
async fn switcher_confirm_activates_the_item_in_its_pane_and_makes_it_most_recent(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    let hidden = add_item(&workspace, &panes[0], "Hidden.kt", false, cx);
    add_item(&workspace, &panes[0], "Shown.kt", false, cx);
    let current = add_item(&workspace, &panes[1], "Current.kt", false, cx);
    assert_eq!(active_pane(&workspace, cx), panes[1]);

    open_switcher(&workspace, cx);
    select_next(cx);
    confirm(cx);

    assert_eq!(active_item(&panes[0], cx), Some(hidden.entity_id()));
    assert_eq!(active_pane(&workspace, cx), panes[0]);
    assert_eq!(slot(&panes[0], cx), None, "confirming clears the preview");
    let order = ids(&workspace, cx);
    assert_eq!(order.last(), Some(&hidden.entity_id()));
    assert_eq!(order.first(), Some(&current.entity_id()));
}

#[gpui::test]
async fn switcher_ret_twice_toggles_between_two_items(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    let first = add_item(&workspace, &pane, "First.kt", false, cx);
    let second = add_item(&workspace, &pane, "Second.kt", false, cx);
    assert_eq!(active_item(&pane, cx), Some(second.entity_id()));

    cx.dispatch_action(SwitchBuffer);
    cx.run_until_parked();
    confirm(cx);
    assert_eq!(active_item(&pane, cx), Some(first.entity_id()));

    cx.dispatch_action(SwitchBuffer);
    cx.run_until_parked();
    confirm(cx);
    assert_eq!(active_item(&pane, cx), Some(second.entity_id()));
}

#[gpui::test]
async fn switcher_item_active_in_another_pane_needs_no_preview_slot(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    let other = add_item(&workspace, &panes[0], "Other.kt", false, cx);
    add_item(&workspace, &panes[1], "Current.kt", false, cx);

    let handle = open_switcher(&workspace, cx);
    assert_eq!(selected_item(&handle, cx), Some(other.entity_id()));
    assert_eq!(slot(&panes[0], cx), None);
    assert_eq!(slot(&panes[1], cx), None);
}

#[gpui::test]
async fn switcher_confirm_on_a_terminal_in_a_closed_dock_opens_the_dock_and_focuses_it(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    let (dock_pane, terminal) = closed_terminal_dock(&workspace, cx);
    add_item(&workspace, &panes[0], "One.kt", false, cx);
    add_item(&workspace, &panes[1], "Two.kt", false, cx);
    let dock_is_open = |cx: &mut VisualTestContext| {
        cx.read(|cx| workspace.read(cx).bottom_dock().read(cx).is_open())
    };
    assert!(!dock_is_open(cx));

    let handle = open_switcher(&workspace, cx);
    select_next(cx);
    assert_eq!(selected_item(&handle, cx), Some(terminal.entity_id()));
    assert!(!dock_is_open(cx), "highlighting opens no dock");
    confirm(cx);
    let focused = cx.update(|window, cx| terminal.read(cx).focus_handle(cx).is_focused(window));

    assert!(dock_is_open(cx));
    assert!(cx.read(|cx| workspace.read(cx).pane_is_visible(&dock_pane, cx)));
    assert_eq!(active_item(&dock_pane, cx), Some(terminal.entity_id()));
    assert!(focused, "the terminal has the focus");
}
