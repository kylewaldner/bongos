//! `bongos --engine-host <project id>`: a process with no window that hosts the workflow engine
//! of one project. It starts only the parts the engine needs, so it runs without a display. It
//! takes the project's writer (from a window that holds it, by a handover; it exits at once when
//! another engine host holds it), serves the project's socket in host mode, runs the lanes'
//! dispatcher, logs to `<logs>/engine-host-<12 hex>.log`, and exits once idle for
//! `lanes.host.idle_exit_seconds`.
//!
//! With `--read <name> [args...]` the same headless start runs one read mode instead (see
//! [`bongos_engine_host::ReadModes`]): it opens the project's store for reading only, never as
//! the writer, prints what the mode returns on stdout and exits. Exit codes: 0 done, 1 a setting
//! is off or the project cannot be read, 2 no read mode of that name is registered.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::{process, rc::Rc, sync::Arc};

use anyhow::anyhow;
use bongos_engine_host::{
    ClientOptions, EngineHostProcess, HostProcessOptions, ReadModes, ReadProject, host_log_path,
    take_host_writer,
};
use bongos_lanes::LanesSettings;
use bongos_project_repo::{
    ClientKind, Location, RepositoryLocations, StateApplyHooks, StateStore, StateWriter,
};
use bongos_work::{OpenedProject, ProjectOpening};
use client::ProxySettings;
use fs::{Fs, RealFs};
use gpui::{App, Task, UpdateGlobal as _};
use gpui_tokio::Tokio;
use http_client::HttpClient;
use parking_lot::Mutex;
use release_channel::{AppCommitSha, AppVersion};
use reqwest_client::ReqwestClient;
use settings::{Settings, SettingsStore};
use util::rotating_log::RotatingLogFile;

/// The message naming the first setting the engine host needs that is off.
fn first_setting_off(cx: &App) -> Option<&'static str> {
    if !bongos_work::WorkSettings::get_global(cx).enabled {
        Some("work is off (work.enabled)")
    } else if !bongos_project_repo::ProjectRepositorySettings::get_global(cx).enabled {
        Some("project repositories are off (project_repository.enabled)")
    } else if !bongos_workflow::WorkflowsSettings::get_global(cx).enabled {
        Some("workflows are off (workflows.enabled)")
    } else if !LanesSettings::get_global(cx).enabled {
        Some("lanes are off (lanes.enabled)")
    } else {
        None
    }
}

/// The message naming the first setting a read mode needs that is off: the lanes' own settings
/// are checked by the command line and by the features whose modes these are.
fn first_read_setting_off(cx: &App) -> Option<&'static str> {
    if !bongos_work::WorkSettings::get_global(cx).enabled {
        Some("work is off (work.enabled)")
    } else if !bongos_project_repo::ProjectRepositorySettings::get_global(cx).enabled {
        Some("project repositories are off (project_repository.enabled)")
    } else {
        None
    }
}

/// What this process does once its engine is initialized.
pub enum Mode {
    Host,
    /// Exit 0 once the engine is initialized and every setting it needs is on.
    Probe,
    /// Run the read mode named first, with the arguments after it.
    Read(Vec<String>),
}

/// How the process ends early: its exit code and the line it prints on stderr.
#[derive(Debug)]
pub(crate) struct Exit {
    pub(crate) code: i32,
    pub(crate) message: String,
}

impl Exit {
    fn new(code: i32, message: impl std::fmt::Display) -> Self {
        Self {
            code,
            message: format!("bongos engine host: {message}"),
        }
    }

    fn exit(self) -> ! {
        eprintln!("{}", self.message);
        process::exit(self.code)
    }
}

fn fail(code: i32, message: impl std::fmt::Display) -> ! {
    Exit::new(code, message).exit()
}

/// What the process goes on to do once its settings are checked.
pub(crate) enum Started {
    /// The probe: every setting the engine host needs is on.
    Ready,
    /// Become the engine host of the project.
    Host,
    /// The read mode runs; the task ends with what it prints.
    Read(Task<anyhow::Result<String>>),
}

/// Reads the user settings file once; a missing file counts as empty.
fn load_user_settings(cx: &mut App) {
    let path = paths::settings_file();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
        Err(error) => fail(1, format!("cannot read {}: {error}", path.display())),
    };
    if let Err(exit) = apply_user_settings(&text, path, cx) {
        exit.exit();
    }
}

/// Applies the text of the user settings file at `path`.
pub(crate) fn apply_user_settings(text: &str, path: &Path, cx: &mut App) -> Result<(), Exit> {
    let parsed = SettingsStore::update_global(cx, |store, cx| store.set_user_settings(text, cx));
    if let Err(error) = parsed.result() {
        return Err(Exit::new(1, format!("{}: {error:#}", path.display())));
    }
    Ok(())
}

/// The application's HTTP client, as every bongos process builds it: reqwest with the `proxy`
/// setting (else the environment's proxy) and `user_agent`, behind the guard that refuses Zed's
/// hosts. Needs the Tokio runtime ([`gpui_tokio::init`]) and the settings.
pub(crate) fn guarded_http_client(
    user_agent: &str,
    cx: &mut App,
) -> anyhow::Result<Arc<dyn HttpClient>> {
    let proxy_url = ProxySettings::get_global(cx).proxy_url();
    let http = {
        let _guard = Tokio::handle(cx).enter();
        ReqwestClient::proxy_and_user_agent(proxy_url, user_agent)?
    };
    Ok(Arc::new(bongos_net::GuardedHttpClient::new(Arc::new(http))))
}

/// Gives the engine host the window's HTTP client, built with `user_agent` (a client without
/// one is gpui's placeholder, over which Slack makes no request), and the Slack of an engine
/// host with its files in `slack_dir`: the `slack-read` and `slack-post` workflow steps, which
/// read `connection.toml` and the user settings file again before every try. Call once the user
/// settings are loaded.
pub(crate) fn init_network(user_agent: &str, slack_dir: PathBuf, cx: &mut App) -> Result<(), Exit> {
    let http = guarded_http_client(user_agent, cx)
        .map_err(|error| Exit::new(1, format!("could not start the HTTP client: {error:#}")))?;
    cx.set_http_client(http);
    bongos_slack::init_engine_host_in(slack_dir, paths::settings_file().clone(), cx);
    Ok(())
}

/// Checks the settings `mode` needs and, for a read mode, starts it. `locations` is the record of
/// project repositories.
pub(crate) fn start(
    project_id: &str,
    mode: Mode,
    locations: Option<&Path>,
    cx: &mut App,
) -> Result<Started, Exit> {
    let checked = |cx: &App| match first_setting_off(cx) {
        Some(message) => Err(Exit::new(1, message)),
        None => Ok(()),
    };
    match mode {
        Mode::Probe => checked(&*cx).map(|()| Started::Ready),
        Mode::Host => checked(&*cx).map(|()| Started::Host),
        Mode::Read(words) => {
            if let Some(message) = first_read_setting_off(cx) {
                return Err(Exit::new(1, message));
            }
            read(project_id, &words, locations, cx).map(Started::Read)
        }
    }
}

/// Starts the engine host for `project_id` and never returns.
pub fn run(project_id: String, mode: Mode) -> ! {
    let file_errors = crate::init_paths();
    if !file_errors.is_empty() {
        fail(1, format!("cannot create directories: {file_errors:?}"));
    }
    let app_commit_sha =
        option_env!("ZED_COMMIT_SHA").map(|commit_sha| AppCommitSha::new(commit_sha.to_string()));
    let app_version = AppVersion::load(
        env!("CARGO_PKG_VERSION"),
        option_env!("ZED_BUILD_ID"),
        app_commit_sha,
    );

    let app = gpui_platform::headless();
    let app_db = db::AppDatabase::new();
    let fs = Arc::new(RealFs::new(None, app.background_executor()));

    app.run(move |cx| {
        cx.set_global(app_db);
        release_channel::init(app_version, cx);
        gpui_tokio::init(cx);
        settings::init(cx);
        load_user_settings(cx);
        let user_agent = bongos_version::user_agent(std::env::consts::OS, std::env::consts::ARCH);
        if let Err(exit) = init_network(&user_agent, bongos_slack::connection::default_dir(), cx) {
            exit.exit();
        }
        <dyn Fs>::set_global(fs.clone(), cx);
        let dirs = bongos_work::WorkDirs::new(util::paths::home_dir(), paths::data_dir());
        let locations = dirs.locations.clone();
        bongos_work::init(fs, dirs, cx);
        bongos_workflow::init(cx);
        bongos_checks::init(cx);
        bongos_lanes::init(cx);
        bongos_engine_host::init(cx);

        match start(&project_id, mode, locations.as_deref(), cx) {
            Err(exit) => exit.exit(),
            Ok(Started::Ready) => {
                println!("engine host ready");
                process::exit(0);
            }
            Ok(Started::Host) => host(&project_id, locations.as_deref(), cx),
            Ok(Started::Read(running)) => cx
                .spawn(async move |_| match running.await {
                    Ok(output) => {
                        let mut stdout = std::io::stdout().lock();
                        let written = writeln!(stdout, "{}", output.trim_end_matches('\n'))
                            .and_then(|()| stdout.flush());
                        if let Err(error) = written {
                            fail(1, format!("writing the output: {error}"));
                        }
                        process::exit(0);
                    }
                    Err(error) => fail(1, format!("{error:#}")),
                })
                .detach(),
        }
    });
    process::exit(0)
}

/// Starts the read mode named by the first of `words` against the project's store, opened for
/// reading only; the task ends with the mode's output.
fn read(
    project_id: &str,
    words: &[String],
    locations: Option<&Path>,
    cx: &mut App,
) -> Result<Task<anyhow::Result<String>>, Exit> {
    // Work's startup opens every known project, by default as its writer. A read mode never takes
    // a project's writer lock, so every such open is refused here (before that startup task runs,
    // which is once this setup returns); the mode reads through its own reader below.
    let refuse = Arc::new(|location: &Location| -> anyhow::Result<OpenedProject> {
        Err(anyhow!(
            "a read mode opens no project as writer ({})",
            location.id
        ))
    });
    ProjectOpening::install(refuse, None, cx);
    let Some((name, args)) = words.split_first() else {
        return Err(Exit::new(2, "--read needs the name of a read mode"));
    };
    if !ReadModes::is_registered(name, cx) {
        return Err(Exit {
            code: 2,
            message: format!("bongos: no read mode {name} is registered"),
        });
    }
    let location = find_location(project_id, locations)?;
    ReadProject::set(location.clone(), cx);
    let reader =
        StateStore::open_reader(&location).map_err(|error| Exit::new(1, format!("{error:#}")))?;
    let Some(running) = ReadModes::run(name, args, &reader, cx) else {
        return Err(Exit::new(2, format!("no read mode {name} is registered")));
    };
    Ok(cx.spawn(async move |_| {
        // The mode was handed the reader for the call only; it stays open until the answer is in.
        let _reader = reader;
        running.await
    }))
}

/// Sends this process's log to the project's engine host log, rotated at
/// `lanes.host.log_max_bytes`.
fn init_log(project_id: &str, cx: &App) {
    let path = host_log_path(paths::logs_dir(), project_id);
    let max_bytes = LanesSettings::get_global(cx).host.log_max_bytes;
    let file = match RotatingLogFile::open(&path, max_bytes) {
        Ok(file) => file,
        Err(error) => fail(1, format!("{error:#}")),
    };
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Info)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} {} {}: {}",
                chrono::Utc::now().to_rfc3339(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .target(env_logger::Target::Pipe(Box::new(file)))
        .init();
}

/// The location of project `project_id` in the record of project repositories.
fn find_location(project_id: &str, locations: Option<&Path>) -> Result<Location, Exit> {
    let Some(path) = locations else {
        return Err(Exit::new(
            1,
            "this build keeps no record of project repositories",
        ));
    };
    let record = RepositoryLocations::load(path)
        .map_err(|error| Exit::new(1, format!("reading the project repositories: {error}")))?;
    record
        .locations()
        .iter()
        .find(|location| location.id == project_id)
        .cloned()
        .ok_or_else(|| Exit::new(1, format!("no project repository {project_id} is known")))
}

/// Becomes the engine host of `project_id`: takes its writer (or exits when another live process
/// holds it), opens only that project, and serves it once it is open.
fn host(project_id: &str, locations: Option<&Path>, cx: &mut App) {
    init_log(project_id, cx);
    log::info!(
        "engine host of {project_id} starting (pid {})",
        process::id()
    );
    let location = match find_location(project_id, locations) {
        Ok(location) => location,
        Err(exit) => exit.exit(),
    };
    // After every feature registered its state hooks: opening the writer seals them. A window
    // that holds the writer is asked to hand it over; this waits (blocking) until it lets go.
    let start_timeout = LanesSettings::get_global(cx).host.start_timeout;
    let mut options = ClientOptions::new(
        ClientKind::Cli,
        AppVersion::global(cx).to_string(),
        host_log_path(paths::logs_dir(), project_id),
    );
    options.moving_timeout = start_timeout;
    let writer = match take_host_writer(
        &location,
        StateApplyHooks::global(),
        options,
        start_timeout,
        cx.background_executor().clone(),
    ) {
        Ok(writer) => Arc::new(writer),
        Err(message) => {
            log::info!("refused: {message}");
            log::logger().flush();
            fail(1, message);
        }
    };
    log::info!("took the writer of {project_id}");

    let held: Arc<Mutex<Option<Arc<StateWriter>>>> = Arc::new(Mutex::new(Some(writer)));
    let id = project_id.to_string();
    let opener = Arc::new(move |opening: &Location| {
        if opening.id != id {
            return Err(anyhow!("the engine host of {id} opens no other project"));
        }
        held.lock()
            .take()
            .map(OpenedProject::Writer)
            .ok_or_else(|| anyhow!("the engine host opened {id} already"))
    });
    let id = project_id.to_string();
    let served = Rc::new(
        move |loaded: &Arc<bongos_project_repo::LoadedProject>, cx: &mut App| {
            let repo = loaded.repo().clone();
            if repo.location().id != id {
                return;
            }
            serve(repo, cx);
        },
    );
    ProjectOpening::install(opener, Some(served), cx);

    // A project that cannot be opened is only logged where it is opened; the host then holds a
    // writer it never serves, so it gives the lock back by exiting.
    let id = project_id.to_string();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(OPEN_TIMEOUT).await;
        if !cx.update(|cx| EngineHostProcess::is_running(cx)) {
            log::error!(
                "{id} was not open after {}s; exiting",
                OPEN_TIMEOUT.as_secs()
            );
            log::logger().flush();
            fail(1, format!("could not open {id}"));
        }
    })
    .detach();
}

/// How long the engine host may take to open its project before it gives up.
const OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Serves the opened project: `host.json`, the socket, the workflow engine, the lanes' commands,
/// and the idle exit.
fn serve(repo: bongos_project_repo::ProjectRepo, cx: &mut App) {
    let id = repo.location().id.clone();
    let options = HostProcessOptions {
        bongos_version: AppVersion::global(cx).to_string(),
        hosts_dir: bongos_engine_host::default_hosts_dir(),
        idle_exit: LanesSettings::get_global(cx).host.idle_exit,
    };
    let on_exit = Box::new(move |_: &mut App| {
        log::info!("engine host of {id} stopped");
        log::logger().flush();
        process::exit(0);
    });
    let busy = Rc::new(|cx: &App| bongos_lanes::dispatcher_running(cx));
    let drain = Rc::new(|cx: &mut App| bongos_lanes::drain_dispatcher(cx));
    if let Err(error) = EngineHostProcess::start(repo, options, busy, drain, on_exit, cx) {
        log::error!("serving the project: {error:#}");
        log::logger().flush();
        fail(1, format!("{error:#}"));
    }
    // The only workflow engine of the project runs here; it takes on the runs it finds.
    if bongos_workflow::WorkflowEngine::ensure(cx).is_none() {
        log::warn!("workflows are off; no runs start here");
    }
}
