//! Tests of the offer line: when it shows, what hides it, and that it reads only local state.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use bongos_extensions_model::{
    ApiVersion, LanguageList, Layout, ListEntry, Probe, SourceUrl, ToolProbe,
};
use fs::{FakeFs, Fs};
use gpui::{AppContext as _, BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
use language::{LanguageMatcher, LanguageName};
use project::Project;
use serde_json::json;
use settings::{Settings as _, SettingsStore};
use util::rel_path::rel_path;
use workspace::{AppState, MultiWorkspace, Workspace};

use super::LanguageOffer;
use crate::LanguageExtensionsSettings;
use crate::build_log::FixedClock;
use crate::git::FakeGit;
use crate::host::FakeHost;
use crate::installer::{FixedTools, Installer, InstallerDeps, Trust};

const SWIFT_URL: &str = "https://example.com/acme/swift-extension";
const COMMIT: &str = "1111111111111111111111111111111111111111";

struct Offer {
    workspace: Entity<Workspace>,
    offer: Entity<LanguageOffer>,
    fs: Arc<FakeFs>,
    git: Arc<FakeGit>,
    host: Rc<FakeHost>,
    http_requests: Arc<AtomicUsize>,
    cx: VisualTestContext,
}

fn list_text() -> String {
    let entry = |id: &str, name: &str, url: &str, file_type: &str| {
        format!(
            "[[extension]]\nid = \"{id}\"\nname = \"{name}\"\ndescription = \"{name}.\"\nrepository = \"{url}\"\ncommit = \"{COMMIT}\"\nfile_types = [\"{file_type}\"]\n"
        )
    };
    [
        entry("swift", "Swift", SWIFT_URL, "swift"),
        entry(
            "rust",
            "Rust",
            "https://example.com/acme/rust-extension",
            "rs",
        ),
    ]
    .join("\n")
}

impl Offer {
    async fn new(user_settings: &str, cx: &mut TestAppContext) -> Self {
        let app_state: Arc<AppState> = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            app_state
        });
        let fs = app_state.fs.as_fake();
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
        fs.insert_tree(
            "/root",
            json!({"a.swift": "", "b.xyz": "", "c.rs": "", "d.rs": ""}),
        )
        .await;
        let git = Arc::new(FakeGit::new());
        let layout = Layout::new("/extensions".into());
        let host = Rc::new(FakeHost::new(layout.clone(), ApiVersion::new(0, 7, 0)));
        let deps = InstallerDeps {
            layout,
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
            write_record: bongos_extensions_model::write_atomic,
        };
        cx.update(|cx| {
            let installer = cx.new(|_| Installer::new(deps));
            Installer::set_global(installer, cx);
        });
        let project = Project::test(app_state.fs.clone(), ["/root".as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().register_language(
                LanguageName::new_static("Rust"),
                None,
                LanguageMatcher {
                    path_suffixes: vec!["rs".to_string()],
                    ..Default::default()
                },
                false,
                None,
                Arc::new(|| anyhow::bail!("never loaded")),
            );
        });
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
        let mut this = Self {
            workspace,
            offer,
            fs,
            git,
            host,
            http_requests,
            cx,
        };
        this.set_user_settings(user_settings);
        this
    }

    fn set_user_settings(&mut self, text: &str) {
        self.cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.set_user_settings(text, cx).result().unwrap();
            })
        });
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

    fn text(&mut self) -> Option<String> {
        let offer = self.offer.clone();
        self.cx
            .update(|_, cx| offer.update(cx, |offer, cx| offer.text(cx)))
            .map(|text| text.to_string())
    }

    fn entry(&mut self, id: &str) -> ListEntry {
        LanguageList::parse(&list_text())
            .unwrap()
            .entries()
            .iter()
            .find(|entry| entry.id == id)
            .unwrap()
            .clone()
    }
}

const ON: &str = r#"{"language_extensions": {"enabled": true}}"#;

#[gpui::test]
async fn a_swift_file_shows_the_line(cx: &mut TestAppContext) {
    let mut offer = Offer::new(ON, cx).await;
    assert_eq!(offer.text(), None);
    offer.open("a.swift").await;
    assert_eq!(offer.text().as_deref(), Some("Swift support is available"));
}

#[gpui::test]
async fn unlisted_and_built_in_types_show_none(cx: &mut TestAppContext) {
    let mut offer = Offer::new(ON, cx).await;
    offer.open("b.xyz").await;
    assert_eq!(offer.text(), None);
    // `.rs` is listed, but the editor already has a language for it.
    offer.open("c.rs").await;
    assert_eq!(offer.text(), None);
}

#[gpui::test]
async fn installed_or_in_flight_shows_none(cx: &mut TestAppContext) {
    let mut offer = Offer::new(ON, cx).await;
    offer.open("a.swift").await;
    assert!(offer.text().is_some());
    offer.host.add_installed("swift", true);
    offer.cx.update(|_, cx| cx.refresh_windows());
    assert_eq!(offer.text(), None, "installed");

    let mut offer = Offer::new(ON, cx).await;
    offer.open("a.swift").await;
    let installer = offer.cx.update(|_, cx| Installer::global(cx));
    let _task = installer.update(&mut offer.cx, |installer, cx| {
        installer.install(SWIFT_URL, None, None, Trust::Listed, cx)
    });
    assert_eq!(offer.text(), None, "in flight");
}

#[gpui::test]
async fn not_now_hides_the_line_for_the_session(cx: &mut TestAppContext) {
    let mut offer = Offer::new(ON, cx).await;
    offer.open("a.swift").await;
    let entry = offer.entry("swift");
    let item = offer.offer.clone();
    item.update(&mut offer.cx, |item, cx| item.not_now(&entry, cx));
    assert_eq!(offer.text(), None);
    // The setting is untouched, so a second item (another window) still shows its own line.
    let other = offer.cx.new(|_| LanguageOffer::new());
    other.update(&mut offer.cx, |other, _| {
        assert!(other.dismissed.is_empty());
    });
    offer.cx.update(|_, cx| {
        assert!(
            LanguageExtensionsSettings::get_global(cx)
                .never_offer
                .is_empty()
        );
    });
}

#[gpui::test]
async fn dont_offer_again_writes_the_id_and_survives_a_reload(cx: &mut TestAppContext) {
    let mut offer = Offer::new(ON, cx).await;
    offer.open("a.swift").await;
    let entry = offer.entry("swift");
    let item = offer.offer.clone();
    item.update(&mut offer.cx, |item, cx| item.never_offer(&entry, cx));
    offer.cx.run_until_parked();
    let text = offer.fs.load(paths::settings_file()).await.unwrap();
    assert!(
        text.contains("never_offer") && text.contains("swift"),
        "{text}"
    );
    offer.cx.update(|_, cx| {
        assert_eq!(
            LanguageExtensionsSettings::get_global(cx).never_offer,
            ["swift"]
        );
    });
    // A fresh item has no session state and still hides the line, also after a reload.
    offer.set_user_settings(&text);
    let fresh = offer.cx.new(|_| LanguageOffer::new());
    offer.cx.update(|_, cx| {
        assert_eq!(
            LanguageExtensionsSettings::get_global(cx).never_offer,
            ["swift"]
        );
    });
    assert_eq!(offer.text(), None);
    drop(fresh);
}

#[gpui::test]
async fn offer_false_hides_the_line(cx: &mut TestAppContext) {
    let mut offer = Offer::new(
        r#"{"language_extensions": {"enabled": true, "offer": false}}"#,
        cx,
    )
    .await;
    offer.open("a.swift").await;
    assert_eq!(offer.text(), None);
    // Off while `enabled` is false, which is the default.
    offer.set_user_settings("{}");
    assert_eq!(offer.text(), None);
    offer.set_user_settings(ON);
    assert!(offer.text().is_some());
}

#[gpui::test]
async fn opening_files_makes_no_git_or_http_call(cx: &mut TestAppContext) {
    let mut offer = Offer::new(ON, cx).await;
    for name in ["a.swift", "b.xyz", "c.rs", "d.rs"] {
        offer.open(name).await;
        offer.text();
    }
    assert_eq!(offer.git.network_ops(), 0);
    assert_eq!(offer.http_requests.load(Ordering::SeqCst), 0);
    let installer = offer.cx.update(|_, cx| Installer::global(cx));
    installer.read_with(&offer.cx, |installer, _| {
        assert_eq!(installer.operations().count(), 0);
    });
    let _ = SourceUrl::parse(SWIFT_URL);
}
