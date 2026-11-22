//! `state/host.json`: which process holds a project's writer, written atomically by that process
//! right after it took the writer lock. It names a live writer only while its pid runs with the
//! start time it recorded; a file naming a dead pid or another start time is never live.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use bongos_project_repo::Location;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::protocol::HostMode;

/// The file's name in a project's state part.
pub const HOST_FILE: &str = "host.json";

/// Set while the writer moves to another process (the handover).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moving {
    pub to_pid: u32,
}

/// The contents of `host.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub pid: u32,
    /// The writer's start time as the system reports it (clock ticks since boot on Linux);
    /// `None` where the system reports none.
    pub start_time: Option<u64>,
    /// The socket the writer serves, `None` when it serves none (lanes off in a window).
    pub socket: Option<PathBuf>,
    pub bongos_version: String,
    pub mode: HostMode,
    pub started: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moving: Option<Moving>,
}

impl HostInfo {
    /// The record of this process as the writer.
    pub fn this_process(
        mode: HostMode,
        socket: Option<PathBuf>,
        bongos_version: &str,
        started: DateTime<Utc>,
    ) -> Self {
        let pid = std::process::id();
        Self {
            pid,
            start_time: bongos_workflow::process::start_time(pid),
            socket,
            bongos_version: bongos_version.to_string(),
            mode,
            started,
            moving: None,
        }
    }

    /// Whether the process it names is running now with the start time it recorded.
    pub fn is_live(&self) -> bool {
        process_is(self.pid, self.start_time)
    }
}

/// Whether `pid` runs now with `start_time`. Where the system reports no start time, a running
/// pid with none recorded counts.
pub(crate) fn process_is(pid: u32, start_time: Option<u64>) -> bool {
    if pid == 0 {
        return false;
    }
    match bongos_workflow::process::start_time(pid) {
        Some(now) => start_time == Some(now),
        None if cfg!(target_os = "linux") => false,
        None => start_time.is_none() && pid_exists(pid),
    }
}

fn pid_exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The path of `host.json` for `location`.
pub fn host_file_path(location: &Location) -> PathBuf {
    location.state.join(HOST_FILE)
}

/// Writes `info` to `host.json` atomically: a temporary file in the same directory, flushed to
/// disk, then renamed over the old one. Only the writer calls it, after taking the lock.
pub fn write_host_file(location: &Location, info: &HostInfo) -> Result<()> {
    let path = host_file_path(location);
    write_atomically(&path, &serde_json::to_vec_pretty(info)?)
        .with_context(|| format!("writing {}", path.display()))
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().context("host.json has no directory")?;
    let temporary = directory.join(format!(".{HOST_FILE}.{}.tmp", std::process::id()));
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// `host.json` of `location` as written, `None` when there is none or it cannot be read.
pub fn read_host_file(location: &Location) -> Option<HostInfo> {
    let bytes = std::fs::read(host_file_path(location)).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(info) => Some(info),
        Err(error) => {
            log::warn!("reading host.json of {}: {error}", location.id);
            None
        }
    }
}

/// `host.json` of `location` when it names a live writer.
pub fn live_host(location: &Location) -> Option<HostInfo> {
    read_host_file(location).filter(HostInfo::is_live)
}
