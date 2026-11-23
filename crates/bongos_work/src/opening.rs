//! How this process opens a project's state: by default it takes the project's writer, but the
//! app may install an opener that attaches to another process's writer instead (a second window
//! of the same project), and a hook that runs once the project is served.

use std::rc::Rc;
use std::sync::Arc;

use anyhow::Result;
use bongos_project_repo::{
    LoadedProject, Location, StateEvent, StateReader, StateSink, StateStore, StateWriter,
};
use futures::channel::mpsc;
use gpui::{App, Global};

/// A project's state as an opener opened it.
pub enum OpenedProject {
    /// This process holds the project's writer.
    Writer(Arc<StateWriter>),
    /// Another process holds it: reads go to `reader`, writes through `sink`, and `changes`
    /// carries what the writer reports as changed.
    Client {
        reader: StateReader,
        sink: Arc<dyn StateSink>,
        changes: Option<mpsc::UnboundedReceiver<StateEvent>>,
    },
}

/// Opens the state of the project at a location. Runs on a background thread.
pub type ProjectOpener = Arc<dyn Fn(&Location) -> Result<OpenedProject> + Send + Sync>;

/// Runs once a project's repository is served in this process, with how it was opened.
pub type ProjectServed = Rc<dyn Fn(&Arc<LoadedProject>, &mut App)>;

/// The app's way of opening and then serving a project's state.
#[derive(Clone)]
pub struct ProjectOpening {
    pub opener: ProjectOpener,
    pub served: Option<ProjectServed>,
}

impl Global for ProjectOpening {}

impl ProjectOpening {
    /// Makes this the way every project opens from now on.
    pub fn install(opener: ProjectOpener, served: Option<ProjectServed>, cx: &mut App) {
        cx.set_global(Self { opener, served });
    }

    pub(crate) fn opener(cx: &App) -> Option<ProjectOpener> {
        cx.try_global::<Self>()
            .map(|opening| opening.opener.clone())
    }

    pub(crate) fn served(loaded: &Arc<LoadedProject>, cx: &mut App) {
        let served = cx
            .try_global::<Self>()
            .and_then(|opening| opening.served.clone());
        if let Some(served) = served {
            served(loaded, cx);
        }
    }
}

/// Opens `location` as the process's writer.
pub(crate) fn open_as_writer(location: &Location) -> Result<OpenedProject> {
    Ok(OpenedProject::Writer(Arc::new(StateStore::open_writer(
        location,
    )?)))
}
