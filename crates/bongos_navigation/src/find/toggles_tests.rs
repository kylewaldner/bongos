//! Tests of the toggles, the scopes and the flag history of the find in files picker.

use bongos_minibuffer::{HistoryStore, SessionView};
use bongos_navigation_model::flags::{CaseMode, Flags};
use db::kvp::KeyValueStore;
use gpui::{Entity, TestAppContext, VisualTestContext};
use workspace::Workspace;
use zed_actions::minibuffer;

use crate::find::engine::FindRow;
use crate::find::flag_history;
use crate::find::picker::tests::{dispatch, find, open_file, rows, setup, type_text};
use crate::find::picker::{FindPicker, HISTORY};
use crate::registry;

fn view(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<SessionView> {
    cx.read(|cx| workspace.read(cx).active_modal::<SessionView>(cx))
        .expect("a session is open")
}

fn prompt(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let view = view(workspace, cx);
    cx.read(|cx| view.read(cx).prompt_text().to_string())
}

fn status(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let view = view(workspace, cx);
    cx.read(|cx| view.read(cx).status().to_string())
}

fn current_picker(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> std::rc::Rc<dyn registry::SessionHandle> {
    cx.read(|cx| registry::current(cx, workspace.entity_id()))
        .expect("a picker is open")
}

/// Runs a find with the regex and whole word flags and confirms its one row, so that the input
/// and its flags are in the history.
async fn confirm_a_regex_find(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    open_file(workspace, "a.txt", cx).await;
    find(workspace, false, cx);
    type_text("alp.a", cx);
    dispatch("navigation::ToggleRegex", cx);
    dispatch("navigation::ToggleWholeWord", cx);
    dispatch("navigation::ScopeOpenBuffers", cx);
    dispatch("menu::Confirm", cx);
    assert!(cx.read(|cx| registry::current(cx, workspace.entity_id()).is_none()));
}

fn all_flags() -> Flags {
    Flags {
        regex: true,
        case: CaseMode::Smart,
        whole_word: true,
        open_buffers: true,
    }
}

#[gpui::test]
async fn toggles_update_the_prompt_and_search_again(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", "alpha alphabet\n")]).await;
    let cx = &mut cx;
    open_file(&workspace, "a.txt", cx).await;
    let picker = find(&workspace, false, cx);
    type_text("alp.a", cx);
    assert_eq!(prompt(&workspace, cx), "Find [Aa]: ");
    assert!(rows(&picker, cx).is_empty());

    dispatch("navigation::ToggleRegex", cx);
    assert_eq!(prompt(&workspace, cx), "Find [.*][Aa]: ");
    assert_eq!(rows(&picker, cx), ["a.txt:1", "a.txt:1"]);

    dispatch("navigation::ToggleWholeWord", cx);
    assert_eq!(prompt(&workspace, cx), "Find [.*][Aa][w]: ");
    assert_eq!(rows(&picker, cx), ["a.txt:1"]);

    dispatch("navigation::ScopeOpenBuffers", cx);
    assert_eq!(prompt(&workspace, cx), "Find [.*][Aa][w][buf]: ");
    assert_eq!(rows(&picker, cx), ["a.txt:1"]);

    dispatch("navigation::ToggleRegex", cx);
    assert_eq!(prompt(&workspace, cx), "Find [Aa][w][buf]: ");
    assert!(rows(&picker, cx).is_empty());
    dispatch("menu::Cancel", cx);
}

#[gpui::test]
async fn case_toggle_cycles_smart_sensitive_insensitive(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", "Alpha\nalpha\n")]).await;
    let cx = &mut cx;
    let picker = find(&workspace, false, cx);
    type_text("Alpha", cx);
    assert_eq!(prompt(&workspace, cx), "Find [Aa]: ");
    assert_eq!(rows(&picker, cx), ["a.txt:1"]);

    dispatch("navigation::ToggleCaseSensitive", cx);
    assert_eq!(prompt(&workspace, cx), "Find [A]: ");
    assert_eq!(rows(&picker, cx), ["a.txt:1"]);

    dispatch("navigation::ToggleCaseSensitive", cx);
    assert_eq!(prompt(&workspace, cx), "Find [a]: ");
    let mut found = rows(&picker, cx);
    found.sort();
    assert_eq!(found, ["a.txt:1", "a.txt:2"]);

    dispatch("navigation::ToggleCaseSensitive", cx);
    assert_eq!(prompt(&workspace, cx), "Find [Aa]: ");
    assert_eq!(rows(&picker, cx), ["a.txt:1"]);
    dispatch("menu::Cancel", cx);
}

#[gpui::test]
async fn an_invalid_regex_shows_its_message_inline(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", "alpha (\n")]).await;
    let cx = &mut cx;
    let picker = find(&workspace, false, cx);
    type_text("(", cx);
    assert_eq!(rows(&picker, cx), ["a.txt:1"]);
    assert_eq!(status(&workspace, cx), "");

    dispatch("navigation::ToggleRegex", cx);
    assert!(
        status(&workspace, cx).starts_with("Invalid regexp"),
        "the status reads {:?}",
        status(&workspace, cx)
    );
    assert!(rows(&picker, cx).is_empty());

    dispatch("navigation::ToggleRegex", cx);
    assert_eq!(status(&workspace, cx), "");
    assert_eq!(rows(&picker, cx), ["a.txt:1"]);
    dispatch("menu::Cancel", cx);
}

#[gpui::test]
async fn open_buffers_scope_searches_the_buffers_of_open_items(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(
        cx,
        &[
            ("a.txt", "needle open\n"),
            ("b.txt", "needle only the project holds\n"),
        ],
    )
    .await;
    let cx = &mut cx;
    open_file(&workspace, "a.txt", cx).await;
    let picker = find(&workspace, false, cx);
    type_text("needle", cx);
    let mut found = rows(&picker, cx);
    found.sort();
    assert_eq!(found, ["a.txt:1", "b.txt:1"]);

    dispatch("navigation::ScopeOpenBuffers", cx);
    assert_eq!(rows(&picker, cx), ["a.txt:1"]);
    dispatch("navigation::ScopeOpenBuffers", cx);
    assert_eq!(rows(&picker, cx).len(), 2);
    dispatch("menu::Cancel", cx);
}

#[gpui::test]
async fn directory_scope_asks_a_directory_and_reopens_with_it(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(
        cx,
        &[
            ("src/main/A.kt", "needle\n"),
            ("src/other/B.kt", "needle\n"),
        ],
    )
    .await;
    let cx = &mut cx;
    let picker = find(&workspace, false, cx);
    type_text("needle", cx);
    assert_eq!(rows(&picker, cx).len(), 2);

    dispatch("navigation::ScopeDirectory", cx);
    assert!(picker.handle().is_none(), "the find session is closed");
    assert_eq!(prompt(&workspace, cx), "Directory: ");
    type_text("src/main", cx);
    dispatch("menu::Confirm", cx);

    assert_eq!(prompt(&workspace, cx), "Find [Aa]: ");
    let reopened = current_picker(&workspace, cx);
    let reopened = reopened
        .as_any()
        .downcast_ref::<FindPicker>()
        .expect("the find picker");
    assert_eq!(reopened.seed(), "needle -- src/main");
    assert_eq!(rows(reopened, cx), ["src/main/A.kt:1"]);
    dispatch("menu::Cancel", cx);
}

#[gpui::test]
async fn a_query_and_its_flags_survive_a_new_history_store(cx: &mut TestAppContext) {
    let kvp = KeyValueStore::open_test_db("navigation_find_flags_survive").await;
    cx.update(|cx| HistoryStore::new(kvp.clone()).install(cx));
    let (workspace, mut cx) = setup(cx, &[("a.txt", "alpha alphabet\n")]).await;
    let cx = &mut cx;
    confirm_a_regex_find(&workspace, cx).await;
    cx.run_until_parked();

    let later = HistoryStore::new(kvp.clone());
    assert_eq!(later.ring(HISTORY).entries(), ["alp.a"]);
    assert_eq!(flag_history::read(later.kvp(), "alp.a"), Some(all_flags()));
    assert_eq!(flag_history::read(later.kvp(), "other"), None);
}

#[gpui::test]
async fn history_prev_restores_the_flags_of_the_entry(cx: &mut TestAppContext) {
    let kvp = KeyValueStore::open_test_db("navigation_find_flags_restore").await;
    cx.update(|cx| HistoryStore::new(kvp.clone()).install(cx));
    let (workspace, mut cx) = setup(cx, &[("a.txt", "alpha alphabet\n")]).await;
    let cx = &mut cx;
    confirm_a_regex_find(&workspace, cx).await;
    cx.run_until_parked();
    cx.update(|_, cx| HistoryStore::new(kvp.clone()).install(cx));

    let picker = find(&workspace, false, cx);
    assert_eq!(prompt(&workspace, cx), "Find [Aa]: ");
    cx.dispatch_action(minibuffer::HistoryPrev);
    cx.run_until_parked();
    assert_eq!(picker.input(), "alp.a");
    assert_eq!(picker.flags(), all_flags());
    assert_eq!(prompt(&workspace, cx), "Find [.*][Aa][w][buf]: ");
    // Walking the rows with M-n would leave the history entry, so only the selected row is read.
    let selected = cx
        .read(|cx| picker.handle().expect("the session is open").selected(cx))
        .expect("the search found a row");
    assert!(matches!(
        selected.downcast_ref::<FindRow>(),
        Some(FindRow::Match { line: 1, .. })
    ));
    assert_eq!(picker.input(), "alp.a");
    dispatch("menu::Cancel", cx);
}
