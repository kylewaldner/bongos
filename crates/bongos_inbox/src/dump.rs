//! `inbox: dump view` (debug builds): the unread count and the rows of the inbox as text, written
//! to `inbox-view.txt` in the logs directory, for tests and the smoke scenario. With an inbox view
//! in the window it writes that view's rows and cursor; without one it writes the rows of the
//! inbox's latest snapshot. The first line is `unread: <n>`, the badge's count, or `unread: off`.

use crate::rows;
use crate::view::{InboxView, now};
use crate::{Inbox, InboxSettings};
use gpui::{App, Context, Global, Window};
use settings::Settings as _;
use std::path::{Path, PathBuf};
use workspace::Workspace;

/// The file the dump is written to, in the logs directory.
pub const DUMP_FILE: &str = "inbox-view.txt";

/// Where the dump goes instead of the logs directory, when set (tests).
pub struct DumpDir(pub PathBuf);

impl Global for DumpDir {}

/// The text of the dump when no inbox view is open: the latest snapshot's rows, no cursor.
pub fn snapshot_text(cx: &App) -> String {
    let inbox = Inbox::global(cx);
    let enabled = InboxSettings::get_global(cx).enabled;
    let state = inbox
        .as_ref()
        .map(|inbox| inbox.read(cx))
        .filter(|inbox| enabled && inbox.is_enabled());
    let Some(state) = state else {
        return rows::dump_text(None, &[], None, now(cx));
    };
    match state.snapshot() {
        Some(snapshot) => rows::dump_text(
            Some(snapshot.unread_count()),
            &rows::view_rows(&snapshot),
            None,
            now(cx),
        ),
        None => rows::dump_text(Some(0), &[], None, now(cx)),
    }
}

fn write(text: &str, dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(DUMP_FILE);
    std::fs::write(&path, text)?;
    Ok(path)
}

/// `inbox: dump view`.
pub(crate) fn dump_view(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let text = match workspace.items_of_type::<InboxView>(cx).next() {
        Some(view) => view.read(cx).dump_text(cx),
        None => snapshot_text(cx),
    };
    let dir = cx
        .try_global::<DumpDir>()
        .map_or_else(|| paths::logs_dir().clone(), |dir| dir.0.clone());
    let message = match write(&text, &dir) {
        Ok(path) => format!("Wrote {}", path.display()),
        Err(error) => format!("Could not write the inbox view: {error}"),
    };
    bongos_minibuffer::message(window, cx, message);
}
