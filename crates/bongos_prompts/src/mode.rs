//! The prompts mode (design D5): an editor addon that an editor on a `Prompts` buffer gets while
//! `prompts.enabled` is true, and loses (rules and highlights removed) when the language changes or
//! the setting turns off. It keeps a [`PromptIndex`] of the marker rows in step with the buffer,
//! edit by edit, and hands the display (rules and highlights) to [`crate::display`].

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

use bongos_prompts_model::{
    BlockInfo, MarkerSyntax, PromptIndex, RowEdit, merge_row_edits, split_rows,
};
use collections::HashMap;
use editor::display_map::CustomBlockId;
use editor::{Addon, Editor, EditorEvent, ToPoint as _};
use gpui::{App, Context, Entity, Global, KeyContext, WeakEntity};
use language::{Buffer, Point, TextBufferSnapshot};
use settings::{Settings as _, SettingsStore};

use crate::{LANGUAGE_NAME, PromptsSettings, display};

/// The key context identifier the mode adds, so the keymap context is `Editor && prompts`.
pub const KEY_CONTEXT: &str = "prompts";

/// More changed rows than this in one sync rebuild the index instead of editing it.
const REBUILD_ROWS: u32 = 2_000;

/// Counters tests read: how many calls the display made to the editor's block map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeStats {
    pub insert_blocks_calls: usize,
    pub remove_blocks_calls: usize,
}

/// The state of the mode in one editor.
pub struct PromptsMode {
    buffer: Entity<Buffer>,
    version: clock::Global,
    pub(crate) index: PromptIndex,
    /// The rule block of each marker, by marker id.
    pub(crate) rules: HashMap<u64, CustomBlockId>,
    /// The block of the newest selection's head, as of the last sync or selection change.
    pub(crate) current: Option<BlockInfo>,
    pub(crate) stats: ModeStats,
}

impl PromptsMode {
    pub fn index(&self) -> &PromptIndex {
        &self.index
    }

    pub fn current(&self) -> Option<&BlockInfo> {
        self.current.as_ref()
    }

    pub fn stats(&self) -> ModeStats {
        self.stats
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// The addon that carries the mode. The editor calls it while focused, adding [`KEY_CONTEXT`].
pub struct PromptsAddon {
    mode: Rc<RefCell<PromptsMode>>,
}

impl Addon for PromptsAddon {
    fn extend_key_context(&self, context: &mut KeyContext, _: &App) {
        context.add(KEY_CONTEXT);
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

/// The mode of `editor`, when it has one.
pub fn mode_of(editor: &Editor) -> Option<Rc<RefCell<PromptsMode>>> {
    editor
        .addon::<PromptsAddon>()
        .map(|addon| addon.mode.clone())
}

/// Whether `editor` has the prompts mode.
pub fn is_attached(editor: &Editor) -> bool {
    editor.addon::<PromptsAddon>().is_some()
}

/// The prompts editor that was focused last, for the commands that act on "the prompts view".
struct LastPromptsEditor(WeakEntity<Editor>);

impl Global for LastPromptsEditor {}

/// The prompts editor focused last, while it still has the mode.
pub fn last_focused(cx: &App) -> Option<Entity<Editor>> {
    let editor = cx.try_global::<LastPromptsEditor>()?.0.upgrade()?;
    is_attached(editor.read(cx)).then_some(editor)
}

/// Watches every new full editor on a single buffer: it gets the mode when its buffer is (or
/// becomes) a `Prompts` buffer while `prompts.enabled` is true.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _window, cx: &mut Context<Editor>| {
        if !editor.mode().is_full() || !editor.buffer().read(cx).is_singleton() {
            return;
        }
        let this = cx.entity();
        cx.subscribe(&this, |editor, _, event: &EditorEvent, cx| {
            on_event(editor, event, cx)
        })
        .detach();
        cx.observe_global::<SettingsStore>(refresh).detach();
        refresh(editor, cx);
    })
    .detach();
}

fn on_event(editor: &mut Editor, event: &EditorEvent, cx: &mut Context<Editor>) {
    match event {
        // Also emitted on a language change; the language is checked again.
        EditorEvent::Reparsed(_) => refresh(editor, cx),
        EditorEvent::BufferEdited => {
            if let Some(mode) = mode_of(editor) {
                sync(&mut mode.borrow_mut(), editor, cx);
            }
        }
        EditorEvent::SelectionsChanged { .. } => {
            if let Some(mode) = mode_of(editor) {
                let mut mode = mode.borrow_mut();
                sync(&mut mode, editor, cx);
                display::update_current(&mut mode, editor, false, cx);
            }
        }
        EditorEvent::Focused => {
            if is_attached(editor) {
                let this = cx.entity().downgrade();
                cx.set_global(LastPromptsEditor(this));
            }
        }
        _ => {}
    }
}

fn singleton_buffer(editor: &Editor, cx: &App) -> Option<Entity<Buffer>> {
    editor.buffer().read(cx).as_singleton()
}

/// Whether the editor's buffer has the `Prompts` language.
pub fn is_prompts_buffer(editor: &Editor, cx: &App) -> bool {
    singleton_buffer(editor, cx)
        .and_then(|buffer| buffer.read(cx).language().cloned())
        .is_some_and(|language| language.name().as_ref() == LANGUAGE_NAME)
}

/// Attaches or detaches the mode to match the setting and the buffer's language.
pub(crate) fn refresh(editor: &mut Editor, cx: &mut Context<Editor>) {
    let wanted = PromptsSettings::get_global(cx).enabled && is_prompts_buffer(editor, cx);
    match (wanted, is_attached(editor)) {
        (true, false) => attach(editor, cx),
        (false, true) => detach(editor, cx),
        _ => {}
    }
}

fn attach(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(buffer) = singleton_buffer(editor, cx) else {
        return;
    };
    let (text, version) = {
        let buffer = buffer.read(cx);
        (buffer.text(), buffer.version())
    };
    let index = PromptIndex::build(MarkerSyntax::default(), &split_rows(&text));
    let mut mode = PromptsMode {
        buffer,
        version,
        index,
        rules: HashMap::default(),
        current: None,
        stats: ModeStats::default(),
    };
    let added: Vec<u64> = mode
        .index
        .markers()
        .iter()
        .map(|marker| marker.id)
        .collect();
    display::sync_rules(&mut mode, editor, &[], &added, cx);
    display::update_current(&mut mode, editor, true, cx);
    editor.register_addon(PromptsAddon {
        mode: Rc::new(RefCell::new(mode)),
    });
    cx.notify();
}

fn detach(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(mode) = mode_of(editor) else {
        return;
    };
    editor.unregister_addon::<PromptsAddon>();
    display::clear(&mut mode.borrow_mut(), editor, cx);
    cx.notify();
}

fn line_text(snapshot: &TextBufferSnapshot, row: u32) -> String {
    snapshot
        .text_for_range(Point::new(row, 0)..Point::new(row, snapshot.line_len(row)))
        .collect()
}

/// Brings the index up to date with the buffer: every edit since the last sync, as whole rows,
/// edits on the same row merged into one, applied last to first. Then the rules and highlights.
pub(crate) fn sync(mode: &mut PromptsMode, editor: &mut Editor, cx: &mut Context<Editor>) {
    let (edits, snapshot) = {
        let buffer = mode.buffer.read(cx);
        if !buffer.version().changed_since(&mode.version) {
            return;
        }
        let edits: Vec<RowEdit> = buffer
            .edits_since::<Point>(&mode.version)
            .map(|edit| RowEdit {
                old_rows: edit.old.start.row..edit.old.end.row + 1,
                new_rows: edit.new.start.row..edit.new.end.row + 1,
            })
            .collect();
        (edits, buffer.text_snapshot())
    };
    mode.version = snapshot.version().clone();
    let merged = merge_row_edits(&edits);
    let touched: u32 = merged
        .iter()
        .map(|edit| edit.new_rows.len().max(edit.old_rows.len()) as u32)
        .sum();
    let mut removed = Vec::new();
    let mut added = Vec::new();
    if touched > REBUILD_ROWS {
        removed.extend(mode.index.markers().iter().map(|marker| marker.id));
        let text = snapshot.text();
        mode.index.rebuild(&split_rows(&text));
        added.extend(mode.index.markers().iter().map(|marker| marker.id));
    } else {
        for edit in merged.iter().rev() {
            let lines: Vec<String> = edit
                .new_rows
                .clone()
                .map(|row| line_text(&snapshot, row))
                .collect();
            let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
            let change = mode.index.edit(edit.old_rows.clone(), &lines);
            removed.extend(change.removed);
            added.extend(change.added);
        }
    }
    let markers_changed = !removed.is_empty() || !added.is_empty();
    display::sync_rules(mode, editor, &removed, &added, cx);
    display::update_current(mode, editor, markers_changed, cx);
}

/// The buffer row of the newest selection's head.
pub(crate) fn head_row(editor: &Editor, cx: &App) -> u32 {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    editor
        .selections
        .newest_anchor()
        .head()
        .to_point(&snapshot)
        .row
}
