//! Helpers for tests of window commands: a snapshot of the whole layout to compare before and
//! after a command, and builders for pane trees with docks and test panels.

use std::sync::{Arc, atomic::AtomicUsize};

use editor::Editor;
use gpui::{Action as _, App, AppContext as _, Axis, Entity, EntityId, NoAction, Point, Window};
use terminal::TerminalBuilder;
use terminal_view::TerminalView;
use workspace::{
    Pane, SplitDirection, Workspace,
    dock::{DockPosition, test::TestPanel},
    pane_group::Member,
};

/// One pane: what a window command could change in it.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneSnapshot {
    pub items: Vec<EntityId>,
    pub active_index: usize,
    pub pinned_count: usize,
    pub preview_item: Option<EntityId>,
    /// Entries on the back and forward navigation stacks.
    pub nav_history: (usize, usize),
    /// The scroll position of each item, `None` for an item that is not an editor.
    pub scroll_positions: Vec<Option<Point<f64>>>,
}

/// The pane tree of the center.
#[derive(Debug, Clone, PartialEq)]
pub enum TreeSnapshot {
    Pane(PaneSnapshot),
    Axis {
        axis: Axis,
        flexes: Vec<f32>,
        members: Vec<TreeSnapshot>,
    },
}

/// The center's tree shape, flexes and the state of every pane in it.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutSnapshot {
    pub tree: TreeSnapshot,
}

/// Captures the center layout of `workspace`.
pub fn layout_snapshot(workspace: &Entity<Workspace>, cx: &mut App) -> LayoutSnapshot {
    let root = workspace.read(cx).center_group().root.clone();
    LayoutSnapshot {
        tree: snapshot_member(&root, cx),
    }
}

fn snapshot_member(member: &Member, cx: &mut App) -> TreeSnapshot {
    match member {
        Member::Pane(pane) => TreeSnapshot::Pane(snapshot_pane(pane, cx)),
        Member::Axis(axis) => TreeSnapshot::Axis {
            axis: axis.axis,
            flexes: axis.flexes.lock().clone(),
            members: axis
                .members
                .iter()
                .map(|member| snapshot_member(member, cx))
                .collect(),
        },
    }
}

fn snapshot_pane(pane: &Entity<Pane>, cx: &mut App) -> PaneSnapshot {
    let (handles, active_index, pinned_count, preview_item, nav_history) = {
        let pane = pane.read(cx);
        (
            pane.items()
                .map(|item| item.boxed_clone())
                .collect::<Vec<_>>(),
            pane.active_item_index(),
            pane.pinned_count(),
            pane.preview_item_id(),
            pane.nav_history().stack_depths(),
        )
    };
    let scroll_positions = handles
        .iter()
        .map(|item| {
            item.act_as::<Editor>(cx)
                .map(|editor| editor.update(cx, |editor, cx| editor.scroll_position(cx)))
        })
        .collect();
    PaneSnapshot {
        items: handles.iter().map(|item| item.item_id()).collect(),
        active_index,
        pinned_count,
        preview_item,
        nav_history,
        scroll_positions,
    }
}

/// Splits the active center pane to the right until the center has `count` panes, and returns
/// them in tree order. The first pane is the one that exists already.
pub fn split_center(
    workspace: &Entity<Workspace>,
    count: usize,
    window: &mut Window,
    cx: &mut App,
) -> Vec<Entity<Pane>> {
    workspace.update(cx, |workspace, cx| {
        let mut last = workspace.center_panes().pop().expect("a center pane");
        for _ in workspace.center_panes().len()..count {
            last = workspace.split_pane(last, SplitDirection::Right, window, cx);
        }
        workspace.center_panes()
    })
}

/// Opens a new single-line editor in `pane`, as its active item.
pub fn add_editor(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Editor> {
    let editor = cx.new(|cx| Editor::single_line(window, cx));
    workspace.update(cx, |workspace, cx| {
        workspace.add_item(
            pane.clone(),
            Box::new(editor.clone()),
            None,
            true,
            true,
            window,
            cx,
        );
    });
    editor
}

/// Opens a display-only terminal (no process) in `pane`, as its active item.
pub fn add_terminal(
    workspace: &Entity<Workspace>,
    pane: &Entity<Pane>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TerminalView> {
    let project = workspace.read(cx).project().clone();
    let weak_workspace = workspace.downgrade();
    let terminal = cx.new(|cx| {
        TerminalBuilder::new_display_only(
            Default::default(),
            terminal::terminal_settings::AlternateScroll::On,
            None,
            0,
            cx.background_executor(),
            util::paths::PathStyle::local(),
        )
        .subscribe(cx)
    });
    let view = cx.new(|cx| {
        TerminalView::new(
            terminal,
            weak_workspace,
            None,
            project.downgrade(),
            window,
            cx,
        )
    });
    pane.update(cx, |pane, cx| {
        pane.add_item(Box::new(view.clone()), true, false, None, window, cx);
    });
    view
}

/// A pane that belongs to no tree, for a dock panel to show.
pub fn new_dock_pane(
    workspace: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<Pane> {
    let project = workspace.read(cx).project().clone();
    let weak_workspace = workspace.downgrade();
    cx.new(|cx| {
        Pane::new(
            weak_workspace,
            project,
            Arc::new(AtomicUsize::new(0)),
            None,
            NoAction.boxed_clone(),
            false,
            window,
            cx,
        )
    })
}

/// Adds a test panel at `position` whose pane is `pane`, and opens its dock when `open`.
pub fn add_panel(
    workspace: &Entity<Workspace>,
    position: DockPosition,
    priority: u32,
    pane: Option<Entity<Pane>>,
    open: bool,
    window: &mut Window,
    cx: &mut App,
) -> Entity<TestPanel> {
    workspace.update(cx, |workspace, cx| {
        let panel = cx.new(|cx| {
            let mut panel = TestPanel::new(position, priority, cx);
            panel.pane = pane;
            panel
        });
        workspace.add_panel(panel.clone(), window, cx);
        if open {
            workspace.toggle_dock(position, window, cx);
        }
        panel
    })
}
