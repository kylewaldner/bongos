//! The process that holds a project's writer announces itself in `host.json` and, while lanes
//! are on, serves the project's socket. A window that opened the project first is the writer in
//! window mode; with `lanes.enabled` off it serves no socket, so another process gets `HeldBy`
//! and cannot reach it.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use bongos_project_repo::ProjectRepo;
use chrono::Utc;
use gpui::App;

use crate::host_file::{HostInfo, Moving, read_host_file, write_host_file};
use crate::participants::WriterParticipants;
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
    /// Set once a handover began: `host.json` then belongs to the move and to the next writer,
    /// so dropping this host leaves it alone.
    moved: bool,
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
            moved: false,
        };
        let serve = host.options.serve_socket;
        host.announce(serve, cx)?;
        // This process is the project's writer from now on (a window that opened it first, or
        // the engine host right after taking the lock): the writer participants begin here.
        let id = host.repo.location().id.clone();
        WriterParticipants::started(&id, cx);
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

    pub fn repo(&self) -> &ProjectRepo {
        &self.repo
    }

    /// Records in `host.json` that the writer moves to another process (`Some`), or that it
    /// stays here after all (`None`, a take-back). Written atomically, before anything of the
    /// move happens.
    pub fn set_moving(&mut self, moving: Option<Moving>) -> Result<()> {
        if moving.is_some() {
            self.moved = true;
        }
        self.info.moving = moving;
        write_host_file(self.repo.location(), &self.info)
    }

    /// Stops taking new clients and removes the socket, for the next writer to bind; the
    /// clients already connected stay until this host is dropped.
    pub fn stop_accepting(&self) {
        if let Some(server) = &self.server {
            server.stop_accepting();
        }
    }

    /// Serves again after a handover the other process never finished: `host.json` names this
    /// process with no move, and the socket is bound again when it was served.
    pub fn take_back(&mut self, cx: &mut App) -> Result<()> {
        self.info.moving = None;
        self.moved = false;
        let serve = self.options.serve_socket;
        self.announce(serve, cx)
    }

    /// Starts or stops serving the socket, as `lanes.enabled` turns on or off, and rewrites
    /// `host.json` to match.
    pub fn set_serving(&mut self, serve: bool, cx: &mut App) -> Result<()> {
        if self.moved || serve == self.is_serving() {
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
        if self.moved {
            return;
        }
        let location = self.repo.location();
        if read_host_file(location).is_some_and(|info| {
            info.pid == self.info.pid && info.start_time == self.info.start_time
        }) {
            std::fs::remove_file(crate::host_file::host_file_path(location)).ok();
        }
    }
}
