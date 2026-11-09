//! [`ChatRuntime`]: the running chats (design D11 and D12).
//!
//! The runtime is global, not per workspace: it holds the strong handle of each chat's terminal,
//! and a project keeps only weak ones, so closing the workspace that started a chat does not end
//! it. A launch writes the chat's runtime directory, writes the record (the session id a Claude
//! launch chooses, the digest of the given artifacts, `last_active`), and runs the CLI through
//! `Project::create_terminal_task` on a local project, which resolves the shell environment of the
//! chat's directory. A remote project would start the CLI on the remote host, where the chat's
//! files do not exist, so it is never used.
//!
//! Live state comes from the terminal: a bell means the CLI waits for input, output means it runs,
//! no output for `chats.idle_after_secs` means idle, and the end of the process means exited.
//! Keyboard input is a polled flag of the terminal, read on every wakeup and at close; once seen,
//! the chat counts as prompted. Every change rewrites `last_active`, at most once per
//! [`ACTIVE_DEBOUNCE`] per chat.
//!
//! Agents that report their session id only in their own files (Codex) are launched one at a time
//! per directory: the next launch in a directory waits until the previous one's id was captured
//! and claimed, at most [`CAPTURE_WINDOW`].
//!
//! Nothing starts while `chats.enabled` is false.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bongos_chat_model::{Agent, CaptureQuery, Fallback, NamedBy, adapter_for, from_title};
use bongos_work_model::SharedArtifact;
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{self, Shared};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, Subscription, Task};
use project::Project;
use settings::{Settings as _, SettingsStore};
use terminal::{Event as TerminalEvent, Terminal};

use crate::cleanup::{Cleanup, cleanup_verdict};
use crate::launch::{self, LaunchError, LaunchRequest, Prepared};
use crate::{ChatHealth, ChatStore, ChatsSettings, find_program, fix_not_found, fix_not_probed};

/// The shortest time between two writes of one chat's `last_active`.
pub const ACTIVE_DEBOUNCE: Duration = Duration::from_secs(10);
/// How long the runtime looks for a session id the CLI does not report, polling every
/// [`CAPTURE_INTERVAL`]; it looks once more when the CLI exits.
pub const CAPTURE_WINDOW: Duration = Duration::from_secs(60);
pub const CAPTURE_INTERVAL: Duration = Duration::from_secs(1);
/// Output that arrives this soon after a bell belongs to the bell (the terminal reports the output
/// of one read after its bell), so it does not end the waiting state.
const BELL_GRACE: Duration = Duration::from_millis(500);
/// A running chat with output this recent is mid-turn: `chat: close` asks first.
pub const MID_TURN: Duration = Duration::from_secs(2);

/// What a chat is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatState {
    /// The CLI is producing output.
    Running,
    /// The CLI rang the bell: it wants input.
    Waiting,
    /// No output for `chats.idle_after_secs`.
    Idle,
    /// The process ended; `code` is its exit code, when it reported one.
    Exited { code: Option<i32> },
    /// No live terminal.
    Closed,
}

impl fmt::Display for ChatState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatState::Running => write!(f, "running"),
            ChatState::Waiting => write!(f, "waiting"),
            ChatState::Idle => write!(f, "idle"),
            ChatState::Exited { code: Some(code) } => write!(f, "exited (code {code})"),
            ChatState::Exited { code: None } => write!(f, "exited"),
            ChatState::Closed => write!(f, "closed"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatRuntimeEvent {
    /// The live state, title or session id of the chat with this id changed, or it started or
    /// closed.
    Changed(String),
}

/// Counters of what the runtime did, for tests and logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeStats {
    pub launches: usize,
    /// `last_active` writes asked of the store.
    pub active_writes: usize,
    /// Session ids captured from the CLI's files.
    pub captures: usize,
    /// Records the never-prompted cleanup deleted.
    pub cleanups: usize,
}

/// One running (or exited, still shown) chat.
pub struct LiveChat {
    terminal: Entity<Terminal>,
    agent: Agent,
    state: ChatState,
    title: Option<String>,
    started: SystemTime,
    given: Vec<SharedArtifact>,
    fallbacks: Vec<Fallback>,
    missing_skills: Vec<String>,
    record_path: PathBuf,
    cwd: PathBuf,
    command: String,
    session_id: Option<String>,
    prompted: bool,
    /// Something was pasted through [`crate::send`]; the send names the chat, not a title.
    sent: bool,
    /// The chat has a name other than its default one, or a title already tried to name it.
    auto_named: bool,
    last_output: Instant,
    bell_at: Option<Instant>,
    last_active_write: Option<Instant>,
    pending_active_write: Option<Task<()>>,
    /// The id is still to be read from the CLI's files when it exits.
    capture_on_exit: bool,
    /// The never-prompted cleanup already looked at this run of the chat.
    cleanup_checked: bool,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl LiveChat {
    /// Whether the bell rang less than [`BELL_GRACE`] ago.
    fn just_rang(&self) -> bool {
        self.bell_at.is_some_and(|bell| bell.elapsed() < BELL_GRACE)
    }

    pub fn terminal(&self) -> &Entity<Terminal> {
        &self.terminal
    }

    pub fn state(&self) -> ChatState {
        self.state
    }

    /// The title the CLI set with an OSC sequence.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn started(&self) -> SystemTime {
        self.started
    }

    /// The shared artifacts the chat was given at launch. Never written anywhere.
    pub fn given(&self) -> &[SharedArtifact] {
        &self.given
    }

    /// The fallbacks the launch used, for the row.
    pub fn fallbacks(&self) -> &[Fallback] {
        &self.fallbacks
    }

    /// The skills of the session type that were not found at launch, for the row.
    pub fn missing_skills(&self) -> &[String] {
        &self.missing_skills
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Whether anything was typed or sent into the chat.
    pub fn prompted(&self) -> bool {
        self.prompted
    }

    pub fn agent(&self) -> Agent {
        self.agent
    }

    /// Whether the CLI is working on a turn: running, with output in the last [`MID_TURN`].
    pub fn is_mid_turn(&self) -> bool {
        self.state == ChatState::Running && self.last_output.elapsed() < MID_TURN
    }

    pub(crate) fn record_path(&self) -> &PathBuf {
        &self.record_path
    }
}

/// A launch's place in the per-directory queue of launches that capture their session id.
struct CaptureSlot {
    generation: usize,
    done: Shared<oneshot::Receiver<()>>,
}

/// Removes a chat id from the launching set when dropped: at the end of its launch task, or when
/// the task is dropped before it finished, so the chat can be launched again.
struct LaunchGuard {
    launching: Arc<Mutex<HashSet<String>>>,
    chat_id: String,
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        self.launching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.chat_id);
    }
}

/// The global runtime of chats. See the module documentation.
pub struct ChatRuntime {
    store: Entity<ChatStore>,
    home: PathBuf,
    live: HashMap<String, LiveChat>,
    /// Shared with each launch task's [`LaunchGuard`], which removes the id even when the task is
    /// dropped without being awaited.
    launching: Arc<Mutex<HashSet<String>>>,
    capture_slots: HashMap<PathBuf, CaptureSlot>,
    next_generation: usize,
    active_debounce: Duration,
    bracketed_wait: Duration,
    /// Per chat, the last send in line and its number: a send waits until the one before it has
    /// written its Enter, so two pastes never interleave.
    send_queues: HashMap<String, (usize, Shared<oneshot::Receiver<()>>)>,
    next_send: usize,
    /// The agent of the last chat `chat: new` started, offered first next time.
    last_agent: Option<Agent>,
    stats: RuntimeStats,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ChatRuntimeEvent> for ChatRuntime {}

struct GlobalChatRuntime(Entity<ChatRuntime>);

impl Global for GlobalChatRuntime {}

impl ChatRuntime {
    /// `home` is where `~/.bongos` and the agents' own session files are.
    pub fn new(store: Entity<ChatStore>, home: PathBuf, cx: &mut Context<Self>) -> Self {
        Self {
            store,
            home,
            live: HashMap::new(),
            launching: Arc::default(),
            capture_slots: HashMap::new(),
            next_generation: 0,
            active_debounce: ACTIVE_DEBOUNCE,
            bracketed_wait: crate::send::BRACKETED_WAIT,
            send_queues: HashMap::new(),
            next_send: 0,
            last_agent: None,
            stats: RuntimeStats::default(),
            _subscriptions: vec![
                cx.observe_global::<SettingsStore>(|this, cx| this.end_when_disabled(cx)),
            ],
        }
    }

    /// Ends every running chat once `chats.enabled` is false (design D24). The store no longer
    /// writes by then, so nothing is written: the records stay as they were, and a later enabled
    /// period lists them again.
    fn end_when_disabled(&mut self, cx: &mut Context<Self>) {
        if ChatsSettings::get_global(cx).enabled || self.live.is_empty() {
            return;
        }
        let ids: Vec<String> = self.live.keys().cloned().collect();
        for id in ids {
            self.kill(&id, cx);
            self.live.remove(&id);
            cx.emit(ChatRuntimeEvent::Changed(id));
        }
        cx.notify();
    }

    /// The global runtime, once [`crate::init`] has run.
    pub fn global(cx: &App) -> Option<Entity<ChatRuntime>> {
        cx.try_global::<GlobalChatRuntime>()
            .map(|global| global.0.clone())
    }

    pub(crate) fn set_global(runtime: Entity<ChatRuntime>, cx: &mut App) {
        cx.set_global(GlobalChatRuntime(runtime));
    }

    /// Replaces [`ACTIVE_DEBOUNCE`] (tests use a short one).
    pub fn set_active_debounce(&mut self, debounce: Duration) {
        self.active_debounce = debounce;
    }

    /// Replaces [`crate::send::BRACKETED_WAIT`] (tests use a short one).
    pub fn set_bracketed_wait(&mut self, wait: Duration) {
        self.bracketed_wait = wait;
    }

    pub(crate) fn bracketed_wait(&self) -> Duration {
        self.bracketed_wait
    }

    /// Puts a send to `chat_id` in line. Returns what it waits for (the send before it), the
    /// sender to drop once its Enter is written, and its number for [`Self::leave_send_queue`].
    pub(crate) fn join_send_queue(
        &mut self,
        chat_id: &str,
    ) -> (
        Option<Shared<oneshot::Receiver<()>>>,
        oneshot::Sender<()>,
        usize,
    ) {
        let (done, done_rx) = oneshot::channel();
        self.next_send += 1;
        let number = self.next_send;
        let previous = self
            .send_queues
            .insert(chat_id.to_string(), (number, done_rx.shared()));
        (previous.map(|(_, previous)| previous), done, number)
    }

    /// Forgets the queue of `chat_id` when send `number` is still its last.
    pub(crate) fn leave_send_queue(&mut self, chat_id: &str, number: usize) {
        if self
            .send_queues
            .get(chat_id)
            .is_some_and(|(last, _)| *last == number)
        {
            self.send_queues.remove(chat_id);
        }
    }

    pub(crate) fn store(&self) -> &Entity<ChatStore> {
        &self.store
    }

    pub fn stats(&self) -> RuntimeStats {
        self.stats
    }

    /// Where `~/.bongos` and the agents' own files are.
    pub fn home(&self) -> &PathBuf {
        &self.home
    }

    /// The agent of the last chat `chat: new` started in this session.
    pub fn last_agent(&self) -> Option<Agent> {
        self.last_agent
    }

    /// Remembers `agent` as the last one `chat: new` started, in memory and in the key value
    /// store, so the next run offers it first too.
    pub(crate) fn set_last_agent(&mut self, agent: Agent, cx: &mut Context<Self>) {
        self.last_agent = Some(agent);
        crate::view_state::write(
            crate::view_state::LAST_AGENT_KEY,
            Some(agent.as_str().to_string()),
            cx,
        );
    }

    /// The live chat with `chat_id`, running or exited.
    pub fn live(&self, chat_id: &str) -> Option<&LiveChat> {
        self.live.get(chat_id)
    }

    /// The ids of every live chat.
    pub fn live_ids(&self) -> impl Iterator<Item = &str> {
        self.live.keys().map(String::as_str)
    }

    /// The state of `chat_id`: [`ChatState::Closed`] when it has no live terminal.
    pub fn state(&self, chat_id: &str) -> ChatState {
        self.live
            .get(chat_id)
            .map_or(ChatState::Closed, |chat| chat.state)
    }

    /// Starts the chat of `request` in a terminal of the first local project of `projects` (the
    /// active workspace's project first). Refused when chats are off, when no project is local
    /// ("Chats run locally; open a local project first"), when the chat is live, or when the CLI
    /// cannot be found or was not probed yet.
    pub fn launch(
        &mut self,
        request: LaunchRequest,
        projects: &[Entity<Project>],
        cx: &mut Context<Self>,
    ) -> Task<Result<(), LaunchError>> {
        match self.begin_launch(request, projects, cx) {
            Ok(task) => task,
            Err(error) => Task::ready(Err(error)),
        }
    }

    fn begin_launch(
        &mut self,
        request: LaunchRequest,
        projects: &[Entity<Project>],
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<(), LaunchError>>, LaunchError> {
        let settings = ChatsSettings::get_global(cx).clone();
        if !settings.enabled {
            return Err(LaunchError::Off);
        }
        let chat_id = request.record.id.clone();
        if self.live.contains_key(&chat_id) || self.is_launching(&chat_id) {
            return Err(LaunchError::AlreadyRunning);
        }
        let project = projects
            .iter()
            .find(|project| project.read(cx).is_local())
            .cloned()
            .ok_or(LaunchError::NoLocalProject)?;
        let agent = request
            .record
            .known_agent()
            .ok_or_else(|| LaunchError::UnknownAgent(request.record.agent.clone()))?;
        let command = match agent {
            Agent::Claude => settings.claude,
            Agent::Codex => settings.codex,
        };
        let health = ChatHealth::global(cx);
        let base_env = health
            .as_ref()
            .map(|health| health.read(cx).env().clone())
            .unwrap_or_default();
        let caps = request
            .caps
            .clone()
            .or_else(|| {
                health
                    .as_ref()
                    .and_then(|health| health.read(cx).health(agent).caps.clone())
            })
            .ok_or_else(|| LaunchError::NotProbed(fix_not_probed(agent)))?;
        let program = find_program(&command.command, &base_env)
            .ok_or_else(|| LaunchError::NotFound(fix_not_found(agent, &command.command)))?;
        let runtime_root = self.store.read(cx).dirs().runtime.clone();
        let prepared = launch::prepare(&request, &runtime_root, &self.home, &command, &caps)?;

        // A launch that must capture its session id waits for the previous such launch in the
        // same directory to capture (and claim) its own.
        let capture = prepared.spec.capture_session_id.then(|| {
            let (done, done_rx) = oneshot::channel::<()>();
            self.next_generation += 1;
            let generation = self.next_generation;
            let previous = self.capture_slots.insert(
                prepared.spec.cwd.clone(),
                CaptureSlot {
                    generation,
                    done: done_rx.shared(),
                },
            );
            (generation, done, previous.map(|slot| slot.done))
        });
        let (capture_generation, capture_done, previous_capture) = match capture {
            Some((generation, done, previous)) => (Some(generation), Some(done), previous),
            None => (None, None, None),
        };

        self.launching_set().insert(chat_id.clone());
        let guard = LaunchGuard {
            launching: self.launching.clone(),
            chat_id: chat_id,
        };
        let label = format!("{}: {}", agent.as_str(), prepared.record.name);
        let store = self.store.clone();
        let record_path = request.record_path;
        let task_id = request.task_id;
        Ok(cx.spawn(async move |this, cx| {
            let result = async {
                if let Some(previous) = previous_capture {
                    let timeout = cx.background_executor().timer(CAPTURE_WINDOW);
                    future::select(previous, timeout).await;
                }
                let files = prepared.files.clone();
                let dir = prepared.dir.clone();
                cx.background_spawn(async move {
                    launch::write_runtime_dir(&runtime_root, &dir, &files)
                })
                .await
                .map_err(|error| LaunchError::Files(error.to_string()))?;

                let session_id = prepared.record.session_id.clone();
                let digest = prepared.digest.clone();
                let write = store.update(cx, |store, cx| {
                    store.update(
                        record_path.clone(),
                        move |record| {
                            if session_id.is_some() {
                                record.session_id = session_id;
                            }
                            let now = bongos_work_model::now();
                            record.context_digest = Some(digest);
                            record.context_given = Some(now);
                            record.last_active = now;
                            Ok(())
                        },
                        cx,
                    )
                });
                write.await.map_err(LaunchError::Record)?;

                let spawn = launch::spawn_in_terminal(
                    &prepared.record.id,
                    label,
                    &program,
                    &prepared.spec,
                    &base_env,
                );
                let started = SystemTime::now();
                let terminal = project
                    .update(cx, |project, cx| project.create_terminal_task(spawn, cx))
                    .await
                    .map_err(|error| LaunchError::Spawn(format!("{error:#}")))?;
                this.update(cx, |this, cx| {
                    this.register(
                        prepared,
                        record_path,
                        command.command,
                        terminal,
                        started,
                        capture_done,
                        cx,
                    );
                    if let Some(task_id) = &task_id {
                        this.start_todo(task_id, cx);
                    }
                })
                .map_err(|error| LaunchError::Spawn(format!("{error:#}")))?;
                Ok(())
            }
            .await;
            drop(guard);
            this.update(cx, |this, _| {
                if let Some(generation) = capture_generation
                    && result.is_err()
                {
                    this.release_capture_slot(generation);
                }
            })
            .ok();
            result
        }))
    }

    fn launching_set(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.launching.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_launching(&self, chat_id: &str) -> bool {
        self.launching_set().contains(chat_id)
    }

    /// A chat started on the leaf `task_id`: its current todo starts if it had not (design D23).
    fn start_todo(&mut self, task_id: &str, cx: &mut Context<Self>) {
        let work = self.store.read(cx).work().clone();
        let start = crate::live_modes::start_current_todo(&work, task_id, cx);
        cx.spawn(async move |_, _| {
            if let Err(error) = start.await {
                log::warn!("chats: could not start the task's current todo: {error}");
            }
        })
        .detach();
    }

    fn release_capture_slot(&mut self, generation: usize) {
        self.capture_slots
            .retain(|_, slot| slot.generation != generation);
    }

    #[allow(clippy::too_many_arguments)]
    fn register(
        &mut self,
        prepared: Prepared,
        record_path: PathBuf,
        command: String,
        terminal: Entity<Terminal>,
        started: SystemTime,
        capture_done: Option<oneshot::Sender<()>>,
        cx: &mut Context<Self>,
    ) {
        let chat_id = prepared.record.id.clone();
        let subscription = cx.subscribe(&terminal, {
            let chat_id = chat_id.clone();
            move |this, terminal, event, cx| this.on_terminal_event(&chat_id, &terminal, event, cx)
        });
        let exit = terminal.read(cx).wait_for_completed_task(cx);
        let mut tasks = vec![
            cx.spawn({
                let chat_id = chat_id.clone();
                async move |this, cx| {
                    let status = exit.await;
                    this.update(cx, |this, cx| {
                        this.on_exit(&chat_id, status.and_then(|status| status.code()), cx)
                    })
                    .ok();
                }
            }),
            cx.spawn({
                let chat_id = chat_id.clone();
                async move |this, cx| {
                    loop {
                        let Ok(Some(delay)) =
                            this.update(cx, |this, cx| this.check_idle(&chat_id, cx))
                        else {
                            break;
                        };
                        cx.background_executor().timer(delay).await;
                    }
                }
            }),
        ];
        if let Some(done) = capture_done {
            let chat_id = chat_id.clone();
            tasks.push(cx.spawn(async move |this, cx| {
                let tries = (CAPTURE_WINDOW.as_millis() / CAPTURE_INTERVAL.as_millis()).max(1);
                let mut captured = false;
                for _ in 0..tries {
                    match capture_now(&this, &chat_id, cx).await {
                        Some(true) => {
                            captured = true;
                            break;
                        }
                        Some(false) => {}
                        None => return,
                    }
                    cx.background_executor().timer(CAPTURE_INTERVAL).await;
                }
                if !captured {
                    this.update(cx, |this, _| {
                        if let Some(chat) = this.live.get_mut(&chat_id) {
                            chat.capture_on_exit = true;
                        }
                    })
                    .ok();
                }
                // Dropping the sender lets the next launch in this directory go.
                drop(done);
            }));
        }

        // The terminal ran before this subscription: take the title it may already have.
        let title = terminal.read(cx).breadcrumb_text.trim().to_string();
        let chat = LiveChat {
            terminal,
            agent: prepared.agent,
            state: ChatState::Running,
            title: (!title.is_empty()).then_some(title),
            started,
            given: prepared.given,
            fallbacks: prepared.spec.fallbacks,
            missing_skills: prepared.missing_skills,
            record_path,
            cwd: prepared.spec.cwd,
            command,
            session_id: prepared.record.session_id,
            prompted: prepared.record.prompted,
            sent: false,
            auto_named: prepared.record.named_by != NamedBy::None,
            last_output: Instant::now(),
            bell_at: None,
            last_active_write: Some(Instant::now()),
            pending_active_write: None,
            capture_on_exit: false,
            cleanup_checked: false,
            _tasks: tasks,
            _subscriptions: vec![subscription],
        };
        self.live.insert(chat_id.clone(), chat);
        self.stats.launches += 1;
        cx.emit(ChatRuntimeEvent::Changed(chat_id));
        cx.notify();
    }

    fn on_terminal_event(
        &mut self,
        chat_id: &str,
        terminal: &Entity<Terminal>,
        event: &TerminalEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalEvent::Bell => {
                if let Some(chat) = self.live.get_mut(chat_id) {
                    chat.bell_at = Some(Instant::now());
                }
                self.set_state(chat_id, ChatState::Waiting, cx);
            }
            TerminalEvent::BreadcrumbsChanged => {
                let title = terminal.read(cx).breadcrumb_text.trim().to_string();
                let title = (!title.is_empty()).then_some(title);
                let Some(chat) = self.live.get_mut(chat_id) else {
                    return;
                };
                if chat.title != title {
                    chat.title = title;
                    self.name_from_title(chat_id, cx);
                    self.touch_active(chat_id, cx);
                    cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
                    cx.notify();
                }
            }
            TerminalEvent::Wakeup => {
                self.poll_input(chat_id, cx);
                let Some(chat) = self.live.get_mut(chat_id) else {
                    return;
                };
                chat.last_output = Instant::now();
                if matches!(chat.state, ChatState::Waiting | ChatState::Idle) && !chat.just_rang() {
                    self.set_state(chat_id, ChatState::Running, cx);
                }
            }
            _ => {}
        }
    }

    /// Names a chat that is still called by its default name and was prompted only by typing from
    /// its title (design D16), once: the first title that is not the program's name or a spinner.
    fn name_from_title(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        let typed = chat.prompted || chat.terminal.read(cx).keyboard_input_sent();
        if chat.auto_named || chat.sent || !typed {
            return;
        }
        let Some(title) = chat.title.as_deref() else {
            return;
        };
        let program = std::path::Path::new(&chat.command)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| chat.command.clone());
        let name = from_title(title, &program)
            .filter(|_| from_title(title, crate::health::agent_title(chat.agent)).is_some())
            .filter(|_| from_title(title, chat.agent.as_str()).is_some());
        let Some(name) = name else {
            return;
        };
        chat.auto_named = true;
        let path = chat.record_path.clone();
        let write = self.store.update(cx, |store, cx| {
            store.update(
                path,
                move |record| {
                    crate::send::apply_name(record, name);
                    Ok(())
                },
                cx,
            )
        });
        cx.background_spawn(async move {
            if let Err(error) = write.await {
                log::warn!("chats: could not name the chat from its title: {error}");
            }
        })
        .detach();
    }

    /// The text of a send was pasted into the chat: it counts as prompted, and the send, not a
    /// title, names it.
    pub(crate) fn note_pasted(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        chat.sent = true;
        chat.auto_named = true;
        if !chat.prompted {
            chat.prompted = true;
            cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
            cx.notify();
        }
    }

    /// Reads the terminal's keyboard input flag; the first time it is set, the chat is prompted.
    fn poll_input(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        if chat.prompted || !chat.terminal.read(cx).keyboard_input_sent() {
            return;
        }
        chat.prompted = true;
        // Input means the CLI works on it, unless a bell just came: then the input is older
        // than the bell's question.
        if matches!(chat.state, ChatState::Waiting | ChatState::Idle) && !chat.just_rang() {
            chat.state = ChatState::Running;
        }
        self.touch_active(chat_id, cx);
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
    }

    fn set_state(&mut self, chat_id: &str, state: ChatState, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        if chat.state == state || matches!(chat.state, ChatState::Exited { .. }) {
            return;
        }
        chat.state = state;
        self.touch_active(chat_id, cx);
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
    }

    /// Marks the chat idle when it ran without output for `chats.idle_after_secs`. Returns when to
    /// look again, or `None` once the chat is gone or exited.
    fn check_idle(&mut self, chat_id: &str, cx: &mut Context<Self>) -> Option<Duration> {
        let idle_after = Duration::from_secs(ChatsSettings::get_global(cx).idle_after_secs.max(1));
        let chat = self.live.get(chat_id)?;
        if matches!(chat.state, ChatState::Exited { .. }) {
            return None;
        }
        let quiet = chat.last_output.elapsed();
        if quiet < idle_after {
            return Some(idle_after - quiet);
        }
        if chat.state == ChatState::Running {
            self.set_state(chat_id, ChatState::Idle, cx);
        }
        Some(idle_after)
    }

    fn on_exit(&mut self, chat_id: &str, code: Option<i32>, cx: &mut Context<Self>) {
        self.poll_input(chat_id, cx);
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        chat.state = ChatState::Exited { code };
        let capture = std::mem::take(&mut chat.capture_on_exit);
        self.write_active_now(chat_id, cx);
        self.clean_up(chat_id, cx);
        if capture {
            let chat_id = chat_id.to_string();
            cx.spawn(async move |this, cx| {
                capture_now(&this, &chat_id, cx).await;
            })
            .detach();
        }
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
    }

    /// Ends the chat's process (its foreground process group and the process itself). The chat
    /// stays live, exited, so its last output can still be shown. Returns whether it was live.
    pub fn kill(&mut self, chat_id: &str, cx: &mut Context<Self>) -> bool {
        let Some(chat) = self.live.get(chat_id) else {
            return false;
        };
        chat.terminal
            .update(cx, |terminal, _| terminal.kill_active_task());
        true
    }

    /// Ends the chat's process and drops its terminal: the chat is closed. `last_active` is written
    /// now, and the record stays unless the never-prompted cleanup is sure nothing was said in it
    /// (see [`crate::cleanup`]). Returns whether it was live.
    pub fn close(&mut self, chat_id: &str, cx: &mut Context<Self>) -> bool {
        if !self.live.contains_key(chat_id) {
            return false;
        }
        self.poll_input(chat_id, cx);
        self.kill(chat_id, cx);
        self.write_active_now(chat_id, cx);
        self.clean_up(chat_id, cx);
        // Dropping the terminal also ends the process if the kill did not.
        self.live.remove(chat_id);
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
        true
    }

    /// [`ChatRuntime::close`] for a chat that is about to be archived or deleted by Kyle: the
    /// never-prompted cleanup does not run, so the record is left to the caller.
    pub fn close_keeping_record(&mut self, chat_id: &str, cx: &mut Context<Self>) -> bool {
        if let Some(chat) = self.live.get_mut(chat_id) {
            chat.cleanup_checked = true;
        }
        self.close(chat_id, cx)
    }

    /// Asks for a `last_active` write: now when the last one is older than the debounce, else
    /// once the debounce has passed.
    fn touch_active(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let debounce = self.active_debounce;
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        if chat.pending_active_write.is_some() {
            return;
        }
        let wait = chat
            .last_active_write
            .map(|last| debounce.saturating_sub(last.elapsed()))
            .unwrap_or_default();
        if wait.is_zero() {
            self.write_active_now(chat_id, cx);
            return;
        }
        let id = chat_id.to_string();
        chat.pending_active_write = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |this, cx| this.write_active_now(&id, cx))
                .ok();
        }));
    }

    /// Writes `last_active` (and `prompted`, once set) now, replacing a pending write.
    fn write_active_now(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        chat.pending_active_write = None;
        chat.last_active_write = Some(Instant::now());
        let path = chat.record_path.clone();
        let prompted = chat.prompted;
        self.stats.active_writes += 1;
        let write = self.store.update(cx, |store, cx| {
            store.update(
                path,
                move |record| {
                    record.last_active = bongos_work_model::now();
                    if prompted {
                        record.prompted = true;
                    }
                    Ok(())
                },
                cx,
            )
        });
        cx.background_spawn(async move {
            if let Err(error) = write.await {
                log::warn!("chats: could not write last_active: {error}");
            }
        })
        .detach();
    }

    /// The never-prompted cleanup of `chat_id`, which ended or is being closed (design D17), once
    /// per run of the chat. It runs on the store's writer after the writes queued before it, so the
    /// record it reads already has the `prompted` the runtime saw. A deleted record's runtime
    /// directory goes too.
    fn clean_up(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let capture = self.capture_query(chat_id, cx);
        let runtime_dir = self.store.read(cx).dirs().runtime.join(chat_id);
        let home = self.home.clone();
        let env = ChatHealth::global(cx)
            .map(|health| health.read(cx).env().clone())
            .unwrap_or_default();
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        if std::mem::replace(&mut chat.cleanup_checked, true) {
            return;
        }
        let saw_input = chat.prompted || chat.terminal.read(cx).keyboard_input_sent();
        if saw_input {
            return;
        }
        let live_session = chat.session_id.clone();
        let path = chat.record_path.clone();
        let delete = self.store.update(cx, |store, cx| {
            store.delete_if(
                path,
                move |record| {
                    let verdict = cleanup_verdict(
                        record,
                        saw_input,
                        live_session.as_deref(),
                        &home,
                        &env,
                        || capture.as_ref().and_then(CaptureInput::run),
                    );
                    log::info!("chats: cleanup of an unprompted chat: {verdict:?}");
                    verdict == Cleanup::Delete
                },
                cx,
            )
        });
        let chat_id = chat_id.to_string();
        cx.spawn(async move |this, cx| match delete.await {
            Ok(true) => {
                let removed = cx
                    .background_spawn(async move { std::fs::remove_dir_all(runtime_dir) })
                    .await;
                if let Err(error) = removed
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    log::warn!("chats: could not remove a deleted chat's launch files: {error}");
                }
                this.update(cx, |this, cx| {
                    this.stats.cleanups += 1;
                    cx.emit(ChatRuntimeEvent::Changed(chat_id));
                    cx.notify();
                })
                .ok();
            }
            Ok(false) => {}
            Err(error) => log::warn!("chats: the cleanup could not run: {error}"),
        })
        .detach();
    }

    /// What a capture of `chat_id`'s session id reads: `None` when the chat is gone or its id is
    /// known.
    fn capture_query(&self, chat_id: &str, cx: &App) -> Option<CaptureInput> {
        let chat = self.live.get(chat_id)?;
        if chat.session_id.is_some() {
            return None;
        }
        let mut claimed: BTreeSet<String> = self
            .store
            .read(cx)
            .index()
            .values()
            .filter(|indexed| indexed.entry.id() != chat_id)
            .filter_map(|indexed| indexed.entry.record()?.session_id.clone())
            .collect();
        claimed.extend(
            self.live
                .iter()
                .filter(|(id, _)| id.as_str() != chat_id)
                .filter_map(|(_, live)| live.session_id.clone()),
        );
        Some(CaptureInput {
            agent: chat.agent,
            command: chat.command.clone(),
            chat_id: chat_id.to_string(),
            cwd: chat.cwd.clone(),
            since: chat.started,
            claimed,
            home: self.home.clone(),
        })
    }

    fn set_session_id(&mut self, chat_id: &str, session_id: String, cx: &mut Context<Self>) {
        let Some(chat) = self.live.get_mut(chat_id) else {
            return;
        };
        if chat.session_id.is_some() {
            return;
        }
        chat.session_id = Some(session_id.clone());
        self.stats.captures += 1;
        let path = chat.record_path.clone();
        let write = self.store.update(cx, |store, cx| {
            store.update(
                path,
                move |record| {
                    record.session_id = Some(session_id);
                    Ok(())
                },
                cx,
            )
        });
        cx.background_spawn(async move {
            if let Err(error) = write.await {
                log::warn!("chats: could not record the captured session id: {error}");
            }
        })
        .detach();
        cx.emit(ChatRuntimeEvent::Changed(chat_id.to_string()));
        cx.notify();
    }
}

pub(crate) struct CaptureInput {
    agent: Agent,
    command: String,
    chat_id: String,
    cwd: PathBuf,
    since: SystemTime,
    claimed: BTreeSet<String>,
    home: PathBuf,
}

impl CaptureInput {
    pub(crate) fn run(&self) -> Option<String> {
        adapter_for(self.agent, &self.command).capture_session_id(&CaptureQuery {
            chat_id: &self.chat_id,
            cwd: &self.cwd,
            since: self.since,
            claimed: &self.claimed,
            home: &self.home,
        })
    }
}

/// One look for `chat_id`'s session id in the CLI's files. `Some(true)` when it was found (or is
/// known), `Some(false)` when not yet, `None` when the chat is gone.
async fn capture_now(
    this: &gpui::WeakEntity<ChatRuntime>,
    chat_id: &str,
    cx: &mut gpui::AsyncApp,
) -> Option<bool> {
    let input = this
        .update(cx, |this, cx| {
            let known = this.live.get(chat_id)?.session_id.is_some();
            Some(if known {
                None
            } else {
                this.capture_query(chat_id, cx)
            })
        })
        .ok()??;
    let Some(input) = input else {
        return Some(true);
    };
    let found = cx.background_spawn(async move { input.run() }).await;
    let Some(session_id) = found else {
        return Some(false);
    };
    this.update(cx, |this, cx| this.set_session_id(chat_id, session_id, cx))
        .ok()?;
    Some(true)
}
