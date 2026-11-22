//! `bongos --engine-host <project id>`: a process with no window that hosts the workflow engine
//! of one project. It starts only the parts the engine needs, so it runs without a display.

use std::{process, sync::Arc};

use bongos_lanes::LanesSettings;
use fs::{Fs, RealFs};
use gpui::{App, UpdateGlobal as _};
use release_channel::{AppCommitSha, AppVersion};
use settings::{Settings, SettingsStore};

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
        bongos_work::init(
            fs,
            bongos_work::WorkDirs::new(util::paths::home_dir(), paths::data_dir()),
            cx,
        );
        bongos_workflow::init(cx);
        bongos_lanes::init(cx);

        if let Some(message) = first_setting_off(cx) {
            fail(1, message);
        }
        if probe {
            println!("engine host ready");
            process::exit(0);
        }
        fail(
            2,
            format!("hosting project {project_id} is not implemented yet"),
        );
    });
    process::exit(0)
}
