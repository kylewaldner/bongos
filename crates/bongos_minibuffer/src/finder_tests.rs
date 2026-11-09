//! Tests of the file finder (C-x C-f) and the tab switcher (C-x b) in the strip: each is hosted
//! there only while its name is in `minibuffer.enabled_for`, narrows by what is typed, and keeps
//! the centered modal with a preview (the file finder) when nothing is routed.

use std::sync::Arc;
use std::time::Duration;

use file_finder::{FileFinder, FileFinderDelegate};
use gpui::{
    BorrowAppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, WindowHandle,
};
use picker::{Picker, PickerDelegate as _};
use project::{Project, ProjectPath};
use settings::SettingsStore;
use tab_switcher::{TabSwitcher, TabSwitcherDelegate};
use util::{path, rel_path::rel_path};
use workspace::{AppState, MultiWorkspace, ToggleFileFinder, Workspace};
use zed_actions::minibuffer;

/// The file finder waits this long after typing before it searches.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(100);

fn init(cx: &mut TestAppContext, enabled_for: &[&str]) -> Arc<AppState> {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        file_finder::init(cx);
        tab_switcher::init(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for =
                    Some(enabled_for.iter().map(|name| name.to_string()).collect());
            });
        });
    });
    app_state
}

const FILES: [&str; 3] = ["src/alpha.rs", "src/beta.rs", "docs/gamma.md"];

async fn window(
    cx: &mut TestAppContext,
    enabled_for: &[&str],
) -> (Entity<Workspace>, VisualTestContext) {
    let app_state = init(cx, enabled_for);
    for file in FILES {
        let file = std::path::PathBuf::from(format!("{}/{file}", path!("/root")));
        app_state
            .fs
            .create_dir(file.parent().unwrap())
            .await
            .unwrap();
        app_state
            .fs
            .as_fake()
            .insert_file(file, b"text".to_vec())
            .await;
    }
    let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx);
    (workspace, cx)
}

async fn open_buffer(file: &str, workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    let project = workspace.read_with(cx, |workspace, _| workspace.project().clone());
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    let project_path = ProjectPath {
        worktree_id,
        path: rel_path(file).into(),
    };
    workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(project_path, None, true, window, cx)
        })
        .await
        .unwrap();
}

fn in_strip(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    })
}

fn centered_modal(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).has_active_modal()
            && workspace.modal_layer().read(cx).bottom_modal().is_none()
    })
}

fn open_file_finder(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<Picker<FileFinderDelegate>> {
    cx.dispatch_action(ToggleFileFinder::default());
    cx.run_until_parked();
    let finder = workspace
        .read_with(cx, |workspace, cx| workspace.active_modal::<FileFinder>(cx))
        .expect("the file finder is open");
    finder.read_with(cx, |finder, _| finder.picker().clone())
}

fn open_tab_switcher(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<Picker<TabSwitcherDelegate>> {
    cx.dispatch_action(tab_switcher::Toggle::default());
    cx.run_until_parked();
    let switcher = workspace
        .read_with(cx, |workspace, cx| {
            workspace.active_modal::<TabSwitcher>(cx)
        })
        .expect("the tab switcher is open");
    switcher.read_with(cx, |switcher, _| switcher.picker().clone())
}

fn type_in_file_finder(cx: &mut VisualTestContext, input: &str) {
    cx.simulate_input(input);
    cx.executor().advance_clock(SEARCH_DEBOUNCE);
    cx.run_until_parked();
}

fn file_finder_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<FileFinder>(cx).is_some()
    })
}

fn tab_switcher_open(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, cx| {
        workspace.active_modal::<TabSwitcher>(cx).is_some()
    })
}

fn active_title(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_item(cx)
            .map(|item| item.tab_content_text(0, cx).to_string())
            .unwrap_or_default()
    })
}

#[gpui::test]
async fn file_finder_routed_opens_in_the_strip_without_a_preview(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["file_finder"]).await;
    let cx = &mut cx;

    let picker = open_file_finder(&workspace, cx);
    assert!(in_strip(&workspace, cx), "the file finder is in the strip");
    assert!(!centered_modal(&workspace, cx), "no centered modal");
    assert!(cx.debug_bounds("minibuffer-input").is_some());
    picker.read_with(cx, |picker, cx| {
        assert!(!picker.has_preview(), "the strip has no preview");
        assert_eq!(
            picker.delegate.prompt_label(cx).as_deref(),
            Some("Find file: ")
        );
    });
}

#[gpui::test]
async fn file_finder_partial_path_narrows_the_matches(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["file_finder"]).await;
    let cx = &mut cx;
    let picker = open_file_finder(&workspace, cx);

    type_in_file_finder(cx, "src/alp");
    picker.read_with(cx, |picker, _| {
        let texts: Vec<_> = (0..picker.delegate.match_count())
            .filter_map(|ix| picker.delegate.match_text(ix))
            .collect();
        assert_eq!(texts.first().map(String::as_str), Some("src/alpha.rs"));
        assert!(
            texts
                .iter()
                .all(|text| !text.contains("beta") && !text.contains("gamma")),
            "only the path that was typed is left: {texts:?}"
        );
    });

    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert!(!file_finder_open(&workspace, cx));
    assert_eq!(active_title(&workspace, cx), "alpha.rs");
}

#[gpui::test]
async fn file_finder_actions_list_the_splits_and_copy_path(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["file_finder"]).await;
    let cx = &mut cx;
    let picker = open_file_finder(&workspace, cx);
    type_in_file_finder(cx, "gam");

    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    let labels = picker.update_in(cx, |picker, window, cx| {
        picker
            .delegate
            .actions_menu(window, cx)
            .into_iter()
            .filter_map(|action| match action {
                picker::PickerAction::Entry { label, .. } => Some(label.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    });
    for wanted in ["Left", "Right", "Up", "Down", "Copy path"] {
        assert!(
            labels.iter().any(|label| label == wanted),
            "{wanted} is listed in {labels:?}"
        );
    }

    // `c` runs the entry whose label starts with it, which is "Copy path".
    cx.simulate_keystrokes("c");
    cx.run_until_parked();
    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(
        copied.as_deref(),
        Some(format!("{}/docs/gamma.md", path!("/root")).as_str())
    );
    assert!(!file_finder_open(&workspace, cx), "copying closes it");
}

#[gpui::test]
async fn file_finder_actions_split_runs_from_the_menu_key(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["file_finder"]).await;
    let cx = &mut cx;
    open_buffer("src/alpha.rs", &workspace, cx).await;
    open_file_finder(&workspace, cx);
    type_in_file_finder(cx, "gam");
    assert_eq!(workspace.read_with(cx, |w, _| w.panes().len()), 1);

    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    cx.simulate_keystrokes("r");
    cx.run_until_parked();
    assert_eq!(
        workspace.read_with(cx, |w, _| w.panes().len()),
        2,
        "the file opened in a split"
    );
    assert_eq!(active_title(&workspace, cx), "gamma.md");
}

#[gpui::test]
async fn tab_switcher_routed_opens_in_the_strip(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["tab_switcher"]).await;
    let cx = &mut cx;
    open_buffer("src/alpha.rs", &workspace, cx).await;
    open_buffer("src/beta.rs", &workspace, cx).await;

    let picker = open_tab_switcher(&workspace, cx);
    assert!(in_strip(&workspace, cx), "the tab switcher is in the strip");
    assert!(!centered_modal(&workspace, cx), "no centered modal");
    assert!(cx.debug_bounds("minibuffer-input").is_some());
    picker.read_with(cx, |picker, cx| {
        assert_eq!(
            picker.delegate.prompt_label(cx).as_deref(),
            Some("Switch to buffer: ")
        );
        assert_eq!(picker.delegate.match_count(), 2);
    });
}

#[gpui::test]
async fn tab_switcher_routed_partial_name_narrows(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["tab_switcher"]).await;
    let cx = &mut cx;
    open_buffer("src/alpha.rs", &workspace, cx).await;
    open_buffer("src/beta.rs", &workspace, cx).await;
    open_buffer("docs/gamma.md", &workspace, cx).await;

    let picker = open_tab_switcher(&workspace, cx);
    picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.match_count(), 3));

    cx.simulate_input("alp");
    cx.run_until_parked();
    picker.read_with(cx, |picker, _| {
        assert_eq!(picker.delegate.match_count(), 1);
        assert_eq!(picker.delegate.match_text(0).as_deref(), Some("alpha.rs"));
    });

    cx.dispatch_action(minibuffer::Confirm);
    cx.run_until_parked();
    assert!(!tab_switcher_open(&workspace, cx));
    assert_eq!(active_title(&workspace, cx), "alpha.rs");
}

#[gpui::test]
async fn tab_switcher_opened_by_a_chord_does_not_confirm_on_modifier_release(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = window(cx, &["tab_switcher"]).await;
    let cx = &mut cx;
    open_buffer("src/alpha.rs", &workspace, cx).await;
    open_buffer("src/beta.rs", &workspace, cx).await;

    // C-x b: nothing is held when the switcher opens, so letting go of control later is not an
    // answer.
    open_tab_switcher(&workspace, cx);
    cx.simulate_modifiers_change(Modifiers::control());
    cx.simulate_modifiers_change(Modifiers::none());
    cx.run_until_parked();
    assert!(tab_switcher_open(&workspace, cx), "still waiting for input");
    assert!(in_strip(&workspace, cx));
}

#[gpui::test]
async fn tab_switcher_opened_with_a_held_modifier_confirms_on_release(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &["tab_switcher"]).await;
    let cx = &mut cx;
    open_buffer("src/alpha.rs", &workspace, cx).await;
    open_buffer("src/beta.rs", &workspace, cx).await;

    cx.simulate_modifiers_change(Modifiers::control());
    open_tab_switcher(&workspace, cx);
    cx.simulate_modifiers_change(Modifiers::none());
    cx.run_until_parked();
    assert!(!tab_switcher_open(&workspace, cx));
    assert_eq!(active_title(&workspace, cx), "alpha.rs");
}

async fn file_finder_unrouted(cx: &mut TestAppContext, enabled_for: &[&str]) {
    let (workspace, mut cx) = window(cx, enabled_for).await;
    let cx = &mut cx;
    let picker = open_file_finder(&workspace, cx);
    assert!(centered_modal(&workspace, cx), "a centered modal");
    assert!(!in_strip(&workspace, cx));
    assert!(cx.debug_bounds("minibuffer-input").is_none());
    picker.read_with(cx, |picker, _| assert!(picker.has_preview()));
}

#[gpui::test]
async fn file_finder_unrouted_is_centered_with_a_preview(cx: &mut TestAppContext) {
    file_finder_unrouted(cx, &[]).await;
}

#[gpui::test]
async fn file_finder_not_in_enabled_for_stays_centered(cx: &mut TestAppContext) {
    file_finder_unrouted(cx, &["something_else"]).await;
}

#[gpui::test]
async fn tab_switcher_unrouted_is_centered_and_has_no_input(cx: &mut TestAppContext) {
    let (workspace, mut cx) = window(cx, &[]).await;
    let cx = &mut cx;
    open_buffer("src/alpha.rs", &workspace, cx).await;
    open_buffer("src/beta.rs", &workspace, cx).await;

    let picker = open_tab_switcher(&workspace, cx);
    assert!(centered_modal(&workspace, cx), "a centered modal");
    assert!(!in_strip(&workspace, cx));
    assert!(cx.debug_bounds("minibuffer-input").is_none());
    picker.read_with(cx, |picker, _| assert_eq!(picker.delegate.match_count(), 2));
}
