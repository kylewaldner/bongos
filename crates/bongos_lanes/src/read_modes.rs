//! The lanes' read modes: `status`, `dry-run` and `pick` for a process that holds no writer
//! (`bongos lanes status` with no lanes running). They read the project's store through a
//! read-only handle and its repository through git, so they never take the host lock and show the
//! state as of the last write of the process that holds it, if any. They are served by the app
//! binary's `--read` mode through [`ReadModes`].

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use bongos_engine_host::{ReadModes, ReadProject};
use bongos_lanes_model::{
    AssignedWork, DryRun, LaneId, LanesState, PickOptions, PickOutcome, explain, from_work_tree,
    parse, pick,
};
use bongos_project_repo::{
    CommitId, Git, Location, ProjectRepo, StateHandle, StateRead, StateReader, StateStore, TreeRef,
};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use bongos_workflow::WorkflowEngine;
use gpui::{App, AppContext as _, Task};
use serde_json::{Value, json};
use settings::Settings as _;

use crate::accept_graph::{file_at, read_stamp};
use crate::definitions::{LANES_FILE, LANES_FOLDER};
use crate::dispatcher::{
    Dispatcher, DispatcherRecord, JobEngine, RecordedBound, effective_counts, pick_options_with,
    read_dispatcher_record,
};
use crate::lane_control::status_from_read;
use crate::status_doc::{StatusFacts, status_doc_with};
use crate::store::read_state;
use crate::wait_text::WaitTextFormatters;
use crate::{LaneProfileSource, LaneProfiles, LanesSettings};

/// Serves the read modes `status`, `dry-run` and `pick`.
pub fn register(cx: &mut App) {
    ReadModes::register("status", status_mode, cx);
    ReadModes::register_async("dry-run", dry_run_mode, cx);
    ReadModes::register_async("pick", pick_mode, cx);
}

/// The arguments the read modes of the lanes take.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReadArgs {
    json: bool,
    lane: Option<LaneId>,
}

fn parse_args(args: &[String]) -> Result<ReadArgs> {
    let mut parsed = ReadArgs::default();
    let mut words = args.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--json" => parsed.json = true,
            "--lane" => {
                let lane = words.next().context("--lane needs a lane (like build-2)")?;
                parsed.lane = Some(
                    LaneId::parse(lane)
                        .ok_or_else(|| anyhow!("{lane} is not a lane (like build-2)"))?,
                );
            }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(parsed)
}

fn location(cx: &App) -> Result<Location> {
    ReadProject::get(cx).context("no project is open for this read")
}

/// The project's repository with its store open for reading only.
fn read_only_repo(location: &Location) -> Result<ProjectRepo> {
    let git = Git::locate().map_err(|error| anyhow!("{error}"))?;
    let reader = StateStore::open_reader(location)?;
    Ok(ProjectRepo::new(location.clone(), git).with_state(StateHandle::Reader(Arc::new(reader))))
}

fn status_mode(args: &[String], reader: &StateReader, cx: &mut App) -> Result<String> {
    let args = parse_args(args)?;
    let location = location(cx)?;
    let status = reader.read(status_from_read)?;
    let split = read_only_repo(&location)?.split_master()?;
    let doc = status_doc_with(
        &status,
        &StatusFacts {
            project: &location.id,
            split: split.as_ref(),
            goal: &|_| None,
            run_lifecycle: &|_| None,
            waits_on_landing: &|_| true,
        },
        &WaitTextFormatters::global(cx),
    );
    if args.json {
        Ok(serde_json::to_string_pretty(&doc)?)
    } else {
        Ok(status_text(&doc))
    }
}

/// Which of the two picker views a read asks for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pick {
    DryRun,
    One,
}

fn dry_run_mode(args: &[String], reader: &StateReader, cx: &mut App) -> Task<Result<String>> {
    picking(Pick::DryRun, args, reader, cx)
}

fn pick_mode(args: &[String], reader: &StateReader, cx: &mut App) -> Task<Result<String>> {
    picking(Pick::One, args, reader, cx)
}

fn picking(
    view: Pick,
    args: &[String],
    reader: &StateReader,
    cx: &mut App,
) -> Task<Result<String>> {
    let prepared = parse_args(args).and_then(|args| {
        if matches!(view, Pick::One) && args.lane.is_none() {
            bail!("name a lane (--lane build-2)");
        }
        let read = reader.read(read_pick_state)?;
        let limit = LanesSettings::get_global(cx).dry_run_limit as usize;
        Ok((args, location(cx)?, read, limit))
    });
    match prepared {
        Ok((args, location, read, limit)) => {
            let project = location.id.clone();
            let loading = cx.background_spawn(async move { load(&location, limit).await });
            cx.spawn(async move |cx| {
                let mut loaded = loading.await?;
                cx.update(|cx| loaded.use_options_now(&read, &project, cx));
                render_pick(view, &args, &loaded, &read.state)
            })
        }
        Err(error) => Task::ready(Err(error)),
    }
}

/// What a dry run or a pick reads from the store: the lanes state and the dispatcher record,
/// whose bound applies while its run has not ended.
pub(crate) struct PickRead {
    pub(crate) state: LanesState,
    pub(crate) record: Option<DispatcherRecord>,
}

/// Reads a [`PickRead`] in one read of the store.
pub(crate) fn read_pick_state(read: &StateRead) -> Result<PickRead> {
    Ok(PickRead {
        state: read_state(read)?,
        record: read_dispatcher_record(read)?,
    })
}

/// What the picker reads besides the run state, at the commit the project shows.
pub(crate) struct Loaded {
    pub(crate) graph: bongos_lanes_model::PickGraph,
    pub(crate) table: bongos_lanes_model::LaneTable,
    /// The options: after [`load`] only the waiting limit and the recorded stamp; a dry run or a
    /// pick sets the rest with [`Loaded::use_options_now`] before it asks the picker.
    pub(crate) options: PickOptions,
}

impl Loaded {
    /// Sets the options to those a pick in this process uses now (see [`options_now`]).
    pub(crate) fn use_options_now(&mut self, read: &PickRead, project: &str, cx: &App) {
        self.options = options_now(self, read, project, cx);
    }
}

/// The options the dispatcher would pick with now: those of the dispatcher this process runs, or
/// with none the same options built the same way from the effective counts (the profile's
/// overrides over the table's), the bound of the dispatcher record while its run has not ended,
/// and the instance use the run engine of this process knows of.
pub(crate) fn options_now(
    loaded: &Loaded,
    read: &PickRead,
    project: &str,
    cx: &App,
) -> PickOptions {
    let dispatcher = crate::host::running_dispatcher(cx);
    let engine = cx
        .try_global::<WorkflowEngine>()
        .map(|engine| engine as &dyn JobEngine);
    let profile = LaneProfiles::source(cx);
    current_options(
        &OptionSources {
            dispatcher: dispatcher.as_ref(),
            engine,
            profile: profile.as_deref(),
            project,
        },
        loaded,
        read,
    )
}

/// Where [`current_options`] reads the live facts from.
pub(crate) struct OptionSources<'a> {
    /// The dispatcher this process runs, whose own options win.
    pub(crate) dispatcher: Option<&'a Dispatcher>,
    /// The run engine of this process; with none no instance limit is known.
    pub(crate) engine: Option<&'a dyn JobEngine>,
    pub(crate) profile: Option<&'a dyn LaneProfileSource>,
    pub(crate) project: &'a str,
}

/// The options of a dry run or a pick over `read` (see [`options_now`]), with the waiting limit
/// of `loaded`.
pub(crate) fn current_options(
    sources: &OptionSources<'_>,
    loaded: &Loaded,
    read: &PickRead,
) -> PickOptions {
    let mut options = match sources.dispatcher {
        Some(dispatcher) => dispatcher.pick_options(&read.state, &loaded.graph, &loaded.table),
        None => pick_options_with(
            sources.engine,
            &read.state,
            &loaded.graph,
            &loaded.table,
            effective_counts(&loaded.table, sources.project, sources.profile),
            read.record
                .as_ref()
                .filter(|record| record.ended.is_none())
                .and_then(|record| record.bound.as_ref())
                .map(RecordedBound::to_bound),
            loaded.options.stamp.clone(),
        ),
    };
    options.waiting_limit = loaded.options.waiting_limit;
    options
}

pub(crate) async fn load(location: &Location, waiting_limit: usize) -> Result<Loaded> {
    let repo = read_only_repo(location)?;
    let commit = repo.shown_commit().await?.0;
    let tree = repo
        .load_work_tree_at(&TreeRef::Commit(CommitId(commit.clone())))
        .await?;
    let project = tree
        .projects
        .first()
        .context("the project has no top group")?;
    let live = crate::host::live_facts_on_disk(&tree);
    let graph = from_work_tree(&tree, &project.slug, &live)
        .context("the project's task graph cannot be read")?;
    let table_path = format!("{DEFINITIONS_DIR}/{LANES_FOLDER}/{LANES_FILE}");
    let table_text = file_at(&repo, &commit, &table_path)
        .await?
        .unwrap_or_else(|| "schema = 1\n".to_string());
    let table = parse(&table_text)
        .map_err(|error| anyhow!("{table_path}: line {}: {}", error.line, error.message))?;
    let options = PickOptions {
        waiting_limit: Some(waiting_limit),
        stamp: read_stamp(&repo, &commit).await?,
        ..PickOptions::default()
    };
    Ok(Loaded {
        graph,
        table,
        options,
    })
}

fn render_pick(view: Pick, args: &ReadArgs, loaded: &Loaded, state: &LanesState) -> Result<String> {
    let (text, doc) = pick_answer(view, args.lane.as_ref(), loaded, state)?;
    if args.json {
        Ok(serde_json::to_string_pretty(&doc)?)
    } else {
        Ok(text)
    }
}

/// The picker's answer over `state`, as text and as the JSON document `--json` prints: the dry
/// run (of `lane` only when one is named), or the pick of `lane`. It writes nothing.
pub(crate) fn pick_answer(
    view: Pick,
    lane: Option<&LaneId>,
    loaded: &Loaded,
    state: &LanesState,
) -> Result<(String, Value)> {
    match view {
        Pick::DryRun => {
            let mut dry = explain(&loaded.graph, state, &loaded.table, &loaded.options);
            if let Some(lane) = lane {
                dry.per_lane.retain(|(id, _)| id == lane);
            }
            Ok((dry_run_text(&dry), serde_json::to_value(&dry)?))
        }
        Pick::One => {
            let lane = lane.context("name a lane (--lane build-2)")?;
            let outcome = pick(&loaded.graph, state, &loaded.table, lane, &loaded.options);
            let doc = json!({ "lane": lane.to_string(), "outcome": outcome });
            Ok((outcome_text(lane, &outcome), doc))
        }
    }
}

fn outcome_text(lane: &LaneId, outcome: &PickOutcome) -> String {
    match outcome {
        PickOutcome::Assign(assignment) => {
            let work = match &assignment.work {
                AssignedWork::Leaf { task, .. } => task.0.clone(),
                AssignedWork::Extension { kind, task, .. } => match task {
                    Some(task) => format!("{kind} {}", task.0),
                    None => kind.clone(),
                },
            };
            format!(
                "{lane}: would start {work} with {} ({})",
                assignment.workflow, assignment.reason
            )
        }
        PickOutcome::Nothing { reasons, .. } => {
            if reasons.is_empty() {
                format!("{lane}: nothing to do")
            } else {
                let reasons = reasons
                    .iter()
                    .map(|waiting| format!("{} waits: {}", waiting.task.0, waiting.reason))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("{lane}: nothing to do ({reasons})")
            }
        }
        PickOutcome::Hold(hold) => format!("{lane}: holds, {} {}", hold.task.0, hold.reason),
    }
}

fn dry_run_text(dry: &DryRun) -> String {
    let mut out = String::new();
    for (lane, outcome) in &dry.per_lane {
        writeln!(out, "{}", outcome_text(lane, outcome)).ok();
    }
    for waiting in &dry.waiting {
        writeln!(out, "waiting: {} ({})", waiting.task.0, waiting.reason).ok();
    }
    write!(
        out,
        "{} ready, critical path {}, {} at once",
        dry.ready, dry.critical_path, dry.parallel
    )
    .ok();
    out
}

/// The plain-text status of a `lanes-status 1` document, the same text the command line prints
/// when it asks a running writer.
fn status_text(doc: &Value) -> String {
    let mut out = String::new();
    let header = &doc["header"];
    let count = |key: &str| header[key].as_u64().unwrap_or(0);
    writeln!(
        out,
        "lanes: {} (dispatcher {}): {} running, {} idle, {} waiting, {} paused, {} stopped, {} halted",
        count("lanes"),
        header["dispatcher"].as_str().unwrap_or("unknown"),
        count("running"),
        count("idle"),
        count("waiting"),
        count("paused"),
        count("stopped"),
        count("halted"),
    )
    .ok();
    if doc.get("master_split").is_some() {
        writeln!(
            out,
            "master is split: every landing waits until a person runs project: resolve split"
        )
        .ok();
    }
    for lane in doc["lanes"].as_array().into_iter().flatten() {
        let text = |key: &str| lane[key].as_str();
        let mut row = format!(
            "{}  {}",
            text("id").unwrap_or("?"),
            text("state").unwrap_or("?")
        );
        if let Some(task) = text("task") {
            write!(row, "  {task}").ok();
            if let Some(goal) = text("goal") {
                write!(row, " ({goal})").ok();
            }
        }
        if let Some(run) = text("run") {
            write!(row, "  run {run}").ok();
            if let Some(workflow) = text("workflow") {
                write!(row, " of {workflow}").ok();
            }
            if let Some(lifecycle) = text("run_lifecycle") {
                write!(row, " ({lifecycle})").ok();
            }
        }
        if let Some(reason) = text("reason") {
            write!(row, "  {reason}").ok();
        }
        write!(
            row,
            "  {} jobs done",
            lane["jobs_done"].as_u64().unwrap_or(0)
        )
        .ok();
        writeln!(out, "{row}").ok();
    }
    out.trim_end().to_string()
}

#[cfg(test)]
#[path = "read_modes_tests.rs"]
mod tests;
