//! Who is on the other end of a project's local socket: the peer's process id, read once when
//! the connection is accepted (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS), what kind of
//! client it says it is, and whether it runs the same executable file as this process. Only a
//! window running the same executable may carry a person's claim (`HostBoundary`).
//!
//! The limits, accepted for now: a pid can be reused between the connect and the lookup (read
//! once, at accept, which narrows but does not close it); an upgraded host and an older window
//! differ, and the window's claims are then not trusted; and any program of the same user can
//! run the bongos executable, so this proves "a bongos process", not "a person".

use std::io;
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

/// What a client of the project's socket says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientKind {
    /// A bongos window.
    Window,
    /// The command-line client, which has no way to send a person's claim.
    Cli,
}

/// The peer of one accepted connection. Built only by [`peer_identity`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    pid: u32,
    client: ClientKind,
    same_executable: bool,
}

impl PeerIdentity {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn client(&self) -> ClientKind {
        self.client
    }

    pub fn same_executable(&self) -> bool {
        self.same_executable
    }

    /// Whether the peer's person claims are trusted: a window running this executable.
    pub fn may_attest_person(&self) -> bool {
        self.client == ClientKind::Window && self.same_executable
    }

    #[cfg(test)]
    pub(crate) fn for_test(pid: u32, client: ClientKind, same_executable: bool) -> Self {
        Self {
            pid,
            client,
            same_executable,
        }
    }
}

/// The identity of the peer of `stream`, read now. `client` is what the peer said it is.
pub fn peer_identity(stream: &UnixStream, client: ClientKind) -> io::Result<PeerIdentity> {
    let pid = peer_pid(stream)?;
    Ok(PeerIdentity {
        pid,
        client,
        same_executable: pid != 0 && same_executable(pid),
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd as _;

    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the descriptor is open for the call, and the buffer and its length describe
    // `credentials`.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    u32::try_from(credentials.pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the peer has no pid"))
}

#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd as _;

    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: the descriptor is open for the call, and the buffer and its length describe `pid`.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    u32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "the peer has no pid"))
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn peer_pid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "reading a socket peer's pid is not supported on this system",
    ))
}

/// Whether process `pid` runs the same file as this process: the same device and inode of
/// `/proc/<pid>/exe` and `/proc/self/exe`, which follow to the mapped file even after it was
/// replaced on disk. A path is never compared.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn same_executable(pid: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let (Ok(peer), Ok(own)) = (
        std::fs::metadata(format!("/proc/{pid}/exe")),
        std::fs::metadata("/proc/self/exe"),
    ) else {
        return false;
    };
    peer.dev() == own.dev() && peer.ino() == own.ino()
}

/// Whether process `pid` runs the same file as this process: the SHA-256 of the file at the
/// peer's `proc_pidpath` against the one of this process's executable, computed once.
#[cfg(target_os = "macos")]
pub(crate) fn same_executable(pid: u32) -> bool {
    use sha2::{Digest as _, Sha256};
    use std::sync::OnceLock;

    fn digest(path: &std::path::Path) -> Option<Vec<u8>> {
        let bytes = std::fs::read(path).ok()?;
        Some(Sha256::digest(&bytes).to_vec())
    }

    static OWN: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    let own = OWN.get_or_init(|| digest(&std::env::current_exe().ok()?));
    let Some(own) = own else {
        return false;
    };
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return false;
    };
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is valid for writes of its length.
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return false;
    }
    buffer.truncate(length as usize);
    let Ok(path) = String::from_utf8(buffer) else {
        return false;
    };
    digest(std::path::Path::new(&path)).as_ref() == Some(own)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub(crate) fn same_executable(_pid: u32) -> bool {
    false
}
