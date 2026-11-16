//! Tests of the installer with a fake git and a fake host: no network, no compiler, and a pinned
//! clock.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use bongos_extensions_model::{
    ApiVersion, InstallRecord, LanguageList, Layout, Probe, ReadOutcome, SourceUrl, ToolProbe,
};
use futures::channel::oneshot;
use gpui::{Entity, TestAppContext};
use parking_lot::Mutex;
use tempfile::TempDir;

use super::*;
use crate::build_log::FixedClock;
use crate::git::{FakeGit, GitRunner, RemoteRef};
use crate::host::{FakeBuild, FakeHost};

const URL: &str = "https://example.com/acme/swift-extension";
const OTHER_URL: &str = "https://example.org/someone/swift-fork";
const RUBY_URL: &str = "https://example.com/acme/ruby-extension";
const C1: &str = "1111111111111111111111111111111111111111";
const C2: &str = "2222222222222222222222222222222222222222";
const C3: &str = "3333333333333333333333333333333333333333";
/// 2023-11-14T22:13:20Z, pinned so no test depends on the real date.
const NOW_SECS: u64 = 1_700_000_000;
const NOW_TEXT: &str = "2023-11-14T22:13:20Z";

const SWIFT_MANIFEST: &str = r#"
id = "swift"
name = "Swift"
version = "0.1.0"
schema_version = 1

[grammars.swift]
repository = "https://github.com/alex-pinkus/tree-sitter-swift"
commit = "0000000000000000000000000000000000000000"
"#;

const CARGO_TOML: &str = r#"
[package]
name = "swift"
version = "0.1.0"

[dependencies]
zed_extension_api = "0.7.0"
"#;

/// A git that serves file contents for each commit on top of [`FakeGit`].
#[derive(Default)]
struct ContentGit {
    inner: FakeGit,
    files: Mutex<HashMap<String, Vec<(String, String)>>>,
}

impl ContentGit {
    fn set_files(&self, commit: &str, files: &[(&str, &str)]) {
        self.files.lock().insert(
            commit.to_string(),
            files
                .iter()
                .map(|(p, t)| (p.to_string(), t.to_string()))
                .collect(),
        );
    }
}

#[async_trait]
impl GitRunner for ContentGit {
    async fn ls_remote(&self, url: &SourceUrl) -> Result<Vec<RemoteRef>> {
        self.inner.ls_remote(url).await
    }

    async fn fetch_commit(&self, url: &SourceUrl, commit: &str, dir: &Path) -> Result<()> {
        self.inner.fetch_commit(url, commit, dir).await?;
        let files = self.files.lock().get(commit).cloned().unwrap_or_default();
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, text)?;
        }
        Ok(())
    }

    async fn clone_local(&self, src: &Path, dir: &Path) -> Result<()> {
        self.inner.clone_local(src, dir).await
    }

    async fn remote_url(&self, dir: &Path) -> Result<String> {
        self.inner.remote_url(dir).await
    }

    async fn head(&self, dir: &Path) -> Result<String> {
        self.inner.head(dir).await
    }
}

fn url(text: &str) -> SourceUrl {
    SourceUrl::parse(text).unwrap()
}

fn failing_record_writer(_: &Path, _: &InstallRecord, _: &str) -> std::io::Result<()> {
    Err(std::io::Error::other("no space left on device"))
}

struct Harness {
    _temp: TempDir,
    layout: Layout,
    git: Arc<ContentGit>,
    host: Rc<FakeHost>,
    installer: Entity<Installer>,
}

impl Harness {
    fn new(cx: &mut TestAppContext) -> Self {
        let temp = TempDir::new().unwrap();
        let layout = Layout::new(temp.path().join("extensions"));
        let git = Arc::new(ContentGit::default());
        git.inner
            .set_refs(&url(URL), &[("HEAD", C1), ("refs/tags/v1", C1)]);
        git.inner.set_refs(&url(OTHER_URL), &[("HEAD", C3)]);
        git.inner.set_refs(&url(RUBY_URL), &[("HEAD", C3)]);
        let swift_files = [
            ("extension.toml", SWIFT_MANIFEST),
            ("Cargo.toml", CARGO_TOML),
        ];
        git.set_files(C1, &swift_files);
        git.set_files(C2, &swift_files);
        git.set_files(
            C3,
            &[(
                "extension.toml",
                "id = \"ruby\"\nname = \"Ruby\"\nversion = \"0.1.0\"\nschema_version = 1\n",
            )],
        );
        let host = Rc::new(FakeHost::new(layout.clone(), ApiVersion::new(0, 7, 0)));
        let list = LanguageList::parse(&format!(
            "[[extension]]\nid = \"swift\"\nname = \"Swift\"\ndescription = \"Swift.\"\nrepository = \"{URL}\"\ncommit = \"{C1}\"\n"
        ))
        .unwrap();
        let deps = InstallerDeps {
            layout: layout.clone(),
            git: git.clone(),
            host: host.clone(),
            tools: Arc::new(FixedTools(ToolProbe {
                git: Probe::Found,
                cargo: Probe::Found,
                rustc_sysroot: Probe::Found,
                wasm_target: Probe::Found,
                rustup: Probe::Found,
            })),
            clock: Arc::new(FixedClock(UNIX_EPOCH + Duration::from_secs(NOW_SECS))),
            list: Arc::new(list),
            is_blocked_host: bongos_net::is_blocked_host,
            write_record: bongos_extensions_model::write_atomic,
        };
        let installer = cx.new(|_| Installer::new(deps));
        Self {
            _temp: temp,
            layout,
            git,
            host,
            installer,
        }
    }

    fn install(
        &self,
        source: &str,
        rev: Option<&str>,
        trust: Trust,
        cx: &mut TestAppContext,
    ) -> InstallTask {
        self.installer.update(cx, |installer, cx| {
            installer.install(source, rev, None, trust, cx)
        })
    }

    fn phases(&self, cx: &mut TestAppContext) -> Vec<Phase> {
        self.installer.read_with(cx, |installer, _| {
            installer.operations().map(|op| op.phase.clone()).collect()
        })
    }

    fn link_target(&self, id: &str) -> Option<PathBuf> {
        std::fs::read_link(self.layout.symlink_path(id)).ok()
    }

    fn record(&self, id: &str) -> Option<InstallRecord> {
        match bongos_extensions_model::read(&self.layout.record_path(id)).unwrap() {
            ReadOutcome::Ok(record) => Some(record),
            _ => None,
        }
    }

    fn staging_is_empty(&self) -> bool {
        std::fs::read_dir(self.layout.staging_dir())
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true)
    }

    fn hold_lock(&self, key: &str) -> File {
        let path = self.layout.lock_path(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap();
        file.try_lock().unwrap();
        file
    }
}

fn pending_build(host: &FakeHost) -> oneshot::Sender<Result<Option<ApiVersion>, String>> {
    let (sender, receiver) = oneshot::channel();
    host.push_build(FakeBuild::Pending(receiver));
    sender
}

#[gpui::test]
async fn listed_install_end_to_end(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    assert!(!h.layout.git_dir().exists(), "first use: no git folder yet");

    let phases = Rc::new(RefCell::new(Vec::<Phase>::new()));
    cx.update({
        let phases = phases.clone();
        |cx| {
            cx.observe(&h.installer, move |installer, cx| {
                for op in installer.read(cx).operations() {
                    let mut phases = phases.borrow_mut();
                    if phases.last() != Some(&op.phase) {
                        phases.push(op.phase.clone());
                    }
                }
            })
            .detach();
        }
    });

    h.host
        .push_build(FakeBuild::Ok(Some(ApiVersion::new(0, 7, 0))));
    let task = h.install(URL, Some("v1"), Trust::Listed, cx);
    cx.run_until_parked();
    let installed = task.await.expect("the install succeeds");

    assert_eq!(installed.id, "swift");
    assert_eq!(installed.commit, C1);
    assert!(!installed.already_installed);
    let checkout = h.layout.checkout_dir("swift", C1);
    assert_eq!(h.link_target("swift"), Some(checkout.clone()));
    assert!(checkout.join("extension.toml").is_file());
    assert!(h.staging_is_empty());
    assert_eq!(h.host.compiled(), vec![checkout.clone()]);

    let record = h.record("swift").expect("the record is written");
    assert_eq!(record.source_url, url(URL).as_str());
    assert_eq!(record.commit, C1);
    assert_eq!(record.requested_ref.as_deref(), Some("v1"));
    assert!(record.listed);
    assert_eq!(record.trusted_commit, None);
    assert_eq!(record.installed_at, NOW_TEXT);

    assert_eq!(installed.log_path, h.layout.log_path("swift"));
    let log = std::fs::read_to_string(&installed.log_path).unwrap();
    assert!(log.contains(&format!("{NOW_TEXT} resolve")), "{log}");
    assert!(
        log.contains(&format!("$ git ls-remote --symref -- {}", url(URL))),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "$ git fetch --quiet --no-tags --depth 1 origin {C1}"
        )),
        "{log}"
    );
    assert!(
        log.contains("downloading the WASI SDK from github.com"),
        "{log}"
    );
    assert!(log.contains("built with extension API 0.7.0"), "{log}");
    assert!(log.contains(&format!("installed swift at {C1}")), "{log}");
    let source_log = h.layout.log_path(&h.layout.source_key(&url(URL)));
    assert!(!source_log.exists(), "the source-keyed log moved to the id");

    let phases = phases.borrow().clone();
    assert!(phases.contains(&Phase::Resolving), "{phases:?}");
    assert!(phases.contains(&Phase::Cloning), "{phases:?}");
    assert!(phases.contains(&Phase::Checking), "{phases:?}");
    assert!(
        phases.contains(&Phase::Building {
            downloading_wasi_sdk: true
        }),
        "the WASI SDK phase is named on first use: {phases:?}"
    );
    assert!(phases.contains(&Phase::Loading), "{phases:?}");
    h.installer.read_with(cx, |installer, _| {
        assert_eq!(installer.operations().count(), 0);
        assert!(installer.last_failure().is_none());
    });

    // The same commit again is already installed: nothing is cloned into place or built.
    let again = h.install(URL, Some(C1), Trust::Listed, cx);
    cx.run_until_parked();
    let again = again.await.expect("the repeat succeeds");
    assert!(again.already_installed);
    assert_eq!(h.host.compiled().len(), 1);
    assert!(h.staging_is_empty());
}

#[derive(Debug, Clone, Copy)]
enum FailAt {
    Preflight,
    Fetch,
    Build,
    Link,
    Record,
}

#[gpui::test]
async fn failure_at_each_phase_leaves_no_checkout_and_a_log(cx: &mut TestAppContext) {
    for fail_at in [
        FailAt::Preflight,
        FailAt::Fetch,
        FailAt::Build,
        FailAt::Link,
        FailAt::Record,
    ] {
        let h = Harness::new(cx);
        // An earlier install of the same source at another commit is the old state to keep.
        let old = h.install(URL, Some(C2), Trust::Confirmed(C2.to_string()), cx);
        cx.run_until_parked();
        old.await.expect("the earlier install succeeds");
        let old_checkout = h.layout.checkout_dir("swift", C2);

        let expected = match fail_at {
            FailAt::Preflight => {
                h.git.set_files(
                    C1,
                    &[(
                        "extension.toml",
                        "id = \"swift\"\nname = \"Swift\"\nversion = \"0.2.0\"\nschema_version = 99\n",
                    )],
                );
                "This extension uses manifest schema version 99"
            }
            FailAt::Fetch => {
                h.git.inner.fail_next_fetch(&url(URL), "connection reset");
                "connection reset"
            }
            FailAt::Build => {
                h.host
                    .push_build(FakeBuild::Fail("error[E0308]: mismatched types".into()));
                "the build failed"
            }
            FailAt::Link => {
                h.host.fail_link("swift", "permission denied");
                "could not load the extension"
            }
            FailAt::Record => {
                h.installer.update(cx, |installer, _| {
                    installer.deps.write_record = failing_record_writer;
                });
                "could not write"
            }
        };

        let task = h.install(URL, Some("v1"), Trust::Listed, cx);
        cx.run_until_parked();
        let err = task.await.expect_err("the install fails");
        let message = format!("{err:#}");
        assert!(message.contains(expected), "{fail_at:?}: {message}");

        assert!(
            !h.layout.checkout_dir("swift", C1).exists(),
            "{fail_at:?}: the new checkout is gone"
        );
        assert!(h.staging_is_empty(), "{fail_at:?}: no staging folder left");
        assert_eq!(
            h.link_target("swift"),
            Some(old_checkout.clone()),
            "{fail_at:?}: the old build stays linked"
        );
        assert!(old_checkout.is_dir(), "{fail_at:?}: the old checkout stays");
        if let FailAt::Link = fail_at {
            assert_eq!(
                h.host.linked().last(),
                Some(&("swift".to_string(), old_checkout.clone())),
                "the old link was put back after the host removed it"
            );
        }
        assert_eq!(h.record("swift").map(|r| r.commit), Some(C2.to_string()));

        let failure = h
            .installer
            .read_with(cx, |installer, _| installer.last_failure().cloned())
            .expect("the failure is kept for the status line");
        let log_path = failure.log_path.expect("a failed install has a log");
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(log.contains("error: "), "{fail_at:?}: {log}");
        assert!(log.contains(expected), "{fail_at:?}: {log}");
        let expected_log = match fail_at {
            FailAt::Fetch => h.layout.log_path(&h.layout.source_key(&url(URL))),
            _ => h.layout.log_path("swift"),
        };
        assert_eq!(log_path, expected_log, "{fail_at:?}");
        h.installer.read_with(cx, |installer, _| {
            assert_eq!(installer.operations().count(), 0, "{fail_at:?}");
        });
    }
}

#[gpui::test]
async fn reinstalling_the_linked_commit_without_a_record_keeps_the_live_checkout(
    cx: &mut TestAppContext,
) {
    let h = Harness::new(cx);
    let first = h.install(URL, Some(C1), Trust::Listed, cx);
    cx.run_until_parked();
    first.await.expect("the first install succeeds");
    let checkout = h.layout.checkout_dir("swift", C1);
    std::fs::remove_file(h.layout.record_path("swift")).unwrap();

    // A failed rebuild of the same commit leaves the live checkout linked and whole.
    h.host
        .push_build(FakeBuild::Fail("error[E0308]: mismatched types".into()));
    let task = h.install(URL, Some(C1), Trust::Listed, cx);
    cx.run_until_parked();
    let err = task.await.expect_err("the rebuild fails");
    assert!(format!("{err:#}").contains("the build failed"), "{err:#}");
    assert_eq!(h.link_target("swift"), Some(checkout.clone()));
    assert!(checkout.join("extension.toml").is_file());
    let built = h.host.compiled().last().cloned().unwrap();
    assert_ne!(built, checkout, "the rebuild ran beside the live checkout");
    assert!(!built.exists(), "the failed rebuild is removed");
    assert!(h.record("swift").is_none());

    // A good rebuild replaces it, and the old folder goes.
    let task = h.install(URL, Some(C1), Trust::Listed, cx);
    cx.run_until_parked();
    let installed = task.await.expect("the rebuild succeeds");
    assert!(!installed.already_installed);
    let built = h.host.compiled().last().cloned().unwrap();
    assert_eq!(h.link_target("swift"), Some(built.clone()));
    assert!(built.join("extension.toml").is_file());
    assert!(!checkout.exists(), "the old checkout is removed");
    assert_eq!(h.record("swift").map(|r| r.commit), Some(C1.to_string()));

    // Installing it again is a no-op.
    let again = h.install(URL, Some(C1), Trust::Listed, cx);
    cx.run_until_parked();
    assert!(again.await.unwrap().already_installed);
}

#[gpui::test]
async fn newer_api_after_build_is_never_linked(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    h.host
        .push_build(FakeBuild::Ok(Some(ApiVersion::new(0, 8, 0))));
    let task = h.install(URL, None, Trust::Listed, cx);
    cx.run_until_parked();
    let err = task.await.expect_err("a too-new API is refused");
    assert_eq!(
        format!("{err}"),
        "This extension needs extension API 0.8.0; this bongos supports up to 0.7.0."
    );
    assert_eq!(h.host.compiled().len(), 1, "the build ran");
    assert!(h.host.linked().is_empty(), "nothing was linked");
    assert_eq!(h.link_target("swift"), None);
    assert!(!h.layout.checkout_dir("swift", C1).exists());
    assert!(h.record("swift").is_none());
}

#[gpui::test]
async fn identical_requests_join(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let release = pending_build(&h.host);
    let first = h.install(URL, Some("v1"), Trust::Listed, cx);
    let second = h.install(URL, Some("v1"), Trust::Listed, cx);
    cx.run_until_parked();
    assert_eq!(h.phases(cx).len(), 1, "one operation for both callers");

    release.send(Ok(None)).unwrap();
    cx.run_until_parked();
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(first, second);
    assert_eq!(h.git.inner.network_ops(), 2, "one ls-remote and one fetch");
    assert_eq!(h.host.compiled().len(), 1);
    assert_eq!(h.host.linked().len(), 1);
}

#[gpui::test]
async fn held_lock_is_refused(cx: &mut TestAppContext) {
    let h = Harness::new(cx);

    // Another process installing the same source.
    let source_lock = h.hold_lock(&h.layout.source_key(&url(URL)));
    let task = h.install(URL, None, Trust::Listed, cx);
    cx.run_until_parked();
    let err = task.await.expect_err("a held source lock refuses");
    assert_eq!(
        format!("{err}"),
        format!("another bongos is installing {}", url(URL))
    );
    assert_eq!(h.git.inner.network_ops(), 0, "nothing was fetched");
    drop(source_lock);

    // Another process installing the same id from elsewhere.
    let id_lock = h.hold_lock("swift");
    let task = h.install(URL, None, Trust::Listed, cx);
    cx.run_until_parked();
    let err = task.await.expect_err("a held id lock refuses");
    assert_eq!(format!("{err}"), "another bongos is installing swift");
    assert!(!h.layout.checkout_dir("swift", C1).exists());
    assert!(h.staging_is_empty());
    assert!(
        !h.layout.log_path("swift").exists(),
        "the other process's log is left alone"
    );
    assert!(h.host.compiled().is_empty());
    drop(id_lock);

    // Released (as when the other process exits): the install goes ahead.
    let task = h.install(URL, None, Trust::Listed, cx);
    cx.run_until_parked();
    task.await.expect("the lock is free again");
}

#[gpui::test]
async fn two_builds_never_overlap(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let release_first = pending_build(&h.host);
    let release_second = pending_build(&h.host);
    let swift = h.install(URL, None, Trust::Listed, cx);
    let ruby = h.install(RUBY_URL, None, Trust::Confirmed(C3.to_string()), cx);
    cx.run_until_parked();

    let phases = h.phases(cx);
    assert_eq!(phases.len(), 2);
    assert_eq!(
        phases
            .iter()
            .filter(|p| matches!(p, Phase::Building { .. }))
            .count(),
        1,
        "{phases:?}"
    );
    assert!(phases.contains(&Phase::WaitingToBuild), "{phases:?}");
    assert_eq!(h.host.compiled().len(), 1);

    release_first.send(Ok(None)).unwrap();
    cx.run_until_parked();
    assert_eq!(h.host.compiled().len(), 2, "the second build started");
    release_second.send(Ok(None)).unwrap();
    cx.run_until_parked();

    swift.await.unwrap();
    ruby.await.unwrap();
    assert_eq!(h.host.max_concurrent_builds(), 1);
}

gpui::actions!(installer_test, [Ping]);

#[gpui::test]
async fn pending_build_never_blocks_the_foreground(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let release = pending_build(&h.host);
    let task = h.install(URL, None, Trust::Listed, cx);
    cx.run_until_parked();
    assert!(
        matches!(h.phases(cx).as_slice(), [Phase::Building { .. }]),
        "the build is running"
    );

    // Foreground work and an action dispatch complete while the build is still pending.
    let pings = Rc::new(RefCell::new(0));
    cx.update({
        let pings = pings.clone();
        |cx| {
            cx.on_action(move |_: &Ping, _| *pings.borrow_mut() += 1);
        }
    });
    let counter = cx.new(|_| 0usize);
    for _ in 0..3 {
        counter.update(cx, |count, cx| {
            *count += 1;
            cx.notify();
        });
        cx.update(|cx| cx.dispatch_action(&Ping));
        cx.run_until_parked();
    }
    assert_eq!(counter.read_with(cx, |count, _| *count), 3);
    assert_eq!(*pings.borrow(), 3);
    assert!(
        matches!(h.phases(cx).as_slice(), [Phase::Building { .. }]),
        "the build is still pending"
    );

    release.send(Ok(None)).unwrap();
    cx.run_until_parked();
    task.await.expect("the build finishes afterwards");
}

#[gpui::test]
async fn same_id_from_another_source_is_refused_while_busy(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    h.git.set_files(
        C3,
        &[
            ("extension.toml", SWIFT_MANIFEST),
            ("Cargo.toml", CARGO_TOML),
        ],
    );
    let release = pending_build(&h.host);
    let listed = h.install(URL, None, Trust::Listed, cx);
    cx.run_until_parked();

    let fork = h.install(OTHER_URL, None, Trust::Confirmed(C3.to_string()), cx);
    cx.run_until_parked();
    let err = fork.await.expect_err("the busy id refuses");
    assert_eq!(format!("{err}"), "swift is busy: installing");
    assert!(!h.layout.checkout_dir("swift", C3).exists());
    assert!(h.staging_is_empty(), "the fork's clone is gone");
    assert_eq!(h.host.compiled().len(), 1, "the fork never built");

    release.send(Ok(None)).unwrap();
    cx.run_until_parked();
    listed.await.expect("the first install finishes");
    assert_eq!(
        h.link_target("swift"),
        Some(h.layout.checkout_dir("swift", C1))
    );
}
