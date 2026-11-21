//! The windows (panes) a window command can act on, in reading order, and the item a new window
//! shows.

use editor::Editor;
use gpui::{App, Entity, Task, Window};
use terminal_view::TerminalView;
use workspace::{Pane, Workspace, item::ItemHandle};

/// The home-row letters that label targets, then the top row.
const LETTERS: [char; 19] = [
    'a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p',
];

/// The first `n` labels: the home row, then the top row. Beyond 19 targets the rest get none.
pub fn letters(n: usize) -> Vec<char> {
    LETTERS.iter().copied().take(n).collect()
}

/// Whether the pane shows an editor or a terminal, the only things a window command targets.
fn shows_editor_or_terminal(pane: &Entity<Pane>, cx: &App) -> bool {
    pane.read(cx).active_item().is_some_and(|item| {
        item.act_as::<Editor>(cx).is_some() || item.downcast::<TerminalView>().is_some()
    })
}

/// Every pane a window command can target, in reading order: the open left dock, the center
/// panes (left before right, top before bottom), the open right dock, then the open bottom
/// dock. A dock contributes the panes of its visible panel that show an editor or a terminal;
/// panels without any (project panel, Work panel, chat list) are skipped. A zoomed pane does
/// not hide the others.
pub fn targets(workspace: &Workspace, _window: &Window, cx: &App) -> Vec<Entity<Pane>> {
    let mut panes: Vec<Entity<Pane>> = Vec::new();
    let mut push = |pane: Entity<Pane>| {
        if !panes.contains(&pane) {
            panes.push(pane);
        }
    };
    let dock_panes = |dock: &Entity<workspace::dock::Dock>| -> Vec<Entity<Pane>> {
        dock.read(cx)
            .visible_panel()
            .map(|panel| panel.panes(cx))
            .unwrap_or_default()
            .into_iter()
            .filter(|pane| shows_editor_or_terminal(pane, cx))
            .collect()
    };
    dock_panes(workspace.left_dock())
        .into_iter()
        .for_each(&mut push);
    workspace.center_panes().into_iter().for_each(&mut push);
    dock_panes(workspace.right_dock())
        .into_iter()
        .for_each(&mut push);
    dock_panes(workspace.bottom_dock())
        .into_iter()
        .for_each(&mut push);
    panes
}

/// The item a new window should show: the most recently activated item of any center pane other
/// than `pane`'s active item that can be split, else `pane`'s active item if it can be split,
/// else none. The caller clones the returned item with [`clone_for_split`].
pub fn recent_other_item(
    workspace: &Workspace,
    pane: &Entity<Pane>,
    cx: &App,
) -> Option<Box<dyn ItemHandle>> {
    let current = pane.read(cx).active_item();
    let current_id = current.as_ref().map(|item| item.item_id());
    let recent = workspace.recently_activated_items(cx);
    let mut best: Option<(usize, Box<dyn ItemHandle>)> = None;
    for center in workspace.center_panes() {
        for item in center.read(cx).items() {
            if Some(item.item_id()) == current_id || !item.can_split(cx) {
                continue;
            }
            let stamp = recent.get(&item.item_id()).copied().unwrap_or(0);
            if best.as_ref().is_none_or(|(top, _)| stamp > *top) {
                best = Some((stamp, item.boxed_clone()));
            }
        }
    }
    best.map(|(_, item)| item)
        .or_else(|| current.filter(|item| item.can_split(cx)))
}

/// Clones `item` for display in a new window.
pub fn clone_for_split(
    workspace: &Workspace,
    item: &dyn ItemHandle,
    window: &mut Window,
    cx: &mut App,
) -> Task<Option<Box<dyn ItemHandle>>> {
    item.clone_on_split(workspace.database_id(), window, cx)
}
