//! The read modes of the app binary: `bongos --engine-host <project id> --read <name> [args...]`
//! opens the project's store for reading only (never the writer), runs the mode registered under
//! `<name>`, prints what it returns and exits. `bongos lanes status` with no running lanes and
//! `bongos profile ...` go through it. Features register their modes with [`ReadModes::register`]
//! (answers at once) or [`ReadModes::register_async`] (the answer is awaited before it is
//! printed), the same way commands for a writer are registered with `HostCommands::register`.

use std::collections::HashMap;

use anyhow::Result;
use bongos_project_repo::{Location, StateReader};
use gpui::{App, Global, Task};

/// Runs a read mode: the arguments after its name, the project's read-only store and the app.
pub type ReadHandler = fn(&[String], &StateReader, &mut App) -> Result<String>;

/// Runs a read mode whose answer takes a while. The reader is only borrowed for the call, so a
/// handler reads what it needs from it before it returns the task.
pub type AsyncReadHandler = fn(&[String], &StateReader, &mut App) -> Task<Result<String>>;

#[derive(Clone, Copy)]
enum Mode {
    Now(ReadHandler),
    Later(AsyncReadHandler),
}

/// The read modes this process serves, by name.
#[derive(Default)]
pub struct ReadModes {
    modes: HashMap<String, Mode>,
}

impl Global for ReadModes {}

impl ReadModes {
    /// Serves the read mode `namespace` with `handler`, replacing an earlier one.
    pub fn register(namespace: &str, handler: ReadHandler, cx: &mut App) {
        cx.default_global::<Self>()
            .modes
            .insert(namespace.to_string(), Mode::Now(handler));
    }

    /// Serves the read mode `namespace` with `handler`, whose task is awaited before the output
    /// is printed.
    pub fn register_async(namespace: &str, handler: AsyncReadHandler, cx: &mut App) {
        cx.default_global::<Self>()
            .modes
            .insert(namespace.to_string(), Mode::Later(handler));
    }

    pub fn is_registered(name: &str, cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|modes| modes.modes.contains_key(name))
    }

    /// Runs the mode `name` with `args`; `None` when nothing is registered under that name.
    pub fn run(
        name: &str,
        args: &[String],
        reader: &StateReader,
        cx: &mut App,
    ) -> Option<Task<Result<String>>> {
        let mode = *cx.try_global::<Self>()?.modes.get(name)?;
        Some(match mode {
            Mode::Now(handler) => Task::ready(handler(args, reader, cx)),
            Mode::Later(handler) => handler(args, reader, cx),
        })
    }
}

/// The location of the project a read mode runs for. The app sets it before it runs the mode,
/// so a mode that needs the project's repository (not only its store) can open it.
pub struct ReadProject(pub Location);

impl Global for ReadProject {}

impl ReadProject {
    pub fn set(location: Location, cx: &mut App) {
        cx.set_global(Self(location));
    }

    pub fn get(cx: &App) -> Option<Location> {
        cx.try_global::<Self>().map(|project| project.0.clone())
    }
}
