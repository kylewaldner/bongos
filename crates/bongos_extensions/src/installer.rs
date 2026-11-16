//! The installer service: installs, updates, rebuilds and removes extensions from git
//! repositories.
//!
//! An install resolves the revision, fetches the commit into a staging folder, runs the preflight,
//! moves the checkout to `git/<id>/<commit>`, builds it (one build at a time), checks the
//! extension API version of the built wasm, links it, then writes the install record. A failure
//! at any point keeps what was installed before, deletes the new checkout, writes the build log
//! and returns one error. A crash between the clone and the record is left for the startup
//! reconcile.
//!
//! Update and rebuild work like an install: the new build goes into a fresh checkout and replaces
//! the old one only once it built, passed the API check and was linked, so a failure keeps the old
//! build loaded and on disk. A rebuild clones the current checkout locally and reaches no network
//! for the extension. A remove marks the record `removing`, unloads the extension, then deletes
//! every checkout of the id.
//!
//! Every file and process operation runs on the background executor; the foreground only updates
//! the observable state and talks to the extension host.

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use bongos_extensions_model::{
    Fix, InstallRecord, LanguageList, Layout, ManifestFacts, Probe, ReadOutcome, RecordState,
    RevSpec, SourceUrl, ToolProbe, check_after_clone, check_before_clone, check_built_api,
    validate_id, validate_relative_path, write_atomic,
};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{Either, LocalBoxFuture, Shared};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, Global, Task, WeakEntity,
};
use util::command::{Stdio, new_command};

use crate::build_log::{BuildLog, Clock, SystemClock, rfc3339};
use crate::git::{GitRunner, RealGit, StagedClone, resolve, stage, stage_local};
use crate::host::{ExtensionStoreHost, HostApi, InstalledExtension};

/// The record field set when the build of a recorded extension is gone and it needs a rebuild or
/// reinstall. A successful install clears it.
pub const BUILD_MISSING_KEY: &str = "build_missing";

/// How an install is trusted. Asking the user is the caller's job; the installer only checks
/// that the answer matches what it is about to install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The source, subdirectory and resolved commit are on the curated list.
    Listed,
    /// The user confirmed installing this commit.
    Confirmed(String),
}

/// What identifies an install request: identical requests join.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequestKey {
    pub url: SourceUrl,
    pub path: Option<String>,
    /// The revision as given; `None` for the default branch.
    pub rev: Option<String>,
}

/// What a running operation is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Resolving,
    Cloning,
    Checking,
    /// Waiting for another build to finish; builds run one at a time.
    WaitingToBuild,
    /// Building; `downloading_wasi_sdk` is true when the builder has to download the WASI SDK
    /// first (an extension with grammars, on first use).
    Building {
        downloading_wasi_sdk: bool,
    },
    Loading,
}

impl Phase {
    /// The words the status line shows.
    pub fn label(&self) -> &'static str {
        match self {
            Phase::Resolving => "resolving",
            Phase::Cloning => "cloning",
            Phase::Checking => "checking",
            Phase::WaitingToBuild => "waiting to build",
            Phase::Building {
                downloading_wasi_sdk: true,
            } => "building (downloading the WASI SDK from github.com)",
            Phase::Building { .. } => "building",
            Phase::Loading => "loading",
        }
    }
}

/// One running install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub key: RequestKey,
    /// The extension id, once the manifest was read.
    pub id: Option<String>,
    pub phase: Phase,
}

/// The last failed install, for the failure line and "show build log".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub key: RequestKey,
    pub id: Option<String>,
    /// The first line of the error.
    pub message: String,
    pub log_path: Option<PathBuf>,
}

/// A finished install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub id: String,
    pub commit: String,
    pub log_path: PathBuf,
    /// The same commit from the same source was already installed; nothing changed.
    pub already_installed: bool,
}

pub type InstallResult = Result<Installed, Arc<anyhow::Error>>;

/// The result of an install, shared by every caller of an identical request.
pub type InstallTask = Shared<LocalBoxFuture<'static, InstallResult>>;

/// Probes the tools a build needs.
#[async_trait]
pub trait ToolProber: Send + Sync {
    async fn probe(&self) -> ToolProbe;
}

/// Probes the real `PATH`; the executor times out a tool that does not answer.
pub struct RealTools(pub BackgroundExecutor);

/// A fixed probe result, for tests.
pub struct FixedTools(pub ToolProbe);

#[async_trait]
impl ToolProber for FixedTools {
    async fn probe(&self) -> ToolProbe {
        self.0
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `program args` and returns its trimmed standard output.
async fn probe_output(
    executor: &BackgroundExecutor,
    program: &str,
    args: &[&str],
) -> Result<String, Probe> {
    let run = async {
        let output = new_command(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|_| Probe::Missing)?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(Probe::Missing)
        }
    };
    let timeout = async {
        executor.timer(PROBE_TIMEOUT).await;
        Err(Probe::TimedOut)
    };
    futures::pin_mut!(run, timeout);
    match futures::future::select(run, timeout).await {
        Either::Left((result, _)) | Either::Right((result, _)) => result,
    }
}

fn probe_of(result: &Result<String, Probe>) -> Probe {
    match result {
        Ok(_) => Probe::Found,
        Err(probe) => *probe,
    }
}

#[async_trait]
impl ToolProber for RealTools {
    async fn probe(&self) -> ToolProbe {
        let executor = &self.0;
        let git = probe_of(&probe_output(executor, "git", &["--version"]).await);
        let cargo = probe_of(&probe_output(executor, "cargo", &["--version"]).await);
        let sysroot = probe_output(executor, "rustc", &["--print", "sysroot"]).await;
        let wasm_target = match &sysroot {
            Ok(sysroot) => {
                if Path::new(sysroot)
                    .join("lib/rustlib/wasm32-wasip2")
                    .is_dir()
                {
                    Probe::Found
                } else {
                    Probe::Missing
                }
            }
            Err(probe) => *probe,
        };
        let rustup = probe_of(&probe_output(executor, "rustup", &["--version"]).await);
        ToolProbe {
            git,
            cargo,
            rustc_sysroot: probe_of(&sysroot),
            wasm_target,
            rustup,
        }
    }
}

/// Writes an install record; tests substitute one that fails.
pub type RecordWriter = fn(&Path, &InstallRecord, &str) -> std::io::Result<()>;

/// Everything the installer reaches outside itself.
pub struct InstallerDeps {
    pub layout: Layout,
    pub git: Arc<dyn GitRunner>,
    pub host: Rc<dyn HostApi>,
    pub tools: Arc<dyn ToolProber>,
    pub clock: Arc<dyn Clock>,
    pub list: Arc<LanguageList>,
    pub is_blocked_host: fn(&str) -> bool,
    pub write_record: RecordWriter,
}

/// The installer service.
pub struct Installer {
    deps: InstallerDeps,
    next_operation: u64,
    operations: Vec<(u64, Operation)>,
    in_flight: HashMap<RequestKey, InstallTask>,
    /// Ids being changed, with the operation changing each and what it is doing ("installing",
    /// "updating", "rebuilding" or "removing").
    busy_ids: HashMap<String, (u64, &'static str)>,
    /// The build semaphore: builds share the builder's cache folder, so one runs at a time.
    build_slot: Arc<futures::lock::Mutex<()>>,
    last_failure: Option<Failure>,
    last_installed: Option<Installed>,
}

struct GlobalInstaller(Entity<Installer>);

impl Global for GlobalInstaller {}

impl Installer {
    pub fn new(deps: InstallerDeps) -> Self {
        Self {
            deps,
            next_operation: 0,
            operations: Vec::new(),
            in_flight: HashMap::new(),
            busy_ids: HashMap::new(),
            build_slot: Arc::default(),
            last_failure: None,
            last_installed: None,
        }
    }

    /// The installer of the app, created on first use with the real git, extension store and
    /// clock. Creating it touches nothing on disk.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalInstaller>() {
            return global.0.clone();
        }
        let store = extension_host::ExtensionStore::global(cx);
        let root = store
            .read(cx)
            .installed_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| paths::extensions_dir().clone());
        let list = LanguageList::load().unwrap_or_else(|err| {
            log::error!("{err:#}");
            LanguageList::parse("").expect("an empty list parses")
        });
        let deps = InstallerDeps {
            layout: Layout::new(root),
            git: Arc::new(RealGit::new()),
            host: Rc::new(ExtensionStoreHost),
            tools: Arc::new(RealTools(cx.background_executor().clone())),
            clock: Arc::new(SystemClock),
            list: Arc::new(list),
            is_blocked_host: bongos_net::is_blocked_host,
            write_record: write_atomic,
        };
        let entity = cx.new(|_| Self::new(deps));
        Self::set_global(entity.clone(), cx);
        entity
    }

    /// Makes `entity` the app's installer.
    pub fn set_global(entity: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalInstaller(entity));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalInstaller>().map(|g| g.0.clone())
    }

    pub fn layout(&self) -> &Layout {
        &self.deps.layout
    }

    /// The running operations, oldest first.
    pub fn operations(&self) -> impl Iterator<Item = &Operation> {
        self.operations.iter().map(|(_, op)| op)
    }

    pub fn last_failure(&self) -> Option<&Failure> {
        self.last_failure.as_ref()
    }

    pub fn last_installed(&self) -> Option<&Installed> {
        self.last_installed.as_ref()
    }

    pub fn dismiss_failure(&mut self, cx: &mut Context<Self>) {
        if self.last_failure.take().is_some() {
            cx.notify();
        }
    }

    /// Installs `source` at `rev` (a tag, branch or full commit; `None` for the default branch),
    /// from the subdirectory `path`. An identical request already running is joined.
    pub fn install(
        &mut self,
        source: &str,
        rev: Option<&str>,
        path: Option<&str>,
        trust: Trust,
        cx: &mut Context<Self>,
    ) -> InstallTask {
        let parsed = (|| -> Result<(SourceUrl, RevSpec, Option<String>)> {
            let url = SourceUrl::parse(source)?;
            let rev = RevSpec::parse(rev)?;
            let path = path
                .map(|p| p.trim().trim_matches('/').to_string())
                .filter(|p| !p.is_empty());
            if let Some(path) = &path {
                validate_relative_path(path)?;
            }
            Ok((url, rev, path))
        })();
        let (url, rev_spec, path) = match parsed {
            Ok(parsed) => parsed,
            Err(err) => return ready(Err(Arc::new(err))),
        };
        let key = RequestKey {
            url,
            path,
            rev: rev
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(String::from),
        };
        // A joining caller shares the running install and its outcome; its own `trust` is not
        // checked again, since the first request's trust was checked when that install started.
        if let Some(task) = self.in_flight.get(&key) {
            return task.clone();
        }

        let op = self.next_operation;
        self.next_operation += 1;
        self.operations.push((
            op,
            Operation {
                key: key.clone(),
                id: None,
                phase: Phase::Resolving,
            },
        ));
        let (sender, receiver) = oneshot::channel();
        let task: InstallTask = async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(Arc::new(anyhow!("the install was cancelled"))))
        }
        .boxed_local()
        .shared();
        self.in_flight.insert(key.clone(), task.clone());
        cx.notify();

        let request = Request {
            op,
            key,
            rev: rev_spec,
            trust: RequestTrust::Given(trust),
            kind: Kind::Install,
        };
        cx.spawn(async move |this, cx| {
            let (result, log_path) = run(this.clone(), request.clone(), cx).await;
            this.update(cx, |this, cx| this.finish(&request, &result, log_path, cx))
                .ok();
            sender.send(result).ok();
        })
        .detach();
        task
    }

    /// Updates the git-installed extension `id` to `rev` of its recorded source (a tag, branch or
    /// full commit; `None` for the default branch). The old build stays loaded until the new one is
    /// built and linked. Refused while another operation on `id` runs.
    pub fn update(
        &mut self,
        id: &str,
        rev: Option<&str>,
        trust: Trust,
        cx: &mut Context<Self>,
    ) -> InstallTask {
        let rev_spec = match RevSpec::parse(rev) {
            Ok(rev) => rev,
            Err(err) => return ready(Err(Arc::new(err.into()))),
        };
        let rev_text = rev
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(String::from);
        self.start_existing(
            id,
            Existing::Update {
                rev: rev_spec,
                rev_text,
                trust,
            },
            cx,
        )
    }

    /// Rebuilds the git-installed extension `id` from a local clone of its current checkout. Reaches
    /// no network for the extension. The old build stays loaded until the new one is built and
    /// linked. Refused while another operation on `id` runs.
    pub fn rebuild(&mut self, id: &str, cx: &mut Context<Self>) -> InstallTask {
        self.start_existing(id, Existing::Rebuild, cx)
    }

    fn start_existing(
        &mut self,
        id: &str,
        existing: Existing,
        cx: &mut Context<Self>,
    ) -> InstallTask {
        if let Err(err) = validate_id(id) {
            return ready(Err(Arc::new(err.into())));
        }
        let op = self.next_operation;
        self.next_operation += 1;
        if let Err(err) = self.claim_id(op, id, existing.doing(), cx) {
            return ready(Err(Arc::new(err)));
        }
        let (sender, receiver) = oneshot::channel();
        let task: InstallTask = async move {
            receiver
                .await
                .unwrap_or_else(|_| Err(Arc::new(anyhow!("the operation was cancelled"))))
        }
        .boxed_local()
        .shared();
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let (result, log_path, request) =
                run_existing(this.clone(), op, id, existing, cx).await;
            this.update(cx, |this, cx| match &request {
                Some(request) => this.finish(request, &result, log_path, cx),
                None => this.release(op, cx),
            })
            .ok();
            sender.send(result).ok();
        })
        .detach();
        task
    }

    /// Removes the git-installed extension `id`: marks its record `removing`, unloads it, then
    /// deletes every checkout under `git/<id>/` and the record. Refused while another operation on
    /// `id` runs. A removal cut short is finished by the startup reconcile.
    pub fn remove(&mut self, id: &str, cx: &mut Context<Self>) -> Task<Result<()>> {
        if let Err(err) = validate_id(id) {
            return Task::ready(Err(err.into()));
        }
        let op = self.next_operation;
        self.next_operation += 1;
        if let Err(err) = self.claim_id(op, id, "removing", cx) {
            return Task::ready(Err(err));
        }
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let result = run_remove(this.clone(), id, cx).await;
            this.update(cx, |this, cx| this.release(op, cx)).ok();
            result
        })
    }

    /// Repairs what a crash left in the extensions directory (see [`crate::reconcile_run`]). Ids
    /// this installer is changing are left alone. Builds, fetches and contacts nothing.
    pub fn reconcile(&mut self, cx: &mut Context<Self>) -> Task<Result<Vec<Fix>>> {
        let inputs = crate::reconcile_run::Inputs {
            layout: self.deps.layout.clone(),
            git: self.deps.git.clone(),
            host: self.deps.host.clone(),
            clock: self.deps.clock.clone(),
            list: self.deps.list.clone(),
            write_record: self.deps.write_record,
            busy: self.busy_ids.keys().cloned().collect(),
        };
        cx.spawn(async move |_, cx| crate::reconcile_run::run(inputs, cx).await)
    }

    fn release(&mut self, op: u64, cx: &mut Context<Self>) {
        self.operations.retain(|(o, _)| *o != op);
        self.busy_ids.retain(|_, (o, _)| *o != op);
        cx.notify();
    }

    fn set_phase(&mut self, op: u64, phase: Phase, cx: &mut Context<Self>) {
        if let Some((_, operation)) = self.operations.iter_mut().find(|(o, _)| *o == op) {
            operation.phase = phase;
            cx.notify();
        }
    }

    /// Claims `id` for operation `op`, which is `doing` something to it, refusing when another
    /// operation holds it.
    fn claim_id(
        &mut self,
        op: u64,
        id: &str,
        doing: &'static str,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        match self.busy_ids.get(id) {
            Some((other, other_doing)) if *other != op => bail!("{id} is busy: {other_doing}"),
            _ => {}
        }
        self.busy_ids.insert(id.to_string(), (op, doing));
        if let Some((_, operation)) = self.operations.iter_mut().find(|(o, _)| *o == op) {
            operation.id = Some(id.to_string());
        }
        cx.notify();
        Ok(())
    }

    fn finish(
        &mut self,
        request: &Request,
        result: &InstallResult,
        log_path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .operations
            .iter()
            .find(|(o, _)| *o == request.op)
            .and_then(|(_, op)| op.id.clone());
        self.operations.retain(|(o, _)| *o != request.op);
        self.busy_ids.retain(|_, (o, _)| *o != request.op);
        if let Kind::Install = request.kind {
            self.in_flight.remove(&request.key);
        }
        match result {
            Ok(installed) => {
                self.last_installed = Some(installed.clone());
                if self
                    .last_failure
                    .as_ref()
                    .is_some_and(|f| f.key == request.key)
                {
                    self.last_failure = None;
                }
            }
            Err(err) => {
                let message = format!("{err:#}");
                let first = message.lines().next().unwrap_or_default().to_string();
                self.last_failure = Some(Failure {
                    key: request.key.clone(),
                    id,
                    message: first,
                    log_path,
                });
            }
        }
        cx.notify();
    }
}

fn ready(result: InstallResult) -> InstallTask {
    futures::future::ready(result).boxed_local().shared()
}

#[derive(Clone)]
struct Request {
    op: u64,
    key: RequestKey,
    rev: RevSpec,
    trust: RequestTrust,
    kind: Kind,
}

#[derive(Clone)]
enum RequestTrust {
    /// Checked against what is about to be installed.
    Given(Trust),
    /// Carried over from the record: the same commit is built again.
    Kept {
        listed: bool,
        trusted_commit: Option<String>,
    },
}

#[derive(Clone)]
enum Kind {
    Install,
    Update {
        id: String,
    },
    /// Builds a local clone of `checkout`, the current checkout of `id`.
    Rebuild {
        id: String,
        checkout: PathBuf,
    },
}

impl Kind {
    /// The id claimed and locked before the operation started.
    fn held_id(&self) -> Option<&str> {
        match self {
            Kind::Install => None,
            Kind::Update { id } | Kind::Rebuild { id, .. } => Some(id),
        }
    }
}

/// An operation on an extension that is already installed from git.
enum Existing {
    Update {
        rev: RevSpec,
        rev_text: Option<String>,
        trust: Trust,
    },
    Rebuild,
}

impl Existing {
    fn doing(&self) -> &'static str {
        match self {
            Existing::Update { .. } => "updating",
            Existing::Rebuild => "rebuilding",
        }
    }
}

/// What an install holds while it runs. Moved to the background for every file operation, so
/// dropping a staged clone or deleting a checkout never happens on the foreground.
struct Work {
    layout: Layout,
    clock: Arc<dyn Clock>,
    log: BuildLog,
    locks: Vec<File>,
    staged: Option<StagedClone>,
    /// The new checkout under `git/<id>/`, deleted on failure.
    checkout: Option<PathBuf>,
}

impl Work {
    fn fail(&mut self, err: &anyhow::Error) {
        self.log.error(err);
        if self.staged.take().is_some() {
            self.log.line("removed the staging folder");
        }
        if let Some(checkout) = self.checkout.take() {
            // Never delete what `installed/<id>` points at (a restore that failed, say).
            let linked = checkout
                .parent()
                .and_then(Path::file_name)
                .and_then(|id| id.to_str())
                .and_then(|id| std::fs::read_link(self.layout.symlink_path(id)).ok())
                .is_some_and(|target| target.starts_with(&checkout));
            if linked {
                self.log.line(format!(
                    "kept the new checkout {}: it is linked",
                    checkout.display()
                ));
                self.locks.clear();
                return;
            }
            match std::fs::remove_dir_all(&checkout) {
                Ok(()) => self
                    .log
                    .line(format!("removed the new checkout {}", checkout.display())),
                Err(err) => self.log.line(format!(
                    "could not remove the new checkout {}: {err}",
                    checkout.display()
                )),
            }
        }
        self.locks.clear();
    }
}

/// Takes the OS file lock at `path`, refusing when another process holds it. The lock goes away
/// when the file is closed or the process dies.
fn take_lock(path: &Path, what: &str) -> Result<File> {
    try_lock(path)?.ok_or_else(|| anyhow!("another bongos is installing {what}"))
}

/// Takes the lock of an installed id for an update, rebuild or remove.
fn take_id_lock(layout: &Layout, id: &str) -> Result<File> {
    try_lock(&layout.lock_path(id))?
        .ok_or_else(|| anyhow!("{id} is busy: another bongos is changing it"))
}

/// Takes the OS file lock at `path`, creating the file; `None` when another process holds it.
pub(crate) fn try_lock(path: &Path) -> Result<Option<File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("could not lock {}", path.display()))
        }
    }
}

/// Runs `f` with the work on the background executor, writes the log and hands the work back.
async fn on_background<T: Send + 'static>(
    cx: &AsyncApp,
    mut work: Work,
    f: impl FnOnce(&mut Work) -> T + Send + 'static,
) -> (Work, T) {
    cx.background_spawn(async move {
        let result = f(&mut work);
        work.log.flush();
        (work, result)
    })
    .await
}

/// Runs one install and returns its result and the path of its build log (none when it stopped
/// before writing one).
async fn run(
    this: WeakEntity<Installer>,
    request: Request,
    cx: &mut AsyncApp,
) -> (InstallResult, Option<PathBuf>) {
    let deps = this.read_with(cx, |this, _| {
        let deps = &this.deps;
        (
            deps.layout.clone(),
            deps.git.clone(),
            deps.tools.clone(),
            deps.clock.clone(),
            deps.list.clone(),
            deps.is_blocked_host,
            deps.write_record,
            deps.host.clone(),
        )
    });
    let Ok((layout, git, tools, clock, list, is_blocked_host, write_record, host)) = deps else {
        return (Err(Arc::new(anyhow!("the installer is gone"))), None);
    };

    let source_key = layout.source_key(&request.key.url);
    let work = Work {
        layout: layout.clone(),
        clock: clock.clone(),
        log: BuildLog::start(layout.clone(), clock.clone(), source_key.clone()),
        locks: Vec::new(),
        staged: None,
        checkout: None,
    };
    let (mut work, result) = {
        let key = request.key.clone();
        on_background(cx, work, move |work| -> Result<()> {
            let what = key.url.to_string();
            let lock = take_lock(&work.layout.lock_path(&source_key), &what)?;
            work.locks.push(lock);
            work.log.line(format!(
                "install {} at {} from {}",
                key.url,
                key.rev.as_deref().unwrap_or("the default branch"),
                key.path.as_deref().unwrap_or("the repository root"),
            ));
            Ok(())
        })
        .await
    };
    if let Err(err) = result {
        // A held lock belongs to another process: its log is not ours to touch.
        return (Err(Arc::new(err)), None);
    }

    let outcome = install_steps(
        &this,
        &request,
        &mut work,
        Steps {
            git,
            tools,
            clock,
            list,
            is_blocked_host,
            write_record,
            host,
        },
        cx,
    )
    .await;
    let log_path = work.log.path();
    let (_, outcome) = on_background(cx, work, move |work| {
        if let Err(err) = &outcome {
            work.fail(err);
        }
        outcome
    })
    .await;
    (outcome.map_err(Arc::new), Some(log_path))
}

/// Runs one step of `work` on the background executor (see [`on_background`]).
macro_rules! background {
    ($cx:expr, $work:expr, $f:expr) => {{
        let taken = std::mem::replace($work, $work.placeholder());
        let (back, result) = on_background($cx, taken, $f).await;
        *$work = back;
        result
    }};
}

/// Runs an update or rebuild of the installed `id` and returns its result, the path of its build
/// log (none when it stopped before writing one) and its request (none when it stopped before
/// reading the record).
async fn run_existing(
    this: WeakEntity<Installer>,
    op: u64,
    id: String,
    existing: Existing,
    cx: &mut AsyncApp,
) -> (InstallResult, Option<PathBuf>, Option<Request>) {
    let gone = || Err(Arc::new(anyhow!("the installer is gone")));
    let Ok((layout, steps)) =
        this.read_with(cx, |this, _| (this.deps.layout.clone(), this.deps.steps()))
    else {
        return (gone(), None, None);
    };
    let work = Work {
        layout: layout.clone(),
        clock: steps.clock.clone(),
        log: BuildLog::start(layout.clone(), steps.clock.clone(), id.clone()),
        locks: Vec::new(),
        staged: None,
        checkout: None,
    };
    let (mut work, prepared) = {
        let id = id.clone();
        on_background(
            cx,
            work,
            move |work| -> Result<(InstallRecord, Option<PathBuf>)> {
                // Lock first: a held lock means the id's log belongs to the other process.
                let lock = take_id_lock(&work.layout, &id)?;
                work.locks.push(lock);
                let record = installed_record(&work.layout, &id)?;
                let linked = std::fs::read_link(work.layout.symlink_path(&id)).ok();
                Ok((record, linked))
            },
        )
        .await
    };
    let (record, linked) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => return (Err(Arc::new(err)), None, None),
    };
    let url = match SourceUrl::parse(&record.source_url) {
        Ok(url) => url,
        Err(err) => {
            let err = anyhow::Error::from(err).context(format!("the record of {id} is damaged"));
            return (Err(Arc::new(err)), None, None);
        }
    };
    let (key_rev, rev, trust, kind) = match existing {
        Existing::Update {
            rev,
            rev_text,
            trust,
        } => (
            rev_text,
            rev,
            RequestTrust::Given(trust),
            Kind::Update { id: id.clone() },
        ),
        Existing::Rebuild => {
            let checkout = linked
                .as_deref()
                .and_then(|target| checkout_of(&layout, &id, target))
                .filter(|checkout| is_checkout_of_commit(checkout, &record.commit))
                .unwrap_or_else(|| layout.checkout_dir(&id, &record.commit));
            (
                record.requested_ref.clone(),
                RevSpec::Commit(record.commit.clone()),
                RequestTrust::Kept {
                    listed: record.listed,
                    trusted_commit: record.trusted_commit.clone(),
                },
                Kind::Rebuild {
                    id: id.clone(),
                    checkout,
                },
            )
        }
    };
    let request = Request {
        op,
        key: RequestKey {
            url: url.clone(),
            path: record.path.clone(),
            rev: key_rev,
        },
        rev,
        trust,
        kind,
    };
    let registered = this.update(cx, |this, cx| {
        this.operations.push((
            op,
            Operation {
                key: request.key.clone(),
                id: Some(id.clone()),
                phase: Phase::Resolving,
            },
        ));
        cx.notify();
    });
    if registered.is_err() {
        return (gone(), None, None);
    }

    let locked = {
        let source_key = layout.source_key(&url);
        let what = url.to_string();
        let line = match &request.kind {
            Kind::Rebuild { checkout, .. } => format!(
                "rebuild {id} at {} from {}",
                record.commit,
                checkout.display()
            ),
            _ => format!(
                "update {id} from {} to {}",
                record.commit,
                request.key.rev.as_deref().unwrap_or("the default branch")
            ),
        };
        let work = &mut work;
        background!(cx, work, move |work| -> Result<()> {
            work.log.line(line);
            let lock = take_lock(&work.layout.lock_path(&source_key), &what)?;
            work.locks.push(lock);
            Ok(())
        })
    };
    let outcome = match locked {
        Ok(()) => install_steps(&this, &request, &mut work, steps, cx).await,
        Err(err) => Err(err),
    };
    let log_path = work.log.path();
    let (_, outcome) = on_background(cx, work, move |work| {
        if let Err(err) = &outcome {
            work.fail(err);
        }
        outcome
    })
    .await;
    (outcome.map_err(Arc::new), Some(log_path), Some(request))
}

/// The record of `id`, which must be installed (not being removed).
fn installed_record(layout: &Layout, id: &str) -> Result<InstallRecord> {
    let path = layout.record_path(id);
    match bongos_extensions_model::read(&path)
        .with_context(|| format!("could not read {}", path.display()))?
    {
        ReadOutcome::Ok(record) if record.state == RecordState::Installed => Ok(record),
        ReadOutcome::Ok(_) => bail!("{id} is being removed"),
        ReadOutcome::Missing | ReadOutcome::Unreadable => bail!("{id} is not installed from git"),
    }
}

/// Runs one remove.
async fn run_remove(this: WeakEntity<Installer>, id: String, cx: &mut AsyncApp) -> Result<()> {
    let (layout, clock, host, write_record) = this.read_with(cx, |this, _| {
        let deps = &this.deps;
        (
            deps.layout.clone(),
            deps.clock.clone(),
            deps.host.clone(),
            deps.write_record,
        )
    })?;
    let work = Work {
        layout: layout.clone(),
        clock: clock.clone(),
        log: BuildLog::start(layout, clock, id.clone()),
        locks: Vec::new(),
        staged: None,
        checkout: None,
    };
    let (mut work, linked) = {
        let id = id.clone();
        on_background(cx, work, move |work| -> Result<bool> {
            // Lock first: a held lock means the id's log belongs to the other process.
            let lock = take_id_lock(&work.layout, &id)?;
            work.locks.push(lock);
            let marked = mark_removing(work, &id, write_record);
            if let Err(err) = &marked {
                work.log.error(err);
            }
            marked
        })
        .await
    };
    if linked? {
        let unlink = cx.update(|cx| host.unlink(&id, cx));
        if let Err(err) = unlink.await {
            let err = err.context(format!("could not unload {id}"));
            let (_, err) = on_background(cx, work, move |work| {
                work.log.error(&err);
                work.log.line("the next start finishes the removal");
                err
            })
            .await;
            return Err(err);
        }
        work.log.line(format!("unloaded {id}"));
    }
    let (_, result) = on_background(cx, work, move |work| -> Result<()> {
        let dir = work.layout.extension_dir(&id);
        let result = match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("could not remove {}", dir.display())),
        };
        match &result {
            Ok(()) => work.log.line(format!("removed {id}")),
            Err(err) => work.log.error(err),
        }
        work.locks.clear();
        result
    })
    .await;
    result
}

/// Marks the record of `id` `removing` and removes `installed/<id>` when it points into
/// `git/<id>/`. Returns whether it did, so the host has to unload the extension.
fn mark_removing(work: &mut Work, id: &str, write_record: RecordWriter) -> Result<bool> {
    let layout = work.layout.clone();
    let link = layout.symlink_path(id);
    let ours = std::fs::read_link(&link).is_ok_and(|t| t.starts_with(layout.extension_dir(id)));
    let record_path = layout.record_path(id);
    let record = bongos_extensions_model::read(&record_path)
        .with_context(|| format!("could not read {}", record_path.display()))?;
    if !ours && !layout.extension_dir(id).is_dir() {
        bail!("{id} is not installed from git");
    }
    work.log.line(format!("remove {id}"));
    if let ReadOutcome::Ok(mut record) = record {
        record.state = RecordState::Removing;
        let nonce = format!("{}-remove", std::process::id());
        write_record(&record_path, &record, &nonce)
            .with_context(|| format!("could not write {}", record_path.display()))?;
        work.log.line("marked the record removing");
    }
    if ours {
        // The host deletes `installed/<id>` recursively; removing the link first keeps that
        // removal from reaching the checkout through it.
        std::fs::remove_file(&link)
            .with_context(|| format!("could not remove {}", link.display()))?;
        work.log.line(format!("removed the link installed/{id}"));
    }
    Ok(ours)
}

impl InstallerDeps {
    fn steps(&self) -> Steps {
        Steps {
            git: self.git.clone(),
            tools: self.tools.clone(),
            clock: self.clock.clone(),
            list: self.list.clone(),
            is_blocked_host: self.is_blocked_host,
            write_record: self.write_record,
            host: self.host.clone(),
        }
    }
}

struct Steps {
    git: Arc<dyn GitRunner>,
    tools: Arc<dyn ToolProber>,
    clock: Arc<dyn Clock>,
    list: Arc<LanguageList>,
    is_blocked_host: fn(&str) -> bool,
    write_record: RecordWriter,
    host: Rc<dyn HostApi>,
}

impl Work {
    /// An empty stand-in while the real work is on the background. It holds nothing and never
    /// writes.
    fn placeholder(&self) -> Work {
        Work {
            layout: self.layout.clone(),
            clock: self.clock.clone(),
            log: BuildLog::start(
                self.layout.clone(),
                self.clock.clone(),
                self.log.key().to_string(),
            ),
            locks: Vec::new(),
            staged: None,
            checkout: None,
        }
    }
}

async fn install_steps(
    this: &WeakEntity<Installer>,
    request: &Request,
    work: &mut Work,
    steps: Steps,
    cx: &mut AsyncApp,
) -> Result<Installed> {
    let url = request.key.url.clone();
    let path = request.key.path.clone();
    let op = request.op;

    // Before the clone: git, the repository host, then the revision.
    let probe = {
        let tools = steps.tools.clone();
        cx.background_spawn(async move { tools.probe().await })
            .await
    };
    check_before_clone(&probe, &url, steps.is_blocked_host)?;
    if !matches!(request.rev, RevSpec::Commit(_)) {
        work.log.line("resolve");
        work.log.command(format!("git ls-remote --symref -- {url}"));
    }
    let commit = {
        let git = steps.git.clone();
        let url = url.clone();
        let rev = request.rev.clone();
        cx.background_spawn(async move { resolve(git.as_ref(), &url, &rev).await })
            .await?
    };
    work.log.line(format!("resolved to {commit}"));
    let listed = match &request.trust {
        RequestTrust::Kept { listed, .. } => *listed,
        RequestTrust::Given(Trust::Listed) => {
            if !steps.list.is_listed(&url, path.as_deref(), &commit) {
                bail!(
                    "{url} at {commit} is not on the list of language extensions; install it from git and confirm it instead"
                );
            }
            true
        }
        RequestTrust::Given(Trust::Confirmed(confirmed)) => {
            if !confirmed.eq_ignore_ascii_case(&commit) {
                bail!(
                    "{url} now resolves to {commit}, not the confirmed {confirmed}; install again to confirm the new commit"
                );
            }
            false
        }
    };

    // Clone.
    this.update(cx, |this, cx| this.set_phase(op, Phase::Cloning, cx))?;
    work.log.line("clone");
    let staged = if let Kind::Rebuild { checkout, .. } = &request.kind {
        work.log.command(format!(
            "git clone --quiet --local -- {} <staging folder>",
            checkout.display()
        ));
        let git = steps.git.clone();
        let layout = work.layout.clone();
        let checkout = checkout.clone();
        let commit = commit.clone();
        let path = path.clone();
        cx.background_spawn(async move {
            stage_local(git.as_ref(), &layout, &checkout, &commit, path.as_deref()).await
        })
        .await?
    } else {
        stage_remote(&url, &commit, path.as_deref(), work, &steps, cx).await?
    };
    work.staged = Some(staged);

    // Read the manifest and claim the id.
    this.update(cx, |this, cx| this.set_phase(op, Phase::Checking, cx))?;
    let facts = background!(cx, work, |work| -> Result<ManifestFacts> {
        work.log.line("check");
        let staged = work.staged.as_ref().expect("staged above");
        read_facts(staged.root(), staged.dir())
    })?;
    if facts.has_extension_toml {
        validate_id(&facts.id)?;
        if let Some(held) = request.kind.held_id() {
            if facts.id != held {
                bail!(
                    "{url} at {commit} is the extension `{}`, not `{held}`",
                    facts.id
                );
            }
        } else {
            this.update(cx, |this, cx| {
                this.claim_id(op, &facts.id, "installing", cx)
            })??;
            let id = facts.id.clone();
            background!(cx, work, move |work| -> Result<()> {
                // Lock first: a held lock means the id's log belongs to the other process.
                let lock = take_lock(&work.layout.lock_path(&id), &id)?;
                work.locks.push(lock);
                work.log.rekey(id);
                Ok(())
            })?;
        }
    }
    let id = facts.id.clone();

    // Preflight after the clone.
    let (installed, api_max, schema_range, wasi_sdk_probe) = cx.update(|cx| {
        (
            steps.host.installed(cx),
            steps.host.api_max(cx),
            steps.host.schema_version_range(),
            steps.host.wasi_sdk_present(cx),
        )
    });
    let wasi_sdk_present = cx.background_spawn(wasi_sdk_probe).await;
    let host = steps.host.clone();
    let is_suppressed = move |id: &str| host.is_suppressed(id);
    let (ours, previous_record) = {
        let id = id.clone();
        let url = url.clone();
        let path = path.clone();
        background!(
            cx,
            work,
            move |work| -> Result<(bool, Option<InstallRecord>)> {
                ownership(&work.layout, &id, &url, path.as_deref(), &installed)
            }
        )?
    };
    check_after_clone(
        &probe,
        &facts,
        steps.is_blocked_host,
        is_suppressed,
        |_| !ours,
        schema_range,
        api_max,
    )?;

    // Move the checkout into place, unless this exact commit is already installed.
    let (already, root) = {
        let id = id.clone();
        let commit = commit.clone();
        // A rebuild builds the same commit again on purpose.
        let same_record = !matches!(request.kind, Kind::Rebuild { .. })
            && previous_record
                .as_ref()
                .is_some_and(|r| r.commit.eq_ignore_ascii_case(&commit));
        let nonce = format!("{}-{op}", std::process::id());
        background!(cx, work, move |work| -> Result<(bool, PathBuf)> {
            place_checkout(work, &id, &commit, same_record, &nonce)
        })?
    };
    if already {
        work.log
            .line(format!("{id} is already installed at {commit}"));
        return Ok(Installed {
            id,
            commit,
            log_path: work.log.path(),
            already_installed: true,
        });
    }

    // Build, one at a time.
    this.update(cx, |this, cx| this.set_phase(op, Phase::WaitingToBuild, cx))?;
    let build_slot = this.read_with(cx, |this, _| this.build_slot.clone())?;
    let _build_guard = build_slot.lock().await;
    let downloading_wasi_sdk = !facts.grammar_repositories.is_empty() && !wasi_sdk_present;
    this.update(cx, |this, cx| {
        this.set_phase(
            op,
            Phase::Building {
                downloading_wasi_sdk,
            },
            cx,
        )
    })?;
    work.log.line(format!("build {}", root.display()));
    if downloading_wasi_sdk {
        work.log
            .line("downloading the WASI SDK from github.com (first use)");
    }
    if facts.cargo_toml.is_some() {
        work.log
            .command("cargo build --target wasm32-wasip2 (through the extension builder)");
    }
    for grammar in &facts.grammar_repositories {
        work.log.command(format!(
            "build the grammar from {grammar} with the WASI SDK clang"
        ));
    }
    let compile = cx.update(|cx| steps.host.compile(root.clone(), cx));
    let built_api = cx
        .background_spawn(compile)
        .await
        .context("the build failed")?;
    drop(_build_guard);

    // The real API version of what was built.
    if let Some(built) = built_api {
        work.log.line(format!(
            "built with extension API {built} (this bongos supports up to {api_max})"
        ));
        check_built_api(built, api_max)?;
    } else {
        work.log
            .line("built (no Rust part, no extension API version)");
    }

    // Link.
    this.update(cx, |this, cx| this.set_phase(op, Phase::Loading, cx))?;
    let previous_target = {
        let id = id.clone();
        background!(cx, work, move |work| -> Option<PathBuf> {
            work.log.line(format!("link installed/{id}"));
            std::fs::read_link(work.layout.symlink_path(&id)).ok()
        })
    };
    let link = cx.update(|cx| steps.host.link(&id, root.clone(), cx));
    if let Err(err) = link.await {
        // The host removes the old link before making the new one, so a failed link can leave
        // none: put back what was linked before.
        let err = err.context("could not load the extension");
        let current = {
            let id = id.clone();
            background!(cx, work, move |work| -> Option<PathBuf> {
                std::fs::read_link(work.layout.symlink_path(&id)).ok()
            })
        };
        if current == previous_target {
            return Err(err);
        }
        return Err(restore_link(&steps, &id, previous_target.as_ref(), err, cx).await);
    }
    {
        let id = id.clone();
        let root = root.clone();
        background!(cx, work, move |work| -> Result<()> {
            let target = std::fs::read_link(work.layout.symlink_path(&id)).ok();
            if target.as_deref() != Some(root.as_path()) {
                bail!("could not link {id}: another operation on it is running");
            }
            Ok(())
        })?;
    }

    // Record, last.
    let record = InstallRecord {
        version: 1,
        id: id.clone(),
        source_url: url.to_string(),
        path: path.clone(),
        requested_ref: request.key.rev.clone(),
        commit: commit.clone(),
        listed,
        trusted_commit: match &request.trust {
            RequestTrust::Given(Trust::Listed) => None,
            RequestTrust::Given(Trust::Confirmed(_)) => Some(commit.clone()),
            RequestTrust::Kept { trusted_commit, .. } => trusted_commit.clone(),
        },
        state: RecordState::Installed,
        installed_at: rfc3339(steps.clock.now()),
        extra: previous_record
            .map(|r| {
                let mut extra = r.extra;
                extra.remove(BUILD_MISSING_KEY);
                extra
            })
            .unwrap_or_default(),
    };
    let write_record = steps.write_record;
    let recorded = {
        let nonce = format!("{}-{op}", std::process::id());
        background!(cx, work, move |work| -> Result<()> {
            work.log.line("record");
            let path = work.layout.record_path(&record.id);
            write_record(&path, &record, &nonce)
                .with_context(|| format!("could not write {}", path.display()))
        })
    };
    if let Err(err) = recorded {
        // Put back what was linked before, so a failed record leaves the old state.
        return Err(restore_link(&steps, &id, previous_target.as_ref(), err, cx).await);
    }

    // Done: the new checkout stays, the old one goes.
    {
        let id = id.clone();
        let commit = commit.clone();
        background!(cx, work, move |work| {
            let new = work.checkout.take();
            if let Some(previous) = previous_target
                && let Some(old) = checkout_of(&work.layout, &id, &previous)
                && new.as_ref() != Some(&old)
                && !root.starts_with(&old)
            {
                std::fs::remove_dir_all(&old).ok();
                work.log
                    .line(format!("removed the old checkout {}", old.display()));
            }
            work.log.line(format!("installed {id} at {commit}"));
        });
    }
    Ok(Installed {
        id,
        commit,
        log_path: work.log.path(),
        already_installed: false,
    })
}

/// Fetches `commit` of `url` into a new staging folder.
async fn stage_remote(
    url: &SourceUrl,
    commit: &str,
    path: Option<&str>,
    work: &mut Work,
    steps: &Steps,
    cx: &mut AsyncApp,
) -> Result<StagedClone> {
    for command in [
        "git init --quiet -- <staging folder>".to_string(),
        format!("git remote add -- origin {url}"),
        format!("git fetch --quiet --no-tags --depth 1 origin {commit}"),
        "git checkout --quiet --detach FETCH_HEAD".to_string(),
    ] {
        work.log.command(command);
    }
    let git = steps.git.clone();
    let layout = work.layout.clone();
    let url = url.clone();
    let commit = commit.to_string();
    let path = path.map(String::from);
    cx.background_spawn(async move {
        stage(git.as_ref(), &layout, &url, &commit, path.as_deref()).await
    })
    .await
}

/// Points `installed/<id>` back at `previous` (or removes it when nothing was linked) after `err`,
/// and returns the error to report.
async fn restore_link(
    steps: &Steps,
    id: &str,
    previous: Option<&PathBuf>,
    err: anyhow::Error,
    cx: &mut AsyncApp,
) -> anyhow::Error {
    let restore = cx.update(|cx| match previous {
        Some(previous) => steps.host.link(id, previous.clone(), cx),
        None => steps.host.unlink(id, cx),
    });
    match restore.await {
        Ok(()) => err,
        Err(restore_err) => err.context(format!(
            "restoring the previous link also failed: {restore_err:#}"
        )),
    }
}

/// The checkout folder under `git/<id>/` that `target` lies in.
fn checkout_of(layout: &Layout, id: &str, target: &Path) -> Option<PathBuf> {
    let base = layout.extension_dir(id);
    let rest = target.strip_prefix(&base).ok()?;
    let first = rest.components().next()?;
    Some(base.join(first))
}

/// Whether `checkout` (a folder under `git/<id>/`) holds `commit`: `<commit>` itself or a
/// `<commit>.rebuild-<nonce>` built beside a live link.
fn is_checkout_of_commit(checkout: &Path, commit: &str) -> bool {
    checkout
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == commit
                || name
                    .strip_prefix(commit)
                    .is_some_and(|rest| rest.starts_with(".rebuild-"))
        })
}

/// Whether an existing install of `id` is this source's (so installing replaces it), and its
/// record. An id the host has with no record of this source is someone else's.
fn ownership(
    layout: &Layout,
    id: &str,
    url: &SourceUrl,
    path: Option<&str>,
    installed: &[InstalledExtension],
) -> Result<(bool, Option<InstallRecord>)> {
    if id.is_empty() {
        return Ok((true, None));
    }
    let record_path = layout.record_path(id);
    let record = match bongos_extensions_model::read(&record_path)
        .with_context(|| format!("could not read {}", record_path.display()))?
    {
        ReadOutcome::Ok(record) => Some(record),
        ReadOutcome::Missing | ReadOutcome::Unreadable => None,
    };
    if let Some(record) = record {
        let same = record.source_url == url.as_str() && record.path.as_deref() == path;
        return Ok((same, same.then_some(record)));
    }
    let linked_here = std::fs::read_link(layout.symlink_path(id))
        .is_ok_and(|target| target.starts_with(layout.extension_dir(id)));
    let in_host = installed.iter().any(|e| e.id == id);
    Ok((linked_here || !in_host, None))
}

/// Moves the staged clone to `git/<id>/<commit>` and returns `(already installed, extension
/// root)`. When that commit is the one linked and recorded, nothing moves.
fn place_checkout(
    work: &mut Work,
    id: &str,
    commit: &str,
    same_record: bool,
    nonce: &str,
) -> Result<(bool, PathBuf)> {
    let staged = work.staged.take().expect("staged before placing");
    let relative = staged
        .root()
        .strip_prefix(staged.dir())
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let linked = std::fs::read_link(work.layout.symlink_path(id)).ok();
    let linked_checkout = linked
        .as_deref()
        .and_then(|target| checkout_of(&work.layout, id, target));
    if same_record
        && let (Some(target), Some(linked_checkout)) = (&linked, &linked_checkout)
        && is_checkout_of_commit(linked_checkout, commit)
        && *target == linked_checkout.join(&relative)
    {
        drop(staged);
        return Ok((true, target.clone()));
    }
    let mut checkout = work.layout.checkout_dir(id, commit);
    if linked_checkout.as_ref() == Some(&checkout) {
        // The live link points into this folder (its record is gone), so it must survive until
        // the new build is linked: build beside it instead.
        checkout = work.layout.rebuild_checkout_dir(id, commit, nonce);
    }
    if checkout.exists() {
        // Left behind by an earlier attempt; nothing links it.
        std::fs::remove_dir_all(&checkout)
            .with_context(|| format!("could not remove {}", checkout.display()))?;
    }
    let root = checkout.join(&relative);
    std::fs::create_dir_all(work.layout.extension_dir(id)).with_context(|| {
        format!(
            "could not create {}",
            work.layout.extension_dir(id).display()
        )
    })?;
    let staging = staged.keep();
    if let Err(err) = std::fs::rename(&staging, &checkout) {
        std::fs::remove_dir_all(&staging).ok();
        return Err(err).with_context(|| {
            format!(
                "could not move {} to {}",
                staging.display(),
                checkout.display()
            )
        });
    }
    work.checkout = Some(checkout);
    Ok((false, root))
}

/// Reads what the preflight needs from the extension's root `root` inside the checkout `checkout`.
fn read_facts(root: &Path, checkout: &Path) -> Result<ManifestFacts> {
    let manifest_path = root.join("extension.toml");
    let mut facts = ManifestFacts {
        has_extension_toml: manifest_path.is_file(),
        has_extension_json: root.join("extension.json").is_file(),
        ..ManifestFacts::default()
    };
    if facts.has_extension_toml {
        let text = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("could not read {}", manifest_path.display()))?;
        let manifest: toml::Table = text.parse().context("extension.toml is not valid TOML")?;
        facts.id = manifest
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("extension.toml has no `id`"))?
            .to_string();
        facts.schema_version = manifest
            .get("schema_version")
            .and_then(|v| v.as_integer())
            .map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX));
        if let Some(grammars) = manifest.get("grammars").and_then(|v| v.as_table()) {
            for grammar in grammars.values() {
                if let Some(repository) = grammar.get("repository").and_then(|v| v.as_str()) {
                    facts.grammar_repositories.push(repository.to_string());
                }
            }
        }
    }
    facts.cargo_toml = std::fs::read_to_string(root.join("Cargo.toml")).ok();
    if facts.cargo_toml.is_some() && root != checkout {
        // A `workspace = true` dependency reads the nearest workspace manifest above the root.
        facts.workspace_toml = root
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(checkout))
            .filter_map(|dir| std::fs::read_to_string(dir.join("Cargo.toml")).ok())
            .find(|text| text.contains("[workspace"));
    }
    Ok(facts)
}

#[cfg(test)]
#[path = "installer_tests.rs"]
mod tests;
