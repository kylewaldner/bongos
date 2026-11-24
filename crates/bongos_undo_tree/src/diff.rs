//! The diff of two undo states (`d` in the undo tree panel): the unified diff from the marked
//! state, or the state the tree was opened at, to the state under the cursor, shown read-only in
//! the lower half of the panel.
//!
//! The texts come from a second detached copy of the buffer, moved between states the way the
//! preview's copy is, so the real buffer and the preview are never touched.

use anyhow::Result;
use editor::Editor;
use gpui::{App, AppContext as _, Context, Entity, Task, Window};
use language::{Buffer, Capability, unified_diff};
use text::UndoNodeId;

use crate::preview::move_copy;

/// The name of the language the diff is highlighted with, when it is registered.
const DIFF_LANGUAGE: &str = "Diff";

/// The diff view of one undo tree panel.
pub struct DiffView {
    copy: Entity<Buffer>,
    copy_node: UndoNodeId,
    diff_buffer: Entity<Buffer>,
    editor: Entity<Editor>,
    _language: Task<()>,
}

impl DiffView {
    /// A diff view of `buffer`'s undo states, empty until [`Self::show`]. `buffer` must be in
    /// the state `current`, which its copy starts at.
    pub fn new<T: 'static>(
        buffer: &Entity<Buffer>,
        current: UndoNodeId,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        let (copy, registry) = buffer.update(cx, |buffer, cx| {
            (buffer.detached_copy(cx), buffer.language_registry())
        });
        let diff_buffer = cx.new(|cx| {
            let mut diff_buffer = Buffer::local("", cx);
            diff_buffer.set_capability(Capability::ReadOnly, cx);
            diff_buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::for_buffer(diff_buffer.clone(), None, window, cx);
            editor.set_read_only(true);
            editor
        });
        let language = match registry {
            Some(registry) => {
                let language = registry.language_for_name(DIFF_LANGUAGE);
                let diff_buffer = diff_buffer.downgrade();
                cx.spawn(async move |_, cx| {
                    if let Ok(language) = language.await {
                        diff_buffer
                            .update(cx, |diff_buffer, cx| {
                                diff_buffer.set_language(Some(language), cx)
                            })
                            .ok();
                    }
                })
            }
            None => Task::ready(()),
        };
        Self {
            copy,
            copy_node: current,
            diff_buffer,
            editor,
            _language: language,
        }
    }

    /// Shows the unified diff from the state `from` to the state `to` of `buffer`.
    pub fn show(
        &mut self,
        buffer: &Entity<Buffer>,
        from: UndoNodeId,
        to: UndoNodeId,
        cx: &mut App,
    ) -> Result<()> {
        let old_text = self.text_at(buffer, from, cx)?;
        let new_text = self.text_at(buffer, to, cx)?;
        let diff = unified_diff(&old_text, &new_text);
        self.diff_buffer
            .update(cx, |diff_buffer, cx| diff_buffer.set_text(diff, cx));
        Ok(())
    }

    /// The diff the view shows.
    pub fn text(&self, cx: &App) -> String {
        self.diff_buffer.read(cx).text()
    }

    /// The read-only editor over the diff.
    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    fn text_at(
        &mut self,
        buffer: &Entity<Buffer>,
        node: UndoNodeId,
        cx: &mut App,
    ) -> Result<String> {
        move_copy(buffer, &self.copy, self.copy_node, node, cx)?;
        self.copy_node = node;
        Ok(self.copy.read(cx).text())
    }
}
