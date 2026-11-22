//! The read-only surfaces of the lane commands: `lanes: show` opens the Lanes view, `lanes: dry
//! run` opens the dry run in a read-only buffer (it writes nothing), and `lanes: start` shows the
//! pre-run summary in a read-only buffer before it asks whether to start. Each reads the project's
//! store and repository only, so a client window shows the same.

use std::rc::Rc;

use bongos_lanes_model::LaneTable;
use bongos_project_repo::ProjectRepo;
use editor::{Editor, MultiBuffer};
use gpui::{AppContext as _, Context, Window};
use language::Capability;
use settings::Settings as _;
use workspace::Workspace;

use crate::LanesSettings;
use crate::actions::{OFF_MESSAGE, ONE_PROJECT_MESSAGE, lanes, pick, show_toast};
use crate::dry_run::{DryRunInputs, dry_run_buffer_in};
use crate::lanes_view::{StoreLanes, show_lanes_view};
use crate::read_modes::load;
use crate::store::read_state;
use crate::summary::{
    HandoverNote, KindCount, SummaryInputs, SummarySections, WorkflowSummary, summary_text,
};

/// The title of the dry-run buffer.
pub const DRY_RUN_TITLE: &str = "Lanes: dry run";
/// The title of the summary buffer.
pub const SUMMARY_TITLE: &str = "Lanes: start";
/// The choices after the summary; the first starts.
pub const START_CHOICES: [&str; 2] = ["Start lanes", "Cancel"];

/// The only project repository open in the window, or a toast saying to open one.
fn sole_repo(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> Option<ProjectRepo> {
    let repos = bongos_work::OpenProjectRepos::listed(cx);
    match repos.as_slice() {
        [project] => Some(project.repo().clone()),
        _ => {
            show_toast(workspace, ONE_PROJECT_MESSAGE, cx);
            None
        }
    }
}

/// `lanes: show`: the Lanes view of the project.
pub fn show_action(
    workspace: &mut Workspace,
    _: &lanes::Show,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !LanesSettings::get_global(cx).enabled {
        show_toast(workspace, OFF_MESSAGE, cx);
        return;
    }
    let Some(repo) = sole_repo(workspace, cx) else {
        return;
    };
    show_lanes_view(Rc::new(StoreLanes { repo }), workspace, window, cx);
}

/// Opens `text` in a read-only buffer titled `title` in the active pane. The buffer is in memory
/// only, so opening it writes no file.
pub fn open_text_buffer(
    title: String,
    text: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let buffer = project.update(cx, |project, cx| project.create_buffer(None, false, cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(buffer) = buffer.await else {
            workspace
                .update(cx, |workspace, cx| {
                    show_toast(workspace, "Could not open the buffer", cx)
                })
                .ok();
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.set_text(text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
        });
        workspace
            .update_in(cx, |workspace, window, cx| {
                let multi_buffer =
                    cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title));
                let editor = cx.new(|cx| {
                    let mut editor =
                        Editor::for_multibuffer(multi_buffer, Some(project), window, cx);
                    editor.set_read_only(true);
                    editor
                });
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
            })
            .ok();
    })
    .detach();
}

/// `lanes: dry run`: what each lane would take now, in a read-only buffer. Nothing is written.
pub fn dry_run_action(
    workspace: &mut Workspace,
    _: &lanes::DryRun,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if !LanesSettings::get_global(cx).enabled {
        show_toast(workspace, OFF_MESSAGE, cx);
        return;
    }
    let Some(repo) = sole_repo(workspace, cx) else {
        return;
    };
    let Some(state) = repo.state().and_then(|state| state.read(read_state).ok()) else {
        show_toast(workspace, "The lanes cannot be read", cx);
        return;
    };
    let location = repo.location().clone();
    let limit = LanesSettings::get_global(cx).dry_run_limit as usize;
    let loading = cx.background_spawn(async move { load(&location, limit).await });
    cx.spawn_in(window, async move |workspace, cx| {
        let loaded = loading.await;
        workspace
            .update_in(cx, |workspace, window, cx| match loaded {
                Ok(loaded) => {
                    let text = dry_run_buffer_in(
                        &DryRunInputs {
                            graph: &loaded.graph,
                            state: &state,
                            table: &loaded.table,
                            options: &loaded.options,
                        },
                        cx,
                    );
                    open_text_buffer(DRY_RUN_TITLE.to_string(), text, workspace, window, cx);
                }
                Err(error) => show_toast(workspace, &format!("The dry run failed: {error:#}"), cx),
            })
            .ok();
    })
    .detach();
}

/// The summary inputs the lane table alone gives: each kind's count (the layer is the table, and
/// the profile when it sets one), the workflows its rules name, and the kinds that say whether
/// they run unattended. The steps that merge or deploy without a gate need the definitions and
/// are added by the caller that has them.
pub fn summary_inputs(table: &LaneTable, handover: HandoverNote) -> SummaryInputs {
    let counts = table
        .kinds
        .iter()
        .map(|kind| KindCount {
            kind: kind.name.clone(),
            count: kind.count,
            layer: "lanes.toml".to_string(),
        })
        .collect();
    let mut names: Vec<String> = table
        .rules
        .iter()
        .filter_map(|rule| {
            rule.workflow
                .as_ref()
                .map(|workflow| workflow.value.clone())
        })
        .collect();
    names.sort();
    names.dedup();
    let unattended = table
        .kinds
        .iter()
        .filter_map(|kind| {
            kind.unattended
                .map(|unattended| crate::summary::UnattendedChoice {
                    kind: kind.name.clone(),
                    unattended,
                    source: "lanes.toml".to_string(),
                })
        })
        .collect();
    SummaryInputs {
        counts,
        workflows: names
            .into_iter()
            .map(|name| WorkflowSummary {
                name,
                ungated: Vec::new(),
            })
            .collect(),
        unattended,
        bound: None,
        handover,
    }
}

/// Shows the pre-run summary of `repo` in a read-only buffer, then asks whether to start; `start`
/// runs when the person says to. Nothing starts before that, and nothing is written.
pub fn summarize_then_start(
    repo: ProjectRepo,
    handover: HandoverNote,
    start: Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let location = repo.location().clone();
    let loading = cx.background_spawn(async move { load(&location, 1).await });
    cx.spawn_in(window, async move |workspace, cx| {
        let loaded = loading.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                let table = match loaded {
                    Ok(loaded) => loaded.table,
                    Err(error) => {
                        show_toast(workspace, &format!("Lanes did not start: {error:#}"), cx);
                        return;
                    }
                };
                let inputs = summary_inputs(&table, handover);
                let text = summary_text(&inputs, &SummarySections::sections(cx), cx);
                open_text_buffer(SUMMARY_TITLE.to_string(), text, workspace, window, cx);
                let choices = START_CHOICES.iter().map(ToString::to_string).collect();
                let then = Box::new(
                    move |index: usize,
                          workspace: &mut Workspace,
                          window: &mut Window,
                          cx: &mut Context<Workspace>| {
                        if index == 0 {
                            start(workspace, window, cx);
                        }
                    },
                );
                pick(
                    workspace,
                    "Start lanes?".to_string(),
                    choices,
                    then,
                    window,
                    cx,
                );
            })
            .ok();
    })
    .detach();
}
