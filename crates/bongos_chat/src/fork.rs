//! Fork and fork to task (design D15): branching a chat into a new one.
//!
//! - `chat: fork` branches the shown chat in its own scope: the new record goes in the same
//!   `chats/`, runs in the same directory, and keeps the source's `prompts_file` when it has one.
//! - `chat: fork to task` asks for a task and branches into that task: the record goes in the
//!   task's `chats/`, `type = implement` (`chat: fork to task as` asks the type), `cwd` is where a
//!   new chat on that task runs (its worktree when it has one), and there is no `prompts_file`, so
//!   the chat points at the task's own `prompts.txt`. The chat is given the task's shared
//!   artifacts, never the source's.
//! - The new record has `parent = <source id>` and `fork = "native"` or `"handoff"`.
//!
//! The fork is native when the CLI has the command (Claude `--fork-session`, Codex `fork`) and the
//! source's session id is known. Otherwise it is a handoff: a fresh session whose instructions
//! carry a note naming the source (name, type, agent and the path of its transcript when the
//! agent's own files hold it), never anything of the source's artifacts. A handoff is always
//! labelled; nothing pretends to be a native fork.
//!
//! A fork never writes to the source's record or to anything of its. Opening a chat whose
//! directory is gone offers a fork instead (D6).
//!
//! While `chats.enabled` is false nothing here runs; the commands only say so. The new chat is
//! shown in the Chats dock without moving focus.

use std::fs;
use std::path::{Path, PathBuf};

use bongos_chat_model::{
    CODEX_TABLE, Capabilities, ChatRecord, Fork, HandoffParent, LaunchKind, adapter_for,
    read_record, resolve,
};
use bongos_work::{WorkSettings, pick, with_task};
use bongos_work_model::session_context;
use gpui::{App, AppContext as _, Context, Entity, Task, Window};
use project::Project;
use settings::Settings as _;
use workspace::Workspace;

use crate::actions::chat;
use crate::cleanup::claude_transcript;
use crate::commands::{
    chat_name, chats_on, discard_unstarted, list_types, local_projects, show_when_indexed,
    shown_chat, snapshot,
};
use crate::scope::{IMPLEMENT_TYPE, resolve_scope};
use crate::send::show_toast;
use crate::{
    ChatHealth, ChatRuntime, ChatsSettings, LaunchRequest, SendError, fix_not_probed, task_facts,
};

/// How deep under Codex's sessions directory its session files are (`YYYY/MM/DD`).
const CODEX_SESSION_DEPTH: usize = 3;

/// Where a fork goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ForkTarget {
    /// The source's own scope and directory, with its prompts file.
    SameScope,
    /// A task: its `chats/`, its directory, its own prompts file. `chat_type` `None` is implement.
    Task {
        task_id: String,
        chat_type: Option<String>,
    },
}

/// What a fork is asked.
#[derive(Clone, Debug)]
pub(crate) struct ForkRequest {
    pub source_id: String,
    pub target: ForkTarget,
    /// Start a handoff even where a native fork could run.
    pub force_handoff: bool,
    /// The CLI's capabilities; `None` takes them from the global `ChatHealth`.
    pub caps: Option<Capabilities>,
}

/// A chat that was forked and launched.
#[derive(Clone, Debug)]
pub(crate) struct Forked {
    pub chat_id: String,
    /// The new chat's name.
    pub name: String,
    pub source_name: String,
    pub fork: Fork,
}

impl Forked {
    /// What the toast says.
    pub(crate) fn toast(&self) -> String {
        match self.fork {
            Fork::Native => format!("{}: forked from {}", self.name, self.source_name),
            Fork::Handoff => format!(
                "{}: {} {}",
                self.name,
                fork_label(Fork::Handoff),
                self.source_name
            ),
        }
    }
}

/// How a fork is called on the row, in the toast and in the artifacts view. A handoff never says
/// "forked" alone.
pub(crate) fn fork_label(fork: Fork) -> &'static str {
    match fork {
        Fork::Native => "forked from",
        Fork::Handoff => "forked as new chat with handoff from",
    }
}

/// Where the new chat is recorded and runs, decided from the Work snapshot before anything is
/// read from disk.
#[derive(Clone)]
struct Placement {
    scope_dir: PathBuf,
    task_id: String,
    chat_type: String,
    /// Where it runs, when that directory is there.
    cwd: PathBuf,
    /// Where it runs when `cwd` is gone.
    fallback_cwd: Option<PathBuf>,
}

/// Forks the chat `request.source_id` and launches the fork in the first local project of
/// `projects`. Resolves to the new chat. A refusal is the text to show. When the launch is
/// refused the new record, which never ran, is removed again if the cleanup is sure of that.
pub(crate) fn fork_chat(
    request: ForkRequest,
    projects: Vec<Entity<Project>>,
    cx: &mut App,
) -> Task<Result<Forked, String>> {
    if !ChatsSettings::get_global(cx).enabled {
        return Task::ready(Err(SendError::Off.to_string()));
    }
    let Some(runtime) = ChatRuntime::global(cx) else {
        return Task::ready(Err(SendError::Off.to_string()));
    };
    let store = runtime.read(cx).store().clone();
    let Some(indexed) = store.read(cx).find(&request.source_id).cloned() else {
        return Task::ready(Err(format!("No chat has the id {}", request.source_id)));
    };
    let Some(cached) = indexed.entry.record().cloned() else {
        return Task::ready(Err(
            "The chat's record cannot be read, so it cannot be forked".to_string(),
        ));
    };
    let Some(agent) = cached.known_agent() else {
        return Task::ready(Err(format!("Unknown agent `{}`", cached.agent)));
    };
    let caps = request.caps.clone().or_else(|| {
        ChatHealth::global(cx).and_then(|health| health.read(cx).health(agent).caps.clone())
    });
    let Some(caps) = caps else {
        return Task::ready(Err(fix_not_probed(agent)));
    };
    let tree = match snapshot(cx) {
        Ok(tree) => tree,
        Err(message) => return Task::ready(Err(message)),
    };
    let repositories = WorkSettings::get_global(cx).repositories.clone();
    let placement = match &request.target {
        ForkTarget::SameScope => {
            let Some(task_id) = indexed.task_id.clone() else {
                return Task::ready(Err("This chat is on no task".to_string()));
            };
            let fallback_cwd =
                resolve_scope(&tree, &task_id, &cached.chat_type, &repositories, None)
                    .map(|scope| scope.cwd)
                    .ok()
                    .or_else(|| Some(indexed.scope_dir.clone()));
            Placement {
                scope_dir: indexed.scope_dir.clone(),
                task_id,
                chat_type: cached.chat_type.clone(),
                cwd: cached.cwd,
                fallback_cwd,
            }
        }
        ForkTarget::Task { task_id, chat_type } => {
            let chat_type = chat_type
                .clone()
                .unwrap_or_else(|| IMPLEMENT_TYPE.to_string());
            match resolve_scope(&tree, task_id, &chat_type, &repositories, None) {
                Ok(scope) => Placement {
                    scope_dir: scope.scope_dir,
                    task_id: scope.task_id,
                    chat_type,
                    cwd: scope.cwd,
                    fallback_cwd: None,
                },
                Err(message) => return Task::ready(Err(message)),
            }
        }
    };
    let facts = task_facts(&tree, &placement.task_id).unwrap_or_default();
    let types_root = store.read(cx).types_root(cx);
    let home = runtime.read(cx).home().clone();
    let env = ChatHealth::global(cx)
        .map(|health| health.read(cx).env().clone())
        .unwrap_or_default();
    let runtime_root = store.read(cx).dirs().runtime.clone();
    let source_path = indexed.entry.path.clone();
    let same_scope = request.target == ForkTarget::SameScope;
    let force_handoff = request.force_handoff;
    let launch_caps = caps.clone();

    cx.spawn(async move |cx| {
        let read_home = home.clone();
        let bg = placement.clone();
        let (record, source, native, handoff, resolved, context, source_session) = cx
            .background_spawn(async move {
                let source = read_record(&source_path)
                    .map_err(|error| format!("Could not read the chat to fork: {error}"))?;
                let adapter = adapter_for(agent, agent.as_str());
                let native = !force_handoff
                    && adapter.shape(&caps).native_fork
                    && source.session_id.is_some();
                let resolved = resolve(
                    &bg.chat_type,
                    agent,
                    &types_root,
                    &read_home.join(".claude"),
                )
                .map_err(|error| format!("The session type `{}`: {error}", bg.chat_type))?;
                let context = session_context(&bg.scope_dir).map_err(|error| {
                    format!("Could not read what the task shares with chats: {error}")
                })?;
                let cwd = if bg.cwd.is_dir() {
                    bg.cwd.clone()
                } else {
                    placement
                        .fallback_cwd
                        .clone()
                        .unwrap_or_else(|| bg.cwd.clone())
                };
                let mut record = ChatRecord::new(agent, &bg.chat_type, cwd);
                record.layout = resolved.config.layout;
                if bg.chat_type == source.chat_type {
                    record.model = source.model.clone();
                }
                record.parent = Some(source.id.clone());
                record.fork = Some(if native { Fork::Native } else { Fork::Handoff });
                // A same-scope fork keeps pointing at the source's prompts file; one into a task
                // points at the task's own.
                if same_scope {
                    record.prompts_file = source.prompts_file.clone();
                }
                let handoff = (!native).then(|| HandoffParent {
                    name: source.name.clone(),
                    chat_type: source.chat_type.clone(),
                    agent: source.agent.clone(),
                    transcript: parent_transcript(&read_home, &source),
                });
                let session = source.session_id.clone().unwrap_or_default();
                Ok::<_, String>((record, source, native, handoff, resolved, context, session))
            })
            .await?;

        let chat_id = record.id.clone();
        let name = record.name.clone();
        let fork = if native { Fork::Native } else { Fork::Handoff };
        let created = store.update(cx, |store, cx| {
            store.create(placement.scope_dir.clone(), record.clone(), cx)
        });
        let record_path = created.await.map_err(|error| error.to_string())?;

        let launch_request = LaunchRequest {
            record,
            record_path: record_path.clone(),
            scope_dir: placement.scope_dir,
            task_id: Some(placement.task_id),
            kind: LaunchKind::Fork {
                source_session: source_session,
                native,
            },
            context,
            facts,
            resolved,
            handoff,
            caps: Some(launch_caps),
            force: Vec::new(),
        };
        let launch = runtime.update(cx, |runtime, cx| {
            runtime.launch(launch_request, &projects, cx)
        });
        if let Err(error) = launch.await {
            discard_unstarted(&store, record_path, &chat_id, runtime_root, home, env, cx).await;
            return Err(error.to_string());
        }
        Ok(Forked {
            chat_id,
            name,
            source_name: source.name,
            fork,
        })
    })
}

/// The path of the source's own conversation file, when the agent's files hold it. Claude keeps it
/// under `~/.claude/projects/` by directory and session id, Codex under its sessions directory.
fn parent_transcript(home: &Path, source: &ChatRecord) -> Option<PathBuf> {
    let session = source.session_id.as_deref()?;
    match source.known_agent()? {
        bongos_chat_model::Agent::Claude => claude_transcript(home, &source.cwd, session)??.into(),
        bongos_chat_model::Agent::Codex => {
            let mut root = home.to_path_buf();
            root.extend(CODEX_TABLE.sessions_dir);
            find_rollout(&root, session, CODEX_SESSION_DEPTH)
        }
    }
}

/// The Codex session file `rollout-<time>-<session>.jsonl` under `dir`, looking `depth` levels
/// down.
fn find_rollout(dir: &Path, session: &str, depth: usize) -> Option<PathBuf> {
    let suffix = format!("-{session}.jsonl");
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0
                && let Some(found) = find_rollout(&path, session, depth - 1)
            {
                return Some(found);
            }
            continue;
        }
        let named = path.file_name().and_then(|name| name.to_str());
        if named.is_some_and(|name| {
            name.starts_with(CODEX_TABLE.rollout_prefix) && name.ends_with(&suffix)
        }) {
            return Some(path);
        }
    }
    None
}

/// Registers `chat: fork`, `chat: fork to task` and `chat: fork to task as` on `workspace`.
pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &chat::Fork, window, cx| fork(w, window, cx))
        .register_action(|w, _: &chat::ForkToTask, window, cx| fork_to_task(false, w, window, cx))
        .register_action(|w, _: &chat::ForkToTaskAs, window, cx| fork_to_task(true, w, window, cx));
}

/// `chat: fork`: the shown chat, in its own scope.
fn fork(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(chat_id) = shown_chat(workspace, cx) else {
        return;
    };
    start_fork(chat_id, ForkTarget::SameScope, false, workspace, window, cx);
}

/// `chat: fork to task` (`ask_type`: `chat: fork to task as`): the shown chat, into a task.
fn fork_to_task(
    ask_type: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(chat_id) = shown_chat(workspace, cx) else {
        return;
    };
    pick_task_and_fork(chat_id, ask_type, workspace, window, cx);
}

/// Asks which task, and with `ask_type` which session type, then forks `chat_id` into it.
fn pick_task_and_fork(
    chat_id: String,
    ask_type: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    with_task(
        workspace,
        window,
        cx,
        "Fork to which task?",
        "There is no task",
        |task, _| task.file.is_some(),
        Box::new(move |task_id, workspace, window, cx| {
            if ask_type {
                pick_type_and_fork(chat_id, task_id, workspace, window, cx);
            } else {
                let target = ForkTarget::Task {
                    task_id,
                    chat_type: None,
                };
                start_fork(chat_id, target, false, workspace, window, cx);
            }
        }),
    );
}

fn pick_type_and_fork(
    chat_id: String,
    task_id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(store) = ChatRuntime::global(cx).map(|runtime| runtime.read(cx).store().clone())
    else {
        return;
    };
    let types_root = store.read(cx).types_root(cx);
    let types = list_types(&types_root);
    if types.is_empty() {
        show_toast(
            workspace,
            format!("There is no session type under {}", types_root.display()),
            cx,
        );
        return;
    }
    let (names, labels): (Vec<String>, Vec<String>) = types.into_iter().unzip();
    pick(
        workspace,
        "Fork as which session type?",
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            let target = ForkTarget::Task {
                task_id,
                chat_type: Some(names[index].clone()),
            };
            start_fork(chat_id, target, false, workspace, window, cx)
        }),
        window,
        cx,
    );
}

/// Forks `chat_id` and shows the new chat once the index lists it; a toast says what happened.
fn start_fork(
    chat_id: String,
    target: ForkTarget,
    force_handoff: bool,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let projects = local_projects(workspace, cx);
    let request = ForkRequest {
        source_id: chat_id,
        target,
        force_handoff,
        caps: None,
    };
    let forked = fork_chat(request, projects, cx);
    cx.spawn_in(window, async move |workspace, cx| match forked.await {
        Ok(forked) => {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, forked.toast(), cx)
                })
                .ok();
            show_when_indexed(&workspace, &forked.chat_id, cx).await;
        }
        Err(message) => {
            workspace
                .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                .ok();
        }
    })
    .detach();
}

/// The directory a closed chat resumes in is gone: offers to fork it into the place its scope
/// resolves to now, as a handoff, or into a task. Nothing happens without a choice.
pub(crate) fn offer_fork_for_missing_directory(
    chat_id: String,
    name: String,
    cwd: PathBuf,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !chats_on(workspace, cx) {
        return;
    }
    let name = if name.is_empty() {
        chat_name(&chat_id, cx)
    } else {
        name
    };
    pick(
        workspace,
        format!(
            "The directory of {name} is gone ({}), so it cannot resume there. Fork it?",
            cwd.display()
        ),
        vec![
            format!("Fork {name} here, as a new chat with handoff"),
            format!("Fork {name} to a task"),
            "Cancel".to_string(),
        ],
        0,
        Box::new(move |index, workspace, window, cx| match index {
            0 => start_fork(chat_id, ForkTarget::SameScope, true, workspace, window, cx),
            1 => pick_task_and_fork(chat_id, false, workspace, window, cx),
            _ => {}
        }),
        window,
        cx,
    );
}
