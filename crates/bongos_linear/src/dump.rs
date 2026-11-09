//! `linear: dump state`: the state of the Linear sync as text, written to `linear-state.txt` in
//! bongos's logs directory. For tests and the smoke scenario.
//!
//! The dump says whether a token exists and nothing else about it: no bytes, no prefix, no
//! length. It is never written next to the credential store.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use gpui::{App, Context, Global, Window};
use workspace::Workspace;

use crate::actions::show_toast;
use crate::sync::Snapshot;
use crate::token;

/// The file the dump is written to, in the logs directory.
pub const DUMP_FILE: &str = "linear-state.txt";

struct InstalledLogsDir(PathBuf);

impl Global for InstalledLogsDir {}

/// Replaces the directory the dump is written to, for tests.
pub fn install_logs_dir(dir: PathBuf, cx: &mut App) {
    cx.set_global(InstalledLogsDir(dir));
}

fn logs_dir(cx: &App) -> PathBuf {
    match cx.try_global::<InstalledLogsDir>() {
        Some(installed) => installed.0.clone(),
        None => paths::logs_dir().clone(),
    }
}

/// The dump text of `snapshot`.
pub fn render(snapshot: &Snapshot) -> String {
    let mut text = String::from("linear sync state\n");
    let _ = writeln!(text, "enabled: {}", snapshot.enabled);
    let _ = writeln!(text, "token present: {}", snapshot.token_present);
    let _ = writeln!(text, "requests allowed: {}", snapshot.requests_allowed);
    let _ = writeln!(text, "requests refused: {}", snapshot.requests_refused);
    let _ = writeln!(text, "linked tasks: {}", snapshot.linked.len());
    for id in &snapshot.linked {
        let _ = writeln!(text, "  {id}");
    }
    let _ = writeln!(text, "reads: {}", snapshot.reads.len());
    for (id, state) in &snapshot.reads {
        let _ = writeln!(text, "  {id}: {state}");
    }
    text
}

/// Writes `text` to [`DUMP_FILE`] in `dir` and returns the path.
pub fn write_into(dir: &Path, text: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(DUMP_FILE);
    std::fs::write(&path, text)?;
    Ok(path)
}

/// `linear: dump state`. Asks the token source whether a token exists (and tells the sync), which
/// is all it learns about it.
pub(crate) fn dump_state(_: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>) {
    token::with_token_state(cx, |workspace, _present, cx| {
        let message = match crate::sync::global(cx) {
            None => "The Linear sync is not available".to_string(),
            Some(sync) => {
                let snapshot = sync.read(cx).snapshot(cx);
                match write_into(&logs_dir(cx), &render(&snapshot)) {
                    Ok(path) => format!("Linear state written to {}", path.display()),
                    Err(error) => format!("Could not write the Linear state: {error}"),
                }
            }
        };
        show_toast(workspace, message, cx);
    });
}
