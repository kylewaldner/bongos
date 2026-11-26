//! The writer participant of the pools (48 D19): when this process becomes a project's writer it
//! opens the project's broker (the project store on the writer, the machine store, the pools of
//! the verified commit's `pools/pools.toml`, guard scripts from a pinned copy of it) and serves it
//! to the checks; in a handover it drains the broker, and on a take-back it undrains and starts it
//! again, asking what the drain gave up under new tickets.

use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use bongos_engine_host::WriterParticipant;
use bongos_pools_model::{PoolScope, parse_pools};
use bongos_project_repo::ProjectRepo;
use bongos_project_repo_model::layout::{DEFINITIONS_DIR, PROJECT_FILE};
use bongos_work::OpenProjectRepos;
use gpui::{App, Task};
use settings::Settings as _;

use crate::broker::{BrokerParts, BrokerTiming, MachineStores, PoolBroker};
use crate::checks::PoolsForChecks;
use crate::guards::SystemGuardReaders;
use crate::{
    DeclDefaults, POOLS_FILE, PoolConfig, PoolsSettings, ProjectRef, ProjectStore, StoreContext,
};

/// Opens, drains and restarts the broker of each project this process holds the writer of.
pub struct PoolsParticipant {
    checks: Rc<PoolsForChecks>,
}

impl PoolsParticipant {
    pub fn new(checks: Rc<PoolsForChecks>) -> PoolsParticipant {
        PoolsParticipant { checks }
    }
}

impl WriterParticipant for PoolsParticipant {
    fn started(&self, project: &str, cx: &mut App) {
        if let Some(broker) = self.checks.broker(project) {
            broker.started();
            return;
        }
        let Some(repo) = OpenProjectRepos::listed(cx)
            .into_iter()
            .map(|loaded| loaded.repo().clone())
            .find(|repo| repo.location().id == project)
        else {
            log::info!("pools: no open repository for project {project}; no broker");
            return;
        };
        let settings = PoolsSettings::get_global(cx).clone();
        let checks = self.checks.clone();
        let foreground = cx.foreground_executor().clone();
        let background = cx.background_executor().clone();
        let project = project.to_string();
        cx.foreground_executor()
            .spawn(async move {
                match open_broker(repo, settings, foreground, background).await {
                    Ok(broker) => checks.insert(broker),
                    Err(error) => log::error!("pools: opening the pools of {project}: {error:#}"),
                }
            })
            .detach();
    }

    fn drain(&self, project: &str, _cx: &mut App) -> Task<()> {
        match self.checks.broker(project) {
            Some(broker) => broker.drain(),
            None => Task::ready(()),
        }
    }

    fn undrain(&self, project: &str, _cx: &mut App) -> Result<()> {
        if let Some(broker) = self.checks.broker(project) {
            broker.undrain();
        }
        Ok(())
    }
}

/// The project's title as its `project.toml` gives it.
fn title(definitions: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(definitions.join(PROJECT_FILE)).ok()?;
    let table: toml::Table = text.parse().ok()?;
    table.get("title")?.as_str().map(str::to_string)
}

async fn open_broker(
    repo: ProjectRepo,
    settings: PoolsSettings,
    foreground: gpui::ForegroundExecutor,
    background: gpui::BackgroundExecutor,
) -> Result<Rc<PoolBroker>> {
    let writer = repo
        .state()
        .and_then(|state| state.writer())
        .cloned()
        .context("this process does not hold the project's writer")?;
    let context = StoreContext::this_process().context("this process's start time is unknown")?;
    let verified = repo.verified_commit()?;
    let pin = match &verified {
        Some(commit) => Some(repo.materialize(&commit.0).await?),
        None => None,
    };
    let definitions = pin.as_ref().map(|pin| pin.path().join(DEFINITIONS_DIR));
    let text = definitions
        .as_ref()
        .and_then(|dir| std::fs::read_to_string(dir.join(POOLS_FILE)).ok())
        .unwrap_or_default();
    let decls = parse_pools(&text)
        .map(|parsed| parsed.decls)
        .unwrap_or_default();
    let location = repo.location().clone();
    let project = ProjectRef {
        id: location.id.clone(),
        name: definitions
            .as_deref()
            .and_then(title)
            .unwrap_or_else(|| location.id.clone()),
    };
    let store_context = context.clone();
    let (project_store, pruned) = background
        .spawn(async move { ProjectStore::open(writer, store_context) })
        .await?;
    for (pool, hold) in &pruned.holds {
        log::info!(
            "pools: released {pool}: holder {} exited",
            hold.holder.label.0
        );
    }
    let machine = MachineStores::from_settings(&settings, context, &location.id)?;
    let defaults = DeclDefaults::from_settings(&settings);
    let project_configs = PoolConfig::of_scope(&decls, PoolScope::Project, |_| None, defaults);
    let machine_configs = PoolConfig::of_scope(&decls, PoolScope::Machine, |_| None, defaults);
    let broker = Rc::new(PoolBroker::new(BrokerParts {
        project,
        project_store: Arc::new(project_store),
        machine: Some(machine),
        readers: Arc::new(SystemGuardReaders),
        definitions,
        disk_root: location.folder.clone(),
        timing: BrokerTiming::from_settings(&settings),
        foreground,
        background,
        keep: pin.map(|pin| Box::new(pin) as Box<dyn std::any::Any>),
    }));
    broker.declare(project_configs, machine_configs).await?;
    Ok(broker)
}
