//! The release check of the language list, run on a list of local git repositories: a grammar-free
//! extension passes and one that needs extension API 0.99 fails before any build. No network.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use bongos_extensions::git::RealGit;
use bongos_extensions::installer::FixedTools;
use bongos_extensions::list_check::{
    CheckContext, ListBuilder, check_list, load_list, stable_api_range,
};
use bongos_extensions_model::{ApiVersion, Probe, ToolProbe};
use tempfile::TempDir;
use util::command::{Stdio, new_command};

/// Counts builds and answers with the API version the fixture asked for.
struct FakeBuilder {
    builds: Arc<AtomicUsize>,
}

#[async_trait]
impl ListBuilder for FakeBuilder {
    async fn build(&self, root: &Path) -> Result<Option<ApiVersion>> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        assert!(root.join("extension.toml").is_file());
        Ok(Some(ApiVersion::new(0, 6, 0)))
    }
}

fn git(dir: &Path, home: &Path, args: &[&str]) -> String {
    let output = smol::block_on(
        new_command("git")
            .current_dir(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("HOME", home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .stdin(Stdio::null())
            .output(),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A local repository holding one extension; returns its commit.
fn fixture_repo(root: &Path, home: &Path, id: &str, api: &str) -> String {
    let repo = root.join(id);
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, home, &["init", "--quiet", "-b", "main"]);
    std::fs::write(
        repo.join("extension.toml"),
        format!("id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\nschema_version = 1\n"),
    )
    .unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{id}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\ncrate-type = [\"cdylib\"]\n\n[dependencies]\nzed_extension_api = \"{api}\"\n"
        ),
    )
    .unwrap();
    git(&repo, home, &["add", "."]);
    git(&repo, home, &["commit", "--quiet", "-m", "fixture"]);
    git(&repo, home, &["rev-parse", "HEAD"])
}

#[test]
fn list_check_passes_a_good_extension_and_fails_api_0_99_before_building() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let good_commit = fixture_repo(temp.path(), &home, "good-fixture", "0.6.0");
    let bad_commit = fixture_repo(temp.path(), &home, "new-fixture", "0.99.0");
    let entry = |id: &str, commit: &str| {
        format!(
            "[[extension]]\nid = \"{id}\"\nname = \"{id}\"\ndescription = \"A fixture.\"\nrepository = \"file://{}/{id}\"\ncommit = \"{commit}\"\n",
            temp.path().display()
        )
    };
    let list_file = temp.path().join("languages.toml");
    std::fs::write(
        &list_file,
        [
            entry("good-fixture", &good_commit),
            entry("new-fixture", &bad_commit),
        ]
        .join("\n"),
    )
    .unwrap();
    let list = load_list(Some(&format!("file://{}", list_file.display()))).unwrap();

    let builds = Arc::new(AtomicUsize::new(0));
    let builder = FakeBuilder {
        builds: builds.clone(),
    };
    let git_runner = RealGit::with_env(vec![
        ("HOME".into(), home.into_os_string()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]);
    let tools = FixedTools(ToolProbe {
        git: Probe::Found,
        cargo: Probe::Found,
        rustc_sysroot: Probe::Found,
        wasm_target: Probe::Found,
        rustup: Probe::Found,
    });
    let scratch = temp.path().join("scratch");
    let context = CheckContext {
        git: &git_runner,
        tools: &tools,
        builder: &builder,
        scratch: &scratch,
        api_range: stable_api_range(),
        schema_version_range: 0..=1,
        is_blocked_host: bongos_net::is_blocked_host,
        is_suppressed: extension_host::is_suppressed_extension,
    };
    let reports = smol::block_on(check_list(&list, &context));

    assert_eq!(reports.len(), 2);
    assert!(reports[0].passed(), "{}", reports[0]);
    assert!(!reports[1].passed(), "{}", reports[1]);
    let line = reports[1].to_string();
    assert!(line.starts_with("FAIL  new-fixture"), "{line}");
    assert!(line.contains("0.99.0"), "{line}");
    assert_eq!(
        builds.load(Ordering::SeqCst),
        1,
        "the 0.99 fixture was never built"
    );
}
