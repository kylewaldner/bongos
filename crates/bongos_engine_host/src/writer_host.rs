//! The process that holds a project's writer announces itself in `host.json` and, while lanes
//! are on, serves the project's socket. A window that opened the project first is the writer in
//! window mode; with `lanes.enabled` off it serves no socket, so another process gets `HeldBy`
//! and cannot reach it.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use bongos_project_repo::ProjectRepo;
use chrono::Utc;
use gpui::App;

use crate::host_file::{HostInfo, read_host_file, write_host_file};
use crate::protocol::HostMode;
use crate::server::EngineServer;
use crate::socket::{bind_socket, default_hosts_dir, socket_path};

/// How the writer announces itself.
#[derive(Clone, Debug)]
pub struct WriterOptions {
    pub mode: HostMode,
    /// The version clients must run to be served.
    pub bongos_version: String,
    /// Where the socket goes: [`default_hosts_dir`] outside tests.
    pub hosts_dir: PathBuf,
    /// Whether to serve the socket: a window passes `lanes.enabled`, a host process `true`.
    pub serve_socket: bool,
}

impl WriterOptions {
    /// A window's options: it serves the socket only while `lanes_enabled`.
    pub fn window(bongos_version: impl Into<String>, lanes_enabled: bool) -> Self {
        Self {
            mode: HostMode::Window,
            bongos_version: bongos_version.into(),
            hosts_dir: default_hosts_dir(),
            serve_socket: lanes_enabled,
        }
    }
}

/// This process as a project's writer. Dropping it stops the server and removes `host.json`
/// when it still names this process.
pub struct WriterHost {
    repo: ProjectRepo,
    options: WriterOptions,
    info: HostInfo,
    server: Option<EngineServer>,
}

impl WriterHost {
    /// Writes `host.json` for `repo`, whose writer this process holds, and serves the socket
    /// when `options.serve_socket`. The socket is bound before `host.json` names it.
    pub fn start(repo: ProjectRepo, options: WriterOptions, cx: &mut App) -> Result<Self> {
        repo.state()
            .and_then(|state| state.writer())
            .context("only the process that holds a project's writer announces it")?;
        let mut host = Self {
            info: HostInfo::this_process(options.mode, None, &options.bongos_version, Utc::now()),
            repo,
            options,
            server: None,
        };
        let serve = host.options.serve_socket;
        host.announce(serve, cx)?;
        Ok(host)
    }

    pub fn info(&self) -> &HostInfo {
        &self.info
    }

    pub fn server(&self) -> Option<&EngineServer> {
        self.server.as_ref()
    }

    pub fn is_serving(&self) -> bool {
        self.server.is_some()
    }

    /// Starts or stops serving the socket, as `lanes.enabled` turns on or off, and rewrites
    /// `host.json` to match.
    pub fn set_serving(&mut self, serve: bool, cx: &mut App) -> Result<()> {
        if serve == self.is_serving() {
            return Ok(());
        }
        self.options.serve_socket = serve;
        self.announce(serve, cx)
    }

    fn announce(&mut self, serve: bool, cx: &mut App) -> Result<()> {
        // Stopped first, so its socket file is gone before a new one is bound.
        self.server = None;
        let location = self.repo.location().clone();
        let listener = if serve {
            let path = socket_path(&self.options.hosts_dir, &location.id);
            Some((bind_socket(&path)?, path))
        } else {
            None
        };
        self.info.socket = listener.as_ref().map(|(_, path)| path.clone());
        write_host_file(&location, &self.info)?;
        if let Some((listener, _)) = listener {
            self.server = Some(EngineServer::start(
                listener,
                self.repo.clone(),
                self.info.clone(),
                cx,
            )?);
        }
        Ok(())
    }
}

impl Drop for WriterHost {
    fn drop(&mut self) {
        self.server = None;
        let location = self.repo.location();
        if read_host_file(location).is_some_and(|info| {
            info.pid == self.info.pid && info.start_time == self.info.start_time
        }) {
            std::fs::remove_file(crate::host_file::host_file_path(location)).ok();
        }
    }
}
