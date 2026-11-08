//! The chat commands (design D6, D17 and D26): starting, opening, cycling, closing, archiving,
//! restoring, renaming and deleting chats.
//!
//! - `chat: new` works on the focused task (a task picker asks when none is), asks for the agent
//!   (only CLIs not known to be missing or logged out, the last one used first) and the session
//!   type, then starts the chat where [`crate::scope`] says. `chat: new plan` and the other four
//!   shortcuts skip the type picker. `chat: new with options` also asks the name, the model (with
//!   the type's default) and the layout, and for a project chat in a project with several
//!   repositories, which repository it runs in.
//! - `chat: open` picks one of the focused task's open chats, most recent first (`chat: open any`:
//!   every task's); a running chat is shown, a closed one resumed in its recorded directory with
//!   its recorded session id.
//! - `chat: next` and `chat: previous` cycle through the running chats of the task in list order.
//! - `chat: close` ends the shown chat's process and keeps its record; while the CLI is in the
//!   middle of a turn it asks first. A never-prompted chat may be cleaned up (see
//!   [`crate::cleanup`]).
//!
//! - `chat: archive` ends the shown chat's process if it runs and moves the record to the archive;
//!   `chat: restore` and `chat: show archive` pick an archived chat of the focused task (else of
//!   every task) and move it back to the open list. `chat: rename` sets a name that no later send
//!   or title replaces. `chat: delete` asks, then removes the record and its launch files and
//!   nothing else. None of these deletes anything but `chat: delete`; the CLI's own transcripts
//!   are never touched. See [`crate::lifecycle`] for the offer to archive a closed task's chats.
//!
//! Showing a chat opens the Chats dock without moving focus: the terminal never takes focus by
//! itself. While `chats.enabled` is false every command only says so.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bongos_chat_model::{
    Agent, ChatRecord, LaunchKind, Layout, NamedBy, SHIPPED_TYPES, SKILLS_DIR, TaskFacts,
    parse_type_config, read_record, resolve, timestamp_utc,
};
use bongos_work::{WorkSettings, WorkStore, ask, pick, ready, with_task};
use bongos_work_model::session_context;
use bongos_work_model::store::WorkTree;
use gpui::{
    App, AppContext as _, AsyncApp, AsyncWindowContext, Context, Entity, Task, WeakEntity, Window,
};
use project::Project;
use settings::Settings as _;
use workspace::{MultiWorkspace, Workspace};

use crate::actions::chat;
use crate::cleanup::{Cleanup, cleanup_verdict};
use crate::health::agent_title;
use crate::live_modes::is_open;
use crate::panel::{NO_CHAT, enabled_panel};
use crate::scope::{ChatScope, project_repositories, resolve_scope};
use crate::send::show_toast;
use crate::{
    ChatHealth, ChatPanel, ChatRuntime, ChatStore, ChatsSettings, Health, HealthState,
    LaunchRequest, PROBE_TIMEOUT, SendError, SendOptions, send, task_facts,
};

/// How long a new chat waits for the index to list its record before the panel shows it.
const INDEX_WAIT: Duration = Duration::from_secs(10);
/// How often waits here look again.
const POLL: Duration = Duration::from_millis(50);

/// What `chat: new` and its variants ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NewChat {
    /// The session type; `None` asks.
    pub chat_type: Option<String>,
    /// Ask the name, model, layout (and a project chat's repository).
    pub options: bool,
}

impl NewChat {
    fn asking() -> Self {
        Self {
            chat_type: None,
            options: false,
        }
    }

    fn of(chat_type: &str) -> Self {
        Self {
            chat_type: Some(chat_type.to_string()),
            options: false,
        }
    }
}

/// Everything a new chat is started with.
#[derive(Clone, Debug)]
pub(crate) struct ChatPlan {
    pub scope: ChatScope,
    pub agent: Agent,
    pub chat_type: String,
    /// Kyle's name for it; `None` keeps the default name.
    pub name: Option<String>,
    /// A model other than the type's default.
    pub model: Option<String>,
    /// A layout other than the type's.
    pub layout: Option<Layout>,
}

/// Registers the commands of this module on `workspace`.
pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &chat::New, window, cx| new_chat(NewChat::asking(), w, window, cx))
        .register_action(|w, _: &chat::NewWithOptions, window, cx| {
            let spec = NewChat {
                chat_type: None,
                options: true,
            };
            new_chat(spec, w, window, cx)
        })
        .register_action(|w, _: &chat::NewPlan, window, cx| {
            new_chat(NewChat::of("plan"), w, window, cx)
        })
        .register_action(|w, _: &chat::NewImplement, window, cx| {
            new_chat(NewChat::of("implement"), w, window, cx)
        })
        .register_action(|w, _: &chat::NewReview, window, cx| {
            new_chat(NewChat::of("review"), w, window, cx)
        })
        .register_action(|w, _: &chat::NewThinking, window, cx| {
            new_chat(NewChat::of("thinking"), w, window, cx)
        })
        .register_action(|w, _: &chat::NewFree, window, cx| {
            new_chat(NewChat::of("free"), w, window, cx)
        })
        .register_action(|w, _: &chat::Open, window, cx| open(false, w, window, cx))
        .register_action(|w, _: &chat::OpenAny, window, cx| open(true, w, window, cx))
        .register_action(|w, _: &chat::Next, window, cx| cycle(1, w, window, cx))
        .register_action(|w, _: &chat::Previous, window, cx| cycle(-1, w, window, cx))
        .register_action(|w, _: &chat::Close, window, cx| close(w, window, cx))
        .register_action(|w, _: &chat::Archive, window, cx| archive(w, window, cx))
        .register_action(|w, _: &chat::Restore, window, cx| restore(RESTORE_TITLE, w, window, cx))
        .register_action(|w, _: &chat::ShowArchive, window, cx| {
            restore(ARCHIVE_TITLE, w, window, cx)
        })
        .register_action(|w, _: &chat::Rename, window, cx| rename(w, window, cx))
        .register_action(|w, _: &chat::Delete, window, cx| delete(w, window, cx));
}

/// Whether chats are on; otherwise a toast says they are off.
fn chats_on(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> bool {
    let on = ChatsSettings::get_global(cx).enabled && ChatRuntime::global(cx).is_some();
    if !on {
        show_toast(workspace, SendError::Off.to_string(), cx);
    }
    on
}

/// `chat: new` and its variants: on the focused task, else on a task picked by name.
fn new_chat(
    spec: NewChat,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !chats_on(workspace, cx) {
        return;
    }
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    match bongos_work::focused(&tree, cx) {
        Some(id) => new_chat_on(id, spec, workspace, window, cx),
        None => with_task(
            workspace,
            window,
            cx,
            "Chat on which task?",
            "There is no task",
            |task, _| task.file.is_some(),
            Box::new(move |id, workspace, window, cx| new_chat_on(id, spec, workspace, window, cx)),
        ),
    }
}

/// The scope of a new `chat_type` chat on `task_id`, from the Work snapshot and settings.
fn scope_for(
    task_id: &str,
    chat_type: &str,
    repository: Option<&str>,
    cx: &App,
) -> Result<ChatScope, String> {
    let tree = snapshot(cx)?;
    let repositories = &WorkSettings::get_global(cx).repositories;
    resolve_scope(&tree, task_id, chat_type, repositories, repository)
}

fn snapshot(cx: &App) -> Result<std::sync::Arc<WorkTree>, String> {
    WorkStore::global(cx)
        .and_then(|store| store.read(cx).snapshot())
        .ok_or_else(|| "The Work tree is still loading; try again in a moment".to_string())
}

/// `chat: new` on the task `task_id`: refreshes the CLI health, then asks for the agent.
pub(crate) fn new_chat_on(
    task_id: String,
    spec: NewChat,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // A shortcut knows its type: what cannot start is refused before anything is asked.
    if let Some(chat_type) = &spec.chat_type
        && let Err(message) = scope_for(&task_id, chat_type, None, cx)
    {
        show_toast(workspace, message, cx);
        return;
    }
    let Some(health) = ChatHealth::global(cx) else {
        show_toast(workspace, SendError::Off.to_string(), cx);
        return;
    };
    health.update(cx, |health, cx| health.refresh(cx));
    if !health.read(cx).is_probing() {
        choose_agent(task_id, spec, workspace, window, cx);
        return;
    }
    cx.spawn_in(window, async move |workspace, cx| {
        let deadline = Instant::now() + PROBE_TIMEOUT + Duration::from_secs(1);
        while health.read_with(cx, |health, _| health.is_probing()) && Instant::now() < deadline {
            cx.background_executor().timer(POLL).await;
        }
        workspace
            .update_in(cx, |workspace, window, cx| {
                choose_agent(task_id, spec, workspace, window, cx)
            })
            .ok();
    })
    .detach();
}

/// The agents `chat: new` offers, with their picker labels: every CLI not known to be missing or
/// logged out (design D19), `last` first.
pub fn agent_choices(health: &[Health], last: Option<Agent>) -> Vec<(Agent, String)> {
    let mut offered: Vec<&Health> = health.iter().filter(|health| health.is_offered()).collect();
    if let Some(last) = last {
        offered.sort_by_key(|health| health.agent != last);
    }
    offered
        .into_iter()
        .map(|health| {
            let name = health.agent.as_str();
            let title = agent_title(health.agent);
            let label = match &health.state {
                HealthState::Ready { version, .. } => format!("{name}: {title}, {version}"),
                _ => format!("{name}: {title} ({})", health.fix),
            };
            (health.agent, label)
        })
        .collect()
}

/// The agent offered first: the last one `chat: new` started in this session, else the agent of
/// the newest chat on record.
fn last_agent(cx: &App) -> Option<Agent> {
    let runtime = ChatRuntime::global(cx)?;
    let runtime = runtime.read(cx);
    runtime.last_agent().or_else(|| {
        runtime
            .store()
            .read(cx)
            .index()
            .values()
            .filter_map(|chat| chat.entry.record())
            .max_by_key(|record| timestamp_utc(&record.created))
            .and_then(ChatRecord::known_agent)
    })
}

fn choose_agent(
    task_id: String,
    spec: NewChat,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(health) = ChatHealth::global(cx) else {
        return;
    };
    let choices = agent_choices(health.read(cx).all(), last_agent(cx));
    if choices.is_empty() {
        let fixes: Vec<String> = health
            .read(cx)
            .all()
            .iter()
            .map(|health| health.fix.clone())
            .filter(|fix| !fix.is_empty())
            .collect();
        show_toast(
            workspace,
            format!("No agent CLI can start a chat: {}", fixes.join("; ")),
            cx,
        );
        return;
    }
    let (agents, labels): (Vec<Agent>, Vec<String>) = choices.into_iter().unzip();
    pick(
        workspace,
        "Chat with which agent?",
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            choose_type(task_id, spec, agents[index], workspace, window, cx)
        }),
        window,
        cx,
    );
}

/// The session types under `types_root`, as (name, picker label): every directory with a
/// `type.toml`, the shipped ones first in their order, then the others by name.
pub fn list_types(types_root: &Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(types_root) else {
        return Vec::new();
    };
    let mut types: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            let config = entry.path().join("type.toml");
            if name == SKILLS_DIR || name.starts_with('.') || !config.is_file() {
                return None;
            }
            let description = std::fs::read_to_string(&config)
                .ok()
                .and_then(|text| parse_type_config(&text, Some(&config)).ok())
                .map(|config| config.description)
                .unwrap_or_else(|| "type.toml has an error".to_string());
            let label = format!("{name}: {description}");
            Some((name, label))
        })
        .collect();
    let rank = |name: &str| {
        SHIPPED_TYPES
            .iter()
            .position(|shipped| shipped.name == name)
            .unwrap_or(SHIPPED_TYPES.len())
    };
    types.sort_by(|(a, _), (b, _)| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    types
}

fn choose_type(
    task_id: String,
    spec: NewChat,
    agent: Agent,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(chat_type) = spec.chat_type.clone() {
        configure(task_id, spec, agent, chat_type, workspace, window, cx);
        return;
    }
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
        "Which session type?",
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            let chat_type = names[index].clone();
            configure(task_id, spec, agent, chat_type, workspace, window, cx)
        }),
        window,
        cx,
    );
}

fn configure(
    task_id: String,
    spec: NewChat,
    agent: Agent,
    chat_type: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let scope = match scope_for(&task_id, &chat_type, None, cx) {
        Ok(scope) => scope,
        Err(message) => {
            show_toast(workspace, message, cx);
            return;
        }
    };
    let plan = ChatPlan {
        scope,
        agent,
        chat_type,
        name: None,
        model: None,
        layout: None,
    };
    if !spec.options {
        start_new(plan, workspace, window, cx);
        return;
    }
    let repositories = snapshot(cx)
        .map(|tree| project_repositories(&tree, &task_id))
        .unwrap_or_default();
    if plan.scope.project_chat && repositories.len() > 1 {
        pick(
            workspace,
            "Run the project chat in which repository?",
            repositories.clone(),
            0,
            Box::new(move |index, workspace, window, cx| {
                let repository = &repositories[index];
                match scope_for(&task_id, &plan.chat_type, Some(repository), cx) {
                    Ok(scope) => ask_name(ChatPlan { scope, ..plan }, workspace, window, cx),
                    Err(message) => show_toast(workspace, message, cx),
                }
            }),
            window,
            cx,
        );
        return;
    }
    ask_name(plan, workspace, window, cx);
}

fn ask_name(
    plan: ChatPlan,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // A new record carries the default name, "<type> <agent> <date>".
    let default = ChatRecord::new(plan.agent, &plan.chat_type, PathBuf::new()).name;
    ask(
        workspace,
        format!("Name of the chat (empty: {default})"),
        "",
        true,
        Box::new(move |name, workspace, window, cx| {
            let name = (!name.is_empty()).then_some(name);
            ask_model(ChatPlan { name, ..plan }, workspace, window, cx)
        }),
        window,
        cx,
    );
}

fn ask_model(
    plan: ChatPlan,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let type_model = type_default(&plan, cx).and_then(|(_, model)| model);
    let initial = type_model.clone().unwrap_or_default();
    ask(
        workspace,
        "Model (empty: the CLI's own choice)",
        &initial,
        true,
        Box::new(move |model, workspace, window, cx| {
            // The type's own model is used anyway; only another one is recorded.
            let model = (!model.is_empty() && Some(&model) != type_model.as_ref()).then_some(model);
            ask_layout(ChatPlan { model, ..plan }, workspace, window, cx)
        }),
        window,
        cx,
    );
}

fn ask_layout(
    plan: ChatPlan,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let type_layout = type_default(&plan, cx)
        .map(|(layout, _)| layout)
        .unwrap_or_default();
    let layouts = [Layout::Dual, Layout::ChatOnly];
    let labels = vec![
        "dual: the terminal and the prompts file".to_string(),
        "chat-only: the terminal alone".to_string(),
    ];
    let selected = layouts
        .iter()
        .position(|layout| *layout == type_layout)
        .unwrap_or(0);
    pick(
        workspace,
        "Which layout?",
        labels,
        selected,
        Box::new(move |index, workspace, window, cx| {
            let layout = (layouts[index] != type_layout).then_some(layouts[index]);
            start_new(ChatPlan { layout, ..plan }, workspace, window, cx)
        }),
        window,
        cx,
    );
}

/// The layout and model the plan's type gives its agent, read fresh.
fn type_default(plan: &ChatPlan, cx: &App) -> Option<(Layout, Option<String>)> {
    let runtime = ChatRuntime::global(cx)?;
    let runtime = runtime.read(cx);
    let types_root = runtime.store().read(cx).types_root(cx);
    let resolved = resolve(
        &plan.chat_type,
        plan.agent,
        &types_root,
        &runtime.home().join(".claude"),
    )
    .ok()?;
    Some((resolved.config.layout, resolved.overrides.model))
}

/// Starts the chat of `plan` and shows it once the index lists it. A refusal is a toast.
fn start_new(
    plan: ChatPlan,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let projects = local_projects(workspace, cx);
    let agent = plan.agent;
    let start = start_chat(plan, projects, cx);
    cx.spawn_in(window, async move |workspace, cx| match start.await {
        Ok(chat_id) => {
            cx.update(|_, cx| {
                if let Some(runtime) = ChatRuntime::global(cx) {
                    runtime.update(cx, |runtime, _| runtime.set_last_agent(agent));
                }
            })
            .ok();
            show_when_indexed(&workspace, &chat_id, cx).await;
        }
        Err(message) => {
            workspace
                .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                .ok();
        }
    })
    .detach();
}

/// Creates the record of `plan` and launches it in the first local project of `projects`.
/// Resolves to the chat id. When the launch is refused the record, which never ran, is removed
/// again.
pub(crate) fn start_chat(
    plan: ChatPlan,
    projects: Vec<Entity<Project>>,
    cx: &mut App,
) -> Task<Result<String, String>> {
    let Some(runtime) = ChatRuntime::global(cx) else {
        return Task::ready(Err(SendError::Off.to_string()));
    };
    let store = runtime.read(cx).store().clone();
    let types_root = store.read(cx).types_root(cx);
    let claude_home = runtime.read(cx).home().join(".claude");
    let home = runtime.read(cx).home().clone();
    let env = ChatHealth::global(cx)
        .map(|health| health.read(cx).env().clone())
        .unwrap_or_default();
    let runtime_root = store.read(cx).dirs().runtime.clone();
    let facts = snapshot(cx)
        .ok()
        .and_then(|tree| task_facts(&tree, &plan.scope.task_id))
        .unwrap_or_default();
    cx.spawn(async move |cx| {
        let chat_type = plan.chat_type.clone();
        let agent = plan.agent;
        let scope_dir = plan.scope.scope_dir.clone();
        let (resolved, context) = cx
            .background_spawn(async move {
                let resolved = resolve(&chat_type, agent, &types_root, &claude_home)
                    .map_err(|error| format!("The session type `{chat_type}`: {error}"))?;
                let context = session_context(&scope_dir).map_err(|error| {
                    format!("Could not read what the task shares with chats: {error}")
                })?;
                Ok::<_, String>((resolved, context))
            })
            .await?;

        let mut record = ChatRecord::new(plan.agent, &plan.chat_type, plan.scope.cwd.clone());
        record.layout = plan.layout.unwrap_or(resolved.config.layout);
        if let Some(name) = plan.name {
            record.name = name;
            record.named_by = NamedBy::Kyle;
        }
        record.model = plan.model;
        let chat_id = record.id.clone();
        let created = store.update(cx, |store, cx| {
            store.create(plan.scope.scope_dir.clone(), record.clone(), cx)
        });
        let record_path = created.await.map_err(|error| error.to_string())?;

        let request = LaunchRequest {
            record,
            record_path: record_path.clone(),
            scope_dir: plan.scope.scope_dir,
            task_id: Some(plan.scope.task_id),
            kind: LaunchKind::New,
            context,
            facts,
            resolved,
            handoff: None,
            caps: None,
            force: Vec::new(),
        };
        let launch = runtime.update(cx, |runtime, cx| runtime.launch(request, &projects, cx));
        if let Err(error) = launch.await {
            // Nothing was said in it: the record of this attempt goes, with its launch files, when
            // the cleanup is sure of that (see [`crate::cleanup`]) and stays otherwise.
            let removed = store.update(cx, |store, cx| {
                store.delete_if(
                    record_path,
                    move |record| {
                        cleanup_verdict(record, false, None, &home, &env, || None)
                            == Cleanup::Delete
                    },
                    cx,
                )
            });
            match removed.await {
                Ok(true) => {
                    remove_runtime_dir(&runtime_root, &chat_id, cx).await;
                }
                Ok(false) => {}
                Err(remove_error) => log::warn!(
                    "chats: could not remove the record of a chat that did not start: {remove_error}"
                ),
            }
            return Err(error.to_string());
        }
        Ok(chat_id)
    })
}

/// Resumes the closed chat `chat_id` in its recorded directory with its recorded session id, with
/// freshly generated launch files. Refused for an archived chat and when the directory is gone.
pub(crate) fn resume_chat(
    chat_id: &str,
    projects: Vec<Entity<Project>>,
    cx: &mut App,
) -> Task<Result<(), String>> {
    let Some(runtime) = ChatRuntime::global(cx) else {
        return Task::ready(Err(SendError::Off.to_string()));
    };
    let store = runtime.read(cx).store().clone();
    let Some(indexed) = store.read(cx).find(chat_id).cloned() else {
        return Task::ready(Err(format!("No chat has the id {chat_id}")));
    };
    let types_root = store.read(cx).types_root(cx);
    let claude_home = runtime.read(cx).home().join(".claude");
    let facts = indexed
        .task_id
        .as_deref()
        .and_then(|task_id| task_facts(&*snapshot(cx).ok()?, task_id))
        .unwrap_or_else(TaskFacts::default);
    let record_path = indexed.entry.path.clone();
    let chat_id = chat_id.to_string();
    cx.spawn(async move |cx| {
        let scope_dir = indexed.scope_dir.clone();
        let read_path = record_path.clone();
        let (record, resolved, context) = cx
            .background_spawn(async move {
                let record = read_record(&read_path).map_err(|error| error.to_string())?;
                if record.archived {
                    return Err(format!(
                        "{} is archived; restore it first (chat: restore)",
                        record.name
                    ));
                }
                if !record.cwd.is_dir() {
                    return Err(format!(
                        "The directory of {} is gone ({}), and a resume runs there; fork it into a \
                         task instead (chat: fork to task)",
                        record.name,
                        record.cwd.display()
                    ));
                }
                let agent = record
                    .known_agent()
                    .ok_or_else(|| format!("Unknown agent `{}`", record.agent))?;
                let resolved = resolve(&record.chat_type, agent, &types_root, &claude_home)
                    .map_err(|error| format!("The session type `{}`: {error}", record.chat_type))?;
                let context = session_context(&scope_dir).map_err(|error| {
                    format!("Could not read what the task shares with chats: {error}")
                })?;
                Ok((record, resolved, context))
            })
            .await?;

        let launch = runtime.update(cx, |runtime, cx| {
            // An exited chat the panel still shows gives way to the resumed one.
            if runtime.live(&chat_id).is_some() {
                runtime.close(&chat_id, cx);
            }
            let request = LaunchRequest {
                record,
                record_path,
                scope_dir: indexed.scope_dir,
                task_id: indexed.task_id,
                kind: LaunchKind::Resume,
                context,
                facts,
                resolved,
                handoff: None,
                caps: None,
                force: Vec::new(),
            };
            runtime.launch(request, &projects, cx)
        });
        launch.await.map_err(|error| error.to_string())
    })
}

/// Shows the chat `chat_id`: a running one at once, a closed one once it is resumed. The task
/// resolves when the chat runs and is shown, or to why it could not.
pub(crate) fn open_chat(
    chat_id: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<Result<(), String>> {
    let running = ChatRuntime::global(cx).is_some_and(|runtime| {
        runtime
            .read(cx)
            .live(chat_id)
            .is_some_and(|chat| is_open(chat.state()))
    });
    if running {
        reveal(chat_id, workspace, window, cx);
        return Task::ready(Ok(()));
    }
    let projects = local_projects(workspace, cx);
    let resume = resume_chat(chat_id, projects, cx);
    let chat_id = chat_id.to_string();
    cx.spawn_in(window, async move |workspace, cx| {
        resume.await?;
        show_when_indexed(&workspace, &chat_id, cx).await;
        Ok(())
    })
}

/// Shows `chat_id` in the workspace's Chats panel and opens its dock, without moving focus.
/// Returns whether it is shown.
pub(crate) fn reveal(
    chat_id: &str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(panel) = workspace.panel::<ChatPanel>(cx) else {
        return false;
    };
    let shown = panel.update(cx, |panel, cx| panel.show_chat(chat_id, window, cx));
    if shown {
        workspace.open_panel::<ChatPanel>(window, cx);
    }
    shown
}

/// Waits for the index to list `chat_id` (a new record reaches it through the Work store's
/// watcher), then shows it.
async fn show_when_indexed(
    workspace: &WeakEntity<Workspace>,
    chat_id: &str,
    cx: &mut AsyncWindowContext,
) {
    let deadline = Instant::now() + INDEX_WAIT;
    loop {
        let indexed = cx
            .update(|_, cx| {
                ChatRuntime::global(cx).is_some_and(|runtime| {
                    runtime.read(cx).store().read(cx).find(chat_id).is_some()
                })
            })
            .unwrap_or(false);
        if indexed {
            break;
        }
        if Instant::now() >= deadline {
            log::warn!("chats: a started chat did not reach the index in time");
            return;
        }
        cx.background_executor().timer(POLL).await;
    }
    workspace
        .update_in(cx, |workspace, window, cx| {
            reveal(chat_id, workspace, window, cx)
        })
        .ok();
}

/// The project of `workspace`, then those of the workspaces of other windows: a launch uses the
/// first local one.
fn local_projects(workspace: &Workspace, cx: &mut Context<Workspace>) -> Vec<Entity<Project>> {
    let mut projects = vec![workspace.project().clone()];
    let this = cx.entity_id();
    for window in cx.windows() {
        let Some(multi) = window.downcast::<MultiWorkspace>() else {
            continue;
        };
        // The window being updated cannot be read; its workspace's project is already first.
        let Ok(multi) = multi.read(cx) else {
            continue;
        };
        for other in multi.workspaces() {
            if other.entity_id() == this {
                continue;
            }
            let project = other.read(cx).project().clone();
            if !projects.contains(&project) {
                projects.push(project);
            }
        }
    }
    projects
}

/// One chat of an open list.
struct Listed {
    id: String,
    label: String,
    live: bool,
}

/// The open (not archived) chats of the task `task_id`, or of every task when `None`, in list
/// order: most recently active first. With `archived`, the archived ones instead.
fn open_chats(
    task_id: Option<&str>,
    tree: Option<&WorkTree>,
    archived: bool,
    cx: &App,
) -> Vec<Listed> {
    let Some(runtime) = ChatRuntime::global(cx) else {
        return Vec::new();
    };
    let runtime = runtime.read(cx);
    let store = runtime.store().read(cx);
    let mut chats: Vec<_> = store
        .index()
        .values()
        .filter_map(|chat| Some((chat, chat.entry.record()?)))
        .filter(|(chat, record)| {
            record.archived == archived
                && task_id.is_none_or(|id| chat.task_id.as_deref() == Some(id))
        })
        .collect();
    chats.sort_by(|(_, a), (_, b)| {
        timestamp_utc(&b.last_active)
            .cmp(&timestamp_utc(&a.last_active))
            .then_with(|| a.id.cmp(&b.id))
    });
    chats
        .into_iter()
        .map(|(chat, record)| {
            let state = runtime.state(&record.id);
            let mut label = format!(
                "{}  [{}, {}, {state}]",
                record.name, record.agent, record.chat_type
            );
            if task_id.is_none()
                && let Some(goal) = chat
                    .task_id
                    .as_deref()
                    .and_then(|id| tree?.task_by_id(id))
                    .map(|task| task.goal())
            {
                label = format!("{goal} / {label}");
            }
            Listed {
                id: record.id.clone(),
                label,
                live: runtime.live(&record.id).is_some(),
            }
        })
        .collect()
}

/// `chat: open` (`any`: `chat: open any`). Without a focused task, `chat: open` lists every task's
/// chats too.
fn open(any: bool, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !chats_on(workspace, cx) {
        return;
    }
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let task_id = if any {
        None
    } else {
        bongos_work::focused(&tree, cx)
    };
    let chats = open_chats(task_id.as_deref(), Some(&tree), false, cx);
    if chats.is_empty() {
        let message = if task_id.is_some() {
            "This task has no open chats; start one with chat: new"
        } else {
            "There are no open chats; start one with chat: new"
        };
        show_toast(workspace, message.to_string(), cx);
        return;
    }
    let (ids, labels): (Vec<String>, Vec<String>) =
        chats.into_iter().map(|chat| (chat.id, chat.label)).unzip();
    pick(
        workspace,
        "Open which chat?",
        labels,
        0,
        Box::new(move |index, workspace, window, cx| {
            let opened = open_chat(&ids[index], workspace, window, cx);
            toast_on_error(opened, cx);
        }),
        window,
        cx,
    );
}

fn toast_on_error(task: Task<Result<(), String>>, cx: &mut Context<Workspace>) {
    cx.spawn(async move |workspace, cx| {
        if let Err(message) = task.await {
            workspace
                .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                .ok();
        }
    })
    .detach();
}

/// `chat: next` (`step` 1) and `chat: previous` (`step` -1): the next running chat of the
/// focused task (else of the shown chat's task) in list order, wrapping around.
fn cycle(step: isize, workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !chats_on(workspace, cx) {
        return;
    }
    let Some(panel) = enabled_panel(workspace, cx) else {
        return;
    };
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let shown = panel.read(cx).shown_chat().map(str::to_string);
    let task_id = bongos_work::focused(&tree, cx).or_else(|| {
        let runtime = ChatRuntime::global(cx)?;
        let store = runtime.read(cx).store().read(cx);
        store.find(shown.as_deref()?)?.task_id.clone()
    });
    let running: Vec<String> = open_chats(task_id.as_deref(), Some(&tree), false, cx)
        .into_iter()
        .filter(|chat| chat.live)
        .map(|chat| chat.id)
        .collect();
    if running.is_empty() {
        show_toast(
            workspace,
            "No chat of this task is running; open one with chat: open".to_string(),
            cx,
        );
        return;
    }
    let count = running.len() as isize;
    let current = shown
        .as_deref()
        .and_then(|shown| running.iter().position(|id| id == shown));
    let next = match current {
        Some(index) => (index as isize + step).rem_euclid(count),
        None if step > 0 => 0,
        None => count - 1,
    };
    reveal(&running[next as usize], workspace, window, cx);
}

/// `chat: close`: ends the shown chat's process; its record stays in the open list. Asks first
/// while the CLI is in the middle of a turn.
fn close(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !chats_on(workspace, cx) {
        return;
    }
    let Some(panel) = enabled_panel(workspace, cx) else {
        return;
    };
    let Some(chat_id) = panel.read(cx).shown_chat().map(str::to_string) else {
        show_toast(workspace, NO_CHAT.to_string(), cx);
        return;
    };
    let Some(runtime) = ChatRuntime::global(cx) else {
        return;
    };
    let name = chat_name(&chat_id, cx);
    let Some(mid_turn) = runtime
        .read(cx)
        .live(&chat_id)
        .map(|chat| chat.is_mid_turn())
    else {
        show_toast(workspace, format!("{name} is not running"), cx);
        return;
    };
    if !mid_turn {
        runtime.update(cx, |runtime, cx| runtime.close(&chat_id, cx));
        return;
    }
    pick(
        workspace,
        format!("{name} is in the middle of a turn. Close it?"),
        vec!["Keep it running".to_string(), format!("Close {name} now")],
        0,
        Box::new(move |index, _, _, cx| {
            if index == 1 {
                runtime.update(cx, |runtime, cx| runtime.close(&chat_id, cx));
            }
        }),
        window,
        cx,
    );
}

const RESTORE_TITLE: &str = "Restore which chat?";
const ARCHIVE_TITLE: &str = "Archived chats: pick one to restore";

/// The launch-files directory of `chat_id` under `runtime_root`; `None` when the id is not one
/// plain path component, so nothing outside `runtime_root` can ever be named.
fn runtime_dir_of(runtime_root: &Path, chat_id: &str) -> Option<PathBuf> {
    let mut components = Path::new(chat_id).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(name)), None) if name == chat_id => {
            Some(runtime_root.join(name))
        }
        _ => None,
    }
}

/// Removes `<runtime_root>/<chat_id>`, the launch files of a chat whose record is gone. Nothing
/// else is touched; a directory that is not there is fine.
pub(crate) async fn remove_runtime_dir(runtime_root: &Path, chat_id: &str, cx: &mut AsyncApp) {
    let Some(dir) = runtime_dir_of(runtime_root, chat_id) else {
        log::warn!("chats: not removing launch files for an unusable chat id");
        return;
    };
    let removed = cx
        .background_spawn(async move { std::fs::remove_dir_all(dir) })
        .await;
    if let Err(error) = removed
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("chats: could not remove the launch files of a deleted chat: {error}");
    }
}

/// The runtime and store, and the record file of `chat_id`; why not when there is none.
fn find_chat(
    chat_id: &str,
    cx: &App,
) -> Result<(Entity<ChatRuntime>, Entity<ChatStore>, PathBuf), String> {
    let runtime = ChatRuntime::global(cx).ok_or_else(|| SendError::Off.to_string())?;
    let store = runtime.read(cx).store().clone();
    let path = store
        .read(cx)
        .find(chat_id)
        .map(|chat| chat.entry.path.clone())
        .ok_or_else(|| format!("No chat has the id {chat_id}"))?;
    Ok((runtime, store, path))
}

/// Archives `chat_id`: ends its process first if it runs, then sets `archived` and `archived_at`.
/// Nothing is deleted and every other field of the record is kept; an archived chat stays as it
/// is.
pub(crate) fn archive_chat(chat_id: &str, cx: &mut App) -> Task<Result<(), String>> {
    let (runtime, store, path) = match find_chat(chat_id, cx) {
        Ok(found) => found,
        Err(message) => return Task::ready(Err(message)),
    };
    // Kyle asked for the record to be kept: the never-prompted cleanup stays out of it.
    runtime.update(cx, |runtime, cx| runtime.close_keeping_record(chat_id, cx));
    let write = store.update(cx, |store, cx| {
        store.update(
            path,
            |record| {
                if !record.archived {
                    record.archived = true;
                    record.archived_at = Some(bongos_work_model::now());
                }
                Ok(())
            },
            cx,
        )
    });
    cx.spawn(async move |_| write.await.map(|_| ()).map_err(|error| error.to_string()))
}

/// Restores `chat_id` from the archive: clears `archived` and `archived_at`, keeps the rest.
pub(crate) fn restore_chat(chat_id: &str, cx: &mut App) -> Task<Result<(), String>> {
    let (_, store, path) = match find_chat(chat_id, cx) {
        Ok(found) => found,
        Err(message) => return Task::ready(Err(message)),
    };
    let write = store.update(cx, |store, cx| {
        store.update(
            path,
            |record| {
                record.archived = false;
                record.archived_at = None;
                Ok(())
            },
            cx,
        )
    });
    cx.spawn(async move |_| write.await.map(|_| ()).map_err(|error| error.to_string()))
}

/// Names `chat_id` `name` as Kyle's own: no send and no terminal title replaces it afterward.
pub(crate) fn rename_chat(chat_id: &str, name: String, cx: &mut App) -> Task<Result<(), String>> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Task::ready(Err("A chat needs a name".to_string()));
    }
    let (_, store, path) = match find_chat(chat_id, cx) {
        Ok(found) => found,
        Err(message) => return Task::ready(Err(message)),
    };
    let write = store.update(cx, |store, cx| {
        store.update(
            path,
            move |record| {
                record.name = name;
                record.named_by = NamedBy::Kyle;
                Ok(())
            },
            cx,
        )
    });
    cx.spawn(async move |_| write.await.map(|_| ()).map_err(|error| error.to_string()))
}

/// Deletes `chat_id`: ends its process if it runs, then removes its record file and its launch
/// directory under the chats runtime directory, and nothing else. The CLI's own transcript is not
/// touched. The caller has asked Kyle.
pub(crate) fn delete_chat(chat_id: &str, cx: &mut App) -> Task<Result<(), String>> {
    let (runtime, store, path) = match find_chat(chat_id, cx) {
        Ok(found) => found,
        Err(message) => return Task::ready(Err(message)),
    };
    let runtime_root = store.read(cx).dirs().runtime.clone();
    runtime.update(cx, |runtime, cx| runtime.close_keeping_record(chat_id, cx));
    let delete = store.update(cx, |store, cx| store.delete_if(path, |_| true, cx));
    let chat_id = chat_id.to_string();
    cx.spawn(async move |cx| match delete.await {
        Ok(true) => {
            remove_runtime_dir(&runtime_root, &chat_id, cx).await;
            Ok(())
        }
        Ok(false) => Err("The chat's record could not be read, so it was left alone".to_string()),
        Err(error) => Err(error.to_string()),
    })
}

/// The chat the Chats panel shows; a toast says why not.
fn shown_chat(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> Option<String> {
    if !chats_on(workspace, cx) {
        return None;
    }
    let panel = enabled_panel(workspace, cx)?;
    let shown = panel.read(cx).shown_chat().map(str::to_string);
    if shown.is_none() {
        show_toast(workspace, NO_CHAT.to_string(), cx);
    }
    shown
}

/// Says `done` in a toast when `task` succeeds, else why it did not.
fn toast_on_result(task: Task<Result<(), String>>, done: String, cx: &mut Context<Workspace>) {
    cx.spawn(async move |workspace, cx| {
        let message = match task.await {
            Ok(()) => done,
            Err(message) => message,
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    })
    .detach();
}

/// `chat: archive`: the shown chat.
fn archive(workspace: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>) {
    let Some(chat_id) = shown_chat(workspace, cx) else {
        return;
    };
    let name = chat_name(&chat_id, cx);
    let archived = archive_chat(&chat_id, cx);
    toast_on_result(archived, format!("Archived {name}"), cx);
}

/// `chat: rename`: asks for the shown chat's name.
fn rename(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(chat_id) = shown_chat(workspace, cx) else {
        return;
    };
    let name = chat_name(&chat_id, cx);
    ask(
        workspace,
        format!("Rename {name} to"),
        &name,
        false,
        Box::new(move |new_name, _, _, cx| {
            let renamed = rename_chat(&chat_id, new_name.clone(), cx);
            toast_on_result(renamed, format!("Renamed to {new_name}"), cx);
        }),
        window,
        cx,
    );
}

/// `chat: delete`: asks, and deletes the shown chat's record only on a yes.
fn delete(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(chat_id) = shown_chat(workspace, cx) else {
        return;
    };
    let name = chat_name(&chat_id, cx);
    pick(
        workspace,
        format!("Delete {name}? The record is removed; the CLI's own transcript is not."),
        vec![format!("Keep {name}"), format!("Delete {name}")],
        0,
        Box::new(move |index, _, _, cx| {
            if index == 1 {
                let deleted = delete_chat(&chat_id, cx);
                toast_on_result(deleted, format!("Deleted {name}"), cx);
            }
        }),
        window,
        cx,
    );
}

/// `chat: restore` and `chat: show archive`: the archived chats of the focused task (else of every
/// task), one of which is moved back to the open list.
fn restore(
    title: &'static str,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !chats_on(workspace, cx) {
        return;
    }
    let Some(tree) = ready(workspace, cx) else {
        return;
    };
    let task_id = bongos_work::focused(&tree, cx);
    let chats = open_chats(task_id.as_deref(), Some(&tree), true, cx);
    if chats.is_empty() {
        let message = if task_id.is_some() {
            "This task has no archived chats"
        } else {
            "There are no archived chats"
        };
        show_toast(workspace, message.to_string(), cx);
        return;
    }
    let (ids, labels): (Vec<String>, Vec<String>) =
        chats.into_iter().map(|chat| (chat.id, chat.label)).unzip();
    pick(
        workspace,
        title,
        labels,
        0,
        Box::new(move |index, _, _, cx| {
            let name = chat_name(&ids[index], cx);
            let restored = restore_chat(&ids[index], cx);
            toast_on_result(restored, format!("Restored {name}"), cx);
        }),
        window,
        cx,
    );
}

/// The name of `chat_id` on record, else its id.
fn chat_name(chat_id: &str, cx: &App) -> String {
    ChatRuntime::global(cx)
        .and_then(|runtime| {
            let store = runtime.read(cx).store().read(cx);
            Some(store.find(chat_id)?.entry.record()?.name.clone())
        })
        .unwrap_or_else(|| chat_id.to_string())
}

/// A send found its chat not running: asks "Resume <name> and send?" and, on yes, resumes the
/// chat and sends `text` once it runs. The text stays in its buffer whatever happens.
pub(crate) fn offer_resume_and_send(
    chat_id: String,
    name: String,
    text: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    pick(
        workspace,
        format!("Resume {name} and send?"),
        vec![
            format!("Resume {name} and send"),
            "Cancel: keep the text".to_string(),
        ],
        0,
        Box::new(move |index, workspace, window, cx| {
            if index != 0 {
                return;
            }
            let opened = open_chat(&chat_id, workspace, window, cx);
            cx.spawn(async move |workspace, cx| {
                let result = match opened.await {
                    Ok(()) => {
                        let sent =
                            cx.update(|cx| send(&chat_id, text, SendOptions { submit: true }, cx));
                        sent.await.map(|_| ()).map_err(|error| error.to_string())
                    }
                    Err(message) => Err(message),
                };
                if let Err(message) = result {
                    workspace
                        .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                        .ok();
                }
            })
            .detach();
        }),
        window,
        cx,
    );
}
