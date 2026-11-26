//! The workflow steps `slack-read` and `slack-post`: the built-ins behind program nodes with
//! `use = "slack-read"` and `use = "slack-post"`, and their live status lines in the run view.
//!
//! Every request goes through the [`SlackService`], so its rate-limit wait, caches, post queue and
//! token rules apply. The runtime calls the built-ins with itself locked, so each hands the work to
//! a worker on the app's main thread and returns; what the start gate needs is kept in a snapshot
//! the worker refreshes whenever the service changes.
//!
//! - `slack-read` (`with`: `channel` as `#name`, `@user` or a conversation id, `count` from 1 to
//!   200, default 50, `thread` as a timestamp or a message link) reads up to `count` messages,
//!   100 per request, and answers `messages` (a JSON array of `{ts, time, author, author_id,
//!   text, thread_ts, link}`, oldest first), `quoted` (the messages quoted as data for a prompt),
//!   `count` and `has_older`. A failure of Slack fails the try as `service unavailable`, which is
//!   retried.
//! - `slack-post` (`with`: `channel`, `text`, `thread`) posts the text once and answers `link`,
//!   `channel` and `ts`. Any failure fails the try as `post not sent`, which is never retried.
//!   An attended run posts at once; an unattended run posts at once only to a conversation on
//!   its project's allowed list, and otherwise asks first with the exact text (a start gate). A
//!   run with no project is allowed nothing.
//! - After a crash or a quit interrupted a post, the next try always asks, saying a previous try
//!   may already have posted. A try that started but whose start was never recorded (a crash in
//!   between) is replayed with the same try number: the post's intent file in the attempt's
//!   directory says it may have been sent, and the replay fails as `post not sent`.
//! - `channel` and `text` are templates over the run's inputs, the results of finished nodes and
//!   the fan-out item, as an agent's prompt is.
//! - While Slack is off or not connected, a try fails at once with that message and nothing is
//!   sent.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bongos_slack_model::approval::{Decision, PostSummary, decide};
use bongos_slack_model::render::{self, UserNames};
use bongos_slack_model::types::{
    Conversation, ConversationId, ConversationKind, Cursor, Message, Ts,
};
use bongos_slack_model::{AllowedList, Destination, Origin, quote};
use bongos_workflow::runtime::{Finish, RunBinding};
use bongos_workflow::task_link::prompt_scope;
use bongos_workflow::{
    BuiltinNode, BuiltinNodes, BuiltinStart, NodeStatus, NodeStatusSource, NodeStatusSources, RunId,
};
use bongos_workflow_model::definition::{SLACK_POST_PROGRAM, SLACK_READ_PROGRAM};
use bongos_workflow_model::template::{Scope, render as render_template};
use bongos_workflow_model::validate::{SLACK_READ_DEFAULT_COUNT, SLACK_READ_MAX_COUNT};
use bongos_workflow_model::{Failure, FailureReason, NodeKey, RunState};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AsyncApp, Entity, WeakEntity};
use serde_json::{Value, json};

use crate::allowed;
use crate::connection::{self, Connection};
use crate::read_buffer::{ConversationEntry, conversation_entries, picker_entries};
use crate::read_view::MessageLink;
use crate::service::{
    Answer, NOT_CONNECTED_MESSAGE, OFF_MESSAGE, RECONNECT_MESSAGE, Read, SlackError, SlackService,
    SlackState,
};

/// The most messages one history request asks for.
pub const READ_PAGE: usize = 100;
/// The most pages of a thread's replies one read goes through.
const MAX_REPLY_PAGES: usize = 20;
/// The most unknown authors looked up for one read.
const MAX_AUTHOR_LOOKUPS: usize = 50;

/// What the question of a post that a crash or a quit interrupted starts with.
pub fn previous_try_warning(channel: &str) -> String {
    format!("A previous try may already have posted this; check {channel} first.")
}

/// The project of a run, from its binding; `None` for a run with no project.
pub type ProjectOf = Arc<dyn Fn(Option<&RunBinding>) -> Option<String> + Send + Sync>;

/// The project of the run's task: the open project repository whose files hold the task's
/// directory.
pub fn task_project(cx: &mut App) -> ProjectOf {
    let repos = bongos_work::OpenProjectRepos::global(cx);
    Arc::new(move |binding: Option<&RunBinding>| {
        let binding = binding?;
        repos
            .list()
            .into_iter()
            .map(|loaded| loaded.repo().location().clone())
            .find(|location| binding.task_dir.starts_with(&location.files))
            .map(|location| location.id)
    })
}

/// How a node's `channel` names its conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Channel {
    Id(ConversationId),
    /// `#name` or `@user`, as the picker names it.
    Named(String),
}

impl Channel {
    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if let Ok(id) = ConversationId::parse(text) {
            return Ok(Self::Id(id));
        }
        if text.len() > 1 && (text.starts_with('#') || text.starts_with('@')) {
            return Ok(Self::Named(text.to_string()));
        }
        Err(format!(
            "`with.channel` must be #name, @user or a conversation id, not \"{text}\""
        ))
    }

    /// The conversation this names among `entries`, without asking Slack.
    fn known(&self, entries: &[ConversationEntry]) -> Option<ConversationId> {
        match self {
            Self::Id(id) => Some(id.clone()),
            Self::Named(name) => entries
                .iter()
                .find(|entry| entry.label.eq_ignore_ascii_case(name))
                .map(|entry| entry.id.clone()),
        }
    }

    /// How the channel is shown: as written, so a question asked about it never changes with
    /// what the service has seen since.
    fn label(&self) -> String {
        match self {
            Self::Named(name) => name.clone(),
            Self::Id(id) => id.to_string(),
        }
    }
}

/// A thread named by its timestamp or by a link to one of its messages.
fn parse_thread(text: &str) -> Result<Ts, String> {
    let text = text.trim();
    if let Ok(ts) = Ts::parse(text) {
        return Ok(ts);
    }
    MessageLink::parse(text)
        .map(|link| link.thread())
        .ok_or_else(|| {
            "`with.thread` must be a message timestamp (like 1730000000.000100) or a message link"
                .to_string()
        })
}

/// The values `channel` and `text` may name: the run's inputs, `nodes.<node>.<field>` of every
/// node's last successful result, and `item` in a fan-out branch.
pub fn template_scope(start: &BuiltinStart<'_>) -> Scope {
    let mut scope = start
        .binding
        .map(|binding| prompt_scope(&binding.inputs))
        .unwrap_or_default();
    for finished in &start.state.finished {
        let Some(fields) = finished
            .result
            .as_ref()
            .filter(|_| finished.succeeded())
            .and_then(Value::as_object)
        else {
            continue;
        };
        for (field, value) in fields {
            scope.set(
                format!("nodes.{}.{field}", finished.key.node),
                value_text(value),
            );
        }
    }
    if let Some(item) = branch_item(start) {
        scope.set("item", item);
    }
    scope
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn last_result<'a>(state: &'a RunState, node: &str, branch: &[(String, u32)]) -> Option<&'a Value> {
    state
        .finished
        .iter()
        .rev()
        .filter(|finished| finished.key.node == node && finished.key.branch == branch)
        .find_map(|finished| finished.result.as_ref().filter(|_| finished.succeeded()))
}

/// The item of the innermost fan-out branch the key runs in.
fn branch_item(start: &BuiltinStart<'_>) -> Option<String> {
    let ((fan, index), outer) = start.key.branch.split_last()?;
    let result = last_result(start.state, fan, outer)?;
    let node = start.definition.node(fan)?;
    node.routes
        .iter()
        .filter_map(|route| route.fan_out.as_ref())
        .find_map(|fan_out| match result.get(&fan_out.over) {
            Some(Value::Array(items)) => items.get(*index as usize).map(value_text),
            _ => None,
        })
}

/// `with.<key>` of the node `start` runs, rendered over the run's state.
fn with_text(start: &BuiltinStart<'_>, scope: &Scope, key: &str) -> Result<Option<String>, String> {
    let node = start
        .definition
        .node(&start.key.node)
        .ok_or_else(|| format!("no node '{}' in the definition", start.key.node))?;
    let Some(value) = node.with.get(key) else {
        return Ok(None);
    };
    let text = value
        .as_str()
        .ok_or_else(|| format!("`with.{key}` must be text"))?;
    render_template(text, scope)
        .map(Some)
        .map_err(|error| format!("`with.{key}`: {error}"))
}

/// A read a `slack-read` try asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ReadJob {
    run: RunId,
    key: NodeKey,
    try_number: u32,
    channel: Channel,
    count: usize,
    thread: Option<Ts>,
}

impl ReadJob {
    fn read(start: &BuiltinStart<'_>) -> Result<Self, String> {
        let scope = template_scope(start);
        let channel = with_text(start, &scope, "channel")?
            .ok_or_else(|| "a slack-read node needs `with.channel`".to_string())?;
        let channel = Channel::parse(&channel)?;
        let thread = with_text(start, &scope, "thread")?
            .filter(|thread| !thread.trim().is_empty())
            .map(|thread| parse_thread(&thread))
            .transpose()?;
        let node = start.definition.node(&start.key.node);
        let count = match node.and_then(|node| node.with.get("count")) {
            None => SLACK_READ_DEFAULT_COUNT,
            Some(value) => value
                .as_integer()
                .filter(|count| (1..=SLACK_READ_MAX_COUNT).contains(count))
                .ok_or_else(|| {
                    format!("`with.count` must be a whole number from 1 to {SLACK_READ_MAX_COUNT}")
                })?,
        };
        Ok(Self {
            run: start.run.to_string(),
            key: start.key.clone(),
            try_number: start.try_number,
            channel,
            count: count as usize,
            thread,
        })
    }
}

/// A post a `slack-post` try asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PostJob {
    run: RunId,
    key: NodeKey,
    try_number: u32,
    channel: Channel,
    /// The text as written by the templates; it is escaped when sent.
    text: String,
    thread: Option<Ts>,
    origin: Origin,
    /// The file that says this try's post may have been sent, in the attempt's directory.
    intent: Option<PathBuf>,
}

impl PostJob {
    fn read(start: &BuiltinStart<'_>) -> Result<Self, String> {
        let scope = template_scope(start);
        let channel = with_text(start, &scope, "channel")?
            .ok_or_else(|| "a slack-post node needs `with.channel`".to_string())?;
        let channel = Channel::parse(&channel)?;
        let text = with_text(start, &scope, "text")?
            .ok_or_else(|| "a slack-post node needs `with.text`".to_string())?;
        if text.trim().is_empty() {
            return Err("the post's text is empty".to_string());
        }
        let thread = with_text(start, &scope, "thread")?
            .filter(|thread| !thread.trim().is_empty())
            .map(|thread| parse_thread(&thread))
            .transpose()?;
        let origin = if start.facts.unattended {
            Origin::UnattendedRun
        } else {
            Origin::AttendedRun
        };
        let intent = start
            .binding
            .map(|binding| intent_path(&binding.attempt_dir, start.key, start.try_number));
        Ok(Self {
            run: start.run.to_string(),
            key: start.key.clone(),
            try_number: start.try_number,
            channel,
            text,
            thread,
            origin,
            intent,
        })
    }

    /// Where the post goes, as the question names it.
    fn destination_label(&self) -> String {
        let label = self.channel.label();
        match self.thread {
            Some(_) => format!("a thread in {label}"),
            None => label,
        }
    }
}

/// `slack-post-<key>-<try>.intent` in `attempt_dir`.
fn intent_path(attempt_dir: &Path, key: &NodeKey, try_number: u32) -> PathBuf {
    let key: String = key
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    attempt_dir.join(format!("slack-post-{key}-{try_number}.intent"))
}

/// What the start gate and the starts read without the app: whether Slack can be asked now, and
/// the conversations seen so far.
struct Facts {
    ready: Result<Connection, String>,
    conversations: Vec<ConversationEntry>,
}

impl Facts {
    fn of(service: &Entity<SlackService>, cx: &App) -> Self {
        let service = service.read(cx);
        let ready = if !SlackService::is_enabled(cx) {
            Err(OFF_MESSAGE.to_string())
        } else {
            match service.state(cx) {
                SlackState::NotConnected => Err(NOT_CONNECTED_MESSAGE.to_string()),
                SlackState::Reconnect(_) => Err(RECONNECT_MESSAGE.to_string()),
                SlackState::Connected(connection) => Ok(connection),
            }
        };
        let conversations = service
            .cached_conversations()
            .map(|list| picker_entries(list, &service.user_names()))
            .unwrap_or_default();
        Self {
            ready,
            conversations,
        }
    }
}

/// What the runtime and the worker share.
struct Shared {
    facts: Facts,
    /// The tries that ended and are not polled yet.
    finishes: Vec<Finish>,
    /// The try in flight of each key.
    current: BTreeMap<(RunId, NodeKey), u32>,
    /// The live line of each key in flight.
    lines: BTreeMap<(RunId, NodeKey), String>,
    /// Post keys a crash or a quit interrupted: their next try asks first, whatever the run.
    interrupted: BTreeSet<(RunId, NodeKey)>,
}

impl Shared {
    /// Reports `finish` unless its try was stopped or replaced.
    fn finish(&mut self, finish: Finish) {
        let slot = (finish.run.clone(), finish.key.clone());
        if self.current.get(&slot) == Some(&finish.try_number) {
            self.current.remove(&slot);
            self.lines.remove(&slot);
            self.finishes.push(finish);
        }
    }

    fn stop(&mut self, tries: &[(String, NodeKey)]) {
        for (run, key) in tries {
            self.finishes
                .retain(|finish| !(finish.run == *run && finish.key == *key));
            let slot = (run.clone(), key.clone());
            self.current.remove(&slot);
            self.lines.remove(&slot);
        }
    }
}

enum Job {
    Read(ReadJob),
    Post(PostJob),
}

/// What both built-ins hold.
struct Core {
    shared: Arc<Mutex<Shared>>,
    jobs: mpsc::UnboundedSender<Job>,
    /// The Slack directory, holding `allowed.toml`.
    dir: PathBuf,
    project_of: ProjectOf,
}

impl Core {
    fn shared(&self) -> MutexGuard<'_, Shared> {
        lock(&self.shared)
    }

    /// Records the try as in flight with `line` and hands `job` to the worker.
    fn launch(&self, slot: (RunId, NodeKey), try_number: u32, line: String, job: Job) -> bool {
        {
            let mut shared = self.shared();
            shared.current.insert(slot.clone(), try_number);
            shared.lines.insert(slot.clone(), line);
        }
        if self.jobs.unbounded_send(job).is_err() {
            let mut shared = self.shared();
            shared.current.remove(&slot);
            shared.lines.remove(&slot);
            return false;
        }
        true
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The [`BuiltinNode`] of `use = "slack-read"`.
pub struct SlackReadNode(Arc<Core>);

/// The [`BuiltinNode`] of `use = "slack-post"`.
pub struct SlackPostNode(Arc<Core>);

/// The two built-ins over one service, and the status source of both.
pub struct SlackNodes {
    pub read: Arc<SlackReadNode>,
    pub post: Arc<SlackPostNode>,
    pub status: Rc<SlackNodeStatus>,
}

impl SlackNodes {
    /// The built-ins over `service`, with their worker on the main thread; `dir` holds
    /// `allowed.toml` and `project_of` names a run's project. Nothing is registered.
    pub fn new(
        service: &Entity<SlackService>,
        dir: PathBuf,
        project_of: ProjectOf,
        cx: &mut App,
    ) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            facts: Facts::of(service, cx),
            finishes: Vec::new(),
            current: BTreeMap::new(),
            lines: BTreeMap::new(),
            interrupted: BTreeSet::new(),
        }));
        let watched = Arc::downgrade(&shared);
        cx.observe(service, move |service, cx| {
            if let Some(shared) = watched.upgrade() {
                lock(&shared).facts = Facts::of(&service, cx);
                // The wait line follows the service's rate-limit wait.
                if !lock(&shared).lines.is_empty() {
                    NodeStatusSources::changed(cx);
                }
            }
        })
        .detach();
        let (jobs, mut receiver) = mpsc::unbounded::<Job>();
        let (posts, mut post_receiver) = mpsc::unbounded::<PostJob>();
        let weak = service.downgrade();
        let reads = shared.clone();
        cx.spawn(async move |cx| {
            while let Some(job) = receiver.next().await {
                match job {
                    Job::Read(job) => {
                        let (service, shared) = (weak.clone(), reads.clone());
                        cx.spawn(async move |cx| {
                            let outcome = match service.upgrade() {
                                Some(service) => run_read(&service, &job, cx).await,
                                None => Err(NOT_CONNECTED_MESSAGE.to_string()),
                            };
                            let outcome = outcome.map_err(|detail| {
                                Failure::new(
                                    FailureReason::ServiceUnavailable,
                                    Some(job.key.clone()),
                                    detail,
                                )
                            });
                            report(&shared, &job.run, &job.key, job.try_number, outcome, cx);
                        })
                        .detach();
                    }
                    Job::Post(job) => {
                        posts.unbounded_send(job).ok();
                    }
                }
            }
        })
        .detach();
        // Posts run one at a time, in the order their tries started.
        let weak = service.downgrade();
        let posted = shared.clone();
        cx.spawn(async move |cx| {
            while let Some(job) = post_receiver.next().await {
                let outcome = match weak.upgrade() {
                    Some(service) => run_post(&service, &job, cx).await,
                    None => Err(NOT_CONNECTED_MESSAGE.to_string()),
                };
                let outcome = outcome.map_err(|detail| {
                    Failure::new(FailureReason::PostNotSent, Some(job.key.clone()), detail)
                });
                report(&posted, &job.run, &job.key, job.try_number, outcome, cx);
            }
        })
        .detach();
        let core = Arc::new(Core {
            shared: shared.clone(),
            jobs,
            dir,
            project_of,
        });
        Self {
            read: Arc::new(SlackReadNode(core.clone())),
            post: Arc::new(SlackPostNode(core)),
            status: Rc::new(SlackNodeStatus {
                shared,
                service: service.downgrade(),
            }),
        }
    }

    /// Registers both built-ins and their status source.
    pub fn register(self, cx: &mut App) {
        BuiltinNodes::register(SLACK_READ_PROGRAM, self.read, cx);
        BuiltinNodes::register(SLACK_POST_PROGRAM, self.post, cx);
        NodeStatusSources::register(SLACK_READ_PROGRAM, self.status.clone(), cx);
        NodeStatusSources::register(SLACK_POST_PROGRAM, self.status, cx);
    }
}

/// Registers `slack-read` and `slack-post` over `service`, with the installed Slack directory and
/// the project of each run's task.
pub fn register(service: &Entity<SlackService>, cx: &mut App) {
    let dir = connection::installed_dir(cx);
    let project_of = task_project(cx);
    SlackNodes::new(service, dir, project_of, cx).register(cx);
}

fn report(
    shared: &Mutex<Shared>,
    run: &str,
    key: &NodeKey,
    try_number: u32,
    outcome: Result<Value, Failure>,
    cx: &mut AsyncApp,
) {
    let finish = Finish {
        outcome,
        ..Finish::ok(run, key.clone(), try_number, Value::Null)
    };
    lock(shared).finish(finish);
    cx.update(NodeStatusSources::changed);
}

impl BuiltinNode for SlackReadNode {
    fn start(&self, start: BuiltinStart<'_>) -> Result<(), Failure> {
        let failure =
            |reason, detail: String| Failure::new(reason, Some(start.key.clone()), detail);
        let job = ReadJob::read(&start)
            .map_err(|detail| failure(FailureReason::ProgramResultInvalid, detail))?;
        let line = {
            let shared = self.0.shared();
            if let Err(message) = &shared.facts.ready {
                return Err(failure(FailureReason::ServiceUnavailable, message.clone()));
            }
            format!("reading {}", job.channel.label())
        };
        let slot = (job.run.clone(), job.key.clone());
        if !self.0.launch(slot, job.try_number, line, Job::Read(job)) {
            return Err(failure(
                FailureReason::ServiceUnavailable,
                "the Slack worker is not running".to_string(),
            ));
        }
        Ok(())
    }

    fn poll(&self) -> Vec<Finish> {
        std::mem::take(&mut self.0.shared().finishes)
    }

    fn stop(&self, tries: &[(String, NodeKey)]) {
        self.0.shared().stop(tries);
    }
}

impl BuiltinNode for SlackPostNode {
    fn start_gate(&self, start: &BuiltinStart<'_>) -> Option<String> {
        // A post that cannot be read, or Slack that is off, asks nothing: the start fails it.
        let job = PostJob::read(start).ok()?;
        let shared = self.0.shared();
        let connection = shared.facts.ready.as_ref().ok()?;
        let entries = &shared.facts.conversations;
        let destination = job.destination_label();
        let question = PostSummary {
            destination: &destination,
            workspace: &connection.workspace,
            user: &connection.user,
            text: &job.text,
        }
        .text();
        if shared
            .interrupted
            .contains(&(job.run.clone(), job.key.clone()))
        {
            let warning = previous_try_warning(&job.channel.label());
            return Some(format!("{warning}\n\n{question}"));
        }
        if job.origin == Origin::AttendedRun {
            return None;
        }
        let allowed = (self.0.project_of)(start.binding)
            .map(|project| allowed::load_in(&self.0.dir, &project))
            .unwrap_or_else(AllowedList::default);
        let send = job.channel.known(entries).is_some_and(|conversation| {
            let destination = Destination {
                conversation,
                thread: job.thread.clone(),
            };
            decide(job.origin, &destination, &allowed) == Decision::Send
        });
        (!send).then_some(question)
    }

    fn start(&self, start: BuiltinStart<'_>) -> Result<(), Failure> {
        let failure = |detail: String| {
            Failure::new(FailureReason::PostNotSent, Some(start.key.clone()), detail)
        };
        let job = PostJob::read(&start).map_err(failure)?;
        let slot = (job.run.clone(), job.key.clone());
        let line = {
            let mut shared = self.0.shared();
            if let Err(message) = &shared.facts.ready {
                return Err(failure(message.clone()));
            }
            shared.interrupted.remove(&slot);
            format!("posting to {}", job.channel.label())
        };
        if !self.0.launch(slot, job.try_number, line, Job::Post(job)) {
            return Err(failure("the Slack worker is not running".to_string()));
        }
        Ok(())
    }

    fn poll(&self) -> Vec<Finish> {
        std::mem::take(&mut self.0.shared().finishes)
    }

    fn stop(&self, tries: &[(String, NodeKey)]) {
        self.0.shared().stop(tries);
    }

    fn recover(&self, run: &str, keys: &[NodeKey]) {
        let mut shared = self.0.shared();
        for key in keys {
            shared.interrupted.insert((run.to_string(), key.clone()));
        }
    }
}

/// The conversation `channel` names: an id as it is (with its name when the list was read), a
/// name looked up in the conversation list.
async fn resolve(
    service: &Entity<SlackService>,
    channel: &Channel,
    cx: &mut AsyncApp,
) -> Result<Conversation, String> {
    let cached = |id: &ConversationId, cx: &mut AsyncApp| {
        service.read_with(cx, |service, _| {
            service
                .cached_conversations()
                .and_then(|list| list.iter().find(|conversation| conversation.id == *id))
                .cloned()
        })
    };
    let id = match channel {
        Channel::Id(id) => id.clone(),
        Channel::Named(name) => {
            let entries = cx
                .update(|cx| conversation_entries(service.clone(), cx))
                .await
                .map_err(|error| error.to_string())?;
            channel
                .known(&entries)
                .ok_or_else(|| format!("Slack has no conversation {name} this token can see"))?
        }
    };
    Ok(cached(&id, cx).unwrap_or_else(|| Conversation {
        kind: if id.as_str().starts_with('D') {
            ConversationKind::Dm
        } else {
            ConversationKind::Channel
        },
        id,
        name: String::new(),
        user: None,
    }))
}

/// Reads up to `job.count` messages, oldest first, and whether older ones exist.
async fn read_messages(
    service: &Entity<SlackService>,
    channel: &ConversationId,
    job: &ReadJob,
    cx: &mut AsyncApp,
) -> Result<(Vec<Message>, bool), SlackError> {
    if let Some(thread) = &job.thread {
        let mut all: Vec<Message> = Vec::new();
        let mut cursor: Option<Cursor> = None;
        let mut more = false;
        for _ in 0..MAX_REPLY_PAGES {
            let read = Read::Replies {
                channel: channel.clone(),
                thread_ts: thread.clone(),
                cursor: cursor.take(),
            };
            let answer = service
                .update(cx, |service, cx| service.call(read, cx))
                .await?;
            let Answer::Messages(page) = answer else {
                unreachable!("conversations.replies answers messages");
            };
            all.extend(page.items);
            more = page.next.is_some();
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let older = more || all.len() > job.count;
        let keep = all.len().saturating_sub(job.count);
        return Ok((all.split_off(keep), older));
    }
    let mut newest_first: Vec<Message> = Vec::new();
    let mut latest: Option<Ts> = None;
    let older = loop {
        let wanted = job.count - newest_first.len();
        let page = service
            .update(cx, |service, cx| {
                service.history(channel.clone(), latest.clone(), wanted.min(READ_PAGE), cx)
            })
            .await?;
        let got = page.items.len();
        newest_first.extend(page.items.into_iter().take(wanted));
        if newest_first.len() >= job.count {
            break page.next.is_some() || got > wanted;
        }
        if page.next.is_none() || got == 0 {
            break false;
        }
        latest = newest_first.last().map(|message| message.ts.clone());
    };
    newest_first.reverse();
    Ok((newest_first, older))
}

/// Looks up the authors of `messages` not seen yet.
async fn resolve_authors(service: &Entity<SlackService>, messages: &[Message], cx: &mut AsyncApp) {
    let known = service.read_with(cx, |service, _| service.user_names());
    let mut unknown = Vec::new();
    for user in messages.iter().filter_map(|message| message.user.clone()) {
        if known.get(&user.0).is_none() && !unknown.contains(&user) {
            unknown.push(user);
        }
    }
    unknown.truncate(MAX_AUTHOR_LOOKUPS);
    for user in unknown {
        // A name that cannot be read leaves the id as the author.
        service
            .update(cx, |service, cx| service.user(user, cx))
            .await
            .ok();
    }
}

fn author(message: &Message, users: &UserNames) -> String {
    match &message.user {
        Some(user) => users.get(&user.0).unwrap_or(&user.0).to_string(),
        None => "bot".to_string(),
    }
}

/// The fixed output of a read.
async fn run_read(
    service: &Entity<SlackService>,
    job: &ReadJob,
    cx: &mut AsyncApp,
) -> Result<Value, String> {
    let conversation = resolve(service, &job.channel, cx).await?;
    let (messages, older) = read_messages(service, &conversation.id, job, cx)
        .await
        .map_err(|error| error.to_string())?;
    resolve_authors(service, &messages, cx).await;
    let (users, zone, connection) = service.read_with(cx, |service, cx| {
        (
            service.user_names(),
            service.zone(),
            connection::current(cx),
        )
    });
    let (workspace, url) = connection
        .map(|connection| (connection.workspace, connection.workspace_url))
        .unwrap_or_default();
    let rows: Vec<Value> = messages
        .iter()
        .map(|message| {
            json!({
                "ts": message.ts.as_str(),
                "time": render::format_time(&message.ts, &*zone),
                "author": author(message, &users),
                "author_id": message.user.as_ref().map(|user| user.0.clone()),
                "text": render::text(&message.text, &users),
                "thread_ts": message.thread_ts.as_ref().map(Ts::as_str),
                "link": render::message_link(&url, &conversation.id, &message.ts, message.thread_ts.as_ref()),
            })
        })
        .collect();
    let quoted = quote::render(&conversation, &messages, &workspace, &users, &*zone);
    Ok(json!({
        "messages": Value::Array(rows).to_string(),
        "quoted": quoted,
        "count": messages.len(),
        "has_older": if older { "yes" } else { "no" },
    }))
}

/// Posts once and answers the fixed output of a post.
async fn run_post(
    service: &Entity<SlackService>,
    job: &PostJob,
    cx: &mut AsyncApp,
) -> Result<Value, String> {
    if let Some(intent) = &job.intent {
        if intent.exists() {
            // This try started before, in a process that ended before it recorded the start.
            return Err(previous_try_warning(&job.channel.label()));
        }
        let written = intent
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(intent, job.text.as_bytes()));
        if let Err(error) = written {
            return Err(format!(
                "nothing was sent: could not record the post at {}: {error}",
                intent.display()
            ));
        }
    }
    let conversation = resolve(service, &job.channel, cx).await?;
    let destination = Destination {
        conversation: conversation.id,
        thread: job.thread.clone(),
    };
    let text = render::escape_for_post(&job.text);
    let origin = job.origin;
    let posted = service
        .update(cx, |service, cx| {
            service.post(destination, text, origin, cx)
        })
        .await
        .map_err(|error| error.to_string())?;
    let url = cx
        .update(|cx| connection::current(cx))
        .map(|connection| connection.workspace_url)
        .unwrap_or_default();
    Ok(json!({
        "link": render::message_link(&url, &posted.channel, &posted.ts, job.thread.as_ref()),
        "channel": posted.channel.as_str(),
        "ts": posted.ts.as_str(),
    }))
}

/// The run view's line for a running Slack node: what it reads or posts, or the rate-limit wait.
pub struct SlackNodeStatus {
    shared: Arc<Mutex<Shared>>,
    service: WeakEntity<SlackService>,
}

impl NodeStatusSource for SlackNodeStatus {
    fn status(&self, run_id: &str, key: &NodeKey, cx: &App) -> Option<NodeStatus> {
        let line = lock(&self.shared)
            .lines
            .get(&(run_id.to_string(), key.clone()))
            .cloned()?;
        let wait = self
            .service
            .upgrade()
            .and_then(|service| service.read(cx).wait_left())
            .filter(|left| !left.is_zero());
        let line = match wait {
            Some(left) => format!("Slack asked to wait {} s", left.as_secs_f64().ceil() as u64),
            None => line,
        };
        Some(NodeStatus { line, detail: None })
    }
}
