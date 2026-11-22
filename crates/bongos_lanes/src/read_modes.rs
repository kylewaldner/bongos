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
    AssignedWork, DryRun, LaneId, LanesState, LiveFacts, PickOptions, PickOutcome, explain,
    from_work_tree, parse, pick,
};
use bongos_project_repo::{
    CommitId, Git, Location, ProjectRepo, StateHandle, StateReader, StateStore, TreeRef,
};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use gpui::{App, AppContext as _, Task};
use serde_json::{Value, json};
use settings::Settings as _;

use crate::LanesSettings;
use crate::accept_graph::{file_at, read_stamp};
use crate::definitions::{LANES_FILE, LANES_FOLDER};
use crate::lane_control::status_from_read;
use crate::status_doc::{StatusFacts, status_doc};
use crate::store::read_state;

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
    let doc = status_doc(
        &status,
        &StatusFacts {
            project: &location.id,
            split: split.as_ref(),
            goal: &|_| None,
            run_lifecycle: &|_| None,
            waits_on_landing: &|_| true,
        },
    );
    if args.json {
        Ok(serde_json::to_string_pretty(&doc)?)
    } else {
        Ok(status_text(&doc))
    }
}

/// Which of the two picker views a read asks for.
#[derive(Clone, Copy)]
enum Pick {
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
        let state = reader.read(|read| read_state(read))?;
        let limit = LanesSettings::get_global(cx).dry_run_limit as usize;
        Ok((args, location(cx)?, state, limit))
    });
    match prepared {
        Ok((args, location, state, limit)) => cx.background_spawn(async move {
            let loaded = load(&location, limit).await?;
            render_pick(view, &args, &loaded, &state)
        }),
        Err(error) => Task::ready(Err(error)),
    }
}

/// What the picker reads besides the run state, at the commit the project shows.
struct Loaded {
    graph: bongos_lanes_model::PickGraph,
    table: bongos_lanes_model::LaneTable,
    options: PickOptions,
}

async fn load(location: &Location, waiting_limit: usize) -> Result<Loaded> {
    let repo = read_only_repo(location)?;
    let commit = repo.shown_commit().await?.0;
    let tree = repo
        .load_work_tree_at(&TreeRef::Commit(CommitId(commit.clone())))
        .await?;
    let project = tree
        .projects
        .first()
        .context("the project has no top group")?;
    let graph = from_work_tree(&tree, &project.slug, &LiveFacts::default())
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
    match view {
        Pick::DryRun => {
            let mut dry = explain(&loaded.graph, state, &loaded.table, &loaded.options);
            if let Some(lane) = &args.lane {
                dry.per_lane.retain(|(id, _)| id == lane);
            }
            if args.json {
                Ok(serde_json::to_string_pretty(&dry)?)
            } else {
                Ok(dry_run_text(&dry))
            }
        }
        Pick::One => {
            let lane = args.lane.clone().context("name a lane (--lane build-2)")?;
            let outcome = pick(&loaded.graph, state, &loaded.table, &lane, &loaded.options);
            if args.json {
                Ok(serde_json::to_string_pretty(
                    &json!({ "lane": lane.to_string(), "outcome": outcome }),
                )?)
            } else {
                Ok(outcome_text(&lane, &outcome))
            }
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
