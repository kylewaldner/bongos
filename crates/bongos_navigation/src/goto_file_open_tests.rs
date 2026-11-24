//! Tests of the go to file variants (other pane, split right, split down), the pane entries of its
//! M-o menu, and creating the typed file with C-j: it is made once, never truncated, and opened.

use std::{path::PathBuf, sync::Arc};

use bongos_minibuffer::SessionHandle;
use bongos_window::{
    picker::is_picking,
    test_support::{add_editor, split_center},
};
use fs::FakeFs;
use gpui::{
    Action, BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext,
    WindowHandle,
};
use project::{Fs as _, Project};
use serde_json::json;
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace, Pane, Workspace};
use zed_actions::minibuffer;

use crate::actions::{GoToFileOtherPane, GoToFileSplitDown, GoToFileSplitRight};
use crate::goto_file::{HOST, start};
use crate::target::PaneChoice;

/// A workspace on `/root` holding some Kotlin files, with the `extra` files outside it.
async fn setup(
    cx: &mut TestAppContext,
    extra: &[(&str, &str)],
) -> (Entity<Workspace>, VisualTestContext, Arc<FakeFs>) {
    let params = cx.update(|cx| {
        let params = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        crate::init(cx);
        params
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(vec![HOST.to_string()]);
            });
        });
    });
    let fake = params.fs.as_fake();
    fake.insert_tree(
        "/root",
        json!({
            ".git": {},
            "src": {
                "Main.kt": "fun main() {}\n",
                "Alpha.kt": "alpha\n",
                "Beta.kt": "beta\n",
            },
        }),
    )
    .await;
    for (path, content) in extra {
        let path = PathBuf::from(path);
        let parent = path.parent().expect("a file has a directory");
        fake.create_dir(parent)
            .await
            .expect("the directory is created");
        fake.insert_file(&path, content.as_bytes().to_vec()).await;
    }
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("the window holds a workspace");
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    (workspace, cx, fake)
}

fn center_panes(
    workspace: &Entity<Workspace>,
    count: usize,
    cx: &mut VisualTestContext,
) -> Vec<Entity<Pane>> {
    let panes = cx.update(|window, cx| {
        let panes = split_center(workspace, count, window, cx);
        for pane in &panes {
            add_editor(workspace, pane, window, cx);
        }
        panes
    });
    cx.run_until_parked();
    panes
}

fn activate(workspace: &Entity<Workspace>, pane: &Entity<Pane>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.focus(&pane.focus_handle(cx), cx));
    cx.run_until_parked();
    assert_eq!(
        &workspace.read_with(cx, |w, _| w.active_pane().clone()),
        pane
    );
}

fn all_panes(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Entity<Pane>> {
    workspace.read_with(cx, |workspace, _| workspace.center_panes())
}

fn active_path(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<String> {
    cx.read(|cx| {
        let item = pane.read(cx).active_item()?;
        item.project_path(cx)
            .map(|path| path.path.as_unix_str().to_string())
    })
}

fn has_preview(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> bool {
    cx.read(|cx| pane.read(cx).preview().is_some())
}

/// The absolute path of the active item of the active pane.
fn opened_abs_path(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Option<PathBuf> {
    cx.read(|cx| {
        let workspace = workspace.read(cx);
        let item = workspace.active_pane().read(cx).active_item()?;
        let path = item.project_path(cx)?;
        workspace.project().read(cx).absolute_path(&path, cx)
    })
}

fn open_picker(
    workspace: &Entity<Workspace>,
    choice: PaneChoice,
    cx: &mut VisualTestContext,
) -> SessionHandle {
    let handle = cx.update(|window, cx| start(workspace, choice, window, cx));
    cx.run_until_parked();
    handle.expect("the picker opens")
}

fn type_input(handle: &SessionHandle, text: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| handle.set_input(text, window, cx));
    cx.run_until_parked();
}

fn confirm_input(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::ConfirmInput);
    cx.run_until_parked();
}

fn answer(key: &str, cx: &mut VisualTestContext) {
    cx.simulate_keystrokes(key);
    cx.run_until_parked();
}

/// Starts the picker with the action, types `input` and returns before confirming.
fn start_variant(action: impl Action, input: &str, cx: &mut VisualTestContext) {
    cx.dispatch_action(action);
    cx.run_until_parked();
    cx.simulate_input(input);
    cx.run_until_parked();
}

fn confirm(cx: &mut VisualTestContext) {
    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
}

async fn text_of(fake: &FakeFs, path: &str) -> String {
    fake.load(path.as_ref())
        .await
        .unwrap_or_else(|error| panic!("{path} is readable: {error}"))
}

#[gpui::test]
async fn goto_file_variants_open_in_the_pane_the_choice_resolves_to(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = setup(cx, &[("/elsewhere/outside.txt", "outside\n")]).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[0], cx);

    // Other pane: previews and opens in the next pane, and splits nothing.
    start_variant(GoToFileOtherPane, "Beta", cx);
    assert!(
        has_preview(&panes[1], cx),
        "the preview is in the other pane"
    );
    assert!(!has_preview(&panes[0], cx));
    confirm(cx);
    assert_eq!(active_path(&panes[1], cx).as_deref(), Some("src/Beta.kt"));
    assert_eq!(all_panes(&workspace, cx).len(), 2);

    // Split right: previews in the pane to split, and the confirm splits once.
    activate(&workspace, &panes[0], cx);
    start_variant(GoToFileSplitRight, "Alpha", cx);
    assert!(
        has_preview(&panes[0], cx),
        "the preview is in the pane to split"
    );
    assert_eq!(
        all_panes(&workspace, cx).len(),
        2,
        "previewing adds no pane"
    );
    confirm(cx);
    let all = all_panes(&workspace, cx);
    assert_eq!(all.len(), 3);
    let right = all
        .iter()
        .find(|pane| !panes.contains(pane))
        .expect("a new pane")
        .clone();
    assert_eq!(active_path(&right, cx).as_deref(), Some("src/Alpha.kt"));

    // Split down.
    activate(&workspace, &panes[0], cx);
    start_variant(GoToFileSplitDown, "Main", cx);
    confirm(cx);
    let all = all_panes(&workspace, cx);
    assert_eq!(all.len(), 4);
    let down = all
        .iter()
        .find(|pane| !panes.contains(pane) && **pane != right)
        .expect("a second new pane")
        .clone();
    assert_eq!(active_path(&down, cx).as_deref(), Some("src/Main.kt"));

    // A path outside the project opens in the default pane and splits nothing.
    start_variant(GoToFileSplitRight, "/elsewhere/outside.txt", cx);
    confirm(cx);
    assert_eq!(all_panes(&workspace, cx).len(), 4);
    assert_eq!(
        opened_abs_path(&workspace, cx),
        Some(PathBuf::from("/elsewhere/outside.txt"))
    );
}

#[gpui::test]
async fn goto_file_m_o_lists_the_four_pane_entries(cx: &mut TestAppContext) {
    let (workspace, mut cx, _) = setup(cx, &[]).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[0], cx);

    for (key, count) in [("o", 2), ("r", 3), ("d", 4)] {
        activate(&workspace, &panes[0], cx);
        let handle = open_picker(&workspace, PaneChoice::Default, cx);
        type_input(&handle, "Beta", cx);
        cx.dispatch_action(minibuffer::Actions);
        cx.run_until_parked();
        answer(key, cx);
        assert_eq!(all_panes(&workspace, cx).len(), count, "entry {key}");
        let opened = all_panes(&workspace, cx)
            .into_iter()
            .any(|pane| active_path(&pane, cx).as_deref() == Some("src/Beta.kt"));
        assert!(opened, "entry {key} opens the row");
    }

    activate(&workspace, &panes[0], cx);
    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    type_input(&handle, "Beta", cx);
    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    answer("p", cx);
    assert!(
        cx.update(|window, cx| is_picking(window, cx)),
        "p asks for a pane"
    );
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
}

#[gpui::test]
async fn goto_file_c_j_creates_a_new_file_in_an_existing_directory_and_opens_it(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, fake) = setup(cx, &[]).await;
    let cx = &mut cx;
    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    type_input(&handle, "src/New.kt", cx);
    confirm_input(cx);

    assert!(fake.is_file("/root/src/New.kt".as_ref()).await);
    assert_eq!(text_of(&fake, "/root/src/New.kt").await, "");
    assert_eq!(
        opened_abs_path(&workspace, cx),
        Some(PathBuf::from("/root/src/New.kt"))
    );
}

#[gpui::test]
async fn goto_file_c_j_with_a_missing_directory_asks_and_creates_both_on_yes(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, fake) = setup(cx, &[]).await;
    let cx = &mut cx;
    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    type_input(&handle, "src/new/Deep.kt", cx);
    confirm_input(cx);
    assert!(
        !fake.is_dir("/root/src/new".as_ref()).await,
        "nothing before the answer"
    );

    answer("y", cx);
    assert!(fake.is_dir("/root/src/new".as_ref()).await);
    assert!(fake.is_file("/root/src/new/Deep.kt".as_ref()).await);
    assert_eq!(
        opened_abs_path(&workspace, cx),
        Some(PathBuf::from("/root/src/new/Deep.kt"))
    );
}

#[gpui::test]
async fn goto_file_c_j_creates_nothing_for_an_empty_input(cx: &mut TestAppContext) {
    let (workspace, mut cx, fake) = setup(cx, &[]).await;
    let cx = &mut cx;
    let before = fake.paths(true);
    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    confirm_input(cx);
    assert_eq!(fake.paths(true), before, "an empty input creates nothing");
    type_input(&handle, "   ", cx);
    confirm_input(cx);
    assert_eq!(fake.paths(true), before, "a blank input creates nothing");
}

#[gpui::test]
async fn goto_file_c_j_creates_nothing_when_the_directory_question_is_answered_no(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, fake) = setup(cx, &[]).await;
    let cx = &mut cx;
    let before = fake.paths(true);
    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    type_input(&handle, "src/new/Deep.kt", cx);
    confirm_input(cx);
    answer("n", cx);
    assert_eq!(fake.paths(true), before, "no creates nothing");
    assert_ne!(
        opened_abs_path(&workspace, cx),
        Some(PathBuf::from("/root/src/new/Deep.kt"))
    );
}

#[gpui::test]
async fn goto_file_ret_creates_nothing_before_results_arrive_or_for_a_typo(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, fake) = setup(cx, &[]).await;
    let cx = &mut cx;
    let before = fake.paths(true);
    let handle = open_picker(&workspace, PaneChoice::Default, cx);

    // RET right after typing, before the source has answered.
    cx.update(|window, cx| handle.set_input("src/Typo.kt", window, cx));
    confirm(cx);
    assert_eq!(
        fake.paths(true),
        before,
        "RET before results creates nothing"
    );
    assert!(handle.is_open(), "the picker stays open");

    // RET on an input no row matches.
    type_input(&handle, "zzzqqq", cx);
    confirm(cx);
    assert_eq!(fake.paths(true), before, "RET on a typo creates nothing");
    assert!(handle.is_open());

    // C-j on the same input still creates.
    type_input(&handle, "src/Typo.kt", cx);
    confirm_input(cx);
    assert!(fake.is_file("/root/src/Typo.kt".as_ref()).await);
}

#[gpui::test]
async fn goto_file_c_j_refuses_a_leading_tilde_that_is_not_the_home_directory(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, fake) = setup(cx, &[]).await;
    let cx = &mut cx;
    let before = fake.paths(true);
    for input in ["~", "~user/x.kt"] {
        let handle = open_picker(&workspace, PaneChoice::Default, cx);
        type_input(&handle, input, cx);
        confirm_input(cx);
        assert_eq!(fake.paths(true), before, "{input} creates nothing");
        cx.simulate_keystrokes("ctrl-g");
        cx.run_until_parked();
    }
}

#[gpui::test]
async fn goto_file_c_j_failure_is_shown_and_opens_nothing(cx: &mut TestAppContext) {
    let (workspace, mut cx, fake) = setup(cx, &[]).await;
    let cx = &mut cx;
    // The parent of the new file is a file, so the file cannot be made.
    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    type_input(&handle, "src/Main.kt/Child.kt", cx);
    confirm_input(cx);
    assert!(!fake.is_file("/root/src/Main.kt/Child.kt".as_ref()).await);
    assert_eq!(text_of(&fake, "/root/src/Main.kt").await, "fun main() {}\n");
    assert_eq!(opened_abs_path(&workspace, cx), None, "nothing is opened");
}

#[gpui::test]
async fn goto_file_m_o_entries_open_an_outside_project_row_without_splitting(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, _) = setup(cx, &[("/elsewhere/outside.txt", "outside\n")]).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[0], cx);
    for key in ["r", "d", "o"] {
        let handle = open_picker(&workspace, PaneChoice::Default, cx);
        type_input(&handle, "/elsewhere/outside.txt", cx);
        cx.dispatch_action(minibuffer::Actions);
        cx.run_until_parked();
        answer(key, cx);
        assert_eq!(
            all_panes(&workspace, cx).len(),
            2,
            "entry {key} splits nothing"
        );
        assert_eq!(
            opened_abs_path(&workspace, cx),
            Some(PathBuf::from("/elsewhere/outside.txt")),
            "entry {key} opens the file"
        );
    }
}

#[gpui::test]
async fn goto_file_c_j_never_truncates_an_existing_file(cx: &mut TestAppContext) {
    let (workspace, mut cx, fake) = setup(cx, &[("/elsewhere/outside.txt", "outside\n")]).await;
    let cx = &mut cx;

    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    type_input(&handle, "src/Main.kt", cx);
    confirm_input(cx);
    assert_eq!(text_of(&fake, "/root/src/Main.kt").await, "fun main() {}\n");
    assert_eq!(
        opened_abs_path(&workspace, cx),
        Some(PathBuf::from("/root/src/Main.kt"))
    );

    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    type_input(&handle, "/elsewhere/outside.txt", cx);
    confirm_input(cx);
    assert_eq!(text_of(&fake, "/elsewhere/outside.txt").await, "outside\n");
    assert_eq!(
        opened_abs_path(&workspace, cx),
        Some(PathBuf::from("/elsewhere/outside.txt"))
    );
}

#[gpui::test]
async fn goto_file_a_file_written_while_the_parent_prompt_is_open_keeps_its_content(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, fake) = setup(cx, &[]).await;
    let cx = &mut cx;
    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    type_input(&handle, "src/race/Race.kt", cx);
    confirm_input(cx);

    // Another writer makes the directory and the file while the question is open.
    fake.create_dir("/root/src/race".as_ref())
        .await
        .expect("the directory is created");
    fake.insert_file("/root/src/race/Race.kt", b"other writer\n".to_vec())
        .await;
    answer("y", cx);

    assert_eq!(
        text_of(&fake, "/root/src/race/Race.kt").await,
        "other writer\n"
    );
    assert_eq!(
        opened_abs_path(&workspace, cx),
        Some(PathBuf::from("/root/src/race/Race.kt"))
    );
}

#[gpui::test]
async fn goto_file_a_new_file_outside_the_project_is_made_by_fs_and_opened_by_path(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx, fake) = setup(cx, &[("/elsewhere/outside.txt", "outside\n")]).await;
    let cx = &mut cx;
    let panes = center_panes(&workspace, 2, cx);
    activate(&workspace, &panes[0], cx);

    // The directory is made by `fs.create_dir`; the file by `fs.create_file`, which keeps what a
    // later create finds.
    let handle = open_picker(&workspace, PaneChoice::SplitRight, cx);
    type_input(&handle, "/elsewhere/new/Out.txt", cx);
    confirm_input(cx);
    answer("y", cx);
    assert!(fake.is_file("/elsewhere/new/Out.txt".as_ref()).await);
    assert_eq!(
        opened_abs_path(&workspace, cx),
        Some(PathBuf::from("/elsewhere/new/Out.txt"))
    );
    assert_eq!(
        all_panes(&workspace, cx).len(),
        2,
        "the choice is ignored outside the project"
    );

    fake.insert_file("/elsewhere/new/Out.txt", b"kept\n".to_vec())
        .await;
    let handle = open_picker(&workspace, PaneChoice::Default, cx);
    type_input(&handle, "/elsewhere/new/Out.txt", cx);
    confirm_input(cx);
    assert_eq!(text_of(&fake, "/elsewhere/new/Out.txt").await, "kept\n");
}
