//! Which repository a task belongs to, and what that decides: the kind a new task starts with.
//! Also `work: add repository` and `work: edit worktree root`, which write `work.repositories`.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bongos_work_model::Kind;
use bongos_work_model::commands::{NewTask, Placement};
use bongos_work_model::rules::Place;
use bongos_work_model::store::{Task, WorkTree};
use bongos_work_model::worktree_path::preview;
use gpui::{App, Context, PathPromptOptions, Window};
use settings::{Settings as _, SettingsContent, WorkRepositoryContent};
use util::paths::PathExt as _;
use workspace::Workspace;

use crate::actions::work;
use crate::commands::show_toast;
use crate::settings::Repository;
use crate::{WorkSettings, WorkStore, WriteError, modals};

/// The repository of the task with `id`, from the `work.repositories` list. The name comes from
/// the task's own `repository` field, else the nearest ancestor's, else the project's (when the
/// project names exactly one). `None` when no level names one, when the name is not in the list,
/// or when no task has the id.
pub fn resolve_repository<'a>(
    tree: &WorkTree,
    id: &str,
    repositories: &'a [Repository],
) -> Option<&'a Repository> {
    let place = Place::locate(tree, id).ok()?;
    let name = place.repository(place.task.file.as_ref()?)?;
    find_repository(repositories, &name)
}

/// Whether merging the PR of `task` completes its ships todo: the task's own `merge_is_ship`
/// when set, else the value of its repository, else true (the repository default).
pub fn merge_is_ship(task: &Task, cx: &App) -> bool {
    if let Some(own) = task.file.as_ref().and_then(|file| file.merge_is_ship) {
        return own;
    }
    let tree = WorkStore::global(cx).and_then(|store| store.read(cx).snapshot());
    let repository = tree.as_deref().zip(task.id()).and_then(|(tree, id)| {
        resolve_repository(tree, id, &WorkSettings::get_global(cx).repositories)
    });
    repository.is_none_or(|repository| repository.merge_is_ship)
}

/// The entry of `repositories` called `name`.
pub fn find_repository<'a>(repositories: &'a [Repository], name: &str) -> Option<&'a Repository> {
    repositories
        .iter()
        .find(|repository| repository.name == name)
}

/// The kind a new task defaults to when that differs from the model's own default: `pr-merge`
/// for a task in a repository without deployments. A child under `parent` is in the repository
/// `parent` resolves to unless `new` names one; a one-off names none.
pub fn default_kind(
    tree: &WorkTree,
    placement: &Placement,
    new: &NewTask,
    repositories: &[Repository],
) -> Option<Kind> {
    let repository = match (&new.repository, placement) {
        (Some(name), _) => find_repository(repositories, name),
        (None, Placement::Under(parent)) => resolve_repository(tree, parent, repositories),
        (None, Placement::OneOff { .. }) => None,
    }?;
    (!repository.deployments).then_some(Kind::PrMerge)
}

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &work::AddRepository, window, cx| add_repository(w, window, cx))
        .register_action(|w, action: &work::EditWorktreeRoot, window, cx| {
            edit_worktree_root(w, action, window, cx)
        });
    #[cfg(debug_assertions)]
    workspace.register_action(|w, _: &work::DumpRepositories, _, cx| dump_repositories(w, cx));
}

/// The file `work: dump repositories` writes in the logs directory.
#[cfg(debug_assertions)]
pub(crate) const DUMP_FILE: &str = "work-repositories.txt";

/// `work: dump repositories`: writes the repository overview and the store's counters to
/// [`DUMP_FILE`] in the logs directory.
#[cfg(debug_assertions)]
fn dump_repositories(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    if crate::commands::ready(workspace, cx).is_none() {
        return;
    }
    let Some(store) = WorkStore::global(cx) else {
        return;
    };
    let text = {
        let store = store.read(cx);
        crate::overview::dump_text(store.repository_overview(), &store.stats())
    };
    let logs_dir = paths::logs_dir();
    let written = std::fs::create_dir_all(logs_dir)
        .and_then(|()| std::fs::write(logs_dir.join(DUMP_FILE), &text));
    let message = match written {
        Ok(()) => format!("Wrote {}", logs_dir.join(DUMP_FILE).display()),
        Err(error) => format!("Could not write the repositories: {error}"),
    };
    show_toast(workspace, message, cx);
}

/// `work: add repository`: picks a folder, asks for a name (the folder's name to start with) and
/// a setup command (which may stay empty), then appends the repository to the user settings.
fn add_repository(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !WorkSettings::get_global(cx).enabled {
        show_toast(workspace, WriteError::Off.to_string(), cx);
        return;
    }
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Add repository".into()),
    });
    cx.spawn_in(window, async move |workspace, cx| {
        // A cancelled or failed prompt adds nothing; a failure is worth a toast.
        let folder = match picked.await {
            Ok(Ok(Some(paths))) => paths.into_iter().next(),
            Ok(Err(error)) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(workspace, format!("Could not pick a folder: {error}"), cx)
                    })
                    .ok();
                None
            }
            _ => None,
        };
        if let Some(folder) = folder {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    ask_name(workspace, folder, window, cx)
                })
                .ok();
        }
    })
    .detach();
}

fn ask_name(
    workspace: &mut Workspace,
    folder: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let suggestion = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    modals::ask(
        workspace,
        "Name of the repository",
        &suggestion,
        false,
        Box::new(move |name, workspace, window, cx| {
            if let Some(refusal) = duplicate(&name, cx) {
                show_toast(workspace, refusal, cx);
                return;
            }
            modals::ask(
                workspace,
                "Command that sets up a new worktree of it (empty for none)",
                "",
                true,
                Box::new(move |command, workspace, _, cx| {
                    let setup_command = (!command.is_empty()).then_some(command);
                    // Written the way the user would write it: `~` for the home directory.
                    let path = folder.compact().to_string_lossy().into_owned();
                    write_repository(workspace, name, path, setup_command, cx);
                }),
                window,
                cx,
            );
        }),
        window,
        cx,
    );
}

/// Why `name` cannot be added, if it cannot.
fn duplicate(name: &str, cx: &gpui::App) -> Option<String> {
    find_repository(&WorkSettings::get_global(cx).repositories, name)
        .map(|_| format!("A repository named {name} already exists"))
}

fn write_repository(
    workspace: &mut Workspace,
    name: String,
    path: String,
    setup_command: Option<String>,
    cx: &mut Context<Workspace>,
) {
    if let Some(refusal) = duplicate(&name, cx) {
        show_toast(workspace, refusal, cx);
        return;
    }
    let fs = workspace.app_state().fs.clone();
    let entry = WorkRepositoryContent {
        name,
        path,
        setup_command,
        merge_is_ship: None,
        deployments: None,
        branch_template: None,
        worktree_root: None,
    };
    let written = settings::update_settings_file_with_completion(fs, cx, move |content, _| {
        append_repository(content, entry)
    });
    cx.spawn(async move |workspace, cx| {
        let failure = match written.await {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error.to_string(),
            Err(_) => "the settings write was cancelled".to_string(),
        };
        workspace
            .update(cx, |workspace, cx| {
                show_toast(
                    workspace,
                    format!("Could not save the repository: {failure}"),
                    cx,
                )
            })
            .ok();
    })
    .detach();
}

/// Appends `entry` to `work.repositories` of the user settings, unless one has its name.
fn append_repository(content: &mut SettingsContent, entry: WorkRepositoryContent) {
    let repositories = content
        .work
        .get_or_insert_default()
        .repositories
        .get_or_insert_default();
    if !repositories
        .iter()
        .any(|existing| existing.name == entry.name)
    {
        repositories.push(entry);
    }
}

/// `work: edit worktree root`: picks a repository unless the action names one, then asks for its
/// worktree root, checking the template on every edit, and writes it to that entry's
/// `worktree_root` in the user settings. Empty input removes the key.
fn edit_worktree_root(
    workspace: &mut Workspace,
    action: &work::EditWorktreeRoot,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let settings = WorkSettings::get_global(cx);
    if !settings.enabled {
        show_toast(workspace, WriteError::Off.to_string(), cx);
        return;
    }
    let names: Vec<String> = settings
        .repositories
        .iter()
        .map(|repository| repository.name.clone())
        .collect();
    if let Some(name) = &action.repository {
        if names.contains(name) {
            ask_worktree_root(workspace, name.clone(), window, cx);
        } else {
            show_toast(
                workspace,
                format!("There is no repository named {name}"),
                cx,
            );
        }
        return;
    }
    if names.is_empty() {
        show_toast(
            workspace,
            "There is no repository; add one with work: add repository".to_string(),
            cx,
        );
        return;
    }
    modals::pick(
        workspace,
        "Edit the worktree root of which repository?",
        names.clone(),
        0,
        Box::new(move |index, workspace, window, cx| {
            if let Some(name) = names.get(index) {
                ask_worktree_root(workspace, name.clone(), window, cx);
            }
        }),
        window,
        cx,
    );
}

/// The status line of `work: edit worktree root` for `text` typed as the root of `repository`:
/// the template error as the parser words it, or where new worktrees would go. Empty text falls
/// back to `default_root`, then to the central directory `central`.
pub(crate) fn worktree_root_status(
    text: &str,
    repository: &Repository,
    default_root: Option<&str>,
    home: &Path,
    central: &Path,
) -> Result<String, String> {
    let template = if text.is_empty() {
        default_root
    } else {
        Some(text)
    };
    let applies = |preview: String| {
        format!("Applies to tasks started from now on; new worktrees go to {preview}")
    };
    match preview(
        template,
        &repository.name,
        Path::new(&repository.path),
        home,
        central,
    ) {
        Ok(preview) => Ok(applies(preview)),
        Err(error) if !text.is_empty() => Err(error.to_string()),
        // Clearing is always allowed, even while the default root it falls back to is invalid.
        Err(error) => Ok(format!(
            "Clears the worktree root; the default one has a problem: {error}"
        )),
    }
}

fn ask_worktree_root(
    workspace: &mut Workspace,
    name: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let settings = WorkSettings::get_global(cx);
    let Some(repository) = find_repository(&settings.repositories, &name).cloned() else {
        return;
    };
    let default_root = settings.default_worktree_root.clone();
    let central = crate::central_worktrees_dir(cx).unwrap_or_default();
    let home = util::paths::home_dir().clone();
    let initial = repository.worktree_root.clone().unwrap_or_default();
    let check: modals::Check = Rc::new(move |text| {
        worktree_root_status(text, &repository, default_root.as_deref(), &home, &central)
    });
    modals::ask_checked(
        workspace,
        format!("Worktree root of {name} (empty for the default)"),
        &initial,
        check,
        Box::new(move |root, workspace, _, cx| {
            let root = (!root.is_empty()).then_some(root);
            write_worktree_root(workspace, name, root, cx);
        }),
        window,
        cx,
    );
}

fn write_worktree_root(
    workspace: &mut Workspace,
    name: String,
    root: Option<String>,
    cx: &mut Context<Workspace>,
) {
    let fs = workspace.app_state().fs.clone();
    let written = settings::update_settings_file_with_completion(fs, cx, move |content, _| {
        set_worktree_root(content, &name, root)
    });
    cx.spawn(async move |workspace, cx| {
        let failure = match written.await {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error.to_string(),
            Err(_) => "the settings write was cancelled".to_string(),
        };
        workspace
            .update(cx, |workspace, cx| {
                show_toast(
                    workspace,
                    format!("Could not save the worktree root: {failure}"),
                    cx,
                )
            })
            .ok();
    })
    .detach();
}

/// Sets `worktree_root` of the entry of `work.repositories` called `name` in the user settings,
/// removing the key when `root` is `None`. Other entries are left as they are.
fn set_worktree_root(content: &mut SettingsContent, name: &str, root: Option<String>) {
    let entry = content
        .work
        .as_mut()
        .and_then(|work| work.repositories.as_mut())
        .and_then(|repositories| repositories.iter_mut().find(|entry| entry.name == name));
    if let Some(entry) = entry {
        entry.worktree_root = root;
    }
}
