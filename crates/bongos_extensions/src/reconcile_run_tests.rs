//! Tests of the startup reconcile on real folders, with a fake git and a fake host: no network and
//! a pinned clock.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use bongos_extensions_model::{
    ApiVersion, Fix, InstallRecord, LanguageList, Layout, Probe, ReadOutcome, RecordState,
    SourceUrl, ToolProbe, write_atomic,
};
use gpui::{AppContext as _, BorrowAppContext as _, Entity, TestAppContext};
use settings::SettingsStore;
use tempfile::TempDir;

use super::commit_of_folder;
use crate::build_log::FixedClock;
use crate::git::FakeGit;
use crate::host::FakeHost;
use crate::installer::{BUILD_MISSING_KEY, FixedTools, Installer, InstallerDeps};

const URL: &str = "https://example.com/acme/swift-extension";
const C1: &str = "1111111111111111111111111111111111111111";
const C2: &str = "2222222222222222222222222222222222222222";
/// 2023-11-14T22:13:20Z, pinned so no test depends on the real date.
const NOW_SECS: u64 = 1_700_000_000;
const NOW_TEXT: &str = "2023-11-14T22:13:20Z";

struct Harness {
    _temp: TempDir,
    layout: Layout,
    git: Arc<FakeGit>,
    host: Rc<FakeHost>,
    installer: Entity<Installer>,
}

impl Harness {
    fn new(cx: &mut TestAppContext) -> Self {
        let temp = TempDir::new().unwrap();
        let layout = Layout::new(temp.path().join("extensions"));
        let git = Arc::new(FakeGit::new());
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
            write_record: write_atomic,
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

    async fn reconcile(&self, cx: &mut TestAppContext) -> Vec<Fix> {
        let task = self
            .installer
            .update(cx, |installer, cx| installer.reconcile(cx));
        cx.run_until_parked();
        task.await.expect("the reconcile runs")
    }

    /// Creates `git/<id>/<folder>` holding a manifest.
    fn checkout(&self, id: &str, folder: &str) -> PathBuf {
        let dir = self.layout.extension_dir(id).join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("extension.toml"),
            format!("id = \"{id}\"\nname = \"Swift\"\nversion = \"0.1.0\"\nschema_version = 1\n"),
        )
        .unwrap();
        dir
    }

    fn write_record(&self, id: &str, commit: &str, state: RecordState) {
        let record = InstallRecord {
            version: 1,
            id: id.to_string(),
            source_url: SourceUrl::parse(URL).unwrap().as_str().to_string(),
            path: None,
            requested_ref: Some("v1".to_string()),
            commit: commit.to_string(),
            listed: true,
            trusted_commit: None,
            state,
            installed_at: "2023-01-01T00:00:00Z".to_string(),
            extra: Default::default(),
        };
        write_atomic(&self.layout.record_path(id), &record, "test").unwrap();
    }

    fn record(&self, id: &str) -> Option<InstallRecord> {
        match bongos_extensions_model::read(&self.layout.record_path(id)).unwrap() {
            ReadOutcome::Ok(record) => Some(record),
            _ => None,
        }
    }

    fn link(&self, id: &str, target: &Path) {
        std::fs::create_dir_all(self.layout.installed_dir()).unwrap();
        std::os::unix::fs::symlink(target, self.layout.symlink_path(id)).unwrap();
    }

    fn link_target(&self, id: &str) -> Option<PathBuf> {
        std::fs::read_link(self.layout.symlink_path(id)).ok()
    }

    fn folders(&self, id: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.layout.extension_dir(id))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn staging(&self, nonce: &str) -> PathBuf {
        let dir = self.layout.staging_for(nonce);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("half-cloned"), "x").unwrap();
        dir
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

#[test]
fn rebuild_suffix_is_stripped_from_the_commit() {
    assert_eq!(commit_of_folder(&format!("{C1}.rebuild-123-4")), C1);
    assert_eq!(commit_of_folder(C1), C1);
    assert_eq!(commit_of_folder(".rebuild-x"), ".rebuild-x");
}

#[gpui::test]
async fn empty_data_dir_is_left_alone(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    assert!(h.reconcile(cx).await.is_empty());
    assert!(!h.layout.root().exists(), "nothing was created");

    std::fs::create_dir_all(h.layout.installed_dir()).unwrap();
    assert!(h.reconcile(cx).await.is_empty());
    assert!(!h.layout.git_dir().exists(), "no git folder was created");
    assert_eq!(h.git.network_ops(), 0);
    assert!(h.host.linked().is_empty());
}

#[gpui::test]
async fn staging_folders_are_swept(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let staging = h.staging("abc");
    let fixes = h.reconcile(cx).await;
    assert_eq!(
        fixes,
        vec![Fix::DeleteStaging {
            nonce: "abc".into()
        }]
    );
    assert!(!staging.exists());
}

#[gpui::test]
async fn unreferenced_checkout_is_deleted(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let live = h.checkout("swift", C1);
    let stale = h.checkout("swift", C2);
    h.write_record("swift", C1, RecordState::Installed);
    h.link("swift", &live);

    let fixes = h.reconcile(cx).await;
    assert_eq!(
        fixes,
        vec![Fix::DeleteCheckout {
            id: "swift".into(),
            folder: C2.into()
        }]
    );
    assert!(!stale.exists());
    assert!(live.join("extension.toml").is_file());
    assert_eq!(h.link_target("swift"), Some(live));
}

#[gpui::test]
async fn record_is_rebuilt_from_the_symlink_without_network(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let live = h.checkout("swift", C1);
    let stale = h.checkout("swift", C2);
    h.git
        .add_repository(&live, SourceUrl::parse(URL).unwrap().as_str(), C1);
    h.link("swift", &live);
    h.staging("left-over");

    let fixes = h.reconcile(cx).await;
    assert_eq!(h.git.network_ops(), 0, "reconcile reaches no network");
    assert!(
        fixes.contains(&Fix::WriteRecordFromSymlink {
            id: "swift".into(),
            commit: C1.into(),
            origin_url: None
        }),
        "{fixes:?}"
    );
    let record = h.record("swift").expect("the record is written");
    assert_eq!(record.commit, C1);
    assert_eq!(record.source_url, SourceUrl::parse(URL).unwrap().as_str());
    assert!(record.listed);
    assert_eq!(record.state, RecordState::Installed);
    assert_eq!(record.installed_at, NOW_TEXT);
    assert!(!stale.exists());
    assert!(h.host.compiled().is_empty(), "nothing was built");
    assert!(h.host.linked().is_empty(), "nothing was relinked");
}

#[gpui::test]
async fn missing_symlink_is_relinked_to_the_recorded_commit(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let old = h.checkout("swift", C1);
    // A crashed update: its new checkout is there, the link was lost and the record is the old one.
    let half = h.checkout("swift", C2);
    h.write_record("swift", C1, RecordState::Installed);

    let fixes = h.reconcile(cx).await;
    assert!(
        fixes.contains(&Fix::Relink {
            id: "swift".into(),
            commit: C1.into()
        }),
        "{fixes:?}"
    );
    assert_eq!(h.link_target("swift"), Some(old.clone()));
    assert_eq!(h.host.linked(), vec![("swift".to_string(), old)]);
    assert!(!half.exists());
    assert_eq!(h.git.network_ops(), 0);
}

#[gpui::test]
async fn record_without_a_build_is_marked_build_missing(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    h.write_record("swift", C1, RecordState::Installed);
    std::fs::create_dir_all(h.layout.extension_dir("swift").join(C1)).unwrap();

    let fixes = h.reconcile(cx).await;
    assert_eq!(fixes, vec![Fix::MarkBuildMissing { id: "swift".into() }]);
    let record = h.record("swift").unwrap();
    assert_eq!(
        record.extra.get(BUILD_MISSING_KEY),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(h.host.linked().is_empty());
}

#[gpui::test]
async fn removing_record_finishes_the_removal(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let checkout = h.checkout("swift", C1);
    h.write_record("swift", C1, RecordState::Removing);
    h.link("swift", &checkout);
    h.host.set_unlink_follows_links(true);

    let fixes = h.reconcile(cx).await;
    assert_eq!(fixes, vec![Fix::FinishRemoval { id: "swift".into() }]);
    assert!(!h.layout.extension_dir("swift").exists());
    assert_eq!(h.link_target("swift"), None);
    assert_eq!(
        h.host.unlinked(),
        vec![("swift".to_string(), vec![C1.to_string()])],
        "the link was gone before the host unloaded"
    );
}

#[gpui::test]
async fn id_locked_by_another_process_is_untouched(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let checkout = h.checkout("swift", C1);
    let other = h.checkout("swift", C2);
    h.write_record("swift", C1, RecordState::Removing);
    h.link("swift", &checkout);
    let staging = h.staging("theirs");
    let lock = h.hold_lock("swift");

    let fixes = h.reconcile(cx).await;
    assert!(fixes.is_empty(), "{fixes:?}");
    assert!(checkout.is_dir() && other.is_dir() && staging.is_dir());
    assert_eq!(h.link_target("swift"), Some(checkout));
    assert_eq!(h.record("swift").unwrap().state, RecordState::Removing);
    assert!(h.host.unlinked().is_empty());
    drop(lock);

    // A held lock of a source being installed (its id not known yet) keeps staging too.
    let source_lock = h.hold_lock("source-0000abcd");
    let fixes = h.reconcile(cx).await;
    assert!(
        !fixes.iter().any(|f| matches!(f, Fix::DeleteStaging { .. })),
        "{fixes:?}"
    );
    assert!(staging.is_dir());
    drop(source_lock);
}

#[gpui::test]
async fn live_rebuild_folder_is_kept_and_named_by_its_commit(cx: &mut TestAppContext) {
    // A same-commit reinstall with a missing record leaves the live link in
    // `<commit>.rebuild-<pid>-<op>`, never renamed over `<commit>`.
    let h = Harness::new(cx);
    let rebuilt = h.checkout("swift", &format!("{C1}.rebuild-123-4"));
    h.git
        .add_repository(&rebuilt, SourceUrl::parse(URL).unwrap().as_str(), C1);
    h.link("swift", &rebuilt);

    let fixes = h.reconcile(cx).await;
    assert_eq!(
        fixes,
        vec![Fix::WriteRecordFromSymlink {
            id: "swift".into(),
            commit: C1.into(),
            origin_url: None
        }]
    );
    assert_eq!(h.record("swift").unwrap().commit, C1);
    assert!(
        rebuilt.join("extension.toml").is_file(),
        "the live build stays"
    );
    assert_eq!(h.link_target("swift"), Some(rebuilt.clone()));

    // With its record back, a stale plain folder of the same commit goes and the live one stays.
    let stale = h.checkout("swift", C1);
    let fixes = h.reconcile(cx).await;
    assert_eq!(fixes.len(), 1, "{fixes:?}");
    assert!(matches!(&fixes[0], Fix::DeleteCheckout { .. }), "{fixes:?}");
    assert!(!stale.exists());
    assert_eq!(h.folders("swift"), vec![format!("{C1}.rebuild-123-4")]);
    assert_eq!(h.record("swift").unwrap().commit, C1);

    // A second run finds nothing to do.
    assert!(h.reconcile(cx).await.is_empty());

    // Its link lost too: the recorded commit is relinked to the rebuild folder.
    std::fs::remove_file(h.layout.symlink_path("swift")).unwrap();
    let fixes = h.reconcile(cx).await;
    assert_eq!(
        fixes,
        vec![Fix::Relink {
            id: "swift".into(),
            commit: C1.into()
        }]
    );
    assert_eq!(h.link_target("swift"), Some(rebuilt.clone()));
    assert!(rebuilt.is_dir());
    assert_eq!(h.git.network_ops(), 0);
}

fn use_settings(enabled: bool, cx: &mut TestAppContext) {
    cx.update(|cx| {
        if !cx.has_global::<SettingsStore>() {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            crate::init(cx);
        }
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .extension
                    .language_extensions
                    .get_or_insert_default()
                    .enabled = Some(enabled);
            });
        });
    });
}

#[gpui::test]
async fn startup_reconcile_waits_for_the_setting(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let staging = h.staging("abc");
    use_settings(false, cx);
    cx.update(|cx| {
        Installer::set_global(h.installer.clone(), cx);
        crate::init_reconcile(cx);
    });
    cx.run_until_parked();
    assert!(
        staging.exists(),
        "nothing runs while language extensions are off"
    );

    use_settings(true, cx);
    cx.run_until_parked();
    assert!(!staging.exists(), "turning them on runs the reconcile");

    // Only once.
    let again = h.staging("def");
    use_settings(false, cx);
    use_settings(true, cx);
    cx.run_until_parked();
    assert!(again.exists());
}
