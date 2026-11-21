//! With nothing asked of it, language extensions makes no network call: not at start, not on
//! first use with an empty data directory, and not while a day goes by with a file of a listed
//! type open. Every HTTP request and every network operation of git is counted.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use bongos_extensions::build_log::FixedClock;
use bongos_extensions::git::FakeGit;
use bongos_extensions::host::FakeHost;
use bongos_extensions::installer::{FixedTools, Installer, InstallerDeps};
use bongos_extensions::offer::LanguageOffer;
use bongos_extensions_model::{
    ApiVersion, InstallRecord, LanguageList, Layout, Probe, RecordState, SourceUrl, ToolProbe,
    write_atomic,
};
use fs::Fs;
use gpui::{AppContext as _, BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use tempfile::TempDir;
use util::rel_path::rel_path;
use workspace::{AppState, MultiWorkspace, Workspace};

const COMMIT: &str = "1111111111111111111111111111111111111111";
const SWIFT_URL: &str = "https://example.com/acme/swift-extension";
const IDS: [&str; 3] = ["alpha", "beta", "gamma"];

struct Idle {
    _temp: TempDir,
    layout: Layout,
    workspace: Entity<Workspace>,
    offer: Entity<LanguageOffer>,
    git: Arc<FakeGit>,
    http_requests: Arc<AtomicUsize>,
    installer: Entity<Installer>,
    cx: VisualTestContext,
}

fn list_text() -> String {
    format!(
        "[[extension]]\nid = \"swift\"\nname = \"Swift\"\ndescription = \"Swift.\"\nrepository = \"{SWIFT_URL}\"\ncommit = \"{COMMIT}\"\nfile_types = [\"swift\"]\n"
    )
}

/// Three install records with their checkouts; two of them are linked and loaded.
fn install_three(layout: &Layout, host: &FakeHost) {
    for id in IDS {
        let checkout = layout.checkout_dir(id, COMMIT);
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(
            checkout.join("extension.toml"),
            format!("id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\nschema_version = 1\n"),
        )
        .unwrap();
        let record = InstallRecord {
            version: 1,
            id: id.to_string(),
            source_url: SourceUrl::parse(&format!("https://example.com/acme/{id}"))
                .unwrap()
                .as_str()
                .to_string(),
            path: None,
            requested_ref: None,
            commit: COMMIT.to_string(),
            listed: false,
            trusted_commit: None,
            state: RecordState::Installed,
            installed_at: "2023-01-01T00:00:00Z".to_string(),
            extra: Default::default(),
        };
        write_atomic(&layout.record_path(id), &record, "test").unwrap();
    }
    std::fs::create_dir_all(layout.installed_dir()).unwrap();
    for id in &IDS[..2] {
        link(layout, id, &layout.checkout_dir(id, COMMIT));
        host.add_installed(id, true);
    }
}

fn link(layout: &Layout, id: &str, target: &Path) {
    std::os::unix::fs::symlink(target, layout.symlink_path(id)).unwrap();
}

impl Idle {
    async fn new(with_records: bool, cx: &mut TestAppContext) -> Self {
        let app_state: Arc<AppState> = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            bongos_extensions::init(cx);
            app_state
        });
        let http_requests = Arc::new(AtomicUsize::new(0));
        let counter = http_requests.clone();
        cx.update(|cx| {
            <dyn Fs>::set_global(app_state.fs.clone(), cx);
            cx.set_http_client(http_client::FakeHttpClient::create(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(http_client::Response::builder()
                        .status(404)
                        .body(Default::default())
                        .unwrap())
                }
            }));
        });
        app_state
            .fs
            .as_fake()
            .insert_tree("/root", json!({"a.swift": ""}))
            .await;
        let temp = TempDir::new().unwrap();
        let layout = Layout::new(temp.path().join("extensions"));
        let git = Arc::new(FakeGit::new());
        let host = Rc::new(FakeHost::new(layout.clone(), ApiVersion::new(0, 7, 0)));
        if with_records {
            install_three(&layout, &host);
        }
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
            clock: Arc::new(FixedClock(UNIX_EPOCH + Duration::from_secs(1_700_000_000))),
            list: Arc::new(LanguageList::parse(&list_text()).unwrap()),
            is_blocked_host: bongos_net::is_blocked_host,
            write_record: write_atomic,
        };
        let installer = cx.update(|cx| {
            let installer = cx.new(|_| Installer::new(deps));
            Installer::set_global(installer.clone(), cx);
            installer
        });
        let project = Project::test(app_state.fs.clone(), ["/root".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .unwrap();
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let offer = cx.new(|_| LanguageOffer::new());
        workspace.update_in(&mut cx, |workspace, window, cx| {
            workspace.status_bar().update(cx, |status_bar, cx| {
                status_bar.add_left_item(offer.clone(), window, cx);
            });
        });
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(
                        r#"{"cursor_blink": false, "language_extensions": {"enabled": true}}"#,
                        cx,
                    )
                    .result()
                    .unwrap();
            })
        });
        Self {
            _temp: temp,
            layout,
            workspace,
            offer,
            git,
            http_requests,
            installer,
            cx,
        }
    }

    /// The startup reconcile, as `init_reconcile` runs it.
    fn start(&mut self) {
        self.cx
            .update(|_, cx| bongos_extensions::init_reconcile(cx));
        self.cx.run_until_parked();
    }

    async fn open(&mut self, name: &str) {
        let worktree_id = self.workspace.read_with(&self.cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .worktrees(cx)
                .next()
                .unwrap()
                .read(cx)
                .id()
        });
        let open = self
            .workspace
            .update_in(&mut self.cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path(name)), None, true, window, cx)
            });
        open.await.unwrap();
        self.cx.run_until_parked();
    }

    fn offer_text(&mut self) -> Option<String> {
        let offer = self.offer.clone();
        self.cx
            .update(|_, cx| offer.update(cx, |offer, cx| offer.text(cx)))
            .map(|text| text.to_string())
    }

    fn assert_no_calls(&self) {
        assert_eq!(
            self.http_requests.load(Ordering::SeqCst),
            0,
            "HTTP requests"
        );
        assert_eq!(self.git.network_ops(), 0, "git network operations");
    }
}

#[gpui::test]
async fn idle_a_day_with_a_listed_file_open_makes_no_call(cx: &mut TestAppContext) {
    let mut idle = Idle::new(true, cx).await;
    idle.start();
    idle.open("a.swift").await;
    assert_eq!(
        idle.offer_text().as_deref(),
        Some("Swift support is available"),
        "the offer line logic ran"
    );
    idle.cx
        .executor()
        .advance_clock(Duration::from_secs(24 * 60 * 60));
    idle.cx.run_until_parked();
    assert_eq!(
        idle.offer_text().as_deref(),
        Some("Swift support is available")
    );
    idle.assert_no_calls();
    idle.installer.read_with(&idle.cx, |installer, _| {
        assert_eq!(installer.operations().count(), 0);
    });
}

#[gpui::test]
async fn idle_the_reconcile_at_start_makes_no_call(cx: &mut TestAppContext) {
    let mut idle = Idle::new(true, cx).await;
    idle.start();
    idle.assert_no_calls();
    let layout: PathBuf = idle.layout.record_path("gamma");
    assert!(layout.is_file(), "the records are still there");
    for id in IDS {
        assert!(idle.layout.record_path(id).is_file());
    }
}

#[gpui::test]
async fn idle_first_use_with_an_empty_data_directory_makes_no_call(cx: &mut TestAppContext) {
    let mut idle = Idle::new(false, cx).await;
    idle.start();
    idle.open("a.swift").await;
    idle.offer_text();
    idle.cx.executor().advance_clock(Duration::from_secs(60));
    idle.cx.run_until_parked();
    idle.assert_no_calls();
    assert!(!idle.layout.root().exists(), "nothing was created");
}
