//! The undo tree's size. Every buffer keeps at most `undo_tree.node_limit` nodes, and a change to
//! that setting reaches the buffers that are already open. When the last full editor of a buffer
//! closes while the buffer lives on (it has unsaved changes, say), the buffer's undo history is
//! dropped, so a buffer nobody looks at does not hold a tree.

use editor::Editor;
use gpui::{App, Context, Entity, EntityId, Global, WeakEntity, Window};
use language::Buffer;
use settings::{Settings as _, SettingsStore};

use crate::settings::UndoTreeSettings;

/// The live buffers, so that a settings change can reach them.
#[derive(Default)]
struct Buffers(Vec<WeakEntity<Buffer>>);

impl Global for Buffers {}

/// The live full editors, so that a closing editor can tell whether another one shows a buffer.
#[derive(Default)]
struct Editors(Vec<WeakEntity<Editor>>);

impl Global for Editors {}

/// The node limit applied last.
struct AppliedLimit(usize);

impl Global for AppliedLimit {}

/// Gives new buffers the node limit, follows the setting, and drops the history of a buffer
/// whose last full editor closes.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(
        |buffer: &mut Buffer, _: Option<&mut Window>, cx: &mut Context<Buffer>| {
            let Some(limit) = UndoTreeSettings::try_get(cx).map(|settings| settings.node_limit)
            else {
                return;
            };
            buffer.set_undo_node_limit(limit);
            let this = cx.weak_entity();
            let buffers = cx.default_global::<Buffers>();
            buffers.0.retain(|buffer| buffer.upgrade().is_some());
            buffers.0.push(this);
            cx.set_global(AppliedLimit(limit));
        },
    )
    .detach();

    cx.observe_global::<SettingsStore>(|cx| {
        let Some(limit) = UndoTreeSettings::try_get(cx).map(|settings| settings.node_limit) else {
            return;
        };
        if cx.try_global::<AppliedLimit>().map(|applied| applied.0) == Some(limit) {
            return;
        }
        cx.set_global(AppliedLimit(limit));
        let buffers: Vec<Entity<Buffer>> = {
            let registry = cx.default_global::<Buffers>();
            registry.0.retain(|buffer| buffer.upgrade().is_some());
            registry.0.iter().filter_map(WeakEntity::upgrade).collect()
        };
        for buffer in buffers {
            buffer.update(cx, |buffer, _| buffer.set_undo_node_limit(limit));
        }
    })
    .detach();

    cx.observe_new(
        |editor: &mut Editor, _: Option<&mut Window>, cx: &mut Context<Editor>| {
            if !editor.mode().is_full() {
                return;
            }
            // A buffer loaded from disk is not created through `cx.new`, so no buffer observer
            // sees it; the editor that opens it gives it the limit.
            for buffer in editor.buffer().read(cx).all_buffers() {
                track(&buffer, cx);
            }
            let this = cx.weak_entity();
            let editors = cx.default_global::<Editors>();
            editors.0.retain(|editor| editor.upgrade().is_some());
            editors.0.push(this);
            let id = cx.entity_id();
            cx.on_release(move |editor, cx| on_release(editor, id, cx))
                .detach();
        },
    )
    .detach();
}

/// Gives a buffer the node limit and starts following it, unless that already happened.
fn track(buffer: &Entity<Buffer>, cx: &mut App) {
    let Some(limit) = UndoTreeSettings::try_get(cx).map(|settings| settings.node_limit) else {
        return;
    };
    let registry = cx.default_global::<Buffers>();
    registry.0.retain(|buffer| buffer.upgrade().is_some());
    if registry
        .0
        .iter()
        .any(|known| known.entity_id() == buffer.entity_id())
    {
        return;
    }
    registry.0.push(buffer.downgrade());
    buffer.update(cx, |buffer, _| buffer.set_undo_node_limit(limit));
}

/// A full editor is released: resets the undo tree of each of its buffers that no other full
/// editor shows.
fn on_release(editor: &mut Editor, id: EntityId, cx: &mut App) {
    let others: Vec<Entity<Editor>> = {
        let editors = cx.default_global::<Editors>();
        editors
            .0
            .retain(|other| other.entity_id() != id && other.upgrade().is_some());
        editors.0.iter().filter_map(WeakEntity::upgrade).collect()
    };
    let mut shown = Vec::new();
    for other in &others {
        shown.extend(other.read(cx).buffer().read(cx).all_buffers());
    }
    for buffer in editor.buffer().read(cx).all_buffers() {
        if !shown.contains(&buffer) {
            buffer.update(cx, |buffer, _| buffer.reset_undo_tree());
        }
    }
}
