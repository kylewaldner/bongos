//! The file of a chat's prompts: `prompts: use file`, `prompts: use default` and `prompts: open`
//! (design D17 and D18).
//!
//! Switching never drops unsaved text. Both commands refuse, with "Save the prompts file first",
//! while the buffer of the prompts editor has unsaved changes, and ask again right before the
//! record is written, since the buffer may have changed while a picker was open. The record is
//! written with `bongos_chat::set_prompts_file` (through [`crate::attach::ChatLink`]); the
//! chat's session view follows the record, and keeps a buffer with unsaved changes if one appears
//! meanwhile.

use std::path::{Path, PathBuf};

use bongos_chat::ChatsSettings;
use bongos_work::model::layout::prompts_file;
use bongos_work::model::store::{Task, WorkTree};
use editor::Editor;
use fs::CreateOptions;
use gpui::{App, Context, Entity, Window};
use settings::Settings as _;
use workspace::Workspace;

use crate::actions::show_toast;
use crate::attach::{Attached, PromptChoice, link, with_chat};
use crate::commands::{NO_VIEW_MESSAGE, target};

/// The toast of a command refused because the prompts buffer has unsaved changes.
pub(crate) const SAVE_FIRST_MESSAGE: &str = "Save the prompts file first (C-x C-s)";
/// The toast when no chat uses the prompts file, so there is no record to write.
pub(crate) const NO_CHAT_MESSAGE: &str = "No chat uses this prompts file";
/// The title of the picker of `prompts: use file`.
pub(crate) const WHICH_FILE_QUESTION: &str = "Use which prompts file?";
/// The last choice, which asks for a path.
pub(crate) const ANOTHER_FILE: &str = "Another file...";
/// The title of the line that asks for a path.
pub(crate) const PATH_QUESTION: &str = "Prompts file";
/// The toast after `prompts: use default`.
pub(crate) const DEFAULT_MESSAGE: &str = "Using the default prompts file";

/// The prompts files offered for a chat scoped to `scope_dir`: the project's (when the scope is in
/// a project), then each task of the project, or each one-off of the activity, with the file of
/// the task the chat is on among them.
pub(crate) fn choices_for(tree: &WorkTree, scope_dir: &Path) -> Vec<PromptChoice> {
    let task = |task: &Task| {
        let goal = task.goal();
        PromptChoice {
            label: format!(
                "task: {}",
                if goal.is_empty() {
                    task.slug.as_str()
                } else {
                    goal
                }
            ),
            path: prompts_file(&task.dir),
        }
    };
    if let Some(project) = tree
        .projects
        .iter()
        .find(|project| project.task.walk().iter().any(|task| task.dir == scope_dir))
    {
        let title = project
            .file
            .as_ref()
            .map_or(project.slug.as_str(), |file| file.title.as_str());
        let mut choices = vec![PromptChoice {
            label: format!("project: {title}"),
            path: prompts_file(&project.task.dir),
        }];
        choices.extend(project.task.walk().into_iter().skip(1).map(task));
        return choices;
    }
    tree.activities
        .iter()
        .find(|activity| {
            activity
                .oneoffs
                .iter()
                .any(|oneoff| oneoff.walk().iter().any(|task| task.dir == scope_dir))
        })
        .map(|activity| {
            activity
                .oneoffs
                .iter()
                .flat_map(Task::walk)
                .map(task)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the buffer of `editor` has unsaved changes.
fn dirty(editor: &Entity<Editor>, cx: &App) -> bool {
    editor.read(cx).buffer().read(cx).is_dirty(cx)
}

/// The chat a command acts on: the chat of `attached`, a closed one included, since its record is
/// what a resume reads.
fn chat_of(attached: Attached) -> Option<String> {
    match attached {
        Attached::Chat(id) => Some(id),
        Attached::Closed(chat) => Some(chat.id),
        Attached::Several(_) | Attached::NoChat => None,
    }
}

/// Runs `then` with the target editor and its chat, after the refusals common to the commands.
fn with_file_chat(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(Entity<Editor>, String, &mut Workspace, &mut Window, &mut Context<Workspace>)
    + 'static,
) {
    let Some(editor) = target(workspace, window, cx) else {
        show_toast(workspace, NO_VIEW_MESSAGE.to_string(), cx);
        return;
    };
    if dirty(&editor, cx) {
        show_toast(workspace, SAVE_FIRST_MESSAGE.to_string(), cx);
        return;
    }
    let for_then = editor.clone();
    with_chat(
        workspace,
        editor,
        window,
        cx,
        move |attached, workspace, window, cx| match chat_of(attached) {
            Some(chat_id) => then(for_then, chat_id, workspace, window, cx),
            None => show_toast(workspace, NO_CHAT_MESSAGE.to_string(), cx),
        },
    );
}

/// `prompts: use file`: picks the file among the project's and its tasks', or asks for a path.
pub(crate) fn use_file(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    with_file_chat(
        workspace,
        window,
        cx,
        |editor, chat_id, workspace, window, cx| {
            let link = link(cx);
            let scope = link.scope_dir(&chat_id, cx);
            let choices = scope
                .as_deref()
                .map(|scope| link.prompt_choices(scope, cx))
                .unwrap_or_default();
            let mut labels: Vec<String> =
                choices.iter().map(|choice| choice.label.clone()).collect();
            labels.push(ANOTHER_FILE.to_string());
            bongos_work::pick(
                workspace,
                WHICH_FILE_QUESTION,
                labels,
                0,
                Box::new(move |index, workspace, window, cx| {
                    if let Some(choice) = choices.get(index) {
                        switch(
                            editor,
                            chat_id,
                            Some(choice.path.clone()),
                            workspace,
                            window,
                            cx,
                        );
                        return;
                    }
                    bongos_work::ask(
                        workspace,
                        PATH_QUESTION,
                        "",
                        false,
                        Box::new(move |text, workspace, window, cx| {
                            let path = typed_path(&text, scope.as_deref());
                            switch(editor, chat_id, Some(path), workspace, window, cx);
                        }),
                        window,
                        cx,
                    );
                }),
                window,
                cx,
            );
        },
    );
}

/// `prompts: use default`: back to the prompts file of the chat's task or project.
pub(crate) fn use_default(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    with_file_chat(
        workspace,
        window,
        cx,
        |editor, chat_id, workspace, window, cx| {
            switch(editor, chat_id, None, workspace, window, cx)
        },
    );
}

/// A path as typed: `~` is the home directory, and a relative path starts at the scope directory
/// (the same reading as `bongos_chat::set_prompts_file`).
fn typed_path(text: &str, scope_dir: Option<&Path>) -> PathBuf {
    let path = bongos_work::expand_home(text.trim());
    match scope_dir {
        Some(scope) if path.is_relative() => scope.join(path),
        _ => path,
    }
}

/// Writes the record: `path` (created empty when missing), or `None` for the default. Refused
/// when the buffer has unsaved changes by now.
fn switch(
    editor: Entity<Editor>,
    chat_id: String,
    path: Option<PathBuf>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let fs = workspace.app_state().fs.clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let created = match &path {
            Some(path) => create_empty(fs.as_ref(), path).await,
            None => Ok(()),
        };
        let message = match created {
            Err(message) => message,
            Ok(()) => {
                let write = workspace
                    .update(cx, |workspace, cx| {
                        if dirty(&editor, cx) {
                            show_toast(workspace, SAVE_FIRST_MESSAGE.to_string(), cx);
                            return None;
                        }
                        Some(link(cx).set_prompts_file(&chat_id, path.clone(), cx))
                    })
                    .ok()
                    .flatten();
                let Some(write) = write else {
                    return;
                };
                match (write.await, &path) {
                    (Err(error), _) => error,
                    (Ok(()), Some(path)) => format!("Using {}", path.display()),
                    (Ok(()), None) => DEFAULT_MESSAGE.to_string(),
                }
            }
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    })
    .detach();
}

/// Creates `path` empty when it is not there, with its directories. The text of a file that is
/// there is never touched.
async fn create_empty(fs: &dyn fs::Fs, path: &Path) -> Result<(), String> {
    if fs.is_dir(path).await {
        return Err(format!("{} is a directory", path.display()));
    }
    if let Some(parent) = path.parent() {
        fs.create_dir(parent)
            .await
            .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
    }
    fs.create_file(
        path,
        CreateOptions {
            overwrite: false,
            ignore_if_exists: true,
        },
    )
    .await
    .map_err(|error| format!("Could not create {}: {error}", path.display()))
}

/// `prompts: open`: the prompts pane of the chat shown, else the focused task's file in the
/// center. With chats off the handler of `chat: focus prompts` is not called (it would only say
/// "Chats are off"); the file opens in the center directly.
pub(crate) fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let link = link(cx);
    if ChatsSettings::get_global(cx).enabled {
        link.focus_prompts(workspace, window, cx);
    } else {
        link.open_prompts_in_center(workspace, window, cx);
    }
}
