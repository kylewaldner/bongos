//! Where a project's socket lives and how the writer binds it: `<state dir>/hosts/<12 hex>.sock`
//! (short enough for the 108-byte limit of a socket path), the directory mode 0700 and the
//! socket 0600. A stale socket left by a writer that died is removed before binding.

use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use sha2::{Digest as _, Sha256};

/// The directory of every project's socket on this machine.
pub fn default_hosts_dir() -> PathBuf {
    paths::state_dir().join("hosts")
}

/// The socket of project `project_id` in `hosts_dir`: the first 12 hex digits of the SHA-256 of
/// the id, so any id gives a short path.
pub fn socket_path(hosts_dir: &Path, project_id: &str) -> PathBuf {
    hosts_dir.join(format!("{}.sock", project_hex(project_id)))
}

/// The first 12 hex digits of the SHA-256 of `project_id`, which name its socket and its engine
/// host's log.
pub fn project_hex(project_id: &str) -> String {
    let digest = Sha256::digest(project_id.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex[..12].to_string()
}

/// The engine host's log of project `project_id` in this machine's logs folder.
pub fn default_host_log_path(project_id: &str) -> PathBuf {
    host_log_path(paths::logs_dir(), project_id)
}

/// The engine host's log of project `project_id`: `<logs_dir>/engine-host-<12 hex>.log`.
pub fn host_log_path(logs_dir: &Path, project_id: &str) -> PathBuf {
    logs_dir.join(format!("engine-host-{}.log", project_hex(project_id)))
}

/// Binds the socket at `path`. A socket file nobody answers on is a dead writer's and is
/// removed; one a live process answers on is refused.
pub fn bind_socket(path: &Path) -> Result<UnixListener> {
    let directory = path.parent().context("the socket path has no directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restricting {}", directory.display()))?;
    if std::fs::symlink_metadata(path).is_ok() {
        if UnixStream::connect(path).is_ok() {
            bail!("another process serves {}", path.display());
        }
        log::info!("removing the stale socket {}", path.display());
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", path.display()))?;
    Ok(listener)
}
