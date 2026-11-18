//! Tests of the commands: the trust question, the prompts for missing arguments and the echo-area
//! results, with a fake git, a fake host and a fake asker. No network, no compiler, and a pinned
//! clock.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use bongos_extensions_model::{
    ApiVersion, InstallRecord, LanguageList, Layout, Probe, ReadOutcome, SourceUrl, ToolProbe,
};
use bongos_minibuffer::{SessionView, minibuffer_for_window};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::LocalBoxFuture;
use gpui::{
    AppContext as _, AsyncApp, BorrowAppContext as _, Entity, Focusable as _, TestAppContext,
    VisualTestContext,
};
use parking_lot::Mutex;
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use tempfile::TempDir;
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::build_log::FixedClock;
use crate::commands::{
    Asker, Outcome, install_from_git, install_language, install_question, language_candidates,
    latest_question, update, update_question,
};
use crate::git::{FakeGit, GitRunner, RemoteRef};
use crate::host::FakeHost;
use crate::installer::{FixedTools, Installer, InstallerDeps};
use crate::{InstallFromGit, OFF_MESSAGE};

/// The listed Swift extension: pinned at `C1`; its upstream `HEAD` is `C2`.
const SWIFT_URL: &str = "https://example.com/acme/swift-extension";
/// An extension that is not on the list.
const ZIG_URL: &str = "https://example.org/someone/zig-extension";
const C1: &str = "1111111111111111111111111111111111111111";
const C2: &str = "2222222222222222222222222222222222222222";
const C3: &str = "3333333333333333333333333333333333333333";
const C4: &str = "4444444444444444444444444444444444444444";
/// 2023-11-14T22:13:20Z, pinned so no test depends on the real date.
const NOW_SECS: u64 = 1_700_000_000;

const SWIFT_MANIFEST: &str =
    "id = \"swift\"\nname = \"Swift\"\nversion = \"0.1.0\"\nschema_version = 1\n";
const ZIG_MANIFEST: &str =
    "id = \"zig\"\nname = \"Zig\"\nversion = \"0.1.0\"\nschema_version = 1\n";

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

/// An asker that records each question and answers from a queue (cancelled when it is empty),
/// or waits on [`FakeAsker::hold`]'s sender.
#[derive(Default)]
struct FakeAsker {
    questions: RefCell<Vec<String>>,
    answers: RefCell<VecDeque<Option<bool>>>,
    held: RefCell<Option<oneshot::Receiver<Option<bool>>>>,
}

impl FakeAsker {
    fn answering(answers: &[Option<bool>]) -> Rc<Self> {
        let asker = Rc::new(Self::default());
        asker.answers.borrow_mut().extend(answers.iter().copied());
        asker
    }

    /// Makes the next question wait until the returned sender answers it.
    fn hold(&self) -> oneshot::Sender<Option<bool>> {
        let (sender, receiver) = oneshot::channel();
        *self.held.borrow_mut() = Some(receiver);
        sender
    }

    fn questions(&self) -> Vec<String> {
        self.questions.borrow().clone()
    }
}

impl Asker for FakeAsker {
    fn ask(&self, question: String, _: &mut AsyncApp) -> LocalBoxFuture<'static, Option<bool>> {
        self.questions.borrow_mut().push(question);
        if let Some(receiver) = self.held.borrow_mut().take() {
            return async move { receiver.await.ok().flatten() }.boxed_local();
        }
        let answer = self.answers.borrow_mut().pop_front().flatten();
        futures::future::ready(answer).boxed_local()
    }
}

fn url(text: &str) -> SourceUrl {
    SourceUrl::parse(text).unwrap()
}

struct Harness {
    _temp: TempDir,
    layout: Layout,
    git: Arc<ContentGit>,
    host: Rc<FakeHost>,
    installer: Entity<Installer>,
}

fn list_pinned_at(pin: &str) -> LanguageList {
    LanguageList::parse(&format!(
        "[[extension]]\nid = \"swift\"\nname = \"Swift\"\ndescription = \"Swift.\"\nrepository = \"{SWIFT_URL}\"\ncommit = \"{pin}\"\n"
    ))
    .unwrap()
}

fn deps(layout: &Layout, git: &Arc<ContentGit>, host: &Rc<FakeHost>, pin: &str) -> InstallerDeps {
    InstallerDeps {
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
        list: Arc::new(list_pinned_at(pin)),
        is_blocked_host: bongos_net::is_blocked_host,
        write_record: bongos_extensions_model::write_atomic,
    }
}

impl Harness {
    fn new(cx: &mut TestAppContext) -> Self {
        let temp = TempDir::new().unwrap();
        let layout = Layout::new(temp.path().join("extensions"));
        let git = Arc::new(ContentGit::default());
        git.inner
            .set_refs(&url(SWIFT_URL), &[("HEAD", C2), ("refs/tags/v1", C1)]);
        git.inner.set_refs(&url(ZIG_URL), &[("HEAD", C3)]);
        for commit in [C1, C2] {
            git.set_files(commit, &[("extension.toml", SWIFT_MANIFEST)]);
        }
        for commit in [C3, C4] {
            git.set_files(commit, &[("extension.toml", ZIG_MANIFEST)]);
        }
        let host = Rc::new(FakeHost::new(layout.clone(), ApiVersion::new(0, 7, 0)));
        let deps = deps(&layout, &git, &host, C1);
        let installer = cx.new(|_| Installer::new(deps));
        Self {
            _temp: temp,
            layout,
            git,
            host,
            installer,
        }
    }

    /// Replaces the installer with one whose list pins Swift at `pin`, over the same folder.
    fn set_pin(&mut self, pin: &str, cx: &mut TestAppContext) {
        let deps = deps(&self.layout, &self.git, &self.host, pin);
        self.installer = cx.new(|_| Installer::new(deps));
    }

    fn record(&self, id: &str) -> Option<InstallRecord> {
        match bongos_extensions_model::read(&self.layout.record_path(id)).unwrap() {
            ReadOutcome::Ok(record) => Some(record),
            _ => None,
        }
    }

    fn checkouts(&self, id: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.layout.extension_dir(id)) else {
            return Vec::new();
        };
        entries
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|name| name != "record.json")
            .collect()
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

    async fn install_git(
        &self,
        source: &str,
        rev: Option<&str>,
        asker: &Rc<FakeAsker>,
        cx: &mut TestAppContext,
    ) -> Result<Outcome, Arc<anyhow::Error>> {
        let asker: Rc<dyn Asker> = asker.clone();
        let task = cx.update(|cx| install_from_git(&self.installer, source, rev, None, asker, cx));
        cx.run_until_parked();
        task.await
    }

    async fn update(
        &self,
        id: &str,
        latest: bool,
        asker: &Rc<FakeAsker>,
        cx: &mut TestAppContext,
    ) -> Result<Outcome, Arc<anyhow::Error>> {
        let asker: Rc<dyn Asker> = asker.clone();
        let task = cx.update(|cx| update(&self.installer, id, latest, asker, cx));
        cx.run_until_parked();
        task.await
    }

    /// Installs the off-list Zig extension at `C3`, answering "yes".
    async fn install_zig(&self, cx: &mut TestAppContext) {
        let asker = FakeAsker::answering(&[Some(true)]);
        let outcome = self.install_git(ZIG_URL, None, &asker, cx).await.unwrap();
        assert!(matches!(outcome, Outcome::Installed(ref i) if i.commit == C3));
    }
}

/// A window with the minibuffer, the actions registered, the setting on and `h`'s installer as
/// the app's.
async fn window(h: &Harness, cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
    let app_state: Arc<AppState> = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        app_state
    });
    cx.update(|cx| {
        crate::init(cx);
        Installer::set_global(h.installer.clone(), cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(vec!["messages".to_string()]);
                content
                    .extension
                    .language_extensions
                    .get_or_insert_default()
                    .enabled = Some(true);
            });
        });
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    (workspace, cx)
}

fn session_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<SessionView>(cx).is_some()
    })
}

fn messages(cx: &mut VisualTestContext) -> Vec<String> {
    let strip = cx
        .update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
        .expect("the window has a strip");
    strip.read_with(cx, |strip, _| {
        strip
            .messages()
            .into_iter()
            .map(|line| line.text.to_string())
            .collect()
    })
}

#[gpui::test]
async fn install_language_by_name_opens_no_session_and_installs_the_pin(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let (workspace, mut cx) = window(&h, cx).await;
    let action = cx
        .update(|_, cx| {
            cx.build_action(
                "extensions::InstallLanguage",
                Some(json!({"name": "Swift"})),
            )
        })
        .expect("the action builds by name");
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();

    assert!(!session_open(&workspace, &mut cx), "no prompt opened");
    let record = h.record("swift").expect("installed");
    assert_eq!(record.commit, C1, "the pin, not the upstream HEAD");
    assert!(record.listed);
    assert_eq!(record.trusted_commit, None);
    assert_eq!(messages(&mut cx), [format!("Installed swift at {C1}")]);
}

#[gpui::test]
async fn install_language_matches_the_id_ignoring_case(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let asker = FakeAsker::answering(&[]);
    let dyn_asker: Rc<dyn Asker> = asker.clone();
    let task = cx.update(|cx| install_language(&h.installer, "SWIFT", dyn_asker, cx));
    cx.run_until_parked();
    let outcome = task.await.unwrap();
    assert!(matches!(outcome, Outcome::Installed(ref i) if i.commit == C1));
    assert!(asker.questions().is_empty());

    let dyn_asker: Rc<dyn Asker> = asker.clone();
    let task = cx.update(|cx| install_language(&h.installer, "Cobol", dyn_asker, cx));
    let err = task.await.unwrap_err();
    assert_eq!(
        format!("{err}"),
        "no language named `Cobol` is on the list of language extensions"
    );
}

#[gpui::test]
async fn install_from_git_with_an_off_list_url_still_asks(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let asker = FakeAsker::answering(&[None]);
    let outcome = h.install_git(ZIG_URL, None, &asker, cx).await.unwrap();
    assert_eq!(outcome, Outcome::Declined);
    assert_eq!(asker.questions(), [install_question(&url(ZIG_URL), C3)]);
    assert_eq!(
        asker.questions()[0],
        format!(
            "Install {ZIG_URL} at {C3}? An extension runs code: it can download and run a language server program on this machine. Type yes or no. "
        )
    );

    // The listed source at another commit is off its pin and asks too.
    let asker = FakeAsker::answering(&[None]);
    h.install_git(SWIFT_URL, None, &asker, cx).await.unwrap();
    assert_eq!(asker.questions(), [install_question(&url(SWIFT_URL), C2)]);
}

#[gpui::test]
async fn answering_no_leaves_no_clone_and_no_record(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let network_before = h.git.inner.network_ops();
    for answer in [Some(false), None] {
        let asker = FakeAsker::answering(&[answer]);
        let outcome = h.install_git(ZIG_URL, None, &asker, cx).await.unwrap();
        assert_eq!(outcome, Outcome::Declined);
        assert_eq!(asker.questions().len(), 1);
    }
    assert_eq!(
        h.git.inner.network_ops() - network_before,
        2,
        "only the two ls-remote calls, no fetch"
    );
    assert!(
        !h.layout.git_dir().exists(),
        "nothing written: {:?}",
        h.layout.git_dir()
    );
    assert!(h.record("zig").is_none());
}

#[gpui::test]
async fn answering_yes_installs_the_shown_commit(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let asker = FakeAsker::answering(&[Some(true)]);
    let outcome = h.install_git(ZIG_URL, None, &asker, cx).await.unwrap();
    assert_eq!(asker.questions(), [install_question(&url(ZIG_URL), C3)]);
    let Outcome::Installed(installed) = outcome else {
        panic!("installed: {outcome:?}")
    };
    assert_eq!(installed.id, "zig");
    assert_eq!(installed.commit, C3);
    let record = h.record("zig").unwrap();
    assert_eq!(record.commit, C3);
    assert!(!record.listed);
    assert_eq!(record.trusted_commit.as_deref(), Some(C3));
    assert_eq!(record.requested_ref, None);
}

#[gpui::test]
async fn a_listed_url_at_its_pin_never_asks(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    for rev in [Some(C1), Some("v1")] {
        let asker = FakeAsker::answering(&[]);
        let outcome = h.install_git(SWIFT_URL, rev, &asker, cx).await.unwrap();
        assert!(asker.questions().is_empty(), "{rev:?} asked");
        assert!(matches!(outcome, Outcome::Installed(ref i) if i.commit == C1));
    }
    let record = h.record("swift").unwrap();
    assert!(record.listed);
    assert_eq!(record.trusted_commit, None);
}

#[gpui::test]
async fn off_list_update_asks_again_naming_both_commits(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    h.install_zig(cx).await;

    // Nothing new upstream: no question, nothing runs.
    let asker = FakeAsker::answering(&[]);
    let outcome = h.update("zig", false, &asker, cx).await.unwrap();
    assert_eq!(
        outcome,
        Outcome::UpToDate {
            id: "zig".into(),
            commit: C3.into()
        }
    );
    assert!(asker.questions().is_empty());

    h.git.inner.set_refs(&url(ZIG_URL), &[("HEAD", C4)]);
    let asker = FakeAsker::answering(&[Some(false)]);
    let outcome = h.update("zig", false, &asker, cx).await.unwrap();
    assert_eq!(outcome, Outcome::Declined);
    let question = update_question(&url(ZIG_URL), C3, C4);
    assert!(question.contains(C3) && question.contains(C4), "{question}");
    assert_eq!(asker.questions(), std::slice::from_ref(&question));
    assert_eq!(h.record("zig").unwrap().commit, C3, "nothing changed");

    // `latest` on an off-list install refetches its ref, and asks the same.
    let asker = FakeAsker::answering(&[Some(true)]);
    let outcome = h.update("zig", true, &asker, cx).await.unwrap();
    assert_eq!(asker.questions(), [question]);
    assert!(matches!(outcome, Outcome::Updated(ref i) if i.commit == C4));
    let record = h.record("zig").unwrap();
    assert_eq!(record.commit, C4);
    assert_eq!(record.trusted_commit.as_deref(), Some(C4));
}

#[gpui::test]
async fn listed_update_does_not_ask_and_latest_does(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let asker = FakeAsker::answering(&[]);
    h.install_git(SWIFT_URL, Some(C1), &asker, cx)
        .await
        .unwrap();

    let outcome = h.update("swift", false, &asker, cx).await.unwrap();
    assert!(asker.questions().is_empty(), "a listed update asks nothing");
    assert_eq!(
        outcome,
        Outcome::UpToDate {
            id: "swift".into(),
            commit: C1.into()
        }
    );

    // To latest: off the pin, so it asks with the warning and names both commits.
    let asker = FakeAsker::answering(&[Some(true)]);
    let outcome = h.update("swift", true, &asker, cx).await.unwrap();
    let question = latest_question(&url(SWIFT_URL), C1, C2);
    assert!(
        question.starts_with("Update to the latest upstream commit (not tested with this bongos)"),
        "{question}"
    );
    assert_eq!(asker.questions(), [question]);
    assert!(matches!(outcome, Outcome::Updated(ref i) if i.commit == C2));
    let record = h.record("swift").unwrap();
    assert!(!record.listed);
    assert_eq!(record.trusted_commit.as_deref(), Some(C2));
}

#[gpui::test]
async fn listed_update_moves_to_a_new_pin_without_asking(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    let asker = FakeAsker::answering(&[]);
    h.install_git(SWIFT_URL, Some(C1), &asker, cx)
        .await
        .unwrap();

    // A newer bongos pins Swift at C2.
    h.set_pin(C2, cx);
    let outcome = h.update("swift", false, &asker, cx).await.unwrap();
    assert!(asker.questions().is_empty(), "a listed update asks nothing");
    assert!(
        matches!(outcome, Outcome::Updated(ref i) if i.commit == C2),
        "{outcome:?}"
    );
    let record = h.record("swift").unwrap();
    assert_eq!(record.commit, C2);
    assert!(record.listed);
    assert_eq!(record.trusted_commit, None);
}

#[gpui::test]
async fn install_from_a_blocked_host_is_refused_before_any_network_call(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let asker = FakeAsker::answering(&[]);
    let network_before = h.git.inner.network_ops();
    let err = h
        .install_git("https://github.zed.dev/x/y", None, &asker, cx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("zed.dev"), "{err}");
    assert_eq!(h.git.inner.network_ops(), network_before);
    assert!(asker.questions().is_empty());
}

#[gpui::test]
async fn missing_url_opens_the_read(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let (workspace, mut cx) = window(&h, cx).await;
    let network_before = h.git.inner.network_ops();
    cx.dispatch_action(InstallFromGit::default());
    cx.run_until_parked();
    assert!(session_open(&workspace, &mut cx), "the URL read is open");
    assert_eq!(h.git.inner.network_ops(), network_before);
    assert!(!h.layout.git_dir().exists());
}

#[gpui::test]
async fn missing_name_opens_the_language_picker(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let (workspace, mut cx) = window(&h, cx).await;
    cx.dispatch_action(crate::InstallLanguage::default());
    cx.run_until_parked();
    assert!(session_open(&workspace, &mut cx));
}

#[gpui::test]
async fn disabled_answers_off_and_opens_nothing(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let (workspace, mut cx) = window(&h, cx).await;
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content
                    .extension
                    .language_extensions
                    .get_or_insert_default()
                    .enabled = Some(false);
            });
        });
    });
    cx.dispatch_action(InstallFromGit::default());
    cx.run_until_parked();
    assert!(!session_open(&workspace, &mut cx));
    assert_eq!(messages(&mut cx), [OFF_MESSAGE]);
}

#[gpui::test]
fn first_use_picker_lists_all_entries_with_description_and_source(_: &mut TestAppContext) {
    let list = LanguageList::embedded().unwrap();
    let candidates = language_candidates(&list);
    assert_eq!(candidates.len(), 18);
    for (candidate, entry) in candidates.iter().zip(list.entries()) {
        assert_eq!(candidate.primary.as_ref(), entry.name);
        let secondary = candidate.secondary.as_ref().unwrap();
        assert!(secondary.contains(&entry.description), "{secondary}");
        assert!(secondary.contains(&entry.repository), "{secondary}");
    }
}

#[gpui::test]
async fn ls_remote_failure_asks_nothing_and_writes_nothing(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    h.git
        .inner
        .fail_next_ls_remote(&url(ZIG_URL), "could not resolve host");
    let asker = FakeAsker::answering(&[Some(true)]);
    let err = h.install_git(ZIG_URL, None, &asker, cx).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("could not resolve host"),
        "{err:#}"
    );
    assert!(asker.questions().is_empty());
    assert!(!h.layout.git_dir().exists());
}

#[gpui::test]
async fn failure_after_yes_before_the_clone_leaves_nothing_and_asks_again(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    h.git
        .inner
        .fail_next_fetch(&url(ZIG_URL), "connection reset");
    let asker = FakeAsker::answering(&[Some(true), Some(false)]);
    h.install_git(ZIG_URL, None, &asker, cx).await.unwrap_err();
    assert!(h.record("zig").is_none());
    assert!(h.checkouts("zig").is_empty());

    h.install_git(ZIG_URL, None, &asker, cx).await.unwrap();
    assert_eq!(asker.questions().len(), 2, "the next attempt asks again");
}

#[gpui::test]
async fn a_held_lock_answers_busy_without_asking(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let lock = h.hold_lock(&h.layout.source_key(&url(ZIG_URL)));
    let asker = FakeAsker::answering(&[Some(true)]);
    let err = h.install_git(ZIG_URL, None, &asker, cx).await.unwrap_err();
    assert_eq!(
        format!("{err}"),
        format!("another bongos is installing {}", url(ZIG_URL))
    );
    assert!(asker.questions().is_empty());
    drop(lock);

    h.install_zig(cx).await;
    h.git.inner.set_refs(&url(ZIG_URL), &[("HEAD", C4)]);
    let lock = h.hold_lock("zig");
    let asker = FakeAsker::answering(&[Some(true)]);
    let err = h.update("zig", false, &asker, cx).await.unwrap_err();
    assert_eq!(
        format!("{err}"),
        "zig is busy: another bongos is changing it"
    );
    assert!(asker.questions().is_empty());
    drop(lock);
}

#[gpui::test]
async fn an_identical_command_while_asking_joins_the_question(cx: &mut TestAppContext) {
    let h = Harness::new(cx);
    let asker = Rc::new(FakeAsker::default());
    let answer = asker.hold();
    let first: Rc<dyn Asker> = asker.clone();
    let second: Rc<dyn Asker> = asker.clone();
    let first = cx.update(|cx| install_from_git(&h.installer, ZIG_URL, None, None, first, cx));
    cx.run_until_parked();
    let second = cx.update(|cx| install_from_git(&h.installer, ZIG_URL, None, None, second, cx));
    cx.run_until_parked();
    assert_eq!(asker.questions().len(), 1, "one question for both");
    answer.send(Some(true)).unwrap();
    cx.run_until_parked();
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(first, second);
    assert!(matches!(first, Outcome::Installed(ref i) if i.commit == C3));
}
