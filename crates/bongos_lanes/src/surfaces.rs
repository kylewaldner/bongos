//! The read-only surfaces of the lane commands: `lanes: show` opens the Lanes view, `lanes: dry
//! run` opens the dry run in a read-only buffer (it writes nothing), and `lanes: start` shows the
//! pre-run summary in a read-only buffer before it asks whether to start. Each reads the project's
//! store and repository only, so a client window shows the same.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use bongos_lanes_model::LaneTable;
use bongos_project_repo::ProjectRepo;
use bongos_workflow::WorkflowEngine;
use bongos_workflow_model::LayerDirs;
use editor::{Editor, MultiBuffer};
use gpui::{AppContext as _, Context, Window};
use language::Capability;
use settings::Settings as _;
use workspace::Workspace;

use crate::LanesSettings;
use crate::actions::{OFF_MESSAGE, ONE_PROJECT_MESSAGE, lanes, pick, show_toast};
use crate::dispatcher::PROJECT_UNATTENDED;
use crate::dry_run::{DryRunInputs, dry_run_buffer_in};
use crate::lanes_view::{StoreLanes, show_lanes_view};
use crate::profile::{LaneProfileSource, LaneProfiles};
use crate::read_modes::{load, read_pick_state};
use crate::roots::LaneTables;
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
    let Some(read) = repo
        .state()
        .and_then(|state| state.read(read_pick_state).ok())
    else {
        show_toast(workspace, "The lanes cannot be read", cx);
        return;
    };
    let location = repo.location().clone();
    let project_id = location.id.clone();
    let limit = LanesSettings::get_global(cx).dry_run_limit as usize;
    let loading = cx.background_spawn(async move { load(&location, limit).await });
    cx.spawn_in(window, async move |workspace, cx| {
        let loaded = loading.await;
        workspace
            .update_in(cx, |workspace, window, cx| match loaded {
                Ok(mut loaded) => {
                    loaded.use_options_now(&read, &project_id, cx);
                    LaneTables::set(project_id, Arc::new(loaded.table.clone()), cx);
                    let text = dry_run_buffer_in(
                        &DryRunInputs {
                            graph: &loaded.graph,
                            state: &read.state,
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

/// What the summary needs besides the lane table.
#[derive(Clone, Default)]
pub struct SummaryContext {
    /// The project id, which the profile is asked about.
    pub project_id: String,
    /// The project profile, when one is registered: its counts win over the table's.
    pub profile: Option<Arc<dyn LaneProfileSource>>,
    /// For each workflow name, the steps that merge or deploy without a gate.
    pub ungated: BTreeMap<String, Vec<String>>,
    /// The group the lanes are bounded to, when the start names one.
    pub within: Option<String>,
}

/// The summary inputs from the lane table: each kind's effective count with its layer (the
/// project profile's when it sets one, else the table), the workflows its rules name with their
/// ungated steps from `context`, each kind's unattended choice with its source (the kind's own,
/// else the project default the dispatcher uses), and the bound.
pub fn summary_inputs(
    table: &LaneTable,
    handover: HandoverNote,
    context: &SummaryContext,
) -> SummaryInputs {
    let counts = table
        .kinds
        .iter()
        .map(|kind| {
            let profile = context.profile.as_deref();
            match profile.and_then(|profile| profile.count(&context.project_id, &kind.name)) {
                Some(count) => KindCount {
                    kind: kind.name.clone(),
                    count,
                    layer: match profile.and_then(|profile| profile.name(&context.project_id)) {
                        Some(name) => format!("project profile {name}"),
                        None => "the project profile".to_string(),
                    },
                },
                None => KindCount {
                    kind: kind.name.clone(),
                    count: kind.count,
                    layer: "lanes.toml".to_string(),
                },
            }
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
        .map(|kind| match kind.unattended {
            Some(unattended) => crate::summary::UnattendedChoice {
                kind: kind.name.clone(),
                unattended,
                source: "lanes.toml".to_string(),
            },
            None => crate::summary::UnattendedChoice {
                kind: kind.name.clone(),
                unattended: PROJECT_UNATTENDED.value,
                source: "the project default".to_string(),
            },
        })
        .collect();
    SummaryInputs {
        counts,
        workflows: names
            .into_iter()
            .map(|name| WorkflowSummary {
                ungated: context.ungated.get(&name).cloned().unwrap_or_default(),
                name,
            })
            .collect(),
        unattended,
        bound: context
            .within
            .as_ref()
            .map(|group| format!("within {group}")),
        handover,
    }
}

/// The steps of each workflow in `names` that merge, or wait on a deploy, with no gate. A name no
/// layer defines, or a definition that does not parse, is left out (the definition check says
/// why).
pub fn ungated_steps(dirs: &LayerDirs, names: &[String]) -> BTreeMap<String, Vec<String>> {
    let resolution = bongos_workflow_model::layers::resolve(dirs);
    let mut found = BTreeMap::new();
    for name in names {
        let Some(resolved) = resolution.get(name) else {
            continue;
        };
        let path = resolved
            .dir
            .join(bongos_workflow_model::layers::DEFINITION_FILE);
        let Some(definition) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| bongos_workflow_model::parse(&text).ok())
        else {
            continue;
        };
        let steps = definition
            .nodes
            .iter()
            .filter(|node| {
                // Only a gate before the step holds it; a gate after it asks once it is done.
                let gated = node.gate.as_ref().is_some_and(|gate| {
                    gate.when == bongos_workflow_model::definition::GateWhen::Before
                });
                !gated && merges_or_deploys(node)
            })
            .map(|node| node.name.clone())
            .collect();
        found.insert(name.clone(), steps);
    }
    found
}

fn merges_or_deploys(node: &bongos_workflow_model::definition::Node) -> bool {
    let used = node.use_node.as_ref().map(|used| used.value.as_str());
    match used {
        Some("merge") => true,
        Some("wait") => {
            node.with.get("condition").and_then(|value| value.as_str()) == Some("deploy")
        }
        _ => false,
    }
}

/// Shows the pre-run summary of `repo` in a read-only buffer, then asks whether to start; `start`
/// runs when the person says to. Nothing starts before that, and nothing is written.
pub fn summarize_then_start(
    repo: ProjectRepo,
    handover: HandoverNote,
    within: Option<String>,
    start: Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let location = repo.location().clone();
    let project_id = location.id.clone();
    let engine_dirs = cx
        .try_global::<WorkflowEngine>()
        .map(|engine| engine.dirs().clone())
        .unwrap_or_default();
    let profile = LaneProfiles::source(cx);
    let loading = cx.background_spawn(async move {
        let loaded = load(&location, 1).await?;
        let mut names: Vec<String> = loaded
            .table
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
        // The project layer is the shown commit's; the shipped and profile layers are the
        // engine's. Reading git here blocks, which this executor allows.
        let mut dirs = engine_dirs;
        let layer = bongos_workflow::definitions::project_layer_in(&repo, None).ok();
        dirs.project = layer.as_ref().map(|layer| layer.dir.clone());
        let ungated = ungated_steps(&dirs, &names);
        Ok::<_, anyhow::Error>((loaded, ungated))
    });
    cx.spawn_in(window, async move |workspace, cx| {
        let loaded = loading.await;
        workspace
            .update_in(cx, |workspace, window, cx| {
                let (table, ungated) = match loaded {
                    Ok((loaded, ungated)) => (loaded.table, ungated),
                    Err(error) => {
                        show_toast(workspace, &format!("Lanes did not start: {error:#}"), cx);
                        return;
                    }
                };
                LaneTables::set(project_id.clone(), Arc::new(table.clone()), cx);
                let context = SummaryContext {
                    project_id,
                    profile,
                    ungated,
                    within,
                };
                let inputs = summary_inputs(&table, handover, &context);
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
