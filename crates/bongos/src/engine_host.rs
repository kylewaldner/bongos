//! `bongos --engine-host <project id>`: a process with no window that hosts the workflow engine
//! of one project. It starts only the parts the engine needs, so it runs without a display. It
//! takes the project's writer (or exits at once when another live process holds it), serves the
//! project's socket in host mode, runs the lanes' dispatcher, logs to
//! `<logs>/engine-host-<12 hex>.log`, and exits once idle for `lanes.host.idle_exit_seconds`.

use std::io::Write as _;
use std::{process, rc::Rc, sync::Arc};

use anyhow::anyhow;
use bongos_engine_host::{EngineHostProcess, HostProcessOptions, host_log_path, open_host_writer};
use bongos_lanes::LanesSettings;
use bongos_project_repo::{Location, RepositoryLocations, StateApplyHooks, StateWriter};
use bongos_work::{OpenedProject, ProjectOpening};
use fs::{Fs, RealFs};
use gpui::{App, UpdateGlobal as _};
use parking_lot::Mutex;
use release_channel::{AppCommitSha, AppVersion};
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

fn fail(code: i32, message: impl std::fmt::Display) -> ! {
    eprintln!("bongos engine host: {message}");
    process::exit(code)
}

/// Reads the user settings file once; a missing file counts as empty.
fn load_user_settings(cx: &mut App) {
    let text = match std::fs::read_to_string(paths::settings_file()) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".to_string(),
        Err(error) => fail(
            1,
            format!("cannot read {}: {error}", paths::settings_file().display()),
        ),
    };
    let parsed = SettingsStore::update_global(cx, |store, cx| store.set_user_settings(&text, cx));
    if let Err(error) = parsed.result() {
        fail(
            1,
            format!("{}: {error:#}", paths::settings_file().display()),
        );
    }
}

/// Starts the engine host for `project_id` and never returns. With `probe` it exits 0 once the
/// engine is initialized and every setting it needs is on.
pub fn run(project_id: String, probe: bool) -> ! {
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
        settings::init(cx);
        load_user_settings(cx);
        <dyn Fs>::set_global(fs.clone(), cx);
        let dirs = bongos_work::WorkDirs::new(util::paths::home_dir(), paths::data_dir());
        let locations = dirs.locations.clone();
        bongos_work::init(fs, dirs, cx);
        bongos_workflow::init(cx);
        bongos_checks::init(cx);
        bongos_lanes::init(cx);
        bongos_engine_host::init(cx);

        if let Some(message) = first_setting_off(cx) {
            fail(1, message);
        }
        if probe {
            println!("engine host ready");
            process::exit(0);
        }
        host(&project_id, locations, cx);
    });
    process::exit(0)
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
fn find_location(project_id: &str, locations: Option<std::path::PathBuf>) -> Location {
    let Some(path) = locations else {
        fail(1, "this build keeps no record of project repositories");
    };
    let record = match RepositoryLocations::load(path) {
        Ok(record) => record,
        Err(error) => fail(1, format!("reading the project repositories: {error}")),
    };
    match record
        .locations()
        .iter()
        .find(|location| location.id == project_id)
    {
        Some(location) => location.clone(),
        None => fail(1, format!("no project repository {project_id} is known")),
    }
}

/// Becomes the engine host of `project_id`: takes its writer (or exits when another live process
/// holds it), opens only that project, and serves it once it is open.
fn host(project_id: &str, locations: Option<std::path::PathBuf>, cx: &mut App) {
    init_log(project_id, cx);
    log::info!(
        "engine host of {project_id} starting (pid {})",
        process::id()
    );
    let location = find_location(project_id, locations);
    // After every feature registered its state hooks: opening the writer seals them.
    let writer = match open_host_writer(&location, StateApplyHooks::global()) {
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
    if let Err(error) = EngineHostProcess::start(repo, options, busy, on_exit, cx) {
        log::error!("serving the project: {error:#}");
        log::logger().flush();
        fail(1, format!("{error:#}"));
    }
    // The only workflow engine of the project runs here; it takes on the runs it finds.
    if bongos_workflow::WorkflowEngine::ensure(cx).is_none() {
        log::warn!("workflows are off; no runs start here");
    }
}
