//! Seams other crates fill so new-format projects (kept in project repositories) show and are
//! written beside the old-format tree the store reads from disk. None changes anything while
//! nothing is registered.
//!
//! [`ProjectSources`] holds sources of loaded projects, which the store merges into its snapshot.
//! [`DefinitionWriters`] holds writers that take a command's change instead of the disk writer
//! when they own it (any task it writes is in their project).
//!
//! Both are read by the store's session on the background executor, so sources and writers are
//! `Send + Sync` and take no app context.

use std::sync::Arc;

use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::{Change, Context};
use bongos_work_model::store::{Project, WorkTree};
use gpui::{App, Global};
use parking_lot::Mutex;

use crate::{WorkStore, WriteError};

/// The id of a change waiting to land.
pub type ChangeId = String;

/// What a write did.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteOutcome {
    /// The change is written and the snapshot shows it.
    Applied(Change),
    /// The change waits to land; the snapshot shows it as pending.
    Landing { change: Change, change_id: ChangeId },
}

impl WriteOutcome {
    pub fn change(&self) -> &Change {
        match self {
            WriteOutcome::Applied(change) | WriteOutcome::Landing { change, .. } => change,
        }
    }

    pub fn into_change(self) -> Change {
        match self {
            WriteOutcome::Applied(change) | WriteOutcome::Landing { change, .. } => change,
        }
    }
}

/// A source of loaded new-format projects.
pub trait ProjectSource: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// The projects as they are now. Called on the background executor on every reload.
    fn projects(&self) -> Vec<Project>;
}

/// The registered project sources. Cloning shares the registry.
#[derive(Clone, Default)]
pub struct ProjectSources(Arc<Mutex<Vec<Arc<dyn ProjectSource>>>>);

impl Global for ProjectSources {}

impl ProjectSources {
    /// The app's registry.
    pub fn global(cx: &mut App) -> Self {
        cx.default_global::<Self>().clone()
    }

    /// Adds `source`, replacing one of the same name, and refreshes the store.
    pub fn register(source: Arc<dyn ProjectSource>, cx: &mut App) {
        let sources = Self::global(cx);
        {
            let mut list = sources.0.lock();
            list.retain(|existing| existing.name() != source.name());
            list.push(source);
        }
        refresh(cx);
    }

    /// Removes the source `name` and refreshes the store.
    pub fn remove(name: &str, cx: &mut App) {
        Self::global(cx)
            .0
            .lock()
            .retain(|existing| existing.name() != name);
        refresh(cx);
    }

    pub fn is_empty(&self) -> bool {
        self.0.lock().is_empty()
    }

    /// Every source's projects, in registration order.
    pub(crate) fn projects(&self) -> Vec<Project> {
        let sources: Vec<_> = self.0.lock().clone();
        sources
            .iter()
            .flat_map(|source| source.projects())
            .collect()
    }
}

/// A writer of changes in new-format projects.
pub trait DefinitionWriter: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// Whether `change`, computed on `tree`, writes anything this writer is responsible for.
    fn owns(&self, tree: &WorkTree, change: &Change) -> bool;

    /// Writes `change`. `attestation` is the person marker the write's context held, moved out
    /// before the command ran, for the approval of a landing. Called on the background executor.
    fn write(
        &self,
        tree: &WorkTree,
        ctx: &Context,
        attestation: Option<PersonAtKeyboard>,
        change: Change,
    ) -> Result<WriteOutcome, WriteError>;
}

/// The registered definition writers. Cloning shares the registry.
#[derive(Clone, Default)]
pub struct DefinitionWriters(Arc<Mutex<Vec<Arc<dyn DefinitionWriter>>>>);

impl Global for DefinitionWriters {}

impl DefinitionWriters {
    /// The app's registry.
    pub fn global(cx: &mut App) -> Self {
        cx.default_global::<Self>().clone()
    }

    /// Adds `writer`, replacing one of the same name.
    pub fn register(writer: Arc<dyn DefinitionWriter>, cx: &mut App) {
        let writers = Self::global(cx);
        let mut list = writers.0.lock();
        list.retain(|existing| existing.name() != writer.name());
        list.push(writer);
    }

    pub fn remove(name: &str, cx: &mut App) {
        Self::global(cx)
            .0
            .lock()
            .retain(|existing| existing.name() != name);
    }

    /// The first writer that owns `change`.
    pub(crate) fn owner(&self, tree: &WorkTree, change: &Change) -> Option<Arc<dyn DefinitionWriter>> {
        let writers: Vec<_> = self.0.lock().clone();
        writers
            .into_iter()
            .find(|writer| writer.owns(tree, change))
    }
}

fn refresh(cx: &mut App) {
    if let Some(store) = WorkStore::global(cx) {
        store.update(cx, |store, cx| store.refresh_sources(cx));
    }
}
