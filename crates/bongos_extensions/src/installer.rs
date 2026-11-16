//! The installer service: installs an extension from a git repository.
//!
//! An install resolves the revision, fetches the commit into a staging folder, runs the preflight,
//! moves the checkout to `git/<id>/<commit>`, builds it (one build at a time), checks the
//! extension API version of the built wasm, links it, then writes the install record. A failure
//! at any point keeps what was installed before, deletes the new checkout, writes the build log
//! and returns one error. A crash between the clone and the record is left for the startup
//! reconcile.
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
    InstallRecord, LanguageList, Layout, ManifestFacts, Probe, ReadOutcome, RecordState, RevSpec,
    SourceUrl, ToolProbe, check_after_clone, check_before_clone, check_built_api, validate_id,
    validate_relative_path, write_atomic,
};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::{Either, LocalBoxFuture, Shared};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, Global, WeakEntity,
};
use util::command::{Stdio, new_command};

use crate::build_log::{BuildLog, Clock, SystemClock, rfc3339};
use crate::git::{GitRunner, RealGit, StagedClone, resolve, stage};
use crate::host::{ExtensionStoreHost, HostApi, InstalledExtension};

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
    /// Ids being installed, and the operation installing each.
    busy_ids: HashMap<String, u64>,
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
            trust,
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

    fn set_phase(&mut self, op: u64, phase: Phase, cx: &mut Context<Self>) {
        if let Some((_, operation)) = self.operations.iter_mut().find(|(o, _)| *o == op) {
            operation.phase = phase;
            cx.notify();
        }
    }

    /// Claims `id` for operation `op`, refusing when another operation holds it.
    fn claim_id(&mut self, op: u64, id: &str, cx: &mut Context<Self>) -> Result<()> {
        match self.busy_ids.get(id) {
            Some(other) if *other != op => bail!("{id} is busy: installing"),
            _ => {}
        }
        self.busy_ids.insert(id.to_string(), op);
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
        self.busy_ids.retain(|_, o| *o != request.op);
        self.in_flight.remove(&request.key);
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
    trust: Trust,
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
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => bail!("another bongos is installing {what}"),
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

struct Steps {
    git: Arc<dyn GitRunner>,
    tools: Arc<dyn ToolProber>,
    clock: Arc<dyn Clock>,
    list: Arc<LanguageList>,
    is_blocked_host: fn(&str) -> bool,
    write_record: RecordWriter,
    host: Rc<dyn HostApi>,
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
        Trust::Listed => {
            if !steps.list.is_listed(&url, path.as_deref(), &commit) {
                bail!(
                    "{url} at {commit} is not on the list of language extensions; install it from git and confirm it instead"
                );
            }
            true
        }
        Trust::Confirmed(confirmed) => {
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
    for command in [
        "git init --quiet -- <staging folder>".to_string(),
        format!("git remote add -- origin {url}"),
        format!("git fetch --quiet --no-tags --depth 1 origin {commit}"),
        "git checkout --quiet --detach FETCH_HEAD".to_string(),
    ] {
        work.log.command(command);
    }
    let staged = {
        let git = steps.git.clone();
        let layout = work.layout.clone();
        let url = url.clone();
        let commit = commit.clone();
        let path = path.clone();
        cx.background_spawn(async move {
            stage(git.as_ref(), &layout, &url, &commit, path.as_deref()).await
        })
        .await?
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
        this.update(cx, |this, cx| this.claim_id(op, &facts.id, cx))??;
        let id = facts.id.clone();
        background!(cx, work, move |work| -> Result<()> {
            // Lock first: a held lock means the id's log belongs to the other process.
            let lock = take_lock(&work.layout.lock_path(&id), &id)?;
            work.locks.push(lock);
            work.log.rekey(id);
            Ok(())
        })?;
    }
    let id = facts.id.clone();

    // Preflight after the clone.
    let (installed, api_max, schema_range, wasi_sdk_present) = cx.update(|cx| {
        (
            steps.host.installed(cx),
            steps.host.api_max(cx),
            steps.host.schema_version_range(),
            steps.host.wasi_sdk_present(cx),
        )
    });
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
        let same_record = previous_record
            .as_ref()
            .is_some_and(|r| r.commit.eq_ignore_ascii_case(&commit));
        background!(cx, work, move |work| -> Result<(bool, PathBuf)> {
            place_checkout(work, &id, &commit, same_record)
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
    link.await.context("could not load the extension")?;
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
            Trust::Listed => None,
            Trust::Confirmed(_) => Some(commit.clone()),
        },
        state: RecordState::Installed,
        installed_at: rfc3339(steps.clock.now()),
        extra: previous_record.map(|r| r.extra).unwrap_or_default(),
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
        let restore = cx.update(|cx| match &previous_target {
            Some(previous) => steps.host.link(&id, previous.clone(), cx),
            None => steps.host.unlink(&id, cx),
        });
        if let Err(restore_err) = restore.await {
            return Err(err.context(format!(
                "restoring the previous link also failed: {restore_err:#}"
            )));
        }
        return Err(err);
    }

    // Done: the new checkout stays, the old one goes.
    {
        let id = id.clone();
        let commit = commit.clone();
        background!(cx, work, move |work| {
            work.checkout = None;
            if let Some(previous) = previous_target
                && let Some(old) = checkout_of(&work.layout, &id, &previous)
                && old != work.layout.checkout_dir(&id, &commit)
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

/// The checkout folder under `git/<id>/` that `target` lies in.
fn checkout_of(layout: &Layout, id: &str, target: &Path) -> Option<PathBuf> {
    let base = layout.extension_dir(id);
    let rest = target.strip_prefix(&base).ok()?;
    let first = rest.components().next()?;
    Some(base.join(first))
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
) -> Result<(bool, PathBuf)> {
    let staged = work.staged.take().expect("staged before placing");
    let relative = staged
        .root()
        .strip_prefix(staged.dir())
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let checkout = work.layout.checkout_dir(id, commit);
    let root = checkout.join(&relative);
    let linked = std::fs::read_link(work.layout.symlink_path(id)).ok();
    if same_record && linked.as_deref() == Some(root.as_path()) {
        drop(staged);
        return Ok((true, root));
    }
    if checkout.exists() {
        // Left behind by an earlier attempt; nothing links it.
        std::fs::remove_dir_all(&checkout)
            .with_context(|| format!("could not remove {}", checkout.display()))?;
    }
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
