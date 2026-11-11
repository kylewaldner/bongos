//! Which repository a task belongs to, and what that decides: the kind a new task starts with.

use std::path::PathBuf;

use bongos_work_model::Kind;
use bongos_work_model::commands::{NewTask, Placement};
use bongos_work_model::rules::Place;
use bongos_work_model::store::{Task, WorkTree};
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
        .register_action(|w, _: &work::AddRepository, window, cx| add_repository(w, window, cx));
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
