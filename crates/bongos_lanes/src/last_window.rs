//! Closing the last window while runs live in it. A window that holds a project's writer runs
//! its workflow runs; once the last window closes the app quits and they stop. With lanes on,
//! `lanes.on_last_window_close` says what happens first: `ask` asks Keep running (the writer is
//! handed over to an engine host, and the window closes only once that handover ended) or Stop
//! (the runs are interrupted, as at quit), with a choice that remembers the answer in the
//! setting; `keep` hands over with no question; `stop` interrupts.
//!
//! The decision ([`close_first`], [`close_last_window`]) goes through [`LastWindowClose`], so a
//! test stands in for the prompt, the setting, the handover and the interrupt.
//! [`close_window_after_lanes`] wires it into the close of a window, from the platform or the
//! `CloseWindow` action (through `workspace::CloseWindowGuard`). Quitting the app does not ask.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use bongos_engine_host::{
    DrainChoice, EngineAccesses, EngineHostProcess, EngineRuns, HandoverRequest, HandoverRuns as _,
    ProgramSpawner, default_host_log_path, start_handover,
};
use bongos_project_repo::ProjectRepo;
use gpui::{AnyWindowHandle, App, Global, PromptLevel, Task, WeakEntity, Window};
use settings::Settings as _;
use workspace::notifications::NotificationId;
use workspace::{MultiWorkspace, Toast};

use crate::{LanesOnLastWindowClose, LanesSettings, LanesToast};

/// What closing a window does before it closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseFirst {
    /// Nothing: it is not the last window, or no run lives in a project whose writer it holds.
    Nothing,
    Ask,
    /// Hand the writers over to engine hosts, then close.
    Keep,
    /// Interrupt the runs, then close.
    Stop,
}

/// What closing a window does first, by `lanes.on_last_window_close`: something only for the
/// last window with live runs in projects whose writer it holds.
pub fn close_first(
    setting: LanesOnLastWindowClose,
    last_window: bool,
    projects_with_live_runs: usize,
) -> CloseFirst {
    if !last_window || projects_with_live_runs == 0 {
        return CloseFirst::Nothing;
    }
    match setting {
        LanesOnLastWindowClose::Ask => CloseFirst::Ask,
        LanesOnLastWindowClose::Keep => CloseFirst::Keep,
        LanesOnLastWindowClose::Stop => CloseFirst::Stop,
    }
}

/// What the person chose when asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LastWindowChoice {
    Keep,
    Stop,
    Cancel,
}

/// The person's answer, and whether to remember it in `lanes.on_last_window_close`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastWindowAnswer {
    pub choice: LastWindowChoice,
    pub remember: bool,
}

/// The question when the last window closes with live runs.
pub const LAST_WINDOW_QUESTION: &str = "Runs are still live in this window";

/// The detail under the question.
pub const LAST_WINDOW_DETAIL: &str = "Keep running hands them to an engine host, which goes on \
    after the window closes. Stop interrupts them; they wait for you the next time you open the \
    project.";

/// The choices offered; [`answer_of`] reads the one picked.
pub const LAST_WINDOW_CHOICES: [&str; 5] = [
    "Keep running",
    "Stop",
    "Always keep running",
    "Always stop",
    "Cancel",
];

/// The answer of the choice at `index` of [`LAST_WINDOW_CHOICES`]; a dismissed prompt cancels.
pub fn answer_of(index: Option<usize>) -> LastWindowAnswer {
    let (choice, remember) = match index {
        Some(0) => (LastWindowChoice::Keep, false),
        Some(1) => (LastWindowChoice::Stop, false),
        Some(2) => (LastWindowChoice::Keep, true),
        Some(3) => (LastWindowChoice::Stop, true),
        _ => (LastWindowChoice::Cancel, false),
    };
    LastWindowAnswer { choice, remember }
}

/// What closing the last window acts through.
pub trait LastWindowClose {
    /// Asks the person.
    fn ask(&self, cx: &mut App) -> Task<LastWindowAnswer>;
    /// Writes `setting` as `lanes.on_last_window_close`.
    fn remember(&self, setting: LanesOnLastWindowClose, cx: &mut App);
    /// Hands the writer of each project with live runs over to an engine host. Resolves once
    /// every handover ended: `Ok` when each writer moved, else why one stayed here (taken back).
    fn hand_over(&self, cx: &mut App) -> Task<Result<()>>;
    /// Interrupts the live runs, as at quit.
    fn stop(&self, cx: &mut App);
}

/// How closing the last window ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseOutcome {
    /// The window may close now.
    Close,
    /// The window stays open: the person cancelled (`None`), or the handover failed and the
    /// writer is back here (what to tell the person).
    StayOpen(Option<String>),
}

/// Does what closing the last window does first (`first`, from [`close_first`]) and resolves
/// with whether the window may close. Keep resolves only once the handover ended.
pub fn close_last_window(
    first: CloseFirst,
    closer: Rc<dyn LastWindowClose>,
    cx: &mut App,
) -> Task<CloseOutcome> {
    match first {
        CloseFirst::Nothing => Task::ready(CloseOutcome::Close),
        CloseFirst::Stop => {
            closer.stop(cx);
            Task::ready(CloseOutcome::Close)
        }
        CloseFirst::Keep => {
            let handing = closer.hand_over(cx);
            cx.spawn(async move |_| handed_over(handing.await))
        }
        CloseFirst::Ask => {
            let asking = closer.ask(cx);
            cx.spawn(async move |cx| {
                let answer = asking.await;
                let setting = match answer.choice {
                    LastWindowChoice::Keep => LanesOnLastWindowClose::Keep,
                    LastWindowChoice::Stop => LanesOnLastWindowClose::Stop,
                    LastWindowChoice::Cancel => return CloseOutcome::StayOpen(None),
                };
                if answer.remember {
                    cx.update(|cx| closer.remember(setting, cx));
                }
                if setting == LanesOnLastWindowClose::Stop {
                    cx.update(|cx| closer.stop(cx));
                    return CloseOutcome::Close;
                }
                let handing = cx.update(|cx| closer.hand_over(cx));
                handed_over(handing.await)
            })
        }
    }
}

fn handed_over(result: Result<()>) -> CloseOutcome {
    match result {
        Ok(()) => CloseOutcome::Close,
        Err(error) => CloseOutcome::StayOpen(Some(format!(
            "The window stays open, its runs here: {error:#}"
        ))),
    }
}

/// The projects whose writer this window holds (no engine host serves them) with live runs.
fn projects_with_live_runs(cx: &mut App) -> Vec<(ProjectRepo, Vec<String>)> {
    if EngineHostProcess::is_running(cx) {
        return Vec::new();
    }
    let mut projects = Vec::new();
    for project in bongos_work::OpenProjectRepos::listed(cx) {
        let repo = project.repo().clone();
        let local =
            EngineAccesses::get(&repo.location().id, cx).is_some_and(|access| access.is_local());
        if !local {
            continue;
        }
        match EngineRuns.live_runs(&repo, cx) {
            Ok(runs) if !runs.is_empty() => projects.push((repo, runs)),
            Ok(_) => {}
            Err(error) => log::warn!(
                "reading the live runs of {} at the window's close: {error:#}",
                repo.location().id
            ),
        }
    }
    projects
}

/// [`LastWindowClose`] of a real window.
struct WindowCloser {
    window: AnyWindowHandle,
    projects: Vec<(ProjectRepo, Vec<String>)>,
}

impl LastWindowClose for WindowCloser {
    fn ask(&self, cx: &mut App) -> Task<LastWindowAnswer> {
        // The close runs while the window is being updated, so the prompt opens once that ended.
        let handle = self.window;
        cx.spawn(async move |cx| {
            let asked = handle.update(cx, |_, window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    LAST_WINDOW_QUESTION,
                    Some(LAST_WINDOW_DETAIL),
                    &LAST_WINDOW_CHOICES,
                    cx,
                )
            });
            match asked {
                Ok(answer) => answer_of(answer.await.ok()),
                Err(_) => answer_of(None),
            }
        })
    }

    fn remember(&self, setting: LanesOnLastWindowClose, cx: &mut App) {
        settings::update_settings_file(<dyn fs::Fs>::global(cx), cx, move |content, _| {
            content.lanes.get_or_insert_default().on_last_window_close = Some(setting);
        });
    }

    fn hand_over(&self, cx: &mut App) -> Task<Result<()>> {
        let spawner = match ProgramSpawner::current_exe() {
            Ok(spawner) => Arc::new(spawner),
            Err(error) => return Task::ready(Err(error)),
        };
        let handing: Vec<_> = self
            .projects
            .iter()
            .map(|(repo, _)| {
                let log = default_host_log_path(&repo.location().id);
                let request = HandoverRequest {
                    choice: DrainChoice::Wait,
                    ask_again: None,
                };
                start_handover(repo, spawner.clone(), log, request, cx)
            })
            .collect();
        // The window stays open until every handover ended, which can take as long as the
        // longest live node; say so, once the window is no longer being updated.
        let handle = self.window;
        cx.defer(move |cx| show_toast(handle, HANDING_OVER, cx));
        cx.spawn(async move |_| {
            // Every handover runs to its end (moved, or taken back) before the answer.
            let mut failed = None;
            for handover in handing {
                if let Err(error) = handover.await
                    && failed.is_none()
                {
                    failed = Some(error);
                }
            }
            failed.map_or(Ok(()), Err)
        })
    }

    fn stop(&self, cx: &mut App) {
        for (_, runs) in &self.projects {
            EngineRuns.interrupt(runs, cx);
        }
    }
}

/// The toast while the window waits on its handovers.
pub const HANDING_OVER: &str = "Handing the live runs over to an engine host; the window \
    closes once it took them.";

/// The toast when a close comes while an earlier one still waits.
pub const STILL_CLOSING: &str = "Still closing: waiting on your answer or the handover.";

fn show_toast(handle: AnyWindowHandle, message: &'static str, cx: &mut App) {
    let Some(handle) = handle.downcast::<MultiWorkspace>() else {
        return;
    };
    handle
        .update(cx, |multi_workspace, _, cx| {
            multi_workspace.workspace().update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<LanesToast>(), message),
                    cx,
                )
            })
        })
        .ok();
}

/// Set while closing the last window waits on the person or a handover, so a second close
/// does not start another.
#[derive(Default)]
struct ClosingLastWindow(Rc<Cell<bool>>);

impl Global for ClosingLastWindow {}

/// Clears [`ClosingLastWindow`] when the close that set it ends, however it ends (its task
/// may be dropped with the window).
pub(crate) struct ClosingReset(Rc<Cell<bool>>);

impl Drop for ClosingReset {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// Starts a close of the last window: the reset to hold until it ended, or `None` when one
/// is already under way.
pub(crate) fn start_closing(cx: &mut App) -> Option<ClosingReset> {
    let closing = cx.default_global::<ClosingLastWindow>().0.clone();
    if closing.replace(true) {
        return None;
    }
    Some(ClosingReset(closing))
}

/// A window is closing (the platform close or the `CloseWindow` action): when it is the last
/// window and runs live in projects whose writer it holds (lanes on), closing first does what
/// `lanes.on_last_window_close` says, then closes it through `multi_workspace` (or shows why it
/// stays open), and this returns true. Otherwise returns false and the caller closes the window
/// as usual.
pub fn close_window_after_lanes(
    multi_workspace: &WeakEntity<MultiWorkspace>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if cx.default_global::<ClosingLastWindow>().0.get() {
        let handle = window.window_handle();
        cx.defer(move |cx| show_toast(handle, STILL_CLOSING, cx));
        return true;
    }
    let settings = LanesSettings::get_global(cx);
    if !settings.enabled {
        return false;
    }
    let setting = settings.on_last_window_close;
    let last_window = cx.windows().len() <= 1;
    if !last_window {
        return false;
    }
    let projects = projects_with_live_runs(cx);
    let first = close_first(setting, last_window, projects.len());
    if first == CloseFirst::Nothing {
        return false;
    }
    let Some(reset) = start_closing(cx) else {
        return true;
    };
    let closer = Rc::new(WindowCloser {
        window: window.window_handle(),
        projects,
    });
    let closing = close_last_window(first, closer, cx);
    let multi_workspace = multi_workspace.clone();
    window
        .spawn(cx, async move |cx| {
            let outcome = closing.await;
            drop(reset);
            multi_workspace
                .update_in(cx, |multi_workspace, window, cx| match outcome {
                    CloseOutcome::Close => multi_workspace.close_window_unguarded(window, cx),
                    CloseOutcome::StayOpen(Some(message)) => {
                        multi_workspace.workspace().update(cx, |workspace, cx| {
                            workspace.show_toast(
                                Toast::new(NotificationId::unique::<LanesToast>(), message),
                                cx,
                            )
                        })
                    }
                    CloseOutcome::StayOpen(None) => {}
                })
                .ok();
        })
        .detach();
    true
}
