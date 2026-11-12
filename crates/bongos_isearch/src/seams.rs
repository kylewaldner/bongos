//! Where isearch hands off to other features.

use std::rc::Rc;

use bongos_emacs::mark::push_mark;
use editor::Editor;
use gpui::{App, Entity, Global, Window};
use multi_buffer::Anchor;

/// Pushes the search's origin onto the buffer's mark ring, so popping the mark returns there.
/// Every exit but an abort calls it before it sets the final selection: the push is skipped
/// while mark mode is on, and the session keeps mark mode off.
pub fn push_origin(editor: &Entity<Editor>, origin: Anchor, window: &mut Window, cx: &mut App) {
    push_mark(editor, Some(origin), false, window, cx);
}

/// Where the text of a yank into the search string comes from: the top of the kill ring once a
/// feature installs it, the system clipboard until then.
pub type KillSource = Rc<dyn Fn(&App) -> Option<String>>;

struct InstalledKillSource(KillSource);

impl Global for InstalledKillSource {}

/// Replaces the kill source `C-y` reads in a search.
pub fn set_kill_source(source: KillSource, cx: &mut App) {
    cx.set_global(InstalledKillSource(source));
}

/// The text `C-y` yanks, if any.
pub fn kill_text(cx: &App) -> Option<String> {
    match cx.try_global::<InstalledKillSource>() {
        Some(InstalledKillSource(source)) => source(cx),
        None => cx.read_from_clipboard().and_then(|item| item.text()),
    }
}
