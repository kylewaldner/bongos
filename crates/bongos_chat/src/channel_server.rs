//! The chat report server: one private Unix socket per bongos process that running chats report
//! their state on (one request line and one reply line per connection, the wire format of
//! [`bongos_chat_model::channel::report`]).
//!
//! The socket is `<dir>/chat-<pid>.sock`, where `<dir>` is the first usable of
//! `$XDG_RUNTIME_DIR/bongos/` (only when that variable names an absolute directory the user owns
//! with mode `0700`) and `<temp dir>/bongos-<uid>/`. The directory is created `0700`; an existing
//! one must be a real directory (not a symlink) owned by the user with no group or other bits, or
//! it is refused (never `chmod`ed or reused) and the next one is tried. The socket is bound, made
//! `0600` and checked with `stat`; a path over [`MAX_SOCKET_PATH_BYTES`] tries the next directory.
//!
//! Every connection is checked three ways: the filesystem modes keep other users out, the peer's
//! uid must be this process's uid, and a report must name a registered chat and carry the token of
//! that chat's current launch. The reply is written right after those checks, before the report is
//! handed to the main thread. The server runs only while `chats.enabled && chats.channels`; it
//! stops (removing the file and dropping every token) when either turns false and on quit.
//!
//! A line whose top-level object has a `request` key within its first [`MAX_LINE_BYTES`] is a
//! request ([`bongos_chat_model::channel::request`]) instead of a report: it may be up to
//! [`MAX_REQUEST_BYTES`] long, passes the same uid, chat and token checks, and is answered by the
//! handler set for its kind ([`ChannelServer::set_request_handler`]), which runs on its own thread.
//! Every other line keeps the report limit. A request longer than [`MAX_LINE_BYTES`] has its chat and
//! token checked from those first bytes ([`request::head_credentials`]) as soon as the line passes
//! them (at most one more 4 KiB read), so only an authorized client can make the server read on
//! toward [`MAX_REQUEST_BYTES`]. A request counts against
//! [`MAX_CONNECTIONS`] until its handler returns and its reply is written; a client that left early
//! loses the reply, and the handler is not cancelled. A reply not written within the write timeout
//! ([`REPLY_WRITE_TIMEOUT`] by default) is abandoned and the connection closed, so a client that
//! stops reading cannot hold its slot.

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{
    DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, PermissionsExt as _,
};
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use bongos_chat_model::channel::report::{
    self, ChatId, MAX_CONNECTIONS, MAX_LINE_BYTES, ParseError, READ_TIMEOUT, ReplyError, Report,
    Token,
};
use bongos_chat_model::channel::request::{
    self, Body, MAX_REQUEST_BYTES, Request, RequestKind, Value,
};
use collections::HashMap;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::{AsyncReadExt as _, AsyncWriteExt as _, FutureExt as _, StreamExt as _};
use gpui::{App, AppContext as _, BackgroundExecutor, Context, Entity, Global, Subscription, Task};
use net::async_net::{UnixListener, UnixStream};
use settings::{Settings as _, SettingsStore};

use crate::ChatsSettings;

/// The longest socket path used. Linux's `sun_path` holds 108 bytes and macOS's 104.
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

/// How long the accept loop waits after a failed accept (such as too many open files).
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// How long a request's reply may take to write before the connection is closed without it.
pub const REPLY_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// What decides the socket directory, read from the process by [`SocketDirInputs::from_env`] and
/// passed by tests, so no test reads the real `$XDG_RUNTIME_DIR`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocketDirInputs {
    /// `$XDG_RUNTIME_DIR`, when set.
    pub xdg_runtime_dir: Option<PathBuf>,
    /// `std::env::temp_dir()`.
    pub temp_dir: PathBuf,
    /// The effective uid of this process.
    pub uid: u32,
    /// The pid the socket is named after.
    pub pid: u32,
}

impl SocketDirInputs {
    pub fn from_env() -> Self {
        Self {
            xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            temp_dir: std::env::temp_dir(),
            uid: nix::unistd::geteuid().as_raw(),
            pid: std::process::id(),
        }
    }

    /// The directories to try, first match wins, each either a directory or why its rule did not
    /// apply.
    fn candidates(&self) -> Vec<Result<PathBuf, String>> {
        let mut candidates = Vec::new();
        if let Some(xdg) = &self.xdg_runtime_dir {
            candidates.push(runtime_dir_usable(xdg, self.uid).map(|()| xdg.join("bongos")));
        }
        candidates.push(Ok(self.temp_dir.join(format!("bongos-{}", self.uid))));
        candidates
    }
}

/// Whether `$XDG_RUNTIME_DIR` (`dir`) is absolute and names a directory owned by `uid` with mode
/// `0700`.
fn runtime_dir_usable(dir: &Path, uid: u32) -> Result<(), String> {
    if !dir.is_absolute() {
        return Err("XDG_RUNTIME_DIR is not an absolute path".to_string());
    }
    let metadata =
        fs::metadata(dir).map_err(|error| format!("XDG_RUNTIME_DIR {}: {error}", dir.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "XDG_RUNTIME_DIR {} is not a directory",
            dir.display()
        ));
    }
    if metadata.uid() != uid || metadata.mode() & 0o777 != 0o700 {
        return Err(format!(
            "XDG_RUNTIME_DIR {} is not a mode 0700 directory of uid {uid}",
            dir.display()
        ));
    }
    Ok(())
}

/// Creates `dir` with mode `0700`, or checks the one that exists: a directory, not a symlink, owned
/// by `uid`, with no group or other bits. Nothing is ever `chmod`ed.
fn prepare_dir(dir: &Path, uid: u32) -> Result<(), String> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("cannot create {}: {error}", dir.display())),
    }
    let metadata =
        fs::symlink_metadata(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("{} is a symlink", dir.display()));
    }
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    if metadata.uid() != uid {
        return Err(format!("{} is not owned by uid {uid}", dir.display()));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(format!(
            "{} is open to other users (mode {:o})",
            dir.display(),
            metadata.mode() & 0o777
        ));
    }
    Ok(())
}

/// Binds the socket in the first usable directory, after sweeping that directory's stale sockets.
/// Returns the socket path and the bound listener, or every rule's reason for refusing.
pub(crate) fn open_socket(inputs: &SocketDirInputs) -> Result<(PathBuf, StdUnixListener), String> {
    let mut causes = Vec::new();
    for candidate in inputs.candidates() {
        match candidate.and_then(|dir| bind_in(&dir, inputs)) {
            Ok(bound) => return Ok(bound),
            Err(cause) => causes.push(cause),
        }
    }
    Err(causes.join("; "))
}

fn bind_in(dir: &Path, inputs: &SocketDirInputs) -> Result<(PathBuf, StdUnixListener), String> {
    let path = dir.join(format!("chat-{}.sock", inputs.pid));
    if path.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(format!(
            "{} is longer than {MAX_SOCKET_PATH_BYTES} bytes",
            path.display()
        ));
    }
    prepare_dir(dir, inputs.uid)?;
    sweep_stale(dir, inputs.pid);
    let listener = StdUnixListener::bind(&path)
        .map_err(|error| format!("cannot bind {}: {error}", path.display()))?;
    if let Err(cause) = secure_socket(&path, inputs.uid) {
        drop(listener);
        fs::remove_file(&path).ok();
        return Err(cause);
    }
    Ok((path, listener))
}

/// Makes the bound socket `0600` and checks the result with `stat`.
fn secure_socket(path: &Path, uid: u32) -> Result<(), String> {
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot chmod {}: {error}", path.display()))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != uid
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(format!(
            "{} is not a mode 0600 socket of uid {uid}",
            path.display()
        ));
    }
    Ok(())
}

/// The pid in a socket file name `chat-<pid>.sock`, or `None` for any other name.
pub(crate) fn socket_pid(name: &str) -> Option<i32> {
    let digits = name.strip_prefix("chat-")?.strip_suffix(".sock")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<i32>().ok().filter(|pid| *pid > 0)
}

/// Whether a process `pid` exists (`kill(pid, 0)` does not answer `ESRCH`; a process of another
/// user answers `EPERM` and counts as alive).
pub(crate) fn pid_alive(pid: i32) -> bool {
    !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

/// Removes every `chat-<pid>.sock` in `dir` whose process is gone, and the one named after
/// `own_pid`. Live sockets and other names are kept. Returns the removed paths.
pub(crate) fn sweep_stale(dir: &Path, own_pid: u32) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(socket_pid) else {
            continue;
        };
        if u32::try_from(pid).is_ok_and(|pid| pid == own_pid) || !pid_alive(pid) {
            let path = entry.path();
            if fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
    }
    removed
}

/// Whether a connection whose peer has uid `peer` (`None` when it could not be read) may talk to a
/// server running as `own`.
pub(crate) fn peer_allowed(peer: Option<u32>, own: u32) -> bool {
    peer == Some(own)
}

/// Reads the uid of a connection's peer (replaced by tests that play another user).
pub(crate) type PeerUid = fn(&UnixStream) -> nix::Result<u32>;

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &UnixStream) -> nix::Result<u32> {
    nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
        .map(|credentials| credentials.uid())
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn peer_uid(stream: &UnixStream) -> nix::Result<u32> {
    nix::unistd::getpeereid(std::os::fd::AsFd::as_fd(stream)).map(|(uid, _)| uid.as_raw())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
fn peer_uid(stream: &UnixStream) -> nix::Result<u32> {
    let _ = stream;
    Err(nix::errno::Errno::ENOSYS)
}

/// Whether two tokens are equal, comparing every byte whatever the first difference is.
pub(crate) fn tokens_match(expected: &str, given: &str) -> bool {
    let (expected, given) = (expected.as_bytes(), given.as_bytes());
    if expected.len() != given.len() {
        return false;
    }
    expected
        .iter()
        .zip(given)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Answers one request: called with the chat id and the body, on a thread of its own, so it may
/// block.
pub type RequestHandler = Arc<dyn Fn(ChatId, Body) -> Value + Send + Sync>;

type RequestHandlers = Arc<Mutex<HashMap<RequestKind, RequestHandler>>>;

/// What the accept loop and the connections share with the entity.
#[derive(Default)]
struct Shared {
    /// The current launch token of every registered chat.
    tokens: Mutex<HashMap<ChatId, Token>>,
    /// The connections being served.
    open: AtomicUsize,
    /// The request handlers, shared with the entity so later ones apply at once.
    handlers: RequestHandlers,
}

impl Shared {
    /// Whether `token` is the current token of registered chat `chat`.
    fn authorize(&self, chat: &ChatId, token: &Token) -> Result<(), ReplyError> {
        let tokens = lock(&self.tokens);
        let expected = tokens.get(chat).ok_or(ReplyError::UnknownChat)?;
        if !tokens_match(expected.as_str(), token.as_str()) {
            return Err(ReplyError::BadToken);
        }
        Ok(())
    }

    /// Parses `line` and checks its chat and token.
    fn check(&self, line: &[u8]) -> Result<Report, ReplyError> {
        let report = report::parse(line).map_err(ParseError::reply_error)?;
        self.authorize(&report.chat, &report.token)?;
        Ok(report)
    }

    /// Checks the chat and token in the first [`MAX_LINE_BYTES`] of request `line`.
    fn check_request_head(&self, line: &[u8]) -> Result<(), ReplyError> {
        let (chat, token) = request::head_credentials(line).map_err(ParseError::reply_error)?;
        self.authorize(&chat, &token)
    }

    /// Parses request `line`, checks its chat and token like a report's, and finds its handler.
    fn check_request(&self, line: &[u8]) -> Result<(Request, RequestHandler), ReplyError> {
        let request = request::parse(line).map_err(ParseError::reply_error)?;
        self.authorize(&request.chat, &request.token)?;
        let handler = lock(&self.handlers)
            .get(&request.kind)
            .cloned()
            .ok_or(ReplyError::UnknownRequest)?;
        Ok((request, handler))
    }
}

/// One launch's registered token, from [`ChannelServer::register_launch`]. Dropping it forgets the
/// token unless a later launch of the chat replaced it, so a launch that fails, ends or is closed
/// leaves nothing registered. It holds the registry weakly: after the server stopped (which dropped
/// every token) it does nothing.
pub struct Registration {
    chat: ChatId,
    token: Token,
    tokens: Weak<Shared>,
}

impl Registration {
    pub fn chat(&self) -> &ChatId {
        &self.chat
    }

    /// Whether `token` is this launch's token and is still the chat's current one in the server
    /// that registered it.
    pub fn is_current(&self, token: &Token) -> bool {
        if !tokens_match(self.token.as_str(), token.as_str()) {
            return false;
        }
        self.tokens.upgrade().is_some_and(|shared| {
            lock(&shared.tokens)
                .get(&self.chat)
                .is_some_and(|current| tokens_match(current.as_str(), self.token.as_str()))
        })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let Some(shared) = self.tokens.upgrade() else {
            return;
        };
        let mut tokens = lock(&shared.tokens);
        if tokens
            .get(&self.chat)
            .is_some_and(|current| tokens_match(current.as_str(), self.token.as_str()))
        {
            tokens.remove(&self.chat);
        }
    }
}

impl std::fmt::Debug for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registration")
            .field("chat", &self.chat)
            .finish_non_exhaustive()
    }
}

/// Counts a connection as open until it is dropped.
struct OpenConnection(Arc<Shared>);

impl Drop for OpenConnection {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A bound server. Dropping it stops the accept loop, drops every token and removes the file.
struct Running {
    path: PathBuf,
    shared: Arc<Shared>,
    _accept: Task<()>,
    _deliver: Task<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        lock(&self.shared.tokens).clear();
        fs::remove_file(&self.path).ok();
    }
}

/// Where verified reports go, on the main thread.
pub type ReportHandler = Rc<dyn Fn(Report, &mut App)>;

/// The report server of this process. See the module documentation.
pub struct ChannelServer {
    inputs: SocketDirInputs,
    read_timeout: Duration,
    write_timeout: Duration,
    on_report: ReportHandler,
    request_handlers: RequestHandlers,
    peer_uid: PeerUid,
    running: Option<Running>,
    /// Why the last start found no usable socket.
    unavailable: Option<String>,
    /// Whether the settings asked for the server at the last check.
    wanted: bool,
    _settings: Subscription,
}

struct GlobalChannelServer(Entity<ChannelServer>);

impl Global for GlobalChannelServer {}

impl ChannelServer {
    /// A server that follows `chats.enabled && chats.channels`, starting at once when both are
    /// true. Verified reports go to a handler that does nothing until one is set.
    pub fn new(inputs: SocketDirInputs, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            inputs,
            read_timeout: READ_TIMEOUT,
            write_timeout: REPLY_WRITE_TIMEOUT,
            on_report: Rc::new(|_, _| {}),
            request_handlers: RequestHandlers::default(),
            peer_uid,
            running: None,
            unavailable: None,
            wanted: false,
            _settings: cx.observe_global::<SettingsStore>(|this, cx| this.follow_settings(cx)),
        };
        this.follow_settings(cx);
        this
    }

    /// Starts or stops the server when `chats.enabled && chats.channels` changed.
    fn follow_settings(&mut self, cx: &mut Context<Self>) {
        let settings = ChatsSettings::get_global(cx);
        let wanted = settings.enabled && settings.channels;
        if wanted == self.wanted {
            return;
        }
        self.wanted = wanted;
        if wanted {
            self.start(cx);
        } else {
            self.stop();
        }
        cx.notify();
    }

    /// Binds a new socket with an empty token registry, replacing a running one.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.stop();
        let (path, listener) = match open_socket(&self.inputs) {
            Ok(bound) => bound,
            Err(cause) => {
                log::error!("chats: report socket unavailable: {cause}");
                self.unavailable = Some(cause);
                return;
            }
        };
        let listener = match UnixListener::try_from(listener) {
            Ok(listener) => listener,
            Err(error) => {
                fs::remove_file(&path).ok();
                let cause = format!("cannot watch {}: {error}", path.display());
                log::error!("chats: report socket unavailable: {cause}");
                self.unavailable = Some(cause);
                return;
            }
        };
        self.unavailable = None;
        let shared = Arc::new(Shared {
            handlers: self.request_handlers.clone(),
            ..Shared::default()
        });
        let (reports, mut received) = unbounded();
        let executor = cx.background_executor().clone();
        let accept = cx.background_spawn(accept_loop(
            listener,
            shared.clone(),
            self.inputs.uid,
            self.peer_uid,
            self.read_timeout,
            reports,
            ReplyWriter {
                executor,
                timeout: self.write_timeout,
            },
        ));
        let deliver = cx.spawn(async move |this, cx| {
            while let Some(report) = received.next().await {
                let delivered = this.update(cx, |this, cx| {
                    let handler = this.on_report.clone();
                    handler(report, cx);
                });
                if delivered.is_err() {
                    break;
                }
            }
        });
        self.running = Some(Running {
            path,
            shared,
            _accept: accept,
            _deliver: deliver,
        });
    }

    /// Stops the server: the socket closes, its file is removed and every token is dropped.
    pub fn stop(&mut self) {
        self.running = None;
    }

    /// Records `token` as the current launch token of `chat`, replacing an earlier launch's.
    /// Returns false, recording nothing, while the server is not running.
    pub fn register(&mut self, chat: ChatId, token: Token) -> bool {
        let Some(running) = &self.running else {
            return false;
        };
        lock(&running.shared.tokens).insert(chat, token);
        true
    }

    /// [`Self::register`] for one launch: the token stays registered while the returned
    /// [`Registration`] lives. `None`, recording nothing, while the server is not running.
    pub fn register_launch(&mut self, chat: ChatId, token: Token) -> Option<Registration> {
        let running = self.running.as_ref()?;
        lock(&running.shared.tokens).insert(chat.clone(), token.clone());
        Some(Registration {
            chat,
            token,
            tokens: Arc::downgrade(&running.shared),
        })
    }

    /// Forgets `chat`'s token when it is still `token` (a later launch's token stays).
    pub fn unregister(&mut self, chat: &ChatId, token: &Token) {
        let Some(running) = &self.running else {
            return;
        };
        let mut tokens = lock(&running.shared.tokens);
        if tokens
            .get(chat)
            .is_some_and(|current| tokens_match(current.as_str(), token.as_str()))
        {
            tokens.remove(chat);
        }
    }

    /// Forgets `chat`'s token, whichever launch it belongs to.
    pub fn unregister_chat(&mut self, chat: &ChatId) {
        if let Some(running) = &self.running {
            lock(&running.shared.tokens).remove(chat);
        }
    }

    /// A fresh launch token: 32 random hex digits.
    pub fn new_token() -> Token {
        Token::new(&uuid::Uuid::new_v4().simple().to_string())
            .expect("a simple UUID is a valid token")
    }

    /// The socket path while the server runs.
    pub fn socket_path(&self) -> Option<&Path> {
        self.running.as_ref().map(|running| running.path.as_path())
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Why the server could not start, as a launch's fallback reason: `report socket unavailable:
    /// <cause>`. `None` while it runs or was never asked to.
    pub fn unavailable_reason(&self) -> Option<String> {
        self.unavailable
            .as_ref()
            .map(|cause| format!("report socket unavailable: {cause}"))
    }

    /// The connections being served right now.
    pub fn open_connections(&self) -> usize {
        self.running
            .as_ref()
            .map_or(0, |running| running.shared.open.load(Ordering::SeqCst))
    }

    /// Sets where verified reports go (the runtime, or a test's recorder).
    pub fn set_report_handler(&mut self, handler: impl Fn(Report, &mut App) + 'static) {
        self.on_report = Rc::new(handler);
    }

    /// Sets the handler that answers requests of `kind`, replacing an earlier one. A request of a
    /// kind with no handler is answered `unknown request`.
    pub fn set_request_handler(
        &mut self,
        kind: RequestKind,
        handler: impl Fn(ChatId, Body) -> Value + Send + Sync + 'static,
    ) {
        lock(&self.request_handlers).insert(kind, Arc::new(handler));
    }

    /// Removes the handler of `kind`; its requests are answered `unknown request` from now on.
    pub fn remove_request_handler(&mut self, kind: &RequestKind) {
        lock(&self.request_handlers).remove(kind);
    }

    /// Replaces how the peer's uid is read, for servers started from now on.
    #[cfg(test)]
    pub(crate) fn set_peer_uid(&mut self, peer_uid: PeerUid) {
        self.peer_uid = peer_uid;
    }

    /// Replaces [`READ_TIMEOUT`] for servers started from now on (tests hold connections open).
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = timeout;
    }

    /// Replaces [`REPLY_WRITE_TIMEOUT`] for servers started from now on (tests use a short one).
    pub fn set_write_timeout(&mut self, timeout: Duration) {
        self.write_timeout = timeout;
    }

    /// The global server, once [`crate::init`] has run.
    pub fn global(cx: &App) -> Option<Entity<ChannelServer>> {
        cx.try_global::<GlobalChannelServer>()
            .map(|global| global.0.clone())
    }

    pub(crate) fn set_global(server: Entity<ChannelServer>, cx: &mut App) {
        cx.set_global(GlobalChannelServer(server));
    }
}

/// Creates the global server over the process's real directories, stopped on quit.
pub(crate) fn init(cx: &mut App) -> Entity<ChannelServer> {
    let server = cx.new(|cx| ChannelServer::new(SocketDirInputs::from_env(), cx));
    let weak = server.downgrade();
    cx.on_app_quit(move |cx| {
        weak.update(cx, |server, _| server.stop()).ok();
        async {}
    })
    .detach();
    ChannelServer::set_global(server.clone(), cx);
    server
}

/// Writes requests' replies, each within `timeout`.
#[derive(Clone)]
struct ReplyWriter {
    executor: BackgroundExecutor,
    timeout: Duration,
}

impl ReplyWriter {
    /// Writes `reply`, giving up after the timeout: a client that stops reading a reply larger than
    /// the socket buffer would otherwise hold its connection forever. The caller then drops the
    /// stream, closing it.
    async fn write(&self, stream: &mut UnixStream, reply: &str) {
        let write = async {
            if stream.write_all(reply.as_bytes()).await.is_ok() {
                stream.flush().await.ok();
            }
        };
        futures::select_biased! {
            () = write.fuse() => {}
            () = self.executor.timer(self.timeout).fuse() => {}
        }
    }
}

async fn accept_loop(
    listener: UnixListener,
    shared: Arc<Shared>,
    uid: u32,
    peer_uid: PeerUid,
    read_timeout: Duration,
    reports: UnboundedSender<Report>,
    writer: ReplyWriter,
) {
    let executor = writer.executor.clone();
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                log::warn!("chats: report socket accept failed: {error}");
                executor.timer(ACCEPT_RETRY).await;
                continue;
            }
        };
        if shared.open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            drop(stream);
            continue;
        }
        shared.open.fetch_add(1, Ordering::SeqCst);
        let connection = OpenConnection(shared.clone());
        let timeout = executor.timer(read_timeout);
        executor
            .spawn(serve(
                stream,
                connection,
                uid,
                peer_uid,
                timeout,
                reports.clone(),
                writer.clone(),
            ))
            .detach();
    }
}

/// Serves one connection: peer check, one line, one reply line, then the report goes on (or, for a
/// request, its handler's answer is the reply).
async fn serve(
    mut stream: UnixStream,
    connection: OpenConnection,
    uid: u32,
    peer_uid: PeerUid,
    timeout: Task<()>,
    reports: UnboundedSender<Report>,
    writer: ReplyWriter,
) {
    if !peer_allowed(peer_uid(&stream).ok(), uid) {
        return;
    }
    let line = futures::select_biased! {
        line = read_line(&mut stream, &connection.0).fuse() => line,
        () = timeout.fuse() => return,
    };
    let line = match line {
        None => return,
        Some(Line::Report(line)) => line,
        Some(Line::Request(line)) => {
            serve_request(stream, connection, &line, &writer).await;
            return;
        }
        Some(Line::RefusedRequest(error)) => {
            writer
                .write(&mut stream, &request::render_reply(Err(error)))
                .await;
            return;
        }
    };
    let checked = connection.0.check(&line);
    let reply = report::render_reply(match &checked {
        Ok(_) => Ok(()),
        Err(error) => Err(*error),
    });
    if stream.write_all(reply.as_bytes()).await.is_ok() {
        stream.flush().await.ok();
    }
    drop(stream);
    if let Ok(report) = checked {
        reports.unbounded_send(report).ok();
    }
}

/// Answers one request line. The connection stays counted until the handler returned and its reply
/// was written or timed out; a failed write (the client left) drops the reply quietly.
async fn serve_request(
    mut stream: UnixStream,
    connection: OpenConnection,
    line: &[u8],
    writer: &ReplyWriter,
) {
    let reply = match connection.0.check_request(line) {
        Ok((request, handler)) => {
            let Some(reply) = run_handler(handler, request.chat, request.body).await else {
                return;
            };
            request::render_reply(Ok(&reply))
        }
        Err(error) => request::render_reply(Err(error)),
    };
    writer.write(&mut stream, &reply).await;
    drop(stream);
    drop(connection);
}

/// Runs `handler` on a thread of its own, so a slow one blocks neither the accept loop nor the
/// executor. `None` when the thread could not start or the handler panicked.
async fn run_handler(handler: RequestHandler, chat: ChatId, body: Body) -> Option<Value> {
    let (sender, receiver) = futures::channel::oneshot::channel();
    std::thread::Builder::new()
        .name("chat-request".to_string())
        .spawn(move || {
            sender.send(handler(chat, body)).ok();
        })
        .ok()?;
    receiver.await.ok()
}

/// One line read from a connection.
enum Line {
    Report(Vec<u8>),
    Request(Vec<u8>),
    /// A request longer than [`MAX_LINE_BYTES`] whose chat or token those first bytes refused; the
    /// rest of it was never read.
    RefusedRequest(ReplyError),
}

/// Reads up to and including the first newline. A report stops once it is longer than
/// [`MAX_LINE_BYTES`] and a request once it is longer than [`MAX_REQUEST_BYTES`] (the parse then
/// refuses it as too large). Whether the line is a request is decided from its first
/// [`MAX_LINE_BYTES`] alone, before anything past them is read, and so are a longer request's chat
/// and token. `None` on a read error or an empty connection.
async fn read_line(stream: &mut UnixStream, shared: &Shared) -> Option<Line> {
    let mut line = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut request: Option<bool> = None;
    let done = |line: Vec<u8>, request: Option<bool>| {
        let request = request.unwrap_or_else(|| request::is_request(&line));
        Some(if request {
            Line::Request(line)
        } else {
            Line::Report(line)
        })
    };
    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            if line.is_empty() {
                return None;
            }
            return done(line, request);
        }
        let start = line.len();
        line.extend_from_slice(&chunk[..read]);
        if let Some(end) = line[start..].iter().position(|byte| *byte == b'\n') {
            line.truncate(start + end + 1);
            return done(line, request);
        }
        if request.is_none() && line.len() > MAX_LINE_BYTES {
            let is_request = request::is_request(&line);
            if is_request && let Err(error) = shared.check_request_head(&line) {
                return Some(Line::RefusedRequest(error));
            }
            request = Some(is_request);
        }
        let limit = if request == Some(true) {
            MAX_REQUEST_BYTES
        } else {
            MAX_LINE_BYTES
        };
        if line.len() > limit {
            return done(line, Some(request.unwrap_or(false)));
        }
    }
}
