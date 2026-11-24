//! Tests of exporting the find in files results to a project search item: from the key and from
//! the more row, with the toggles and the scope, and the pane it opens in.

use bongos_minibuffer::SessionView;
use gpui::{Entity, KeyBinding, TestAppContext, VisualTestContext};
use search::{ProjectSearchView, SearchOptions};
use workspace::{OpenTarget, Pane, Workspace};

use crate::actions::ExportToMultibuffer;
use crate::find::engine::FindRow;
use crate::find::picker::KEY_CONTEXT;
use crate::find::picker::tests::{dispatch, find, open_file, setup, type_text};
use crate::registry;

fn search_views(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Vec<Entity<ProjectSearchView>> {
    cx.read(|cx| {
        workspace
            .read(cx)
            .items_of_type::<ProjectSearchView>(cx)
            .collect()
    })
}

fn picker_is_closed(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    cx.read(|cx| {
        registry::current(cx, workspace.entity_id()).is_none()
            && workspace.read(cx).active_modal::<SessionView>(cx).is_none()
    })
}

fn lines(count: usize) -> String {
    "needle\n".repeat(count)
}

#[gpui::test]
async fn the_export_key_opens_a_project_search_with_every_match_and_closes_the_picker(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", &lines(300))]).await;
    let cx = &mut cx;
    cx.update(|_, cx| {
        cx.bind_keys([KeyBinding::new(
            "ctrl-c ctrl-o",
            ExportToMultibuffer,
            Some(&format!("Minibuffer > {KEY_CONTEXT} > Editor")),
        )]);
    });
    find(&workspace, false, cx);
    type_text("needle", cx);
    assert!(!picker_is_closed(&workspace, cx));

    cx.simulate_keystrokes("ctrl-c ctrl-o");
    cx.run_until_parked();

    assert!(picker_is_closed(&workspace, cx));
    let views = search_views(&workspace, cx);
    assert_eq!(views.len(), 1);
    cx.read(|cx| {
        assert_eq!(views[0].read(cx).search_query_text(cx), "needle");
        assert_eq!(views[0].read(cx).get_matches(cx).len(), 300);
    });
}

#[gpui::test]
async fn the_exported_search_shows_the_toggles_and_the_scope(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(
        cx,
        &[
            ("src/a.txt", "needle needles\n"),
            ("other/b.txt", "needle\n"),
        ],
    )
    .await;
    let cx = &mut cx;
    open_file(&workspace, "src/a.txt", cx).await;
    find(&workspace, false, cx);
    type_text("nee.le -- src", cx);
    dispatch("navigation::ToggleRegex", cx);
    dispatch("navigation::ToggleCaseSensitive", cx);
    dispatch("navigation::ToggleWholeWord", cx);
    dispatch("navigation::ScopeOpenBuffers", cx);
    dispatch("navigation::ExportToMultibuffer", cx);

    assert!(picker_is_closed(&workspace, cx));
    let views = search_views(&workspace, cx);
    assert_eq!(views.len(), 1);
    cx.read(|cx| {
        let view = views[0].read(cx);
        assert_eq!(view.search_query_text(cx), "nee.le");
        assert_eq!(
            view.search_options(),
            SearchOptions::REGEX | SearchOptions::CASE_SENSITIVE | SearchOptions::WHOLE_WORD
        );
        assert!(view.opened_only());
        assert_eq!(
            view.file_filters(cx),
            ("src/**".to_string(), String::new(), true)
        );
        assert_eq!(view.get_matches(cx).len(), 1);
    });
}

#[gpui::test]
async fn confirming_the_more_row_exports_every_match(cx: &mut TestAppContext) {
    // 41 files of 50 matching lines: 2,050 matches, 2,000 of them shown.
    let files: Vec<(String, String)> = (0..41)
        .map(|ix| (format!("f{ix:02}.txt"), lines(50)))
        .collect();
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_str()))
        .collect();
    let (workspace, mut cx) = setup(cx, &files).await;
    let cx = &mut cx;
    let picker = find(&workspace, false, cx);
    type_text("needle", cx);
    dispatch("menu::SelectLast", cx);
    let selected = cx
        .read(|cx| picker.handle().expect("the session is open").selected(cx))
        .expect("a row is selected");
    assert!(matches!(
        selected.downcast_ref::<FindRow>(),
        Some(FindRow::More { count: 50, .. })
    ));

    dispatch("menu::Confirm", cx);

    assert!(picker_is_closed(&workspace, cx));
    let views = search_views(&workspace, cx);
    assert_eq!(views.len(), 1);
    cx.read(|cx| assert_eq!(views[0].read(cx).get_matches(cx).len(), 2_050));
}

#[gpui::test]
async fn the_export_opens_in_the_pane_an_open_goes_to(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", &lines(3))]).await;
    let cx = &mut cx;
    open_file(&workspace, "a.txt", cx).await;
    let active = cx.read(|cx| workspace.read(cx).active_pane().clone());
    let other = workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(active.clone(), workspace::SplitDirection::Right, window, cx)
    });
    cx.update(|_, cx| active.update(cx, |pane, cx| pane.set_dedicated(true, cx)));
    let current = cx.read(|cx| workspace.read(cx).active_pane().clone());
    let target = cx.read(|cx| workspace.read(cx).open_target(&current, cx));
    let OpenTarget::Pane(expected) = target else {
        panic!("a pane that is not dedicated takes the open");
    };
    assert_ne!(expected, active, "the dedicated pane is skipped");
    assert_eq!(expected, other);

    find(&workspace, false, cx);
    type_text("needle", cx);
    dispatch("navigation::ExportToMultibuffer", cx);

    assert!(picker_is_closed(&workspace, cx));
    let holds_search = |pane: &Entity<Pane>, cx: &mut VisualTestContext| {
        cx.read(|cx| {
            pane.read(cx)
                .items()
                .any(|item| item.downcast::<ProjectSearchView>().is_some())
        })
    };
    assert!(holds_search(&expected, cx));
    assert!(!holds_search(&active, cx));
}
