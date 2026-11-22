//! The `lanes` actions, hidden from the command palette while `lanes.enabled` is false. While it
//! is false each one only says that lanes are off. Lanes run in the engine host. `lanes: start`
//! in a window that holds the project's writer starts one and hands the writer over to it
//! ([`bongos_engine_host::start_handover`]); the runs of the window finish their current step
//! first, or are interrupted when the person says so. The other commands go to the writer.
//!
//! `lanes: accept graph` runs here, in the writer process: it records the person who pressed the
//! key from the action and lands the graph stamp as that person. `lanes: adopt` records its
//! author the same way: it asks for a stopped or halted task, one of the free lanes
//! ([`adopt_prompt`]) and the reason, then runs [`adopt_as_person`] on the lanes this window
//! reaches ([`WindowLanes`]).

use std::any::TypeId;
use std::rc::Rc;
use std::sync::Arc;

use crate::host::LanesPayload;
use anyhow::Result;
use bongos_engine_host::{
    DrainChoice, DrainPrompt, EngineAccess, EngineAccesses, EngineHostProcess, EngineRuns,
    HandoverRequest, HandoverRuns as _, HostCommand, LanesVerb, ProgramSpawner,
    default_host_log_path, start_handover,
};
use bongos_lanes_model::{AdoptRefusal, Author, LaneId, LaneState, LanesState, OwnerState, TaskId};
use bongos_project_repo::{ProjectRepo, ProjectRepositorySettings};
use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::Context as WorkContext;
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::store::WorkTree;
use command_palette_hooks::CommandPaletteFilter;
use gpui::{App, AppContext as _, Context, Global, Task, Window};
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::LanesSettings;
use crate::accept_graph::{Accepted, accept_graph};
use crate::adopt::{AdoptRequest, Adopted};
use crate::dispatcher::Lanes;
use crate::store::read_state;

pub mod lanes {
    use gpui::actions;

    actions!(
        lanes,
        [
            /// Shows each lane, what it owns and what it waits on.
            Show,
            /// Starts the lanes of the project.
            Start,
            /// Stops the lanes of the project at their next job boundary.
            Stop,
            /// Pauses a lane after its current job.
            Pause,
            /// Resumes a paused or stopped lane.
            Resume,
            /// Shows what each lane would take next, without starting anything.
            DryRun,
            /// Makes a lane look for work now.
            Pick,
            /// Gives a task another lane owns to a free lane.
            Adopt,
            /// Accepts the current task graph as the recorded one.
            AcceptGraph,
            /// Writes the lanes state to a file in the logs folder, for debugging.
            Dump,
        ]
    );
}

pub mod task {
    use gpui::actions;

    actions!(
        task,
        [
            /// Runs the focused group's tasks on lanes: `lanes: start` bounded to the group.
            Run,
        ]
    );
}

/// Every action of lanes, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    vec![
        TypeId::of::<lanes::Show>(),
        TypeId::of::<lanes::Start>(),
        TypeId::of::<lanes::Stop>(),
        TypeId::of::<lanes::Pause>(),
        TypeId::of::<lanes::Resume>(),
        TypeId::of::<lanes::DryRun>(),
        TypeId::of::<lanes::Pick>(),
        TypeId::of::<lanes::Adopt>(),
        TypeId::of::<lanes::AcceptGraph>(),
        TypeId::of::<lanes::Dump>(),
        TypeId::of::<task::Run>(),
    ]
}

/// Hides the actions of lanes from the command palette while `lanes.enabled` is false and shows
/// them while it is true. Call after `command_palette_hooks::init` and [`crate::init`].
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

fn sync_palette(cx: &mut App) {
    let enabled = LanesSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

/// A toast of the lane commands.
pub struct LanesToast;

/// The message of the last toast of the lane commands, for tests.
#[cfg(test)]
pub(crate) struct LastToast(pub String);

#[cfg(test)]
impl Global for LastToast {}

/// The toast a command shows while `lanes.enabled` is false.
pub const OFF_MESSAGE: &str = "Lanes are off (lanes.enabled)";

/// The toast a command shows while lanes are on: the engine host runs them.
pub const ENGINE_HOST_MESSAGE: &str =
    "Lanes run in the engine host, which this window does not reach yet";

pub(crate) fn show_toast(workspace: &mut Workspace, message: &str, cx: &mut Context<Workspace>) {
    #[cfg(test)]
    cx.set_global(LastToast(message.to_string()));
    workspace.show_toast(
        Toast::new(NotificationId::unique::<LanesToast>(), message.to_string()),
        cx,
    );
}

fn register_action<A: gpui::Action>(workspace: &mut Workspace) {
    workspace.register_action(
        |workspace, _: &A, _: &mut Window, cx: &mut Context<Workspace>| {
            let message = if LanesSettings::get_global(cx).enabled {
                ENGINE_HOST_MESSAGE
            } else {
                OFF_MESSAGE
            };
            show_toast(workspace, message, cx);
        },
    );
}

/// The toast of `lanes: accept graph` with no single project repository open in this window.
pub const ONE_PROJECT_MESSAGE: &str = "Open one project repository to accept its task graph";

/// `lanes: accept graph`: lands the stamp of master's graph as the person who pressed the key.
fn accept_graph_action(
    workspace: &mut Workspace,
    _: &lanes::AcceptGraph,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !LanesSettings::get_global(cx).enabled {
        show_toast(workspace, OFF_MESSAGE, cx);
        return;
    }
    let repos = bongos_work::OpenProjectRepos::global(cx).list();
    let [project] = repos.as_slice() else {
        show_toast(workspace, ONE_PROJECT_MESSAGE, cx);
        return;
    };
    let repo = project.repo().clone();
    let prefix = ProjectRepositorySettings::get_global(cx)
        .branch_prefix
        .clone();
    let ctx = WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx));
    if let Some((_, access)) = sole_project(cx) {
        // The writer lands the stamp: it takes the person from this window's claim, and from no
        // other peer's.
        let payload = LanesPayload {
            person: Some(ctx.author.name),
            ..LanesPayload::default()
        };
        send_lanes_command(access, LanesVerb::AcceptGraph, payload, window, cx);
        return;
    }
    let accepting = accept_graph(repo, ctx, prefix, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let message = accepted_message(&accepting.await);
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, &message, cx))
            .ok();
    })
    .detach();
}

/// Sends a `lanes:` command to the writer and shows what it answers.
fn send_lanes_command(
    access: EngineAccess,
    verb: LanesVerb,
    payload: LanesPayload,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let sending = access.request(
        HostCommand::Lanes {
            verb,
            payload: serde_json::to_value(payload).unwrap_or_default(),
        },
        cx,
    );
    cx.spawn_in(window, async move |workspace, cx| {
        let message = match sending.await {
            Ok(answer) => answer_message(&answer),
            Err(error) => format!("{error:#}"),
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, &message, cx))
            .ok();
    })
    .detach();
}

/// A `lanes:` command that goes to the engine host: asks which lane for the ones that act on a
/// lane, then sends it. With no engine host to reach it says the lanes run there.
fn register_host_command<A: gpui::Action>(workspace: &mut Workspace, verb: LanesVerb) {
    workspace.register_action(
        move |workspace, _: &A, window: &mut Window, cx: &mut Context<Workspace>| {
            if !LanesSettings::get_global(cx).enabled {
                show_toast(workspace, OFF_MESSAGE, cx);
                return;
            }
            let Some((repo, access)) = sole_project(cx) else {
                show_toast(workspace, ENGINE_HOST_MESSAGE, cx);
                return;
            };
            if !matches!(verb, LanesVerb::Stop | LanesVerb::Pause | LanesVerb::Resume) {
                send_lanes_command(access, verb, LanesPayload::default(), window, cx);
                return;
            }
            let Some(state) = repo.state().and_then(|state| state.read(read_state).ok()) else {
                show_toast(workspace, "The lanes cannot be read", cx);
                return;
            };
            let mut lanes: Vec<Option<LaneId>> =
                state.lanes.values().map(|l| Some(l.id())).collect();
            if verb == LanesVerb::Stop {
                lanes.insert(0, None);
            }
            let labels = lanes
                .iter()
                .map(|lane| {
                    lane.as_ref()
                        .map_or("All lanes".to_string(), ToString::to_string)
                })
                .collect();
            let then = Box::new(
                move |index: usize,
                      _: &mut Workspace,
                      window: &mut Window,
                      cx: &mut Context<Workspace>| {
                    let Some(choice) = lanes.get(index) else {
                        return;
                    };
                    let ctx = WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx));
                    let payload = LanesPayload {
                        lane: choice.as_ref().map(ToString::to_string),
                        person: Some(ctx.author.name),
                        ..LanesPayload::default()
                    };
                    send_lanes_command(access.clone(), verb, payload, window, cx);
                },
            );
            pick(workspace, "Lane".to_string(), labels, then, window, cx);
        },
    );
}

/// The toast of an accept.
pub fn accepted_message(result: &Result<Accepted>) -> String {
    match result {
        Ok(Accepted::Landed { commit, .. }) => format!(
            "Task graph accepted: the stamp landed in {}",
            &commit[..commit.len().min(12)]
        ),
        Ok(Accepted::Unchanged { .. }) => "The task graph is already accepted".to_string(),
        Ok(Accepted::Landing { change_id, .. }) => format!(
            "Accepting the task graph: its landing runs on (change {change_id}); if it waits, \
             approve it with project: approve"
        ),
        Err(error) => format!("Could not accept the task graph: {error:#}"),
    }
}

/// What the minibuffer asks for `lanes: adopt` of a task: one of the free lanes (idle and owning
/// nothing), then the reason, which may not be empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptPrompt {
    pub task: TaskId,
    /// The lanes offered, in lane order.
    pub lanes: Vec<LaneId>,
    pub lane_prompt: String,
    pub reason_prompt: String,
}

/// The prompts of `lanes: adopt` for `task`, offering only the free lanes of `state`.
pub fn adopt_prompt(state: &LanesState, task: TaskId) -> AdoptPrompt {
    let lanes = state
        .lanes
        .values()
        .filter(|lane| {
            lane.state == LaneState::Idle
                && lane.job.is_none()
                && state.task_of(&lane.id()).is_none()
        })
        .map(|lane| lane.id())
        .collect();
    AdoptPrompt {
        lane_prompt: format!("Adopt {task} on lane: "),
        reason_prompt: format!("Why adopt {task} (required): "),
        task,
        lanes,
    }
}

/// What the minibuffer collected for `lanes: adopt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptArgs {
    pub task: TaskId,
    pub lane: LaneId,
    pub reason: String,
    /// The person confirmed dropping the work of a tree that cannot be read.
    pub confirm_drop: bool,
}

/// Runs `lanes: adopt` with the collected `args`, authored by the person of `ctx` (built from the
/// action with [`PersonAtKeyboard::from_action`]).
pub fn adopt_as_person(
    lanes: &Lanes,
    args: AdoptArgs,
    ctx: &WorkContext,
    cx: &mut App,
) -> Task<Result<Adopted>> {
    let request = AdoptRequest {
        task: args.task,
        lane: args.lane,
        reason: args.reason,
        by: Author::Person(ctx.author.name.clone()),
        confirm_drop: args.confirm_drop,
    };
    lanes.adopt(request, cx)
}

/// What `lanes: adopt` runs against: the lanes this window holds, or the engine host reached
/// through the socket (the adopt then runs in the writer, with this window's person claim).
#[derive(Clone)]
enum AdoptHost {
    Window(Rc<Lanes>),
    Socket {
        repo: ProjectRepo,
        access: EngineAccess,
    },
}

impl AdoptHost {
    /// The host `lanes: adopt` runs against: the engine host of the only project open, else the
    /// lanes this window holds.
    fn of(cx: &App) -> Option<Self> {
        if let Some((repo, access)) = sole_project(cx) {
            return Some(Self::Socket { repo, access });
        }
        cx.try_global::<WindowLanes>()
            .and_then(|window_lanes| window_lanes.0.clone())
            .map(Self::Window)
    }

    fn read_state(&self) -> Result<LanesState> {
        match self {
            Self::Window(lanes) => lanes.parts.writer.read(read_state),
            Self::Socket { repo, .. } => repo
                .state()
                .ok_or_else(|| anyhow::anyhow!("the project's state is not open"))?
                .read(read_state),
        }
    }
}

/// What an adopt came to, as the window shows it.
enum AdoptOutcome {
    Done(String),
    /// A halted job's tree cannot be read; carries the refusal to show above the drop choice.
    Unreadable(String),
    Failed(String),
}

/// The only project repository open and the way to reach its engines, when a window has both.
pub(crate) fn sole_project(cx: &App) -> Option<(ProjectRepo, EngineAccess)> {
    let repos = bongos_work::OpenProjectRepos::listed(cx);
    let [project] = repos.as_slice() else {
        return None;
    };
    let repo = project.repo().clone();
    let access = EngineAccesses::get(&repo.location().id, cx)?;
    Some((repo, access))
}

/// The lanes this window reaches, when it reaches them. Without them (and without an engine host
/// to ask), `lanes: adopt` says the lanes run in the engine host.
#[derive(Default)]
pub struct WindowLanes(pub Option<Rc<Lanes>>);

impl Global for WindowLanes {}

/// The choices of the last list `lanes: adopt` opened, for tests.
#[cfg(test)]
pub(crate) struct LastChoices(pub Vec<String>);

#[cfg(test)]
impl Global for LastChoices {}

/// The last adopt `lanes: adopt` started, until its result is shown, for tests to wait on.
#[cfg(test)]
pub(crate) struct PendingAdopt(pub Option<Task<()>>);

#[cfg(test)]
impl Global for PendingAdopt {}

/// The toast of `lanes: adopt` with no stopped or halted task.
pub const NOTHING_TO_ADOPT_MESSAGE: &str = "No stopped or halted task to adopt";

/// The choices of the drop confirmation of `lanes: adopt`; the second confirms.
pub const DROP_CHOICES: [&str; 2] = ["Keep the work: do not adopt", "Drop the work and adopt"];

/// The tasks `lanes: adopt` offers: the stopped and halted ones, in task order.
pub fn adoptable_tasks(state: &LanesState) -> Vec<TaskId> {
    state
        .owners
        .iter()
        .filter(|(_, owner)| matches!(owner.state, OwnerState::Stopped | OwnerState::Halted))
        .map(|(task, _)| task.clone())
        .collect()
}

/// The toast of an adopt: what moved, or the refusal.
pub fn adopted_message(result: &Result<Adopted>) -> String {
    match result {
        Ok(adopted) => {
            let mut message = match &adopted.from {
                Some(from) => format!("{} adopted by {} from {from}", adopted.task, adopted.lane),
                None => format!("{} adopted by {}", adopted.task, adopted.lane),
            };
            if adopted.dropped_work {
                message.push_str("; the unreadable old work was dropped");
            }
            message
        }
        Err(error) => format!("{error:#}"),
    }
}

pub(crate) fn pick(
    workspace: &mut Workspace,
    placeholder: String,
    choices: Vec<String>,
    then: bongos_work::Then<usize>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    #[cfg(test)]
    cx.set_global(LastChoices(choices.clone()));
    bongos_work::pick(workspace, placeholder, choices, 0, then, window, cx);
}

fn read_lanes_state(
    host: &AdoptHost,
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) -> Option<LanesState> {
    match host.read_state() {
        Ok(state) => Some(state),
        Err(error) => {
            show_toast(
                workspace,
                &format!("The lanes cannot be read: {error:#}"),
                cx,
            );
            None
        }
    }
}

/// `lanes: adopt`: asks for the task, one of its free lanes and the reason, then adopts as the
/// person who pressed the key.
fn adopt_action(
    workspace: &mut Workspace,
    _: &lanes::Adopt,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !LanesSettings::get_global(cx).enabled {
        show_toast(workspace, OFF_MESSAGE, cx);
        return;
    }
    let Some(lanes) = AdoptHost::of(cx) else {
        show_toast(workspace, ENGINE_HOST_MESSAGE, cx);
        return;
    };
    let Some(state) = read_lanes_state(&lanes, workspace, cx) else {
        return;
    };
    let tasks = adoptable_tasks(&state);
    if tasks.is_empty() {
        show_toast(workspace, NOTHING_TO_ADOPT_MESSAGE, cx);
        return;
    }
    let labels = tasks.iter().map(ToString::to_string).collect();
    let then = Box::new(
        move |index: usize,
              workspace: &mut Workspace,
              window: &mut Window,
              cx: &mut Context<Workspace>| {
            if let Some(task) = tasks.get(index) {
                ask_lane(lanes, task.clone(), workspace, window, cx);
            }
        },
    );
    pick(
        workspace,
        "Adopt task".to_string(),
        labels,
        then,
        window,
        cx,
    );
}

fn ask_lane(
    lanes: AdoptHost,
    task: TaskId,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(state) = read_lanes_state(&lanes, workspace, cx) else {
        return;
    };
    let prompt = adopt_prompt(&state, task);
    if prompt.lanes.is_empty() {
        let message = format!("No lane is free to adopt {}", prompt.task);
        show_toast(workspace, &message, cx);
        return;
    }
    let labels = prompt.lanes.iter().map(ToString::to_string).collect();
    let placeholder = prompt
        .lane_prompt
        .trim_end()
        .trim_end_matches(':')
        .to_string();
    let then = Box::new(
        move |index: usize,
              workspace: &mut Workspace,
              window: &mut Window,
              cx: &mut Context<Workspace>| {
            let Some(lane) = prompt.lanes.get(index).cloned() else {
                return;
            };
            let title = prompt
                .reason_prompt
                .trim_end()
                .trim_end_matches(':')
                .to_string();
            let task = prompt.task.clone();
            let then = Box::new(
                move |reason: String,
                      workspace: &mut Workspace,
                      window: &mut Window,
                      cx: &mut Context<Workspace>| {
                    let args = AdoptArgs {
                        task,
                        lane,
                        reason,
                        confirm_drop: false,
                    };
                    run_adopt(lanes, args, workspace, window, cx);
                },
            );
            bongos_work::ask(workspace, title, "", false, then, window, cx);
        },
    );
    pick(workspace, placeholder, labels, then, window, cx);
}

/// Starts the adopt of `args` as the person at the keyboard: in the lanes this window holds, or
/// through the socket with the person's claim, which the writer trusts only from a window.
fn start_adopt(
    lanes: &AdoptHost,
    args: &AdoptArgs,
    ctx: &WorkContext,
    cx: &mut App,
) -> Task<AdoptOutcome> {
    match lanes {
        AdoptHost::Window(lanes) => {
            let adopting = adopt_as_person(lanes, args.clone(), ctx, cx);
            cx.spawn(async move |_| {
                let result = adopting.await;
                let message = adopted_message(&result);
                match &result {
                    Ok(_) => AdoptOutcome::Done(message),
                    Err(error)
                        if matches!(
                            error.downcast_ref::<AdoptRefusal>(),
                            Some(AdoptRefusal::UnreadableTree(_))
                        ) =>
                    {
                        AdoptOutcome::Unreadable(message)
                    }
                    Err(_) => AdoptOutcome::Failed(message),
                }
            })
        }
        AdoptHost::Socket { access, .. } => {
            let payload = LanesPayload {
                task: Some(args.task.to_string()),
                lane: Some(args.lane.to_string()),
                reason: Some(args.reason.clone()),
                confirm_drop: args.confirm_drop,
                within: None,
                person: Some(ctx.author.name.clone()),
            };
            let sending = access.request(
                HostCommand::Lanes {
                    verb: LanesVerb::Adopt,
                    payload: serde_json::to_value(payload).unwrap_or_default(),
                },
                cx,
            );
            cx.spawn(async move |_| match sending.await {
                Ok(answer) => match answer.get("unreadable").and_then(|text| text.as_str()) {
                    Some(text) => AdoptOutcome::Unreadable(text.to_string()),
                    None => AdoptOutcome::Done(answer_message(&answer)),
                },
                Err(error) => AdoptOutcome::Failed(format!("{error:#}")),
            })
        }
    }
}

/// Runs the adopt as the person at the keyboard and shows its result. A tree that cannot be read
/// asks whether to drop its work, and a confirmed drop runs the adopt again with `confirm_drop`.
fn run_adopt(
    lanes: AdoptHost,
    args: AdoptArgs,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if args.reason.trim().is_empty() {
        show_toast(workspace, &AdoptRefusal::BlankReason.to_string(), cx);
        return;
    }
    let ctx = WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx));
    let adopting = start_adopt(&lanes, &args, &ctx, cx);
    let showing = cx.spawn_in(window, async move |workspace, cx| {
        let outcome = adopting.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                let placeholder = match outcome {
                    AdoptOutcome::Done(message) | AdoptOutcome::Failed(message) => {
                        show_toast(workspace, &message, cx);
                        return;
                    }
                    AdoptOutcome::Unreadable(message) if args.confirm_drop => {
                        show_toast(workspace, &message, cx);
                        return;
                    }
                    AdoptOutcome::Unreadable(message) => message,
                };
                let choices = DROP_CHOICES.iter().map(ToString::to_string).collect();
                let then = Box::new(
                    move |index: usize,
                          workspace: &mut Workspace,
                          window: &mut Window,
                          cx: &mut Context<Workspace>| {
                        if index == 1 {
                            let args = AdoptArgs {
                                confirm_drop: true,
                                ..args
                            };
                            run_adopt(lanes, args, workspace, window, cx);
                        }
                    },
                );
                pick(workspace, placeholder, choices, then, window, cx);
            })
            .ok();
    });
    // The adopt runs git, which a test waits on outside the executor.
    #[cfg(test)]
    cx.set_global(PendingAdopt(Some(showing)));
    #[cfg(not(test))]
    showing.detach();
}

/// The text a host answered with.
fn answer_message(answer: &serde_json::Value) -> String {
    answer
        .get("message")
        .and_then(|text| text.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The choices `lanes: start` offers while runs of this window still run a node; the first
/// waits for them, the second interrupts them.
pub const DRAIN_CHOICES: [&str; 3] = [
    "Start lanes; they finish their step here first",
    "Interrupt them now",
    "Cancel",
];

/// The choices offered again when the runs still run past the handover timeout.
pub const DRAIN_AGAIN_CHOICES: [&str; 2] = ["Keep waiting", "Interrupt them now"];

/// What `lanes: start` says while runs of this window still run a node.
pub fn drain_question(busy: usize) -> String {
    match busy {
        1 => "1 run in this window finishes its current step first".to_string(),
        n => format!("{n} runs in this window finish their current step first"),
    }
}

/// `lanes: start`. Lanes run in an engine host: a window that holds the project's writer starts
/// one, which takes the writer over (the runs here finish their current step first, or are
/// interrupted when the person says so), and the start then goes to it. A window that reaches
/// an engine host already sends the start there.
fn start_action(
    workspace: &mut Workspace,
    _: &lanes::Start,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    start_within(workspace, None, window, cx);
}

/// `lanes: start`, bounded to the group `within` when it names one (`task: run` on a group).
fn start_within(
    workspace: &mut Workspace,
    within: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !LanesSettings::get_global(cx).enabled {
        show_toast(workspace, OFF_MESSAGE, cx);
        return;
    }
    let Some((repo, access)) = sole_project(cx) else {
        show_toast(workspace, ENGINE_HOST_MESSAGE, cx);
        return;
    };
    let window_mode = access.is_local() && !EngineHostProcess::is_running(cx);
    let runs_in_progress = if window_mode {
        EngineRuns
            .live_runs(&repo, cx)
            .map(|runs| EngineRuns.in_flight(&runs, cx))
            .unwrap_or(0)
    } else {
        0
    };
    let handover = crate::summary::HandoverNote {
        window_mode,
        runs_in_progress,
    };
    let bound = within.clone();
    crate::surfaces::summarize_then_start(
        repo,
        handover,
        bound,
        Box::new(move |workspace, window, cx| start_confirmed(workspace, within, window, cx)),
        window,
        cx,
    );
}

/// What `lanes: start` does once the person confirmed the summary.
fn start_confirmed(
    workspace: &mut Workspace,
    within: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((repo, access)) = sole_project(cx) else {
        show_toast(workspace, ENGINE_HOST_MESSAGE, cx);
        return;
    };
    let ctx = WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx));
    let person = ctx.author.name;
    if !access.is_local() || EngineHostProcess::is_running(cx) {
        let payload = LanesPayload {
            person: Some(person),
            within,
            ..LanesPayload::default()
        };
        send_lanes_command(access, LanesVerb::Start, payload, window, cx);
        return;
    }
    let runs = match EngineRuns.live_runs(&repo, cx) {
        Ok(runs) => runs,
        Err(error) => {
            show_toast(workspace, &format!("Lanes did not start: {error:#}"), cx);
            return;
        }
    };
    let busy = EngineRuns.in_flight(&runs, cx);
    if busy == 0 {
        hand_over_and_start(repo, DrainChoice::Wait, person, within, window, cx);
        return;
    }
    let labels = DRAIN_CHOICES.iter().map(ToString::to_string).collect();
    let then = Box::new(
        move |index: usize, _: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
            let choice = match index {
                0 => DrainChoice::Wait,
                1 => DrainChoice::InterruptNow,
                _ => return,
            };
            hand_over_and_start(repo, choice, person, within, window, cx);
        },
    );
    pick(workspace, drain_question(busy), labels, then, window, cx);
}

/// Starts the engine host, which takes this window's writer over, then sends it the start.
fn hand_over_and_start(
    repo: ProjectRepo,
    choice: DrainChoice,
    person: String,
    within: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let spawner = match ProgramSpawner::current_exe() {
        Ok(spawner) => spawner,
        Err(error) => {
            let message = format!("Lanes did not start: {error:#}");
            cx.spawn_in(window, async move |workspace, cx| {
                workspace
                    .update(cx, |workspace, cx| show_toast(workspace, &message, cx))
                    .ok();
            })
            .detach();
            return;
        }
    };
    let workspace_handle = cx.entity().downgrade();
    let window_handle = window.window_handle();
    let ask_again: DrainPrompt = Rc::new(move |busy, cx: &mut App| {
        let (sender, answer) = futures::channel::oneshot::channel();
        let sender = Rc::new(std::cell::RefCell::new(Some(sender)));
        let workspace = workspace_handle.clone();
        window_handle
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let labels = DRAIN_AGAIN_CHOICES
                        .iter()
                        .map(ToString::to_string)
                        .collect();
                    let then = Box::new(
                        move |index: usize,
                              _: &mut Workspace,
                              _: &mut Window,
                              _: &mut Context<Workspace>| {
                            let choice = if index == 1 {
                                DrainChoice::InterruptNow
                            } else {
                                DrainChoice::Wait
                            };
                            if let Some(sender) = sender.borrow_mut().take() {
                                sender.send(choice).ok();
                            }
                        },
                    );
                    pick(workspace, drain_question(busy), labels, then, window, cx);
                })
            })
            .ok();
        // Dismissed, the drain keeps waiting.
        cx.background_spawn(async move { answer.await.unwrap_or(DrainChoice::Wait) })
    });
    let log = default_host_log_path(&repo.location().id);
    let request = HandoverRequest {
        choice,
        ask_again: Some(ask_again),
    };
    let handing = start_handover(&repo, Arc::new(spawner), log, request, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let message = match handing.await {
            Ok(client) => {
                // This window is a client of the engine host now; its lanes are the host's.
                let sending = workspace.update(cx, |_, cx| {
                    cx.set_global(WindowLanes(None));
                    let payload = LanesPayload {
                        person: Some(person),
                        within,
                        ..LanesPayload::default()
                    };
                    EngineAccess::Remote(client).request(
                        HostCommand::Lanes {
                            verb: LanesVerb::Start,
                            payload: serde_json::to_value(payload).unwrap_or_default(),
                        },
                        cx,
                    )
                });
                match sending {
                    Ok(sending) => match sending.await {
                        Ok(answer) => answer_message(&answer),
                        Err(error) => format!("{error:#}"),
                    },
                    Err(error) => format!("{error:#}"),
                }
            }
            Err(error) => format!("Lanes did not start: {error:#}"),
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, &message, cx))
            .ok();
    })
    .detach();
}

/// What `task: run` does for the task at point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunTarget {
    /// A group: `lanes: start` bounded to it.
    Group(String),
    /// A leaf: feature 20's `workflow: run`, which asks for the leaf's workflow.
    Leaf,
    /// No task at point: `lanes: start` over the whole project.
    Project,
}

/// Where `task: run` goes for `task` (the task at point) in `tree`.
pub fn run_target(tree: Option<&WorkTree>, task: Option<&str>) -> RunTarget {
    let (Some(tree), Some(id)) = (tree, task) else {
        return RunTarget::Project;
    };
    match tree.task_by_id(id) {
        None => RunTarget::Project,
        Some(found) if derive::shape(found, tree) == Shape::Group => {
            RunTarget::Group(id.to_string())
        }
        Some(_) => RunTarget::Leaf,
    }
}

/// `task: run`: on a group it is `lanes: start` bounded to the group; on a leaf it is
/// `workflow: run`; with no task at point it starts the lanes over the whole project.
fn run_alias_action(
    workspace: &mut Workspace,
    _: &task::Run,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !LanesSettings::get_global(cx).enabled {
        show_toast(workspace, OFF_MESSAGE, cx);
        return;
    }
    let tree = bongos_work::WorkStore::global(cx).and_then(|store| store.read(cx).snapshot());
    let at_point = bongos_work::task_at_point(workspace, window, cx);
    match run_target(tree.as_deref(), at_point.as_deref()) {
        RunTarget::Group(id) => start_within(workspace, Some(id), window, cx),
        RunTarget::Project => start_within(workspace, None, window, cx),
        RunTarget::Leaf => window.dispatch_action(Box::new(bongos_workflow::workflow::Run), cx),
    }
}

/// Registers the handlers of the actions of lanes on `workspace`. None is bound to a key.
pub fn register(workspace: &mut Workspace) {
    workspace.register_action(crate::surfaces::show_action);
    workspace.register_action(start_action);
    register_host_command::<lanes::Stop>(workspace, LanesVerb::Stop);
    register_host_command::<lanes::Pause>(workspace, LanesVerb::Pause);
    register_host_command::<lanes::Resume>(workspace, LanesVerb::Resume);
    workspace.register_action(crate::surfaces::dry_run_action);
    register_host_command::<lanes::Pick>(workspace, LanesVerb::Pick);
    workspace.register_action(run_alias_action);
    workspace.register_action(adopt_action);
    workspace.register_action(accept_graph_action);
    register_action::<lanes::Dump>(workspace);
}
