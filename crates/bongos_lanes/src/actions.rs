//! The `lanes` actions, hidden from the command palette while `lanes.enabled` is false. While it
//! is false each one only says that lanes are off. Lanes run in the engine host; while they are
//! on, the commands that act on the lanes say that this window does not reach it yet.
//!
//! `lanes: accept graph` runs here, in the writer process: it records the person who pressed the
//! key from the action and lands the graph stamp as that person. `lanes: adopt` records its
//! author the same way; [`adopt_as_person`] takes the arguments the minibuffer collected
//! ([`adopt_prompt`] says what it offers) and runs the adopt.

use std::any::TypeId;

use anyhow::Result;
use bongos_lanes_model::{Author, LaneId, LaneState, LanesState, TaskId};
use bongos_project_repo::ProjectRepositorySettings;
use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::Context as WorkContext;
use command_palette_hooks::CommandPaletteFilter;
#[cfg(test)]
use gpui::Global;
use gpui::{App, Context, Task, Window};
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{Toast, Workspace};

use crate::LanesSettings;
use crate::accept_graph::{Accepted, accept_graph};
use crate::adopt::{AdoptRequest, Adopted};
use crate::dispatcher::Lanes;

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

fn show_toast(workspace: &mut Workspace, message: &str, cx: &mut Context<Workspace>) {
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
    let accepting = accept_graph(repo, ctx, prefix, cx);
    cx.spawn_in(window, async move |workspace, cx| {
        let message = accepted_message(&accepting.await);
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, &message, cx))
            .ok();
    })
    .detach();
}

/// The toast of an accept.
pub fn accepted_message(result: &Result<Accepted>) -> String {
    match result {
        Ok(Accepted::Landed { commit, .. }) => format!(
            "Task graph accepted: the stamp landed in {}",
            &commit[..commit.len().min(12)]
        ),
        Ok(Accepted::Unchanged { .. }) => "The task graph is already accepted".to_string(),
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

/// Registers the handlers of the actions of lanes on `workspace`. None is bound to a key.
pub fn register(workspace: &mut Workspace) {
    register_action::<lanes::Show>(workspace);
    register_action::<lanes::Start>(workspace);
    register_action::<lanes::Stop>(workspace);
    register_action::<lanes::Pause>(workspace);
    register_action::<lanes::Resume>(workspace);
    register_action::<lanes::DryRun>(workspace);
    register_action::<lanes::Pick>(workspace);
    // The adopt runs where the lanes run, in the engine host, which this window does not reach
    // yet; [`adopt_as_person`] is what it runs there.
    register_action::<lanes::Adopt>(workspace);
    workspace.register_action(accept_graph_action);
    register_action::<lanes::Dump>(workspace);
}
