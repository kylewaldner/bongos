//! The checks `bongos --engine-host <id>` makes before it starts, run in this process: the probe
//! starts only when work, project repositories, workflows and lanes are on, and `--read <name>`
//! needs only work and project repositories.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bongos_engine_host::ReadModes;
use bongos_project_repo::StateReader;
use gpui::{App, TestAppContext};
use settings::{Settings as _, SettingsStore};

use crate::engine_host::{Exit, Mode, Started, apply_user_settings, init_network, start};

const SETTINGS: [&str; 4] = ["work", "project_repository", "workflows", "lanes"];

/// Registers the settings the engine host checks and turns each of `SETTINGS` on except `off`.
fn with_settings(off: Option<&str>, cx: &mut App) {
    let store = SettingsStore::test(cx);
    cx.set_global(store);
    bongos_work::WorkSettings::register(cx);
    bongos_project_repo::ProjectRepositorySettings::register(cx);
    bongos_workflow::WorkflowsSettings::register(cx);
    bongos_lanes::LanesSettings::register(cx);
    let text = SETTINGS
        .iter()
        .map(|name| format!("\"{name}\": {{ \"enabled\": {} }}", Some(*name) != off))
        .collect::<Vec<_>>()
        .join(", ");
    let text = format!("{{ {text} }}");
    if let Err(exit) = apply_user_settings(&text, Path::new("settings.json"), cx) {
        panic!("the settings load: {exit:?}");
    }
}

/// A stand-in for the `status` mode the lanes register.
fn status(_args: &[String], _reader: &StateReader, _cx: &mut App) -> Result<String> {
    Ok("status".to_string())
}

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| word.to_string()).collect()
}

/// Starts `mode` for `project-1` with each setting on except `off`, against an empty record of
/// project repositories in a fresh temporary directory.
fn started(mode: Mode, off: Option<&str>, cx: &mut TestAppContext) -> Result<Started, Exit> {
    let temp = tempfile::tempdir().expect("a temporary directory");
    let locations: PathBuf = temp.path().join("locations.toml");
    cx.update(|cx| {
        with_settings(off, cx);
        ReadModes::register("status", status, cx);
        start("project-1", mode, Some(locations.as_path()), cx)
    })
}

fn exit_of(result: Result<Started, Exit>) -> Exit {
    match result {
        Err(exit) => exit,
        Ok(Started::Ready) => panic!("started ready"),
        Ok(Started::Host) => panic!("started hosting"),
        Ok(Started::Read(_)) => panic!("started the read mode"),
    }
}

fn assert_refused(off: &str, cx: &mut TestAppContext) {
    let exit = exit_of(started(Mode::Probe, Some(off), cx));
    assert_eq!(exit.code, 1, "{exit:?}");
    assert!(
        exit.message.contains(&format!("({off}.enabled)")),
        "{exit:?}"
    );
}

#[gpui::test]
fn engine_host_probe_is_ready_with_every_setting_on(cx: &mut TestAppContext) {
    assert!(matches!(started(Mode::Probe, None, cx), Ok(Started::Ready)));
}

#[gpui::test]
fn engine_host_probe_refuses_while_work_is_off(cx: &mut TestAppContext) {
    assert_refused("work", cx);
}

#[gpui::test]
fn engine_host_probe_refuses_while_project_repositories_are_off(cx: &mut TestAppContext) {
    assert_refused("project_repository", cx);
}

#[gpui::test]
fn engine_host_probe_refuses_while_workflows_are_off(cx: &mut TestAppContext) {
    assert_refused("workflows", cx);
}

#[gpui::test]
fn engine_host_probe_refuses_while_lanes_are_off(cx: &mut TestAppContext) {
    assert_refused("lanes", cx);
}

#[gpui::test]
fn a_read_of_a_name_nothing_is_registered_under_exits_two(cx: &mut TestAppContext) {
    let exit = exit_of(started(
        Mode::Read(words(&["nosuchmode", "--json"])),
        None,
        cx,
    ));
    assert_eq!(exit.code, 2, "{exit:?}");
    assert!(exit.message.contains("nosuchmode"), "{exit:?}");
}

#[gpui::test]
fn a_read_of_a_registered_name_goes_on_to_the_project(cx: &mut TestAppContext) {
    // `status` is registered, so this gets past the registry and stops at the project, which
    // the empty record of project repositories does not have.
    let exit = exit_of(started(Mode::Read(words(&["status", "--json"])), None, cx));
    assert_eq!(exit.code, 1, "{exit:?}");
    assert!(
        exit.message.contains("no project repository project-1"),
        "{exit:?}"
    );
}

#[gpui::test]
fn a_read_runs_with_lanes_and_workflows_off(cx: &mut TestAppContext) {
    // `profile` is registered by no feature yet: the exit is the unknown name's, not a refused
    // setting's.
    for off in ["lanes", "workflows"] {
        let exit = exit_of(started(
            Mode::Read(words(&["profile", "show"])),
            Some(off),
            cx,
        ));
        assert_eq!(exit.code, 2, "{off}: {exit:?}");
        assert!(exit.message.contains("profile"), "{off}: {exit:?}");
    }
}

#[gpui::test]
fn a_read_refuses_while_work_or_project_repositories_are_off(cx: &mut TestAppContext) {
    for off in ["work", "project_repository"] {
        let exit = exit_of(started(Mode::Read(words(&["status"])), Some(off), cx));
        assert_eq!(exit.code, 1, "{off}: {exit:?}");
        assert!(
            exit.message.contains(&format!("({off}.enabled)")),
            "{off}: {exit:?}"
        );
    }
}

#[gpui::test]
fn the_engine_host_gets_a_real_http_client_and_the_slack_workflow_steps(cx: &mut TestAppContext) {
    const MARKER: &str = "bongos-engine-host-test/0";
    let temp = tempfile::tempdir().expect("a temporary directory");
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        gpui_tokio::init(cx);
        if let Err(exit) = init_network(MARKER, temp.path().join("slack"), cx) {
            panic!("the network starts: {exit:?}");
        }
        let http = cx.http_client();
        assert_eq!(
            http.user_agent().and_then(|agent| agent.to_str().ok()),
            Some(MARKER),
            "the window's client, not gpui's placeholder"
        );
        let builtins = bongos_workflow::BuiltinNodes::global(cx);
        assert!(builtins.get("slack-read").is_some());
        assert!(builtins.get("slack-post").is_some());
        assert!(bongos_slack::SlackService::global(cx).is_some());
    });
}
