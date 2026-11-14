//! Only files that hold UI action handlers may build a `PersonAtKeyboard` from an action: every
//! other source file, which may hold a background, engine or agent write path, fails here. A file
//! that also lists the open windows fails too, since it could pick a window to act through.
//!
//! A later feature that adds an action-handler file adds it to `ALLOWED`.

use std::path::{Path, PathBuf};

/// The files, relative to the workspace's `crates/` directory, that may call `from_action`.
const ALLOWED: &[&str] = &[
    "bongos_work/src/commands.rs",
    "bongos_work/src/structure.rs",
    "bongos_builder/src/apply.rs",
    "bongos_builder/src/plan.rs",
    "bongos_builder/src/auto_apply.rs",
    "bongos_linear/src/subtickets_actions.rs",
    "bongos_linear/src/link.rs",
    "bongos_project_repo/src/commands.rs",
    "bongos_project_repo/src/landing_view.rs",
];

/// The one file that defines `from_action`.
const DEFINITION: &str = "bongos_work_model/src/author.rs";

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crates directory")
        .to_path_buf()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != "tests" && name != "node_modules" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn is_test_file(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    name.contains("_tests") || name == "tests.rs" || name == "test_support.rs"
}

#[test]
fn from_action_is_called_only_in_the_files_that_hold_action_handlers() {
    let crates = crates_dir();
    let mut files = Vec::new();
    for crate_dir in std::fs::read_dir(&crates).unwrap().flatten() {
        let src = crate_dir.path().join("src");
        if src.is_dir()
            && crate_dir
                .file_name()
                .to_string_lossy()
                .starts_with("bongos")
        {
            rust_files(&src, &mut files);
        }
    }
    let mut seen = Vec::new();
    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&crates)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if relative == DEFINITION || is_test_file(&relative) {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let code = &text;
        // Any call, however the type is named: a full path, an alias or `Self`.
        if !code.contains("from_action(") {
            continue;
        }
        seen.push(relative.clone());
        if !ALLOWED.contains(&relative.as_str()) {
            offenders.push(format!("{relative}: not an allowed action-handler file"));
        }
        if code.contains("cx.windows()") {
            offenders.push(format!("{relative}: calls from_action and cx.windows()"));
        }
    }
    assert!(offenders.is_empty(), "{offenders:#?}");
    // The list is not stale: the files of this crate and the linear crate call it.
    for expected in [
        "bongos_work/src/commands.rs",
        "bongos_linear/src/subtickets_actions.rs",
    ] {
        assert!(
            seen.iter().any(|file| file == expected),
            "{expected}: {seen:?}"
        );
    }
}

#[test]
fn the_background_write_paths_do_not_hold_the_marker() {
    let crates = crates_dir();
    for relative in [
        "bongos_builder/src/later.rs",
        "bongos_linear/src/subtickets.rs",
        "bongos_linear/src/refresh.rs",
        "bongos_linear/src/status.rs",
        "bongos_chat/src/live_modes.rs",
    ] {
        let text = std::fs::read_to_string(crates.join(relative)).unwrap();
        assert!(
            !text.contains("PersonAtKeyboard"),
            "{relative} is a background write path"
        );
    }
}
