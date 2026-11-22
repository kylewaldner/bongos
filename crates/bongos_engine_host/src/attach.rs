//! Opening a project's state as the first process or as a client: the first process takes the
//! writer lock; any later one gets `HeldBy`, reads the store through `open_reader`, and attaches
//! to the writer named by a live `host.json`. A client never opens the writer.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, anyhow};
use bongos_project_repo::{Location, OpenError, StateApplyHooks, StateReader, StateStore, StateWriter};
use gpui::BackgroundExecutor;

use crate::client::{ClientOptions, HostClient};
use crate::ensure::POLL_INTERVAL;
use crate::handover::take_writer_checked;
use crate::host_file::{HostInfo, live_host, move_in_progress, read_host_file};

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
///
/// While `host.json` records a move of the writer in progress (a handover), the lock is not
/// tried: this waits for the move to end, up to [`ClientOptions::moving_timeout`], and then
/// attaches to the new writer. An opener that won the lock anyway (it read `host.json` before the
/// move was written) lets it go at once and waits like the others ([`take_writer_checked`]). A
/// stale move, with both processes dead, is ignored. Blocks while it waits.
pub fn open_state_with_hooks(
    location: &Location,
    hooks: &StateApplyHooks,
    options: ClientOptions,
    executor: BackgroundExecutor,
) -> Result<OpenedState> {
    let deadline = Instant::now() + options.moving_timeout;
    loop {
        let moving = read_host_file(location).is_some_and(|info| {
            move_in_progress(&info).is_some_and(|moving| moving.to_pid != options.own_pid)
        });
        let held_by = if moving {
            None
        } else {
            match take_writer_checked(location, hooks, options.own_pid) {
                Ok(Some(writer)) => return Ok(OpenedState::Writer(Arc::new(writer))),
                // Won the lock during a move: let go and wait like the others.
                Ok(None) => None,
                Err(OpenError::HeldBy { pid }) => Some(pid),
                Err(OpenError::Store(error)) => return Err(error),
            }
        };
        let still_moving = read_host_file(location)
            .is_some_and(|info| move_in_progress(&info).is_some());
        match held_by {
            Some(pid) if !still_moving => {
                return attach(location, pid, options, executor);
            }
            _ if Instant::now() >= deadline => {
                // The move did not end in time: the live writer `host.json` names, if any.
                return attach(location, held_by.flatten(), options, executor);
            }
            _ => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Attaches as a client to the live writer `host.json` names.
fn attach(
    location: &Location,
    pid: Option<u32>,
    options: ClientOptions,
    executor: BackgroundExecutor,
) -> Result<OpenedState> {
    let held_by = pid.map_or_else(|| "pid unknown".to_string(), |pid| format!("pid {pid}"));
    let host = live_host(location).ok_or_else(|| {
        anyhow!("this project is open in another bongos process ({held_by}); work there or close it")
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
