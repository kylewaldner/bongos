//! Runs the built binary as `--engine-host <id> --probe` with no display, in a temporary home,
//! and checks that it starts only when work, project repositories, workflows and lanes are on.
//! Also runs it as `--engine-host <id> --read <name>`, which needs only work and project
//! repositories.
// Each test waits on the child process it starts; there is no executor to block.
#![allow(clippy::disallowed_methods)]

use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_bongos");

const SETTINGS: [&str; 4] = ["work", "project_repository", "workflows", "lanes"];

/// Runs the probe with each setting of `SETTINGS` on except `off`, in fresh temporary dirs.
fn probe(off: Option<&str>) -> Output {
    run(&["--probe"], off)
}

/// Runs `--engine-host project-1` with `flags`, with each setting of `SETTINGS` on except `off`.
fn run(flags: &[&str], off: Option<&str>) -> Output {
    let temp = tempfile::tempdir().expect("a temporary directory");
    let root = temp.path();
    let dir = |name: &str| {
        let path = root.join(name);
        std::fs::create_dir_all(&path).expect("a temporary subdirectory");
        path
    };
    let home = dir("home");
    let user_data = dir("user-data");
    let config = dir("user-data/config");
    let settings = SETTINGS
        .iter()
        .map(|name| format!("\"{name}\": {{ \"enabled\": {} }}", Some(*name) != off))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(config.join("settings.json"), format!("{{ {settings} }}"))
        .expect("the settings file");

    Command::new(BINARY)
        .arg("--engine-host")
        .arg("project-1")
        .arg("--user-data-dir")
        .arg(&user_data)
        .args(flags)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", dir("xdg-config"))
        .env("XDG_DATA_HOME", dir("xdg-data"))
        .env("XDG_STATE_HOME", dir("xdg-state"))
        .env("XDG_CACHE_HOME", dir("xdg-cache"))
        .env("XDG_RUNTIME_DIR", dir("xdg-runtime"))
        .current_dir(root)
        .output()
        .expect("the binary runs")
}

fn text(output: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_refused(off: &str) {
    let output = probe(Some(off));
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let named = format!("({off}.enabled)");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&named)
            || String::from_utf8_lossy(&output.stdout).contains(&named),
        "names {named}: {}",
        text(&output)
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("engine host ready"),
        "{}",
        text(&output)
    );
}

#[test]
fn engine_host_probe_is_ready_with_every_setting_on() {
    let output = probe(None);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("engine host ready"),
        "{}",
        text(&output)
    );
}

#[test]
fn engine_host_probe_refuses_while_work_is_off() {
    assert_refused("work");
}

#[test]
fn engine_host_probe_refuses_while_project_repositories_are_off() {
    assert_refused("project_repository");
}

#[test]
fn engine_host_probe_refuses_while_workflows_are_off() {
    assert_refused("workflows");
}

#[test]
fn engine_host_probe_refuses_while_lanes_are_off() {
    assert_refused("lanes");
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_read_of_a_name_nothing_is_registered_under_exits_two() {
    let output = run(&["--read", "nosuchmode", "--json"], None);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(stderr(&output).contains("nosuchmode"), "{}", text(&output));
    assert!(output.stdout.is_empty(), "{}", text(&output));
}

#[test]
fn a_read_of_a_registered_name_goes_on_to_the_project() {
    // `status` is registered, so this gets past the registry and stops at the project, which
    // this temporary home does not have.
    let output = run(&["--read", "status", "--json"], None);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        stderr(&output).contains("no project repository project-1"),
        "{}",
        text(&output)
    );
}

#[test]
fn a_read_runs_with_lanes_and_workflows_off() {
    // `profile` is registered by no feature yet: the exit is the unknown name's, not a refused
    // setting's.
    for off in ["lanes", "workflows"] {
        let output = run(&["--read", "profile", "show"], Some(off));
        assert_eq!(output.status.code(), Some(2), "{off}: {}", text(&output));
    }
}

#[test]
fn a_read_refuses_while_work_or_project_repositories_are_off() {
    for off in ["work", "project_repository"] {
        let output = run(&["--read", "status"], Some(off));
        assert_eq!(output.status.code(), Some(1), "{off}: {}", text(&output));
        assert!(
            stderr(&output).contains(&format!("({off}.enabled)")),
            "{off}: {}",
            text(&output)
        );
    }
}
