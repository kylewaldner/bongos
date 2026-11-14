//! Working copies and master in the editor: `project: edit` and `project: abandon`, the
//! "(master)" marker, the offer on the first edit and on save, and the buffers outside master
//! that save as they always did. Real git, real SQLite and the real file system in a `TempDir`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bongos_project_repo::{
    ChangeFilter, ChangeState, LoadedProject, ProjectRepo, ProjectRepositorySettings,
};
use bongos_work_model::Author;
use chrono::{DateTime, Duration as Days, Utc};
use editor::Editor;
use fs::RealFs;
use gpui::{BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext};
use project::Project;
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;
use workspace::{AppState, MultiWorkspace, OpenOptions, Save, SaveAll, Workspace};

use crate::command_tests::{dispatch, wait_until};
use crate::modals::{ChoicePicker, TextPrompt};
use crate::seam_tests::project_repo;
use crate::store_tests::canonical;
use crate::working_copy_commands::working_copy_label;

const MASTER_FILE: &str = "definitions/tasks/leaf/task.toml";

struct Fixture {
    workspace: Entity<Workspace>,
    repo: ProjectRepo,
    master: PathBuf,
    outside: PathBuf,
    _temp: TempDir,
}

fn start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

async fn fixture(cx: &mut TestAppContext) -> (Fixture, &mut VisualTestContext) {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let folder = canonical(&temp).join("project");
    let repo = project_repo(&folder);
    repo.protect_master_files().unwrap();
    let outside = canonical(&temp).join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("notes.txt"), "plain\n").unwrap();
    cx.update(|cx| {
        AppState::test(cx);
        editor::init(cx);
        ProjectRepositorySettings::register(cx);
        crate::master_files::init(cx);
        let loaded = LoadedProject::new(repo.clone());
        crate::project_repos::note_open_repo(&loaded, cx);
    });
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let project = Project::test(
        fs,
        [repo.location().repository.as_path(), outside.as_path()],
        cx,
    )
    .await;
    let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
        let multi_workspace = MultiWorkspace::test_new(project, window, cx);
        multi_workspace.workspace().update(cx, |workspace, _| {
            crate::working_copy_commands::register(workspace);
            crate::master_files::register(workspace);
        });
        multi_workspace
    });
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx)
    });
    let master = repo.location().repository.join(MASTER_FILE);
    (
        Fixture {
            workspace,
            repo,
            master,
            outside,
            _temp: temp,
        },
        cx,
    )
}

impl Fixture {
    async fn open(&self, cx: &mut VisualTestContext, path: &Path) -> Entity<Editor> {
        let path = path.to_path_buf();
        let opening = self.workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_abs_path(path, OpenOptions::default(), window, cx)
        });
        let item = opening.await.unwrap();
        cx.run_until_parked();
        item.downcast::<Editor>().expect("an editor")
    }

    fn picker(&self, cx: &mut VisualTestContext) -> Option<Entity<ChoicePicker>> {
        self.workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<ChoicePicker>(cx)
        })
    }

    fn prompt(&self, cx: &mut VisualTestContext) -> Option<Entity<TextPrompt>> {
        self.workspace
            .read_with(cx, |workspace, cx| workspace.active_modal::<TextPrompt>(cx))
    }

    async fn pick(&self, cx: &mut VisualTestContext, query: &'static str) {
        wait_until(cx, "a picker", |cx| self.picker(cx).is_some()).await;
        let picker = self.picker(cx).unwrap();
        picker.update_in(cx, |modal, window, cx| {
            modal
                .picker
                .update(cx, |picker, cx| picker.set_query(query, window, cx))
        });
        cx.run_until_parked();
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
    }

    async fn type_text(&self, cx: &mut VisualTestContext, text: &'static str) {
        wait_until(cx, "a prompt", |cx| self.prompt(cx).is_some()).await;
        let prompt = self.prompt(cx).unwrap();
        prompt.update_in(cx, |prompt, window, cx| {
            prompt
                .editor
                .update(cx, |editor, cx| editor.set_text(text, window, cx))
        });
        cx.dispatch_action(menu::Confirm);
        cx.run_until_parked();
    }

    fn master_bytes(&self) -> Vec<u8> {
        std::fs::read(&self.master).unwrap()
    }

    fn copies(&self) -> Vec<bongos_project_repo::ChangeRow> {
        self.repo.changes(&ChangeFilter::default()).unwrap()
    }
}

fn is_dirty(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> bool {
    editor.read_with(cx, |editor, cx| {
        editor.buffer().read(cx).read(cx).is_dirty()
    })
}

fn edit(editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| editor.set_text(text, window, cx));
    cx.run_until_parked();
}

async fn wait_for_clean(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    for _ in 0..200 {
        cx.run_until_parked();
        if !is_dirty(editor, cx) {
            return;
        }
        cx.executor().timer(Duration::from_millis(25)).await;
    }
    panic!("the buffer stayed dirty");
}

#[gpui::test]
async fn a_master_buffer_carries_the_marker_and_a_buffer_outside_master_does_not(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(cx).await;
    let master = fixture.open(cx, &fixture.master).await;
    let outside = fixture.open(cx, &fixture.outside.join("notes.txt")).await;
    let title = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
        editor.read_with(cx, |editor, cx| editor.title(cx).to_string())
    };
    assert!(
        title(&master, cx).ends_with("(master)"),
        "{}",
        title(&master, cx)
    );
    assert!(!title(&outside, cx).contains("(master)"));
    cx.update(|_, cx| {
        assert!(crate::master_file_at(&fixture.master, cx).is_some());
        assert!(crate::master_file_at(&fixture.outside.join("notes.txt"), cx).is_none());
    });
}

#[gpui::test]
async fn saving_a_master_buffer_offers_a_working_copy_and_writes_there(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    let before = fixture.master_bytes();
    let editor = fixture.open(cx, &fixture.master).await;
    edit(&editor, "edited = true\n", cx);

    // The offer on the first edit is dismissed; the buffer is dirty, and a save offers again.
    wait_until(cx, "the first offer", |cx| fixture.picker(cx).is_some()).await;
    cx.dispatch_action(menu::Cancel);
    cx.run_until_parked();
    assert!(is_dirty(&editor, cx));
    cx.dispatch_action(Save { save_intent: None });
    cx.run_until_parked();
    fixture.pick(cx, "New working copy").await;
    fixture.type_text(cx, "fix-leaf").await;
    wait_for_clean(&editor, cx).await;

    let copy = fixture.repo.working_copy_path("fix-leaf");
    assert_eq!(
        std::fs::read_to_string(copy.join(MASTER_FILE)).unwrap(),
        "edited = true\n"
    );
    assert_eq!(fixture.master_bytes(), before);
    assert_eq!(fixture.copies().len(), 1);
    assert_eq!(fixture.copies()[0].branch, "work/fix-leaf");
}

#[gpui::test]
async fn the_first_edit_makes_the_offer_and_leaves_the_buffer_clean_so_nothing_saves_to_master(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(cx).await;
    let before = fixture.master_bytes();
    let existing = fixture
        .repo
        .create_working_copy("existing-one", "work/", &Author::agent("tester"), start())
        .await
        .unwrap();
    let editor = fixture.open(cx, &fixture.master).await;
    edit(&editor, "moved = true\n", cx);

    fixture.pick(cx, "Existing working copy").await;
    fixture.pick(cx, "existing-one").await;
    wait_for_clean(&editor, cx).await;
    assert_eq!(
        std::fs::read_to_string(existing.path.join(MASTER_FILE)).unwrap(),
        "moved = true\n"
    );

    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.workspace.autosave = Some(settings::AutosaveSetting::AfterDelay {
                    milliseconds: 100.try_into().unwrap(),
                });
            });
        });
    });
    cx.dispatch_action(SaveAll { save_intent: None });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(!is_dirty(&editor, cx));
    assert_eq!(fixture.master_bytes(), before);
}

#[gpui::test]
async fn discarding_the_offer_reloads_the_master_buffer(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    let before = fixture.master_bytes();
    let editor = fixture.open(cx, &fixture.master).await;
    edit(&editor, "thrown away\n", cx);
    fixture.pick(cx, "Discard").await;
    wait_for_clean(&editor, cx).await;
    let text = editor.read_with(cx, |editor, cx| editor.text(cx));
    assert_eq!(text.as_bytes(), before.as_slice());
    assert_eq!(fixture.master_bytes(), before);
    assert!(fixture.copies().is_empty());
}

#[gpui::test]
async fn saving_a_buffer_outside_master_saves_normally(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    let editor = fixture.open(cx, &fixture.outside.join("notes.txt")).await;
    edit(&editor, "changed\n", cx);
    assert!(fixture.picker(cx).is_none());
    cx.dispatch_action(Save { save_intent: None });
    wait_for_clean(&editor, cx).await;
    assert_eq!(
        std::fs::read_to_string(fixture.outside.join("notes.txt")).unwrap(),
        "changed\n"
    );
    assert!(fixture.copies().is_empty());
}

#[gpui::test]
async fn project_edit_makes_a_working_copy_and_a_second_one_is_independent(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(cx).await;
    dispatch(cx, "project::Edit");
    fixture.type_text(cx, "first-copy").await;
    wait_until(cx, "the working copy", |_| {
        fixture
            .repo
            .working_copy_path("first-copy")
            .join("definitions")
            .is_dir()
    })
    .await;
    // The command opened the copy in a new workspace; a second edit, made through the same
    // model call the command uses, is independent of the first.
    fixture
        .repo
        .create_working_copy("second-copy", "work/", &Author::agent("tester"), start())
        .await
        .unwrap();
    let rows = fixture.copies();
    assert_eq!(rows.len(), 2);
    let mut branches: Vec<_> = rows.iter().map(|row| row.branch.as_str()).collect();
    branches.sort();
    assert_eq!(branches, ["work/first-copy", "work/second-copy"]);
    assert!(rows.iter().all(|row| row.state == ChangeState::Open));
    assert!(
        !fixture
            .repo
            .working_copy_path("first-copy")
            .starts_with(&fixture.repo.location().repository)
    );
}

#[gpui::test]
async fn project_abandon_asks_then_removes_the_working_copy(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx).await;
    let copy = fixture
        .repo
        .create_working_copy("doomed", "work/", &Author::agent("tester"), start())
        .await
        .unwrap();
    dispatch(cx, "project::Abandon");
    fixture.pick(cx, "doomed").await;
    // Keeping is the default answer of the question.
    fixture.pick(cx, "Keep it").await;
    cx.run_until_parked();
    assert!(copy.path.is_dir());
    dispatch(cx, "project::Abandon");
    fixture.pick(cx, "doomed").await;
    fixture.pick(cx, "Abandon doomed").await;
    wait_until(cx, "the abandon", |_| !copy.path.exists()).await;
    for _ in 0..100 {
        cx.run_until_parked();
        if fixture.copies().is_empty() {
            break;
        }
        cx.executor().timer(Duration::from_millis(25)).await;
    }
    let row = fixture.repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.state, ChangeState::Abandoned);
}

#[test]
fn a_working_copy_idle_for_fifteen_days_is_labeled_stale() {
    let temp = TempDir::new().unwrap();
    let repo = project_repo(&temp.path().join("project"));
    let copy = futures::executor::block_on(repo.create_working_copy(
        "idle-one",
        "work/",
        &Author::agent("tester"),
        start(),
    ))
    .unwrap();
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert!(!working_copy_label(&row, start() + Days::days(3), 14).contains("stale"));
    assert!(working_copy_label(&row, start() + Days::days(15), 14).contains("stale"));
}

#[gpui::test]
fn the_settings_for_working_copies_have_their_defaults(cx: &mut TestAppContext) {
    cx.update(|cx| {
        AppState::test(cx);
        ProjectRepositorySettings::register(cx);
        let settings = ProjectRepositorySettings::get_global(cx);
        assert_eq!(settings.branch_prefix, "work/");
        assert!(!settings.keep_landed);
        assert_eq!(settings.stale_after_days, 14);
    });
}
