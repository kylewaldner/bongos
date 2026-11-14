//! `peer_identity` on real sockets: the peer is this process, a child running `/bin/sh`, a copy
//! of this test binary at another path, and a hard link to it. Each child connects in the forked
//! process before it executes its program, so the connection's peer is the child's pid; the test
//! reads the identity only after the child shows it is running its program.

use std::io::{BufRead as _, BufReader, Read as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::{ClientKind, peer_identity};

#[test]
fn a_peer_in_this_process_runs_the_same_executable() {
    let (ours, _theirs) = UnixStream::pair().unwrap();
    let window = peer_identity(&ours, ClientKind::Window).unwrap();
    assert_eq!(window.pid(), std::process::id());
    assert!(window.same_executable());
    assert!(window.may_attest_person());
    // The command-line client never attests a person, whatever it runs.
    let cli = peer_identity(&ours, ClientKind::Cli).unwrap();
    assert!(cli.same_executable());
    assert!(!cli.may_attest_person());
}

/// Run by the children below, never on its own: waits until its stdin closes.
#[test]
#[ignore]
fn wait_for_stdin_to_close() {
    let mut rest = Vec::new();
    std::io::stdin().read_to_end(&mut rest).ok();
}

/// Starts `program` with `args` in a child that connects to `socket` before it executes it.
#[allow(clippy::disallowed_methods)]
fn connected_child(program: &Path, args: &[&str], socket: &Path) -> Child {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::process::CommandExt as _;

    // SAFETY: an all-zero `sockaddr_un` is a valid value.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = socket.as_os_str().as_bytes();
    assert!(
        bytes.len() < address.sun_path.len(),
        "the socket path is too long"
    );
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as libc::c_char;
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: the closure runs in the forked child before exec and calls only `socket` and
    // `connect`, which are async-signal-safe, on memory prepared before the fork.
    unsafe {
        command.pre_exec(move || {
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let length = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            if libc::connect(fd, (&address as *const libc::sockaddr_un).cast(), length) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().unwrap()
}

/// Accepts the child's connection, waits until the child printed a line (so it runs its own
/// program now), reads the peer's identity, and ends the child.
fn identity_of_child(program: &Path, args: &[&str]) -> (u32, crate::PeerIdentity) {
    let folder = tempfile::TempDir::new().unwrap();
    let socket = folder.path().join("peer.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut child = connected_child(program, args, &socket);
    let (stream, _) = listener.accept().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let identity = peer_identity(&stream, ClientKind::Window).unwrap();
    drop(child.stdin.take());
    child.kill().ok();
    child.wait().unwrap();
    (child.id(), identity)
}

#[test]
fn a_child_running_the_shell_is_not_the_same_executable() {
    let (pid, identity) = identity_of_child(Path::new("/bin/sh"), &["-c", "echo ready; read line"]);
    assert_eq!(identity.pid(), pid);
    assert!(!identity.same_executable());
    assert!(!identity.may_attest_person());
}

/// A folder beside this test binary (on its file system, so a hard link can be made), removed
/// when dropped.
fn beside_this_binary() -> (PathBuf, tempfile::TempDir) {
    let binary = std::env::current_exe().unwrap();
    let folder = tempfile::Builder::new()
        .prefix("peer-test-")
        .tempdir_in(binary.parent().unwrap())
        .unwrap();
    (binary, folder)
}

const HELPER_ARGS: [&str; 4] = [
    "--exact",
    "peer_tests::wait_for_stdin_to_close",
    "--ignored",
    "--nocapture",
];

#[test]
fn a_copy_of_this_binary_at_another_path_is_not_the_same_executable() {
    let (binary, folder) = beside_this_binary();
    let copy = folder.path().join("copy-of-the-tests");
    std::fs::copy(&binary, &copy).unwrap();
    let (pid, identity) = identity_of_child(&copy, &HELPER_ARGS);
    assert_eq!(identity.pid(), pid);
    assert!(!identity.same_executable());
    assert!(!identity.may_attest_person());
}

#[cfg(target_os = "linux")]
#[test]
fn a_hard_link_to_this_binary_is_the_same_executable() {
    let (binary, folder) = beside_this_binary();
    let link = folder.path().join("link-to-the-tests");
    std::fs::hard_link(&binary, &link).unwrap();
    let (pid, identity) = identity_of_child(&link, &HELPER_ARGS);
    assert_eq!(identity.pid(), pid);
    assert!(identity.same_executable());
    assert!(identity.may_attest_person());
}
