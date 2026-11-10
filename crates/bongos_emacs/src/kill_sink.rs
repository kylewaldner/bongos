//! Where killed and copied text goes. Until the kill ring exists it is the system clipboard;
//! the kill ring replaces the sink with `set_kill_sink`.

use std::rc::Rc;

use editor::ClipboardSelection;
use gpui::{App, ClipboardItem, Global, Window};

/// Whether the text was deleted or only copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillKind {
    Kill,
    Copy,
}

/// Which way the command killed, so a following kill can append or prepend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillDirection {
    Forward,
    Backward,
}

/// The text one command killed or copied: one piece per caret, in document order, with the
/// clipboard metadata `editor::Paste` reads (one entry per piece, whole lines marked).
#[derive(Clone, Debug)]
pub struct Kill {
    pub pieces: Vec<String>,
    pub metadata: Vec<ClipboardSelection>,
    pub kind: KillKind,
    pub direction: KillDirection,
    /// The action name of the command, such as `emacs::KillWord`.
    pub command: &'static str,
}

impl Kill {
    /// The pieces joined the way `editor::Copy` joins selections: a newline separates a piece
    /// from the next one unless the piece is a whole line (which already ends in one).
    pub fn joined_text(&self) -> String {
        let mut text = String::new();
        for (ix, piece) in self.pieces.iter().enumerate() {
            if ix > 0 {
                let previous_whole_line = self
                    .metadata
                    .get(ix - 1)
                    .is_some_and(|selection| selection.is_entire_line);
                if !previous_whole_line {
                    text.push('\n');
                }
            }
            text.push_str(piece);
        }
        text
    }
}

type SinkFn = dyn Fn(Kill, &mut Window, &mut App);

struct KillSink(Rc<SinkFn>);

impl Global for KillSink {}

/// Replaces where kills go (the kill ring calls this with its writer).
pub fn set_kill_sink(cx: &mut App, sink: impl Fn(Kill, &mut Window, &mut App) + 'static) {
    cx.set_global(KillSink(Rc::new(sink)));
}

/// Hands a kill to the current sink, the system clipboard when none was set.
pub fn send(kill: Kill, window: &mut Window, cx: &mut App) {
    match cx.try_global::<KillSink>().map(|sink| sink.0.clone()) {
        Some(sink) => sink(kill, window, cx),
        None => write_to_clipboard(kill, cx),
    }
}

/// The default sink: the joined text with one metadata entry per piece, the format
/// `editor::Copy` writes, so `editor::Paste` gives each caret its piece back.
pub fn write_to_clipboard(kill: Kill, cx: &mut App) {
    let text = kill.joined_text();
    cx.write_to_clipboard(ClipboardItem::new_string_with_json_metadata(
        text,
        kill.metadata,
    ));
}
