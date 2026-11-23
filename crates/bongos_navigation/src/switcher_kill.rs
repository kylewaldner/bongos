//! Killing the highlighted buffer from the switcher.
//!
//! A clean item closes at once and the switcher stays open with its input. A dirty item closes the
//! switcher session, keeping its input and selected row, and asks one key: save, discard or
//! cancel. Only a successful save closes the item, so no edit is lost; whatever the answer, the
//! switcher then reopens where it was. The kept input and row live only in the reopened session.

use std::{collections::HashMap, rc::Rc};

use bongos_minibuffer::{ReadKey, SessionHandle, error, message, read_key};
use gpui::{App, Context, Entity, EntityId, Global, Window};
use util::ResultExt as _;
use workspace::item::SaveOptions;
use workspace::{
    ItemHandle, MinibufferMessageKind, MinibufferRouting, Pane, SaveIntent, Workspace,
};

use crate::preview::{Preview, PreviewSession};
use crate::registry::NO_PICKER;
use crate::switcher::{self, HOST, SwitchTarget};

/// What the echo area says when the highlighted row is not an open buffer.
pub const NOT_A_BUFFER: &str = "Not an open buffer";

/// The input and the selected row of a switcher that a kill closed, for the one that replaces it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Kept {
    pub(crate) input: String,
    pub(crate) selection: usize,
}

/// The open switcher of a workspace: its session and the preview that follows its highlight.
#[derive(Clone)]
pub(crate) struct Active {
    pub(crate) handle: SessionHandle,
    preview: Entity<PreviewSession>,
}

#[derive(Default)]
struct Switchers {
    by_workspace: HashMap<EntityId, Active>,
}

impl Global for Switchers {}

/// Records `handle` as the switcher of `workspace`, replacing any earlier one.
pub(crate) fn register(
    cx: &mut App,
    workspace: EntityId,
    handle: SessionHandle,
    preview: Entity<PreviewSession>,
) {
    cx.default_global::<Switchers>()
        .by_workspace
        .insert(workspace, Active { handle, preview });
}

pub(crate) fn current(cx: &App, workspace: EntityId) -> Option<Active> {
    cx.try_global::<Switchers>()?
        .by_workspace
        .get(&workspace)
        .filter(|active| active.handle.is_open())
        .cloned()
}

fn find_item(pane: &Entity<Pane>, item: EntityId, cx: &App) -> Option<Box<dyn ItemHandle>> {
    pane.read(cx)
        .items()
        .find(|candidate| candidate.item_id() == item)
        .map(|candidate| candidate.boxed_clone())
}

/// Kills the highlighted buffer.
pub fn kill(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(active) = current(cx, cx.entity_id()) else {
        MinibufferRouting::message(window, cx, NO_PICKER, MinibufferMessageKind::Message);
        return;
    };
    let Some(value) = active.handle.selected(cx) else {
        return;
    };
    let Some(target) = SwitchTarget::from_value(&value) else {
        message(window, cx, NOT_A_BUFFER);
        return;
    };
    let (pane, item_id) = (target.pane.clone(), target.item);
    let Some(item) = target.item_handle(cx) else {
        // Closed elsewhere: only the list is stale.
        refresh(&active, workspace, window, cx);
        return;
    };
    if item.is_dirty(cx) {
        ask(&active, workspace, item, pane, window, cx);
        return;
    }
    let closing = pane.update(cx, |pane, cx| {
        pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
    });
    cx.spawn_in(window, async move |workspace, cx| {
        closing.await.log_err();
        workspace
            .update_in(cx, |workspace, window, cx| {
                refresh(&active, workspace, window, cx)
            })
            .ok();
    })
    .detach();
}

/// Lists the open items again in the open switcher, which keeps its input and row.
fn refresh(active: &Active, workspace: &Workspace, window: &mut Window, cx: &mut App) {
    let rows = switcher::candidates(workspace, cx);
    if rows.is_empty() {
        active.preview.update(cx, |preview, cx| {
            preview.update(Preview::NoCandidate, window, cx)
        });
    }
    active.handle.set_candidates(rows, window, cx);
}

/// Closes the switcher, keeping where it was, and asks what to do with the unsaved changes of
/// `item`.
fn ask(
    active: &Active,
    workspace: &mut Workspace,
    item: Box<dyn ItemHandle>,
    pane: Entity<Pane>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let kept = Kept {
        input: active.handle.input(cx).unwrap_or_default(),
        selection: active.handle.selected_index(cx).unwrap_or(0),
    };
    let name = item.tab_content_text(0, cx).to_string();
    let item_id = item.item_id();
    let project = workspace.project().clone();
    let weak_workspace = cx.entity().downgrade();
    active.handle.close(window, cx);

    let reopen: Rc<dyn Fn(&mut Window, &mut App)> = {
        let workspace = weak_workspace.clone();
        Rc::new(move |window, cx| {
            if let Some(workspace) = workspace.upgrade() {
                switcher::start_with(&workspace, Some(kept.clone()), window, cx);
            }
        })
    };
    let on_cancel = reopen.clone();
    let prompt = ReadKey::new(
        HOST,
        format!("{name} has unsaved changes: (s)ave, (d)iscard, (c)ancel"),
    )
    .key('s', "")
    .key('d', "")
    .key('c', "")
    .default('c')
    .on_answer(move |key, window, cx| {
        answer(key, &name, &pane, item_id, &project, &reopen, window, cx)
    })
    .on_cancel(move |window, cx| on_cancel(window, cx));
    // After the closing session has finished, which is deferred.
    window.defer(cx, move |window, cx| {
        weak_workspace
            .update(cx, |workspace, cx| read_key(workspace, prompt, window, cx))
            .ok();
    });
}

/// Acts on the key that answered, then reopens the switcher.
fn answer(
    key: char,
    name: &str,
    pane: &Entity<Pane>,
    item_id: EntityId,
    project: &Entity<project::Project>,
    reopen: &Rc<dyn Fn(&mut Window, &mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    // Closed elsewhere while the question was open: nothing to save or discard.
    let Some(item) = find_item(pane, item_id, cx).filter(|_| matches!(key, 's' | 'd')) else {
        reopen(window, cx);
        return;
    };
    let reopen = reopen.clone();
    if key == 'd' {
        let closing = pane.update(cx, |pane, cx| {
            pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
        });
        window
            .spawn(cx, async move |cx| {
                closing.await.log_err();
                cx.update(|window, cx| reopen(window, cx)).ok();
            })
            .detach();
        return;
    }
    // An item that cannot save in place (an untitled buffer) goes through the pane's own save,
    // which asks where to save and closes the item only once it has.
    let saves_in_place = item.can_save(cx);
    let saving = if saves_in_place {
        item.save(SaveOptions::default(), project.clone(), window, cx)
    } else {
        pane.update(cx, |pane, cx| {
            pane.close_item_by_id(item_id, SaveIntent::Save, window, cx)
        })
    };
    let (pane, name) = (pane.clone(), name.to_string());
    window
        .spawn(cx, async move |cx| {
            let saved = saving.await;
            if saved.is_ok() && saves_in_place {
                // Only a saved item closes; one that is dirty again keeps its edits.
                let closing = cx
                    .update(|window, cx| {
                        find_item(&pane, item_id, cx).filter(|item| !item.is_dirty(cx))?;
                        Some(pane.update(cx, |pane, cx| {
                            pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                        }))
                    })
                    .ok()
                    .flatten();
                if let Some(closing) = closing {
                    closing.await.log_err();
                }
            }
            cx.update(|window, cx| {
                reopen(window, cx);
                if let Err(err) = &saved {
                    error(window, cx, format!("{name}: {err:#}"));
                }
            })
            .ok();
        })
        .detach();
}

#[cfg(test)]
#[path = "switcher_kill_tests.rs"]
mod tests;
