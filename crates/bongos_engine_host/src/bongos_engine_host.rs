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
mod host_file;
pub mod protocol;
pub mod server;
mod socket;
mod window;
mod workflow;
mod writer_host;

pub use access::{EngineAccess, EngineAccesses};
pub use attach::{OpenedState, open_state, open_state_with_hooks};
pub use checks::{checks_handler, register_checks};
pub use client::{ANSWER_TIMEOUT, ClientOptions, HostClient, Welcomed, not_answering};
pub use commands::{HostCommands, HostHandler, VerbHandler, local_identity};
pub use host_file::{
    HOST_FILE, HostInfo, Moving, host_file_path, live_host, process_is, read_host_file,
    write_host_file,
};
pub use protocol::{
    ClientMessage, HostCommand, HostCtl, HostEvent, HostMode, HostResult, HostTopic, LanesVerb,
    PROTOCOL_VERSION, ServerMessage, WorkflowVerb, version_refusal,
};
pub use server::EngineServer;
pub use socket::{bind_socket, default_hosts_dir, socket_path};
pub use window::{WindowHosting, WriterHosts, install_window};
pub use workflow::{install_workflow_remote, register_workflow};
pub use writer_host::{WriterHost, WriterOptions};

/// Registers the commands this feature serves in the writer (task checks' `checks`, the
/// workflow engine's commands) and routes this process's workflow commands to a writer elsewhere.
pub fn init(cx: &mut gpui::App) {
    register_checks(cx);
    register_workflow(cx);
    install_workflow_remote(cx);
}

#[cfg(test)]
mod engine_host_tests;
