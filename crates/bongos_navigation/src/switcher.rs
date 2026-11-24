//! The buffer switcher: every item of every pane, most recently used first, with the current one
//! last so that RET alone goes back to the previous one.
//!
//! Highlighting a row previews the item in its own pane (see [`crate::preview`]) without
//! activating anything. Confirming opens the pane's dock when the item sits in a closed one, then
//! activates the item in the pane that holds it. After the open items come the recently closed
//! files, dimmed, which open through the default target. C-k kills the highlighted buffer (see
//! [`crate::switcher_kill`]) and M-o opens the row in a chosen pane.

use bongos_minibuffer::{
    Candidate, Outcome, Preview as SessionPreview, Session, SessionHandle, Source, Value,
};
use bongos_navigation_model::{
    mru,
    rows::{PanePosition, pane_name},
};
use gpui::{App, AppContext as _, Axis, Context, Entity, EntityId, Window};
use project::ProjectPath;
use settings::Settings as _;
use std::collections::HashSet;
use workspace::{ItemHandle, Pane, Workspace, pane_group::Member};

use crate::NavigationSettings;
use crate::goto_file_open::{self, FileTarget};
use crate::preview::{Preview, PreviewSession, PreviewTarget};
use crate::switcher_kill::{self, Kept};
use crate::target::{PaneChoice, add_pane_choices, resolve};

/// How many recently closed files the switcher lists.
const RECENT_CLOSED_LIMIT: usize = 50;

/// The name the switcher's session is routed by.
pub const HOST: &str = "switch_buffer";

/// The key context of the switcher's session.
pub const KEY_CONTEXT: &str = "NavSwitcher";

/// What a row stands for: an item and the pane that holds it.
pub(crate) struct SwitchTarget {
    pub(crate) pane: Entity<Pane>,
    pub(crate) item: EntityId,
}

impl SwitchTarget {
    pub(crate) fn from_value(value: &Value) -> Option<&Self> {
        value.downcast_ref::<Self>()
    }

    pub(crate) fn item_handle(&self, cx: &App) -> Option<Box<dyn ItemHandle>> {
        self.pane
            .read(cx)
            .items()
            .find(|item| item.item_id() == self.item)
            .map(|item| item.boxed_clone())
    }
}

/// Opens the buffer switcher.
pub fn open(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace = cx.entity();
    // The preview session reads the workspace when it is made, which it cannot do while the
    // workspace is being updated.
    window.defer(cx, move |window, cx| {
        start(&workspace, window, cx);
    });
}

pub(crate) fn start(
    workspace: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) -> Option<SessionHandle> {
    start_with(workspace, None, window, cx)
}

/// Opens the switcher, with the input and selection a kill left behind when `kept` holds them.
pub(crate) fn start_with(
    workspace: &Entity<Workspace>,
    kept: Option<Kept>,
    window: &mut Window,
    cx: &mut App,
) -> Option<SessionHandle> {
    let preview = cx.new(|cx| PreviewSession::new(workspace, cx));
    let candidates = candidates(workspace.read(cx), cx);
    let weak_workspace = workspace.downgrade();
    let preview_workspace = weak_workspace.clone();
    let choice_workspace = weak_workspace.clone();
    let session_preview = preview.clone();
    let mut session = Session::completing_read(HOST, "Switch to buffer: ")
        .key_context(KEY_CONTEXT)
        .source(Source::Fixed(candidates))
        .on_preview(move |event, window, cx| {
            let event = match event {
                SessionPreview::Highlight(value) => preview_target(&value, &preview_workspace, cx)
                    .map_or(Preview::NoCandidate, Preview::Highlight),
                SessionPreview::Confirmed => Preview::Confirmed,
                SessionPreview::Cancelled => Preview::Cancelled,
            };
            session_preview.update(cx, |preview, cx| preview.update(event, window, cx));
        })
        .on_confirm(move |outcome, window, cx| {
            let Outcome::Candidate(value) = outcome else {
                return;
            };
            weak_workspace
                .update(cx, |workspace, cx| {
                    open_row(workspace, &value, &PaneChoice::Default, window, cx)
                })
                .ok();
        });
    if let Some(kept) = kept {
        session = session
            .initial_input(kept.input)
            .initial_selection(kept.selection);
    }
    let session = add_pane_choices(
        session,
        choice_workspace.clone(),
        move |value, pane, window, cx| {
            choice_workspace
                .update(cx, |workspace, cx| {
                    open_row(workspace, &value, &PaneChoice::Letter(pane), window, cx)
                })
                .ok();
        },
    );
    let handle = workspace.update(cx, |workspace, cx| {
        bongos_minibuffer::open(workspace, session, window, cx)
    });
    switcher_kill::register(cx, workspace, handle.clone(), preview);
    Some(handle)
}

/// What a highlighted row previews: an open item in its own pane, a recently closed file in the
/// pane the default target resolves to.
fn preview_target(
    value: &Value,
    workspace: &gpui::WeakEntity<Workspace>,
    cx: &App,
) -> Option<PreviewTarget> {
    if let Some(target) = SwitchTarget::from_value(value) {
        return Some(PreviewTarget::Item {
            pane: target.pane.clone(),
            item: target.item_handle(cx)?,
        });
    }
    let FileTarget::Project(path) = value.downcast_ref::<FileTarget>()? else {
        return None;
    };
    let workspace = workspace.upgrade()?;
    let pane = resolve(workspace.read(cx), &PaneChoice::Default, cx).preview_pane;
    Some(PreviewTarget::Path {
        path: path.clone(),
        row: None,
        ranges: Vec::new(),
        pane,
    })
}

/// Opens a row. An open item is activated where it is when `choice` is the default, and moved to
/// the pane `choice` names otherwise, without closing any tab of the pane it leaves. A recently
/// closed file opens in the pane `choice` realizes.
fn open_row(
    workspace: &mut Workspace,
    value: &Value,
    choice: &PaneChoice,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(target) = SwitchTarget::from_value(value) {
        match choice {
            PaneChoice::Letter(pane) if *pane != target.pane => {
                workspace.move_item_keeping_pane(&target.pane, pane, target.item, true, window, cx);
            }
            PaneChoice::Letter(pane) => activate(workspace, pane, target.item, window, cx),
            _ => activate(workspace, &target.pane, target.item, window, cx),
        }
    } else if let Some(target) = value.downcast_ref::<FileTarget>() {
        goto_file_open::open(workspace, target, choice, window, cx);
    }
}

/// Opens the dock of `pane` when it sits in a closed one, then activates the item in it.
fn activate(
    workspace: &mut Workspace,
    pane: &Entity<Pane>,
    item: EntityId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(ix) = pane
        .read(cx)
        .items()
        .position(|candidate| candidate.item_id() == item)
    else {
        return;
    };
    workspace.reveal_pane(pane, window, cx);
    pane.update(cx, |pane, cx| {
        pane.activate_item(ix, true, true, window, cx)
    });
}

/// One row per item of every pane, previous item first and the current one last, then the
/// recently closed files when the setting asks for them.
pub(crate) fn candidates(workspace: &Workspace, cx: &App) -> Vec<Candidate> {
    let current = workspace
        .active_pane()
        .read(cx)
        .active_item()
        .map(|item| item.item_id());
    let mut stamps = Vec::new();
    let mut rows = Vec::new();
    for pane in workspace.item_panes(cx) {
        let name = pane_label(workspace, &pane);
        let in_dock = !contains(&workspace.center_group().root, &pane);
        let pane_ref = pane.read(cx);
        let active = pane_ref.active_item().map(|item| item.item_id());
        let focused_at = pane_ref.focus_timestamp();
        for item in pane_ref.items() {
            let id = item.item_id();
            let mut stamp = pane_ref
                .activation_history()
                .iter()
                .filter(|entry| entry.entity_id == id)
                .map(|entry| entry.timestamp)
                .max();
            if active == Some(id) {
                stamp = stamp.max(focused_at);
            }
            stamps.push((id, stamp.unwrap_or(0) as u64));
            rows.push((id, row(item.as_ref(), &pane, name, in_dock, cx)));
        }
    }
    let mut rows: Vec<Candidate> = mru::order(stamps, current)
        .into_iter()
        .filter_map(|id| {
            let position = rows.iter().position(|(row_id, _)| *row_id == id)?;
            Some(rows.swap_remove(position).1)
        })
        .collect();
    if NavigationSettings::get_global(cx).include_recent_closed {
        rows.extend(recently_closed(workspace, cx));
    }
    rows
}

/// The files of the recent navigation history that no pane holds open, newest first and dimmed.
fn recently_closed(workspace: &Workspace, cx: &App) -> Vec<Candidate> {
    let open: HashSet<ProjectPath> = workspace
        .item_panes(cx)
        .iter()
        .flat_map(|pane| {
            pane.read(cx)
                .items()
                .flat_map(|item| item.project_paths(cx))
                .collect::<Vec<_>>()
        })
        .collect();
    workspace
        .recent_navigation_history(None, cx)
        .into_iter()
        .filter(|(path, _)| !open.contains(path))
        .take(RECENT_CLOSED_LIMIT)
        .map(|(path, _)| {
            let name = path
                .path
                .file_name()
                .unwrap_or_else(|| path.path.as_unix_str())
                .to_string();
            let directory = path
                .path
                .parent()
                .map(|parent| parent.as_unix_str().to_string())
                .filter(|directory| !directory.is_empty());
            let mut row = Candidate::new(name, FileTarget::Project(path)).kind("closed");
            if let Some(directory) = directory {
                row = row.secondary(directory);
            }
            row.dimmed()
        })
        .collect()
}

fn row(
    item: &dyn ItemHandle,
    pane: &Entity<Pane>,
    pane_name: &str,
    in_dock: bool,
    cx: &App,
) -> Candidate {
    let dirty = if item.is_dirty(cx) { "● " } else { "" };
    let directory = item
        .project_path(cx)
        .and_then(|path| {
            path.path
                .parent()
                .map(|parent| parent.as_unix_str().to_string())
        })
        .filter(|directory| !directory.is_empty());
    let secondary = directory
        .or_else(|| item.telemetry_event_text(cx).map(str::to_string))
        .or_else(|| in_dock.then(|| "terminal".to_string()))
        .unwrap_or_default();
    Candidate::new(
        item.tab_content_text(0, cx),
        SwitchTarget {
            pane: pane.clone(),
            item: item.item_id(),
        },
    )
    .secondary(secondary)
    .kind(format!("{dirty}{pane_name}"))
}

/// The name of `pane`: by its place in the center tree, or `dock: terminal` for a dock pane.
fn pane_label(workspace: &Workspace, pane: &Entity<Pane>) -> &'static str {
    let root = &workspace.center_group().root;
    if !contains(root, pane) {
        return pane_name(PanePosition::Terminal);
    }
    let (horizontal_index, horizontal_count) = place(root, pane, Axis::Horizontal);
    let (vertical_index, vertical_count) = place(root, pane, Axis::Vertical);
    pane_name(PanePosition::Center {
        horizontal_index,
        horizontal_count,
        vertical_index,
        vertical_count,
    })
}

fn contains(member: &Member, pane: &Entity<Pane>) -> bool {
    match member {
        Member::Pane(candidate) => candidate == pane,
        Member::Axis(axis) => axis.members.iter().any(|member| contains(member, pane)),
    }
}

/// The index of the member holding `pane`, and the member count, of the outermost split along
/// `axis` that holds it; `(0, 1)` when no split along that axis does.
fn place(member: &Member, pane: &Entity<Pane>, axis: Axis) -> (usize, usize) {
    let Member::Axis(split) = member else {
        return (0, 1);
    };
    let holder = split
        .members
        .iter()
        .position(|member| contains(member, pane));
    match holder {
        Some(index) if split.axis == axis => (index, split.members.len()),
        Some(index) => place(&split.members[index], pane, axis),
        None => (0, 1),
    }
}

#[cfg(test)]
#[path = "switcher_tests.rs"]
mod tests;
