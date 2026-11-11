#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
use std::process::Command;

fn main() {
    if std::env::var("ZED_UPDATE_EXPLANATION").is_ok() {
        println!(r#"cargo:rustc-cfg=feature="no-bundled-uninstall""#);
    }

    if cfg!(target_os = "macos") {
        println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET=10.15.7");
    }

    // Populate git sha environment variable if git is available
    watch_git_head();
    if let Some(output) = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
    {
        let git_sha = String::from_utf8_lossy(&output.stdout);
        let git_sha = git_sha.trim();

        println!("cargo:rustc-env=ZED_COMMIT_SHA={git_sha}");
    }
    if let Some(build_identifier) = option_env!("GITHUB_RUN_NUMBER") {
        println!("cargo:rustc-env=ZED_BUILD_ID={build_identifier}");
    }

    #[cfg(windows)]
    {
        println!("cargo:rerun-if-env-changed=RELEASE_CHANNEL");
        println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");

        windows_resources::compile(false).expect("failed to compile Windows resources");
    }
}

/// Tells cargo to rerun this script when the checked-out commit changes, so the
/// embedded `ZED_COMMIT_SHA` stays current.
///
/// Every path given to `rerun-if-changed` must exist: cargo treats a missing one
/// as changed on every build, which reruns this script and rebuilds the crate and
/// everything that depends on it. A fixed `../../.git/logs/HEAD` is missing in a
/// linked worktree (where `.git` is a file pointing at the real git directory) and
/// in a source tree without git, so the git directory is resolved instead:
/// - `HEAD` in the per-worktree git directory always exists, and changes on
///   checkout, and on commit when the head is detached;
/// - `logs/HEAD` next to it changes on every commit, checkout and reset;
/// - the loose branch ref holds the sha a branch commit moves, or `packed-refs`
///   when the branch exists only there.
///
/// Without git nothing git-related is watched and the build still succeeds.
fn watch_git_head() {
    println!("cargo:rerun-if-changed=build.rs");
    let Some(output) = Command::new("git")
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
    else {
        return;
    };
    let output = String::from_utf8_lossy(&output.stdout);
    let mut lines = output.lines().map(std::path::Path::new);
    let (Some(git_dir), Some(common_dir)) = (lines.next(), lines.next()) else {
        return;
    };
    let watch = |path: std::path::PathBuf| {
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
            true
        } else {
            false
        }
    };
    let head = git_dir.join("HEAD");
    let head_ref = std::fs::read_to_string(&head).ok();
    watch(head);
    watch(git_dir.join("logs").join("HEAD"));
    if let Some(branch) = head_ref
        .as_deref()
        .and_then(|head| head.trim().strip_prefix("ref: "))
        && !watch(common_dir.join(branch))
    {
        watch(common_dir.join("packed-refs"));
    }
}
