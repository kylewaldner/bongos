//! The report server against real sockets in temporary directories. Clients are blocking
//! `std::os::unix::net::UnixStream`s; every exchange that waits for the server runs on a helper
//! thread, because the server's tasks run on the test thread. Parking is allowed, so the sockets'
//! wakeups reach the test; waits poll on the executor's timers and never assume a real-time bound.

use std::cell::RefCell;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{
    DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt as _,
};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use bongos_chat_model::channel::client::CHAT_ID_ENV;
use bongos_chat_model::channel::report::{
    self, AgentName, ChatId, Event, MAX_LINE_BYTES, ReplyError, Report, Token,
};
use bongos_chat_model::channel::request::{
    self, Body, MAX_REQUEST_BYTES, Request, RequestKind, SendError, Value,
};
use bongos_chat_model::{CHANNEL_SOCKET_ENV, CHANNEL_TOKEN_ENV};
use futures::StreamExt as _;
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::ChatsSettings;
use crate::channel_server::{
    ChannelServer, SocketDirInputs, open_socket, peer_allowed, pid_alive, socket_pid, sweep_stale,
    tokens_match,
};

/// A read timeout no test reaches, for connections a test holds open on purpose.
const HOLD: Duration = Duration::from_secs(3600);

fn uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

pub(super) fn inputs(temp_dir: &Path, xdg: Option<&Path>) -> SocketDirInputs {
    SocketDirInputs {
        xdg_runtime_dir: xdg.map(Path::to_path_buf),
        temp_dir: temp_dir.to_path_buf(),
        uid: uid(),
        pid: std::process::id(),
    }
}

/// A temporary directory short enough for socket paths under it (`sun_path` is about 100 bytes): the
/// system temp directory when it is short, else `/tmp`. It is removed when dropped.
pub(super) fn temp() -> (TempDir, PathBuf) {
    let system = std::env::temp_dir();
    let root = if system.as_os_str().len() <= 32 {
        system
    } else {
        PathBuf::from("/tmp")
    };
    let temp = tempfile::Builder::new()
        .prefix("bcs")
        .tempdir_in(root)
        .unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    assert!(
        base.as_os_str().len() <= 40,
        "{base:?} is too long for the socket tests"
    );
    (temp, base)
}

fn private_dir(path: &Path) {
    fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

/// The pid of a child that has exited and been reaped, so no process has it.
// A plain test thread with no executor to block, waiting on a process that exits at once.
#[allow(clippy::disallowed_methods)]
fn dead_pid() -> i32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = i32::try_from(child.id()).unwrap();
    child.wait().unwrap();
    assert!(!pid_alive(pid), "the reaped child {pid} is gone");
    pid
}

fn report_line(chat: &str, token: &Token) -> Vec<u8> {
    report::render(&Report {
        chat: ChatId::new(chat).unwrap(),
        token: token.clone(),
        at: 1,
        agent: AgentName::claude(),
        event: Event::PromptSubmitted,
        session: None,
    })
    .unwrap()
    .into_bytes()
}

/// Reads until the first newline, the end of the connection or a read error.
fn read_reply(stream: &mut UnixStream) -> String {
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = stream.read(&mut byte) {
        reply.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    String::from_utf8(reply).unwrap()
}

/// Connects, writes `request` and returns what the server replied before closing ("" when it
/// closed without a reply), from a helper thread.
pub(super) async fn exchange(path: &Path, request: Vec<u8>) -> String {
    let path = path.to_path_buf();
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let mut stream = UnixStream::connect(&path).unwrap();
        // A server that already closed refuses the write; the reply read still tells.
        stream.write_all(&request).ok();
        sender.send(read_reply(&mut stream)).ok();
    });
    receiver.await.unwrap()
}

pub(super) const OK: &str = "{\"ok\":true}\n";

pub(super) fn refused(error: &str) -> String {
    format!("{{\"ok\":false,\"error\":\"{error}\"}}\n")
}

fn set_chats(cx: &mut TestAppContext, enabled: bool, channels: bool) {
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                let chats = content.chats.get_or_insert_default();
                chats.enabled = Some(enabled);
                chats.channels = Some(channels);
            });
        });
    });
}

struct Setup {
    _temp: TempDir,
    /// `<temp>/bongos-<uid>`, the directory the server uses.
    dir: PathBuf,
    server: Entity<ChannelServer>,
    /// The reports the handler received.
    reports: Rc<RefCell<Vec<Report>>>,
}

impl Setup {
    fn path(&self, cx: &mut TestAppContext) -> PathBuf {
        self.server
            .read_with(cx, |server, _| server.socket_path().map(Path::to_path_buf))
            .expect("the server runs")
    }

    fn register(&self, cx: &mut TestAppContext, chat: &str, token: &Token) {
        let registered = self.server.update(cx, |server, _| {
            server.register(ChatId::new(chat).unwrap(), token.clone())
        });
        assert!(registered);
    }

    fn delivered(&self, cx: &mut TestAppContext) -> Vec<String> {
        cx.run_until_parked();
        self.reports
            .borrow()
            .iter()
            .map(|report| report.chat.as_str().to_string())
            .collect()
    }
}

/// A server over a temporary directory with `chats.channels` false; nothing is bound yet.
fn setup(cx: &mut TestAppContext, read_timeout: Duration) -> Setup {
    cx.executor().allow_parking();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        ChatsSettings::register(cx);
    });
    set_chats(cx, true, false);
    let (temp, base) = temp();
    let reports: Rc<RefCell<Vec<Report>>> = Rc::default();
    let server = cx.new(|cx| {
        let mut server = ChannelServer::new(inputs(&base, None), cx);
        server.set_read_timeout(read_timeout);
        let reports = reports.clone();
        server.set_report_handler(move |report, _| reports.borrow_mut().push(report));
        server
    });
    Setup {
        _temp: temp,
        dir: base.join(format!("bongos-{}", uid())),
        server,
        reports,
    }
}

/// `setup`, then `chats.channels` turned on.
fn running(cx: &mut TestAppContext, read_timeout: Duration) -> Setup {
    let setup = setup(cx, read_timeout);
    set_chats(cx, true, true);
    setup
}

/// Polls `done` on the executor's timer until it holds.
async fn wait_until(
    cx: &mut TestAppContext,
    what: &str,
    done: impl Fn(&mut TestAppContext) -> bool,
) {
    for _ in 0..1000 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        cx.executor().timer(Duration::from_millis(10)).await;
    }
    panic!("never happened: {what}");
}

fn socket_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".sock"))
        .collect();
    names.sort();
    names
}

#[test]
fn the_directory_is_0700_and_the_socket_0600() {
    let (_temp, base) = temp();
    let (path, _listener) = open_socket(&inputs(&base, None)).unwrap();
    let dir = base.join(format!("bongos-{}", uid()));
    assert_eq!(path, dir.join(format!("chat-{}.sock", std::process::id())));
    assert_eq!(mode(&dir), 0o700);
    let metadata = fs::symlink_metadata(&path).unwrap();
    assert!(metadata.file_type().is_socket());
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(metadata.uid(), uid());
}

#[test]
fn a_private_runtime_dir_is_used_first() {
    let (_temp, base) = temp();
    let xdg = base.join("xdg");
    private_dir(&xdg);
    let (path, _listener) = open_socket(&inputs(&base, Some(&xdg))).unwrap();
    assert_eq!(path.parent().unwrap(), xdg.join("bongos"));
    assert_eq!(mode(&xdg.join("bongos")), 0o700);
}

#[test]
fn an_unusable_runtime_dir_falls_back_to_the_temp_dir() {
    let (_temp, base) = temp();
    let fallback = base.join(format!("bongos-{}", uid()));
    // Open to others.
    let open = base.join("open");
    private_dir(&open);
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    // Not absolute.
    let relative = PathBuf::from("relative/run");
    // Missing.
    let missing = base.join("missing");
    for xdg in [&open, &relative, &missing] {
        let (path, listener) = open_socket(&inputs(&base, Some(xdg))).unwrap();
        assert_eq!(path.parent().unwrap(), fallback, "XDG_RUNTIME_DIR {xdg:?}");
        drop(listener);
        fs::remove_file(&path).unwrap();
    }
    assert!(!open.join("bongos").exists());
}

#[test]
fn a_group_writable_dir_is_refused_and_the_next_rule_used() {
    let (_temp, base) = temp();
    let xdg = base.join("xdg");
    private_dir(&xdg);
    let shared = xdg.join("bongos");
    private_dir(&shared);
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o770)).unwrap();
    let (path, _listener) = open_socket(&inputs(&base, Some(&xdg))).unwrap();
    assert_eq!(
        path.parent().unwrap(),
        base.join(format!("bongos-{}", uid()))
    );
    // Refused, never repaired.
    assert_eq!(mode(&shared), 0o770);
    assert!(socket_files(&shared).is_empty());
}

#[test]
fn a_symlinked_dir_is_refused_and_the_next_rule_used() {
    let (_temp, base) = temp();
    let xdg = base.join("xdg");
    private_dir(&xdg);
    let target = base.join("target");
    private_dir(&target);
    std::os::unix::fs::symlink(&target, xdg.join("bongos")).unwrap();
    let (path, _listener) = open_socket(&inputs(&base, Some(&xdg))).unwrap();
    assert_eq!(
        path.parent().unwrap(),
        base.join(format!("bongos-{}", uid()))
    );
    assert!(socket_files(&target).is_empty());
}

#[test]
fn a_too_long_path_is_refused_and_the_next_rule_used() {
    let (_temp, base) = temp();
    let xdg = base.join("x".repeat(100));
    private_dir(&xdg);
    let (path, _listener) = open_socket(&inputs(&base, Some(&xdg))).unwrap();
    assert_eq!(
        path.parent().unwrap(),
        base.join(format!("bongos-{}", uid()))
    );
    assert!(path.as_os_str().len() <= crate::MAX_SOCKET_PATH_BYTES);
    assert!(
        !xdg.join("bongos").exists(),
        "no directory made for a refused path"
    );
}

#[test]
fn no_usable_dir_leaves_the_server_off_with_a_cause() {
    let (_temp, base) = temp();
    let other = base.join("other");
    private_dir(&other);
    std::os::unix::fs::symlink(&other, base.join(format!("bongos-{}", uid()))).unwrap();
    let cause = open_socket(&inputs(&base, None)).unwrap_err();
    assert!(cause.contains("symlink"), "{cause}");
    assert!(socket_files(&other).is_empty());
}

#[test]
fn stale_sockets_are_swept_and_live_ones_kept() {
    let (_temp, base) = temp();
    let dir = base.join("dir");
    private_dir(&dir);
    let dead = format!("chat-{}.sock", dead_pid());
    let live = format!("chat-{}.sock", std::os::unix::process::parent_id());
    let own = format!("chat-{}.sock", std::process::id());
    for name in [
        &dead,
        &live,
        &own,
        &"chat-abc.sock".to_string(),
        &"notes.sock".to_string(),
    ] {
        fs::write(dir.join(name), "").unwrap();
    }
    let mut removed = sweep_stale(&dir, std::process::id());
    removed.sort();
    let mut expected = vec![dir.join(&dead), dir.join(&own)];
    expected.sort();
    assert_eq!(removed, expected);
    let mut kept = vec![live, "chat-abc.sock".to_string(), "notes.sock".to_string()];
    kept.sort();
    assert_eq!(socket_files(&dir), kept);
}

#[test]
fn opening_sweeps_the_dir_and_replaces_its_own_file() {
    let (_temp, base) = temp();
    let dir = base.join(format!("bongos-{}", uid()));
    private_dir(&dir);
    let dead = format!("chat-{}.sock", dead_pid());
    let live = format!("chat-{}.sock", std::os::unix::process::parent_id());
    let own = format!("chat-{}.sock", std::process::id());
    fs::write(dir.join(&dead), "").unwrap();
    fs::write(dir.join(&live), "").unwrap();
    fs::write(dir.join(&own), "").unwrap();
    let (path, _listener) = open_socket(&inputs(&base, None)).unwrap();
    assert_eq!(path, dir.join(&own));
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
    let mut expected = vec![live, own];
    expected.sort();
    assert_eq!(socket_files(&dir), expected);
}

#[test]
fn socket_names_parse_to_pids() {
    assert_eq!(socket_pid("chat-123.sock"), Some(123));
    for name in [
        "chat-.sock",
        "chat-0.sock",
        "chat--1.sock",
        "chat-12a.sock",
        "chat-99999999999.sock",
        "chat-1.sock.bak",
        "other-1.sock",
    ] {
        assert_eq!(socket_pid(name), None, "{name}");
    }
}

#[test]
fn only_the_same_uid_is_served() {
    assert!(peer_allowed(Some(1000), 1000));
    assert!(!peer_allowed(Some(1001), 1000));
    assert!(!peer_allowed(Some(0), 1000));
    assert!(!peer_allowed(None, 1000));
}

#[test]
fn tokens_compare_whole() {
    assert!(tokens_match("abc123", "abc123"));
    assert!(!tokens_match("abc123", "abc124"));
    assert!(!tokens_match("abc123", "xbc123"));
    assert!(!tokens_match("abc123", "abc12"));
    assert!(!tokens_match("abc123", ""));
}

#[gpui::test]
async fn reports_need_a_registered_chat_and_its_current_token(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let first = ChannelServer::new_token();
    let current = ChannelServer::new_token();
    let other = ChannelServer::new_token();
    setup.register(cx, "chat-a", &first);
    // A resume is a new launch: its token replaces the first.
    setup.register(cx, "chat-a", &current);
    setup.register(cx, "chat-b", &other);

    let cases = [
        (
            "an unknown chat",
            report_line("chat-z", &current),
            "unknown chat",
        ),
        (
            "a wrong token",
            report_line("chat-a", &ChannelServer::new_token()),
            "bad token",
        ),
        (
            "the previous launch's token",
            report_line("chat-a", &first),
            "bad token",
        ),
        (
            "another chat's token",
            report_line("chat-a", &other),
            "bad token",
        ),
        (
            "another chat's id with this token",
            report_line("chat-b", &current),
            "bad token",
        ),
    ];
    for (what, line, error) in cases {
        assert_eq!(exchange(&path, line).await, refused(error), "{what}");
        assert!(setup.delivered(cx).is_empty(), "{what} reached the runtime");
    }

    assert_eq!(exchange(&path, report_line("chat-a", &current)).await, OK);
    assert_eq!(setup.delivered(cx), ["chat-a"]);
    assert_eq!(exchange(&path, report_line("chat-b", &other)).await, OK);
    assert_eq!(setup.delivered(cx), ["chat-a", "chat-b"]);
}

#[gpui::test]
async fn unregistering_rejects_later_reports(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let old = ChannelServer::new_token();
    let new = ChannelServer::new_token();
    setup.register(cx, "chat-a", &new);
    // A late unregister of an earlier launch leaves the current one.
    setup.server.update(cx, |server, _| {
        server.unregister(&ChatId::new("chat-a").unwrap(), &old)
    });
    assert_eq!(exchange(&path, report_line("chat-a", &new)).await, OK);
    setup.server.update(cx, |server, _| {
        server.unregister(&ChatId::new("chat-a").unwrap(), &new)
    });
    assert_eq!(
        exchange(&path, report_line("chat-a", &new)).await,
        refused("unknown chat")
    );
    assert_eq!(setup.delivered(cx), ["chat-a"]);
}

#[gpui::test]
async fn bad_lines_are_refused(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);

    let mut oversized = b"{\"pad\":\"".to_vec();
    oversized.extend(std::iter::repeat_n(b'a', 5000));
    oversized.extend_from_slice(b"\"}\n");
    let good = String::from_utf8(report_line("chat-a", &token)).unwrap();
    let future_version = good.replacen("\"v\":1", "\"v\":2", 1);
    assert_ne!(future_version, good);

    let cases = [
        ("an oversized line", oversized, "too large"),
        ("garbage", b"garbage\n".to_vec(), "bad message"),
        (
            "an unknown version",
            future_version.into_bytes(),
            "bad message",
        ),
        (
            "a missing token",
            b"{\"v\":1,\"chat\":\"chat-a\"}\n".to_vec(),
            "bad message",
        ),
    ];
    for (what, line, error) in cases {
        assert_eq!(exchange(&path, line).await, refused(error), "{what}");
    }
    assert!(setup.delivered(cx).is_empty());
}

#[gpui::test]
async fn the_33rd_connection_is_closed(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);

    // Connecting does not wait for the accept, so the test thread can hold these.
    let mut held: Vec<UnixStream> = (0..32)
        .map(|_| UnixStream::connect(&path).unwrap())
        .collect();
    let server = setup.server.clone();
    wait_until(cx, "32 connections open", |cx| {
        server.read_with(cx, |server, _| server.open_connections()) == 32
    })
    .await;

    assert_eq!(exchange(&path, report_line("chat-a", &token)).await, "");
    assert!(
        setup.delivered(cx).is_empty(),
        "the 33rd report was dropped"
    );

    held.pop();
    wait_until(cx, "one connection closed", |cx| {
        server.read_with(cx, |server, _| server.open_connections()) == 31
    })
    .await;
    assert_eq!(exchange(&path, report_line("chat-a", &token)).await, OK);
    assert_eq!(setup.delivered(cx), ["chat-a"]);
    drop(held);
}

#[gpui::test]
async fn a_silent_connection_is_closed_after_the_read_timeout(cx: &mut TestAppContext) {
    let setup = running(cx, Duration::from_millis(50));
    let path = setup.path(cx);
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let mut stream = UnixStream::connect(&path).unwrap();
        sender.send(read_reply(&mut stream)).ok();
    });
    assert_eq!(receiver.await.unwrap(), "");
    let server = setup.server.clone();
    wait_until(cx, "the connection closed", |cx| {
        server.read_with(cx, |server, _| server.open_connections()) == 0
    })
    .await;
}

#[gpui::test]
async fn stop_removes_the_file_and_drops_every_token(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);
    assert_eq!(exchange(&path, report_line("chat-a", &token)).await, OK);
    // The reply goes out before the report reaches the main thread, and a stop drops reports not
    // yet delivered, so let this one arrive first.
    assert_eq!(setup.delivered(cx), ["chat-a"]);

    setup.server.update(cx, |server, _| server.stop());
    assert!(!path.exists(), "the socket file is removed");
    assert!(UnixStream::connect(&path).is_err());
    let registered = setup.server.update(cx, |server, _| {
        server.register(ChatId::new("chat-b").unwrap(), ChannelServer::new_token())
    });
    assert!(!registered, "nothing registers while stopped");

    setup.server.update(cx, |server, cx| server.start(cx));
    let path = setup.path(cx);
    assert_eq!(
        exchange(&path, report_line("chat-a", &token)).await,
        refused("unknown chat"),
        "a token from before the stop"
    );
    assert_eq!(setup.delivered(cx), ["chat-a"]);
}

#[gpui::test]
async fn the_server_follows_the_settings(cx: &mut TestAppContext) {
    let setup = setup(cx, HOLD);
    let own = format!("chat-{}.sock", std::process::id());
    let is_running = |cx: &mut TestAppContext| setup.server.read_with(cx, |s, _| s.is_running());

    assert!(!is_running(cx));
    assert!(
        socket_files(&setup.dir).is_empty(),
        "nothing binds while off"
    );

    set_chats(cx, false, true);
    assert!(!is_running(cx), "chats.enabled is false");
    assert!(socket_files(&setup.dir).is_empty());

    set_chats(cx, true, true);
    assert!(is_running(cx));
    assert_eq!(socket_files(&setup.dir), std::slice::from_ref(&own));
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);

    set_chats(cx, true, false);
    assert!(!is_running(cx));
    assert!(
        socket_files(&setup.dir).is_empty(),
        "stopping removes the file"
    );

    set_chats(cx, true, true);
    let path = setup.path(cx);
    assert_eq!(
        exchange(&path, report_line("chat-a", &token)).await,
        refused("unknown chat"),
        "a new server starts with an empty registry"
    );

    set_chats(cx, false, true);
    assert!(!is_running(cx));
    assert!(socket_files(&setup.dir).is_empty());
    assert!(setup.delivered(cx).is_empty());
}

#[gpui::test]
async fn an_unusable_dir_leaves_the_server_off_with_a_reason(cx: &mut TestAppContext) {
    let setup = setup(cx, HOLD);
    let other = setup.dir.with_file_name("other");
    private_dir(&other);
    std::os::unix::fs::symlink(&other, &setup.dir).unwrap();
    set_chats(cx, true, true);
    let (running, reason) = setup.server.read_with(cx, |server, _| {
        (server.is_running(), server.unavailable_reason())
    });
    assert!(!running);
    let reason = reason.expect("a reason");
    assert!(
        reason.starts_with("report socket unavailable: ") && reason.contains("symlink"),
        "{reason}"
    );
    let registered = setup.server.update(cx, |server, _| {
        server.register(ChatId::new("chat-a").unwrap(), ChannelServer::new_token())
    });
    assert!(!registered);
    assert!(socket_files(&other).is_empty());
}

/// The request kind the request tests serve.
const KIND: &str = "echo";

/// The chat ids a test's request handler was called with, in call order.
type Calls = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

fn calls(calls: &Calls) -> Vec<String> {
    calls.lock().unwrap().clone()
}

/// Sets a handler for [`KIND`] that records the chat id and answers `{"chat":<id>}`.
fn echo_handler(setup: &Setup, cx: &mut TestAppContext) -> Calls {
    let recorded = Calls::default();
    let calls = recorded.clone();
    setup.server.update(cx, |server, _| {
        server.set_request_handler(RequestKind::new(KIND).unwrap(), move |chat, _body| {
            calls.lock().unwrap().push(chat.as_str().to_string());
            Value::Object(Body::from_iter([(
                "chat".to_string(),
                Value::from(chat.as_str()),
            )]))
        })
    });
    recorded
}

/// What a held handler tells the test, and how the test lets it answer.
struct Held {
    started: futures::channel::mpsc::UnboundedReceiver<()>,
    finished: futures::channel::mpsc::UnboundedReceiver<()>,
    release: std::sync::mpsc::Sender<()>,
}

/// Sets a handler for [`KIND`] that says it started, waits until the test releases it, and then
/// answers `"done"` and says it finished.
fn held_handler(setup: &Setup, cx: &mut TestAppContext) -> Held {
    let (started_sender, started) = futures::channel::mpsc::unbounded();
    let (finished_sender, finished) = futures::channel::mpsc::unbounded();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = std::sync::Mutex::new(released);
    setup.server.update(cx, |server, _| {
        server.set_request_handler(RequestKind::new(KIND).unwrap(), move |_chat, _body| {
            started_sender.unbounded_send(()).ok();
            released.lock().unwrap().recv().ok();
            finished_sender.unbounded_send(()).ok();
            Value::from("done")
        })
    });
    Held {
        started,
        finished,
        release,
    }
}

/// Starts sending a [`KIND`] request through [`request::send`] with an environment naming `path`,
/// `chat` and `token`, waiting at most `reply_wait`, on a helper thread; the result arrives on the
/// returned channel.
fn spawn_request(
    path: &Path,
    chat: &str,
    token: &Token,
    reply_wait: Duration,
) -> futures::channel::oneshot::Receiver<Result<Value, SendError>> {
    let env: collections::HashMap<&str, String> = [
        (CHANNEL_SOCKET_ENV, path.to_string_lossy().into_owned()),
        (CHAT_ID_ENV, chat.to_string()),
        (CHANNEL_TOKEN_ENV, token.as_str().to_string()),
    ]
    .into_iter()
    .collect();
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let env = |name: &str| env.get(name).cloned();
        sender
            .send(request::send(&env, KIND, Body::new(), reply_wait))
            .ok();
    });
    receiver
}

/// [`spawn_request`], awaiting its result.
async fn send_request(
    path: &Path,
    chat: &str,
    token: &Token,
    reply_wait: Duration,
) -> Result<Value, SendError> {
    spawn_request(path, chat, token, reply_wait).await.unwrap()
}

/// Waits until the server serves no connection, so every handler that could run has.
async fn wait_until_idle(setup: &Setup, cx: &mut TestAppContext) {
    let server = setup.server.clone();
    wait_until(cx, "every connection closed", |cx| {
        server.read_with(cx, |server, _| server.open_connections()) == 0
    })
    .await;
}

/// A [`KIND`] request line for `chat` whose body pads it to exactly `bytes` long.
fn padded_request_line(chat: &str, token: &Token, bytes: usize) -> Vec<u8> {
    let render = |pad: usize| {
        request::render(&Request {
            chat: ChatId::new(chat).unwrap(),
            token: token.clone(),
            kind: RequestKind::new(KIND).unwrap(),
            body: Body::from_iter([("pad".to_string(), Value::from("a".repeat(pad)))]),
        })
        .unwrap()
    };
    let empty = render(0).len();
    let line = render(bytes.min(MAX_REQUEST_BYTES) - empty);
    // `render` refuses a line over the limit, so a longer one grows its pad by hand.
    let extra = "a".repeat(bytes - line.len());
    let line = line.replacen("\"pad\":\"", &format!("\"pad\":\"{extra}"), 1);
    assert_eq!(line.len(), bytes);
    line.into_bytes()
}

#[gpui::test]
async fn a_request_reaches_its_handler_with_the_chat_id(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let calls_seen = echo_handler(&setup, cx);
    let token_a = ChannelServer::new_token();
    let token_b = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token_a);
    setup.register(cx, "chat-b", &token_b);

    let reply = send_request(&path, "chat-b", &token_b, HOLD).await;
    assert_eq!(
        reply,
        Ok(Value::Object(Body::from_iter([(
            "chat".to_string(),
            Value::from("chat-b")
        )])))
    );
    assert_eq!(calls(&calls_seen), ["chat-b"]);
    assert!(setup.delivered(cx).is_empty(), "a request is not a report");
}

#[gpui::test]
async fn a_request_with_a_bad_token_is_refused_before_its_handler(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let calls_seen = echo_handler(&setup, cx);
    setup.register(cx, "chat-a", &ChannelServer::new_token());

    let reply = send_request(&path, "chat-a", &ChannelServer::new_token(), HOLD).await;
    assert_eq!(reply, Err(SendError::Refused(ReplyError::BadToken)));
    wait_until_idle(&setup, cx).await;
    assert!(calls(&calls_seen).is_empty(), "the handler never ran");
}

#[gpui::test]
async fn a_request_for_an_unknown_chat_is_refused_before_its_handler(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let calls_seen = echo_handler(&setup, cx);
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);

    let reply = send_request(&path, "chat-z", &token, HOLD).await;
    assert_eq!(reply, Err(SendError::Refused(ReplyError::UnknownChat)));
    wait_until_idle(&setup, cx).await;
    assert!(calls(&calls_seen).is_empty(), "the handler never ran");
}

#[gpui::test]
async fn a_request_from_another_uid_is_refused_before_its_handler(cx: &mut TestAppContext) {
    let setup = setup(cx, HOLD);
    setup.server.update(cx, |server, _| {
        server.set_peer_uid(|_| Ok(nix::unistd::geteuid().as_raw().wrapping_add(1)))
    });
    set_chats(cx, true, true);
    let path = setup.path(cx);
    let calls_seen = echo_handler(&setup, cx);
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);

    let reply = send_request(&path, "chat-a", &token, HOLD).await;
    // The server closes without reading, so the client sees the close on its write or its read.
    assert!(
        matches!(reply, Err(SendError::Closed | SendError::Write)),
        "{reply:?}"
    );
    wait_until_idle(&setup, cx).await;
    assert!(calls(&calls_seen).is_empty(), "the handler never ran");
}

#[gpui::test]
async fn a_report_over_4_kib_is_still_refused(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let calls_seen = echo_handler(&setup, cx);
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);

    let pad = "a".repeat(MAX_LINE_BYTES);
    let report = String::from_utf8(report_line("chat-a", &token)).unwrap();
    let padded_report = report.replacen('{', &format!("{{\"pad\":\"{pad}\","), 1);
    // A `request` key only past the first 4 KiB does not make the line a request.
    let late_request_key = format!(
        "{{\"pad\":\"{pad}\",\"v\":1,\"chat\":\"chat-a\",\"token\":\"{}\",\"request\":\"{KIND}\",\"body\":{{}}}}\n",
        token.as_str()
    );
    for (what, line) in [
        ("a padded report", padded_report),
        ("a late request key", late_request_key),
    ] {
        assert!(line.len() > MAX_LINE_BYTES, "{what}");
        assert_eq!(
            exchange(&path, line.into_bytes()).await,
            refused("too large"),
            "{what}"
        );
    }
    wait_until_idle(&setup, cx).await;
    assert!(setup.delivered(cx).is_empty());
    assert!(calls(&calls_seen).is_empty());
}

#[gpui::test]
async fn a_256_kib_request_passes_and_a_larger_one_is_refused(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let calls_seen = echo_handler(&setup, cx);
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);

    let largest = padded_request_line("chat-a", &token, MAX_REQUEST_BYTES);
    assert_eq!(
        exchange(&path, largest).await,
        "{\"ok\":true,\"reply\":{\"chat\":\"chat-a\"}}\n"
    );
    assert_eq!(calls(&calls_seen), ["chat-a"]);

    let too_large = padded_request_line("chat-a", &token, MAX_REQUEST_BYTES + 1);
    assert_eq!(exchange(&path, too_large).await, refused("too large"));
    wait_until_idle(&setup, cx).await;
    assert_eq!(calls(&calls_seen), ["chat-a"], "no handler ran for it");
}

#[gpui::test]
async fn a_reply_arrives_after_a_slow_handler(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let mut held = held_handler(&setup, cx);
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);

    // The client runs on its own thread; the test keeps driving the server meanwhile.
    let reply = spawn_request(&path, "chat-a", &token, HOLD);
    held.started.next().await.expect("the handler started");
    // A held handler blocks neither the accept loop nor other connections.
    assert_eq!(exchange(&path, report_line("chat-a", &token)).await, OK);
    assert_eq!(setup.delivered(cx), ["chat-a"]);

    held.release.send(()).unwrap();
    assert_eq!(reply.await.unwrap(), Ok(Value::from("done")));
    held.finished.next().await.expect("the handler finished");
    wait_until_idle(&setup, cx).await;
}

#[gpui::test]
async fn the_client_times_out_cleanly(cx: &mut TestAppContext) {
    let setup = running(cx, HOLD);
    let path = setup.path(cx);
    let mut held = held_handler(&setup, cx);
    let token = ChannelServer::new_token();
    setup.register(cx, "chat-a", &token);

    // The handler answers only once released, after the client gave up.
    let reply = send_request(&path, "chat-a", &token, Duration::from_millis(50)).await;
    assert_eq!(reply, Err(SendError::Timeout));
    held.started.next().await.expect("the handler started");

    // The client left: the handler still finishes, and its reply is dropped quietly.
    held.release.send(()).unwrap();
    held.finished.next().await.expect("the handler finished");
    wait_until_idle(&setup, cx).await;
}
