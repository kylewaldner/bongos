//! `allowed.toml`, the conversations each project's runs and chat agents may post to without
//! asking: `[projects."<project id>"] conversations = ["C0123", ...]`. A direct message is listed
//! by its conversation id; the person's name is kept beside it in `[projects."<project id>".names]`
//! (conversation id to name), which a reader of the conversations alone ignores.
//!
//! A missing or unreadable file reads as empty. `slack: allow channel` and
//! `slack: disallow channel` are the only writers ([`add_in`], [`remove_in`]): they run on the
//! main thread, keep every other project's entries, and replace the file by writing a temporary
//! file in the same directory and renaming it over the old one, so a failed write leaves the old
//! file as it was.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use bongos_minibuffer::{Candidate, Outcome, Session, Source, open};
use bongos_slack_model::{AllowedList, ConversationId};
use gpui::{App, Context, Window};
use serde::{Deserialize, Serialize};
use workspace::Workspace;

use crate::connection::{self, ALLOWED_FILE, allowed_path, default_dir, write_atomic};
use crate::read_buffer::{ConversationEntry, conversation_entries, picker_entries};
use crate::service::{NOT_CONNECTED_MESSAGE, SlackService};
use crate::token_prompt::show_toast;

/// The host name of the Slack prompts in the minibuffer.
const HOST: &str = "slack";
const NO_PROJECT: &str = "No project is open: focus a task or open a project first";

/// Writes a file of a directory in one step (see [`write_atomic`]).
pub(crate) type WriteFile = fn(&Path, &str, &[u8]) -> io::Result<()>;

#[derive(Default, Deserialize, Serialize)]
struct AllowedFile {
    #[serde(default)]
    projects: BTreeMap<String, ProjectEntry>,
}

#[derive(Default, Deserialize, Serialize)]
struct ProjectEntry {
    #[serde(default)]
    conversations: Vec<String>,
    /// The person of each direct message listed, by conversation id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    names: BTreeMap<String, String>,
}

/// Why `allowed.toml` was not changed. The old file, if any, is left as it was.
#[derive(Debug)]
pub enum AllowedWriteError {
    /// The file exists but could not be read.
    Read(io::Error),
    /// The file is not valid; it is not overwritten.
    Invalid(String),
    /// The new file could not be written.
    Write(io::Error),
}

impl fmt::Display for AllowedWriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(f, "Could not read the Slack allowed list: {error}"),
            Self::Invalid(error) => write!(
                f,
                "The Slack allowed list is not valid, so it was not changed: {error}"
            ),
            Self::Write(error) => write!(
                f,
                "Could not write the Slack allowed list; it was not changed: {error}"
            ),
        }
    }
}

impl std::error::Error for AllowedWriteError {}

/// The allowed list of `project` in the application's Slack directory.
pub fn load(project: &str) -> AllowedList {
    load_in(&default_dir(), project)
}

/// Reads `allowed.toml` of `dir`; `Ok(None)` when it is missing.
fn read_file(dir: &Path) -> Result<Option<AllowedFile>, AllowedWriteError> {
    let text = match fs::read_to_string(allowed_path(dir)) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AllowedWriteError::Read(error)),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|error| AllowedWriteError::Invalid(error.to_string()))
}

/// The allowed list of `project` in `allowed.toml` of `dir`; empty when the file is missing or
/// unreadable.
pub fn load_in(dir: &Path, project: &str) -> AllowedList {
    let mut file = match read_file(dir) {
        Ok(Some(file)) => file,
        Ok(None) => return AllowedList::default(),
        Err(error) => {
            log::warn!("{error}");
            return AllowedList::default();
        }
    };
    let conversations = file
        .projects
        .remove(project)
        .map(|entry| entry.conversations)
        .unwrap_or_default();
    AllowedList::new(conversations.into_iter().map(ConversationId))
}

/// The names of the people of the direct messages allowed for `project`, by conversation id.
pub fn names_in(dir: &Path, project: &str) -> BTreeMap<ConversationId, String> {
    let Ok(Some(mut file)) = read_file(dir) else {
        return BTreeMap::new();
    };
    file.projects
        .remove(project)
        .map(|entry| {
            entry
                .names
                .into_iter()
                .map(|(id, name)| (ConversationId(id), name))
                .collect()
        })
        .unwrap_or_default()
}

/// Lists `conversation` for `project` in `allowed.toml` of `dir`, with `person` as the name of a
/// direct message's person. Answers false when it was already listed (a new name is still kept).
pub fn add_in(
    dir: &Path,
    project: &str,
    conversation: &ConversationId,
    person: Option<&str>,
) -> Result<bool, AllowedWriteError> {
    add_with(dir, project, conversation, person, write_atomic)
}

/// [`add_in`], writing through `write`.
pub(crate) fn add_with(
    dir: &Path,
    project: &str,
    conversation: &ConversationId,
    person: Option<&str>,
    write: WriteFile,
) -> Result<bool, AllowedWriteError> {
    let mut file = read_file(dir)?.unwrap_or_default();
    let entry = file.projects.entry(project.to_string()).or_default();
    let id = conversation.as_str().to_string();
    let added = !entry.conversations.contains(&id);
    if added {
        entry.conversations.push(id.clone());
    }
    let named = match person {
        Some(person) => entry.names.insert(id, person.to_string()).as_deref() != Some(person),
        None => false,
    };
    if added || named {
        write_file(dir, &file, write)?;
    }
    Ok(added)
}

/// Takes `conversation` (and its person's name) off the list of `project` in `allowed.toml` of
/// `dir`. Answers false when it was not listed; nothing is written then.
pub fn remove_in(
    dir: &Path,
    project: &str,
    conversation: &ConversationId,
) -> Result<bool, AllowedWriteError> {
    remove_with(dir, project, conversation, write_atomic)
}

/// [`remove_in`], writing through `write`.
pub(crate) fn remove_with(
    dir: &Path,
    project: &str,
    conversation: &ConversationId,
    write: WriteFile,
) -> Result<bool, AllowedWriteError> {
    let Some(mut file) = read_file(dir)? else {
        return Ok(false);
    };
    let Some(entry) = file.projects.get_mut(project) else {
        return Ok(false);
    };
    let id = conversation.as_str();
    let before = entry.conversations.len();
    entry.conversations.retain(|listed| listed != id);
    let removed = entry.conversations.len() != before;
    let unnamed = entry.names.remove(id).is_some();
    if !removed && !unnamed {
        return Ok(false);
    }
    if entry.conversations.is_empty() && entry.names.is_empty() {
        file.projects.remove(project);
    }
    write_file(dir, &file, write)?;
    Ok(removed)
}

fn write_file(dir: &Path, file: &AllowedFile, write: WriteFile) -> Result<(), AllowedWriteError> {
    let text =
        toml::to_string(file).map_err(|error| AllowedWriteError::Invalid(error.to_string()))?;
    write(dir, ALLOWED_FILE, text.as_bytes()).map_err(AllowedWriteError::Write)
}

/// The person of a direct message's picker line (`@name`); `None` for any other conversation.
pub fn person_of(entry: &ConversationEntry) -> Option<String> {
    entry.id.as_str().starts_with('D').then(|| {
        entry
            .label
            .strip_prefix('@')
            .unwrap_or(&entry.label)
            .to_string()
    })
}

/// The picker lines of the conversations in `list`: named as `known` names them, else as `@` and
/// the stored person of a direct message, else by id.
pub fn allowed_entries(
    list: &AllowedList,
    names: &BTreeMap<ConversationId, String>,
    known: &[ConversationEntry],
) -> Vec<ConversationEntry> {
    list.iter()
        .map(|id| {
            let label = known
                .iter()
                .find(|entry| entry.id == *id)
                .map(|entry| entry.label.clone())
                .or_else(|| names.get(id).map(|name| format!("@{name}")))
                .unwrap_or_else(|| id.to_string());
            ConversationEntry {
                label,
                id: id.clone(),
            }
        })
        .collect()
}

/// `slack: allow channel` of `entry` for `project` in `dir`: writes the file and answers what to
/// show.
pub fn allow_in(dir: &Path, project: &str, entry: &ConversationEntry) -> String {
    let person = person_of(entry);
    match add_in(dir, project, &entry.id, person.as_deref()) {
        Ok(true) => format!(
            "Runs and chat agents of {project} may post to {} without asking",
            entry.label
        ),
        Ok(false) => format!("{} was already allowed for {project}", entry.label),
        Err(error) => error.to_string(),
    }
}

/// `slack: disallow channel` of `entry` for `project` in `dir`: writes the file and answers what
/// to show.
pub fn disallow_in(dir: &Path, project: &str, entry: &ConversationEntry) -> String {
    match remove_in(dir, project, &entry.id) {
        Ok(true) => format!(
            "Runs and chat agents of {project} ask again before posting to {}",
            entry.label
        ),
        Ok(false) => format!("{} was not allowed for {project}", entry.label),
        Err(error) => error.to_string(),
    }
}

/// The project of the focused task, when it belongs to an open project repository.
pub fn active_project(cx: &App) -> Option<String> {
    let tree = bongos_work::WorkStore::global(cx)?.read(cx).snapshot()?;
    let id = bongos_work::focused(&tree, cx)?;
    let task = tree.task_by_id(&id)?;
    Some(
        bongos_work::project_repo_of(task, cx)?
            .location()
            .id
            .clone(),
    )
}

/// The ids of the open project repositories.
pub fn open_projects(cx: &App) -> Vec<String> {
    let mut ids: Vec<String> = bongos_work::OpenProjectRepos::listed(cx)
        .iter()
        .map(|loaded| loaded.repo().location().id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

type ProjectStep = fn(&mut Workspace, String, &mut Window, &mut Context<Workspace>);

/// Runs `then` with the project of the focused task, else with one picked among the open ones.
fn with_project(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: ProjectStep,
) {
    if let Some(project) = active_project(cx) {
        then(workspace, project, window, cx);
        return;
    }
    let projects = open_projects(cx);
    if projects.is_empty() {
        show_toast(workspace, NO_PROJECT.to_string(), cx);
        return;
    }
    let candidates = projects
        .into_iter()
        .map(|id| Candidate::new(id.clone(), id))
        .collect();
    let weak = cx.entity().downgrade();
    let session = Session::completing_read(HOST, "Project: ")
        .source(Source::Fixed(candidates))
        .on_confirm(move |outcome, window, cx| {
            let Outcome::Candidate(value) = outcome else {
                return;
            };
            let Some(project) = value.downcast_ref::<String>() else {
                return;
            };
            let project = project.clone();
            weak.update(cx, |workspace, cx| then(workspace, project, window, cx))
                .ok();
        });
    open(workspace, session, window, cx);
}

/// Opens a picker of `entries`; the picked one is written by `change` on the main thread.
fn pick_and_change(
    workspace: &mut Workspace,
    prompt: String,
    entries: Vec<ConversationEntry>,
    project: String,
    change: fn(&Path, &str, &ConversationEntry) -> String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let candidates = entries
        .into_iter()
        .map(|entry| Candidate::new(entry.label.clone(), entry))
        .collect();
    let weak = cx.entity().downgrade();
    let session = Session::completing_read(HOST, prompt)
        .source(Source::Fixed(candidates))
        .on_confirm(move |outcome, _, cx| {
            let Outcome::Candidate(value) = outcome else {
                return;
            };
            let Some(entry) = value.downcast_ref::<ConversationEntry>() else {
                return;
            };
            let entry = entry.clone();
            weak.update(cx, |workspace, cx| {
                let dir = connection::installed_dir(cx);
                let message = change(&dir, &project, &entry);
                show_toast(workspace, message, cx);
            })
            .ok();
        });
    open(workspace, session, window, cx);
}

/// `slack: allow channel`: a conversation picked for the active task's project.
pub(crate) fn allow_channel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    with_project(workspace, window, cx, pick_to_allow);
}

fn pick_to_allow(
    workspace: &mut Workspace,
    project: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(service) = SlackService::global(cx) else {
        show_toast(workspace, NOT_CONNECTED_MESSAGE.to_string(), cx);
        return;
    };
    let entries = conversation_entries(service, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let result = entries.await;
        workspace
            .update_in(cx, |workspace, window, cx| match result {
                Err(error) => show_toast(workspace, error.to_string(), cx),
                Ok(entries) => pick_and_change(
                    workspace,
                    format!("Allow for {project}: "),
                    entries,
                    project,
                    allow_in,
                    window,
                    cx,
                ),
            })
            .ok();
    })
    .detach();
}

/// `slack: disallow channel`: one of the conversations allowed for the active task's project.
pub(crate) fn disallow_channel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    with_project(workspace, window, cx, pick_to_disallow);
}

fn pick_to_disallow(
    workspace: &mut Workspace,
    project: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let dir = connection::installed_dir(cx);
    let list = load_in(&dir, &project);
    if list.is_empty() {
        show_toast(
            workspace,
            format!("No conversation is allowed for {project}"),
            cx,
        );
        return;
    }
    let known = SlackService::global(cx)
        .map(|service| {
            let service = service.read(cx);
            service
                .cached_conversations()
                .map(|list| picker_entries(list, &service.user_names()))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let entries = allowed_entries(&list, &names_in(&dir, &project), &known);
    pick_and_change(
        workspace,
        format!("Disallow for {project}: "),
        entries,
        project,
        disallow_in,
        window,
        cx,
    );
}
