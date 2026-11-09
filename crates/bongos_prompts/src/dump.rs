//! `prompts: dump view` (debug builds): the state of a prompts view as text, for the smoke
//! scenario: the mode line, the current block, the marker rows with their rules, and the display
//! text of the editor. Written to `prompts-view.txt` in the logs directory.

use std::fmt::Write as _;
use std::path::PathBuf;

use editor::Editor;
use gpui::{App, Entity};

use crate::display::rule_label;
use crate::mode::mode_of;

/// The file the dump is written to.
pub fn dump_path() -> PathBuf {
    paths::logs_dir().join("prompts-view.txt")
}

/// The dump of `editor`, or `None` when it does not have the mode.
pub fn dump_text(editor: &Entity<Editor>, cx: &mut App) -> Option<String> {
    let mode = mode_of(editor.read(cx))?;
    let mut text = String::new();
    {
        let mode = mode.borrow();
        writeln!(text, "mode line: {}", mode.mode_line(cx)).ok();
        if let Some(block) = mode.current() {
            writeln!(
                text,
                "current block: {} (body rows {}..{})",
                block.index + 1,
                block.body.start,
                block.body.end
            )
            .ok();
        }
        writeln!(text, "rules: {}", mode.rule_count()).ok();
        for marker in mode.index().markers() {
            writeln!(
                text,
                "marker row {}: {}",
                marker.row,
                rule_label(&marker.marker)
            )
            .ok();
        }
    }
    let display = editor.update(cx, |editor, cx| editor.display_text(cx));
    text.push_str("display:\n");
    text.push_str(&display);
    if !display.ends_with('\n') {
        text.push('\n');
    }
    Some(text)
}
