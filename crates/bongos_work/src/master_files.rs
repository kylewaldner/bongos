//! Master in the editor. A buffer of a file under a project repository's `repository/` folder is
//! a master file: its tab says "(master)", and the first edit, like a save, offers to move the
//! edit to a working copy, so a master buffer never stays dirty and nothing but a landing writes
//! to master. The files themselves are read-only on disk as the last guard.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bongos_project_repo::{LoadedProject, ProjectRepo, ProjectRepositorySettings};
use bongos_project_repo::{clean_name, fresh_name};
use editor::Editor;
use gpui::{App, AppContext as _, Context, Entity, Window};
use language::{Buffer, BufferEvent};
use project::File as ProjectFile;
use settings::Settings as _;
use util::ResultExt as _;
use workspace::{OpenOptions, Save, Workspace};

use crate::commands::{keyboard_author, show_toast};
use crate::modals;
use crate::project_repos::open_repos;
use crate::working_copy_commands::{now, working_copy_label};

/// What the tab of a master file shows after its name.
pub const MASTER_SUFFIX: &str = "(master)";

/// A buffer's file is in master: the repository it is in and its path inside it.
#[derive(Clone)]
pub struct MasterFile {
    pub loaded: Arc<LoadedProject>,
    /// The path inside `repository/`.
    pub relative: PathBuf,
}

impl MasterFile {
    pub fn repo(&self) -> &ProjectRepo {
        self.loaded.repo()
    }
}

/// The master file `path` is, when it is inside the `repository/` of an open project repository.
pub fn master_file_at(path: &Path, cx: &App) -> Option<MasterFile> {
    open_repos(cx).into_iter().find_map(|loaded| {
        let relative = path
            .strip_prefix(&loaded.repo().location().repository)
            .ok()?
            .to_path_buf();
        let in_git = relative
            .components()
            .next()
            .is_some_and(|first| first.as_os_str() == ".git");
        (!relative.as_os_str().is_empty() && !in_git).then_some(MasterFile { loaded, relative })
    })
}

/// The marker of `buffer`: `Some` when its file is a master file.
pub fn master_file(buffer: &Entity<Buffer>, cx: &App) -> Option<MasterFile> {
    let buffer = buffer.read(cx);
    let file = ProjectFile::from_dyn(buffer.file())?;
    let path = file.worktree.read(cx).absolutize(&file.path);
    master_file_at(&path, cx)
}

/// Marks master buffers as editors open them and makes the offer when one is first edited.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new::<Editor>(|editor, window, cx| {
        let Some(window) = window else {
            return;
        };
        let Some(buffer) = editor.buffer().read(cx).as_singleton() else {
            return;
        };
        if master_file(&buffer, cx).is_none() {
            return;
        }
        editor.set_title_suffix(Some(MASTER_SUFFIX.into()), cx);
        cx.subscribe_in(
            &buffer,
            window,
            |editor, buffer, event: &BufferEvent, window, cx| {
                if !matches!(event, BufferEvent::DirtyChanged) || !buffer.read(cx).is_dirty() {
                    return;
                }
                let (Some(master), Some(workspace)) = (master_file(buffer, cx), editor.workspace())
                else {
                    return;
                };
                let buffer = buffer.clone();
                // Deferred: the edit that made the buffer dirty is still being applied.
                window.defer(cx, move |window, cx| {
                    workspace.update(cx, |workspace, cx| {
                        offer_working_copy(workspace, buffer, master, window, cx)
                    });
                });
            },
        )
        .detach();
    })
    .detach();
}

/// Adds the `workspace::Save` listener that runs before the built-in one: when the active item
/// is a master buffer it makes the offer instead of saving, and for any other item it lets the
/// built-in save run.
pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &Save, window, cx| {
        let master = workspace
            .active_item_as::<Editor>(cx)
            .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton())
            .and_then(|buffer| master_file(&buffer, cx).map(|master| (buffer, master)));
        match master {
            Some((buffer, master)) => {
                if buffer.read(cx).is_dirty() {
                    offer_working_copy(workspace, buffer, master, window, cx);
                }
            }
            None => cx.propagate(),
        }
    });
}

const NEW: usize = 0;
const EXISTING: usize = 1;

/// Asks where the edit in `buffer` goes: a new working copy, an existing one, or nowhere.
pub(crate) fn offer_working_copy(
    workspace: &mut Workspace,
    buffer: Entity<Buffer>,
    master: MasterFile,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    modals::pick(
        workspace,
        "This file is in master. Move your edit to a working copy?",
        vec![
            "New working copy".to_string(),
            "Existing working copy".to_string(),
            "Discard my edit".to_string(),
        ],
        0,
        Box::new(move |choice, workspace, window, cx| match choice {
            NEW => ask_name(workspace, buffer, master, window, cx),
            EXISTING => pick_existing(workspace, buffer, master, window, cx),
            _ => reload(&buffer, cx),
        }),
        window,
        cx,
    );
}

fn ask_name(
    workspace: &mut Workspace,
    buffer: Entity<Buffer>,
    master: MasterFile,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    modals::ask(
        workspace,
        "Name of the new working copy",
        &fresh_name(),
        false,
        Box::new(move |name, _, window, cx| {
            let repo = master.repo().clone();
            let prefix = ProjectRepositorySettings::get_global(cx)
                .branch_prefix
                .clone();
            let author = keyboard_author(window, cx);
            let created = cx.background_spawn(async move {
                let name = clean_name(&name)?;
                repo.create_working_copy(&name, &prefix, &author, now())
                    .await
            });
            cx.spawn_in(window, async move |workspace, cx| match created.await {
                Ok(copy) => {
                    workspace
                        .update_in(cx, |_, window, cx| {
                            move_edit(buffer, master, copy.path, window, cx)
                        })
                        .log_err();
                }
                Err(error) => {
                    workspace
                        .update(cx, |workspace, cx| {
                            show_toast(workspace, format!("{error:#}"), cx)
                        })
                        .log_err();
                }
            })
            .detach();
        }),
        window,
        cx,
    );
}

fn pick_existing(
    workspace: &mut Workspace,
    buffer: Entity<Buffer>,
    master: MasterFile,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let rows = master
        .repo()
        .changes(&Default::default())
        .log_err()
        .unwrap_or_default();
    if rows.is_empty() {
        show_toast(
            workspace,
            "There is no working copy yet; making a new one".to_string(),
            cx,
        );
        ask_name(workspace, buffer, master, window, cx);
        return;
    }
    let stale_days = ProjectRepositorySettings::get_global(cx).stale_after_days;
    let labels = rows
        .iter()
        .map(|row| working_copy_label(row, now(), stale_days))
        .collect();
    modals::pick(
        workspace,
        "Move the edit to which working copy?",
        labels,
        0,
        Box::new(move |index, _, window, cx| {
            let Some(row) = rows.get(index) else {
                return;
            };
            let path = master.repo().working_copy_path(&row.name);
            move_edit(buffer, master, path, window, cx);
        }),
        window,
        cx,
    );
}

/// Writes the text of `buffer` to the same relative path in the working copy at `copy`, opens
/// that file, and reloads the buffer from disk, which leaves it clean.
fn move_edit(
    buffer: Entity<Buffer>,
    master: MasterFile,
    copy: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let target = copy.join(&master.relative);
    let text = buffer.read(cx).text();
    let written = cx.background_spawn(async move {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, text)?;
        anyhow::Ok(target)
    });
    cx.spawn_in(window, async move |workspace, cx| {
        match written.await {
            Ok(target) => {
                let opening = workspace.update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(target, OpenOptions::default(), window, cx)
                });
                if let Ok(opening) = opening {
                    opening.await.log_err();
                }
            }
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(
                            workspace,
                            format!("Could not write the edit to the working copy: {error:#}"),
                            cx,
                        )
                    })
                    .log_err();
                return;
            }
        }
        buffer.update(cx, |buffer, cx| drop(buffer.reload(cx)));
    })
    .detach();
}

/// Reloads `buffer` from disk, which drops its edits.
fn reload(buffer: &Entity<Buffer>, cx: &mut App) {
    buffer.update(cx, |buffer, cx| drop(buffer.reload(cx)));
}
