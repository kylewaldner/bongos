//! The engine host: one process per project holds the state writer (a window, or a separate
//! host process) and serves the project's Unix socket, so other windows and the command line
//! reach the engines there. A second process attaches as a client: it reads the store itself and
//! sends every write through the socket, where the server writes only through the project's
//! host boundary.

mod access;
mod attach;
mod checks;
mod client;
mod commands;
mod ensure;
mod handover;
mod host_file;
mod host_process;
mod participants;
pub mod protocol;
mod read_modes;
pub mod server;
mod socket;
mod window;
mod workflow;
mod writer_host;

pub use access::{EngineAccess, EngineAccesses};
pub use attach::{OpenedState, open_state, open_state_with_hooks};
pub use checks::{checks_handler, register_checks};
pub use client::{
    ANSWER_TIMEOUT, ClientOptions, HostClient, MOVING_RETRY, MOVING_TIMEOUT, Welcomed,
    not_answering,
};
pub use commands::{
    HostCommands, HostHandler, STUB_NAMESPACES, VerbHandler, local_identity, register_stub_commands,
};
pub use ensure::{
    Attach, ENGINE_HOST_FLAG, EnsureHost, HostSpawner, POLL_INTERVAL, ProgramSpawner, SpawnedHost,
    ensure_host,
};
pub use handover::{
    CANCELLED, DrainChoice, DrainPrompt, EngineRuns, HandoverRequest, HandoverRuns, HandoverTimes,
    HandoverWatch, MOVING_REFUSAL, QueueSink, StartTimeoutPassed, TAKE_BACK_WARN_EVERY,
    did_not_start, expect_handover, install_handovers, is_moving, set_process_check,
    set_start_timeout_check, set_take_back_warn_every, start_handover, take_writer_checked,
    take_writer_checked_by, take_writer_from_window,
};
pub use host_file::{
    Alive, HOST_FILE, HostInfo, Moving, ProcessCheck, host_file_path, live_host, move_in_progress,
    move_in_progress_by, process_is, read_host_file, system_process_check, write_host_file,
};
pub use host_process::{
    BUSY_RECHECK, EngineHostProcess, HostBusy, HostDrain, HostProcessOptions, held_by_message,
    host_is_busy, open_host_writer, stop_serving, take_host_writer, wait_until_idle,
};
pub use participants::{WriterParticipant, WriterParticipants};
pub use protocol::{
    ClientMessage, HostCommand, HostCtl, HostEvent, HostMode, HostResult, HostTopic, LanesVerb,
    PROTOCOL_VERSION, ServerMessage, WorkflowVerb, version_refusal,
};
pub use read_modes::{AsyncReadHandler, ReadHandler, ReadModes, ReadProject};
pub use server::EngineServer;
pub use socket::{
    bind_socket, default_host_log_path, default_hosts_dir, host_log_path, project_hex, socket_path,
};
pub use window::{WindowHosting, WriterHosts, install_window};
pub use workflow::{install_workflow_remote, register_workflow};
pub use writer_host::{WriterHost, WriterOptions};

/// Registers the commands this feature serves in the writer (task checks' `checks`, the
/// workflow engine's commands) and routes this process's workflow commands to a writer elsewhere.
pub fn init(cx: &mut gpui::App) {
    register_checks(cx);
    register_workflow(cx);
    install_workflow_remote(cx);
    register_stub_commands(cx);
}

#[cfg(test)]
mod engine_host_tests;
#[cfg(test)]
mod handover_tests;
#[cfg(test)]
mod read_modes_tests;
