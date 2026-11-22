//! Opening a project's state as the first process or as a client: the first process takes the
//! writer lock; any later one gets `HeldBy`, reads the store through `open_reader`, and attaches
//! to the writer named by a live `host.json`. A client never opens the writer.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use bongos_project_repo::{
    Location, OpenError, StateApplyHooks, StateReader, StateStore, StateWriter,
};
use gpui::BackgroundExecutor;

use crate::client::{ClientOptions, HostClient};
use crate::host_file::{HostInfo, live_host};

/// A project's state as this process opened it.
pub enum OpenedState {
    /// This process took the writer: it writes in-process and announces itself
    /// (`WriterHost::start`).
    Writer(Arc<StateWriter>),
    /// Another process holds the writer: reads go to `reader`, writes through `client`.
    Client {
        client: HostClient,
        reader: StateReader,
        host: HostInfo,
    },
}

/// Opens `location`'s state, with the state apply hooks registered globally.
pub fn open_state(
    location: &Location,
    options: ClientOptions,
    executor: BackgroundExecutor,
) -> Result<OpenedState> {
    open_state_with_hooks(location, StateApplyHooks::global(), options, executor)
}

/// [`open_state`] whose writer, if this process takes it, runs `hooks`.
pub fn open_state_with_hooks(
    location: &Location,
    hooks: &StateApplyHooks,
    options: ClientOptions,
    executor: BackgroundExecutor,
) -> Result<OpenedState> {
    match StateStore::open_writer_with_hooks(location, hooks) {
        Ok(writer) => Ok(OpenedState::Writer(Arc::new(writer))),
        Err(OpenError::HeldBy { pid }) => {
            let held_by = pid.map_or_else(|| "pid unknown".to_string(), |pid| format!("pid {pid}"));
            let host = live_host(location).ok_or_else(|| {
                anyhow!(
                    "this project is open in another bongos process ({held_by}); work there or close it"
                )
            })?;
            let socket = host.socket.clone().ok_or_else(|| {
                anyhow!(
                    "this project is open in another bongos process ({held_by}) with lanes off; \
                     work there or close it"
                )
            })?;
            let client = HostClient::connect(&socket, options, executor)?;
            let reader = StateStore::open_reader(location)?;
            Ok(OpenedState::Client {
                client,
                reader,
                host,
            })
        }
        Err(OpenError::Store(error)) => Err(error),
    }
}
