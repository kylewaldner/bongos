//! Tests of the find in files picker: its seed and scope, its rows and status while typing, the
//! preview, and what a confirm opens and pushes.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use bongos_emacs::mark;
use bongos_minibuffer::SessionView;
use editor::{Editor, ToPoint as _};
use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle,
};
use language::Point;
use project::{Fs as _, Project, ProjectPath};
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace, Pane, Workspace, WorkspaceHandle as _};
use zed_actions::minibuffer;

use super::{FindPicker, HOST, open};
use crate::find::engine::FindRow;
use crate::registry;
use crate::test_support::{WorldSnapshot, world_snapshot};

/// A workspace on `/root` holding `files`, with the find picker routed to the minibuffer.
async fn setup(
    cx: &mut TestAppContext,
    files: &[(&str, &str)],
) -> (Entity<Workspace>, VisualTestContext) {
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
    fake.create_dir("/root".as_ref())
        .await
        .expect("the root directory is created");
    for (name, content) in files {
        if let Some((dir, _)) = name.rsplit_once('/') {
            fake.create_dir(format!("/root/{dir}").as_ref())
                .await
                .expect("the directory is created");
        }
        fake.insert_file(format!("/root/{name}"), content.as_bytes().to_vec())
            .await;
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
    (workspace, cx)
}

fn project_path(
    workspace: &Entity<Workspace>,
    name: &str,
    cx: &mut VisualTestContext,
) -> ProjectPath {
    cx.read(|cx| workspace.file_project_paths(cx))
        .into_iter()
        .find(|path| path.path.as_unix_str().ends_with(name))
        .expect("the file is in the worktree")
}

/// Opens `name` in the active pane and returns its editor.
async fn open_file(
    workspace: &Entity<Workspace>,
    name: &str,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let path = project_path(workspace, name, cx);
    let task = workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_path(path, None, true, window, cx)
    });
    let item = task.await.expect("the file opens");
    cx.run_until_parked();
    item.downcast::<Editor>().expect("an editor")
}

fn select(editor: &Entity<Editor>, range: std::ops::Range<Point>, cx: &mut VisualTestContext) {
    editor.update_in(cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([range])
        });
    });
    cx.run_until_parked();
}

fn find(workspace: &Entity<Workspace>, scoped: bool, cx: &mut VisualTestContext) -> Rc<FindPicker> {
    let picker = workspace.update_in(cx, |workspace, window, cx| {
        open(workspace, scoped, window, cx)
    });
    cx.run_until_parked();
    picker
}

fn dispatch(name: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let action = cx.build_action(name, None).expect("the action exists");
        window.dispatch_action(action, cx);
    });
    cx.run_until_parked();
}

fn type_text(text: &str, cx: &mut VisualTestContext) {
    cx.simulate_input(text);
    cx.run_until_parked();
}

fn active_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
    cx.read(|cx| workspace.read(cx).active_pane().clone())
}

fn active_editor(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Editor> {
    cx.read(|cx| workspace.read(cx).active_item_as::<Editor>(cx))
        .expect("an editor is active")
}

fn selections(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Vec<std::ops::Range<Point>> {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        editor.selections.ranges::<Point>(&snapshot)
    })
}

fn file_name(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> String {
    cx.read(|cx| {
        let buffer = editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("one buffer");
        let file = buffer.read(cx).file().expect("a file");
        file.path().as_unix_str().to_string()
    })
}

/// The rows the picker shows, as `path:line`, walked with the selection.
fn rows(picker: &FindPicker, cx: &mut VisualTestContext) -> Vec<String> {
    let handle = picker.handle().expect("the session is open");
    let mut seen: Vec<bongos_minibuffer::Value> = Vec::new();
    let mut rows = Vec::new();
    while let Some(value) = cx.read(|cx| handle.selected(cx)) {
        if seen.iter().any(|old| Arc::ptr_eq(old, &value)) {
            break;
        }
        if let Some(FindRow::Match { buffer, line, .. }) = value.downcast_ref::<FindRow>() {
            let path = cx.read(|cx| {
                buffer
                    .read(cx)
                    .file()
                    .map(|file| file.path().as_unix_str().to_string())
                    .unwrap_or_default()
            });
            rows.push(format!("{path}:{line}"));
        }
        seen.push(value);
        assert!(seen.len() < 100, "the selection wraps");
        dispatch("menu::SelectNext", cx);
    }
    rows
}

/// The world as a picker must leave it. Focus going to the picker and back stamps the pane's
/// active item again, so activations are compared by order, not by stamp.
fn world(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> WorldSnapshot {
    let mut world = cx.update(|_, cx| world_snapshot(workspace, cx));
    for pane in &mut world.panes {
        for (_, stamp) in &mut pane.activation_history {
            *stamp = 0;
        }
    }
    world
}

fn current_picker_seed(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    cx.read(|cx| {
        let current = registry::current(cx, workspace.entity_id()).expect("a picker is open");
        current
            .as_any()
            .downcast_ref::<FindPicker>()
            .expect("the find picker")
            .seed()
            .to_string()
    })
}

#[gpui::test]
async fn find_seeds_from_the_region_else_the_symbol_else_empty(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", "let alpha = beta;\n\nend\n")]).await;
    let cx = &mut cx;
    let editor = open_file(&workspace, "a.txt", cx).await;

    let cases = [
        (Point::new(0, 12)..Point::new(0, 16), "beta"),
        (Point::new(0, 5)..Point::new(0, 5), "alpha"),
        (Point::new(1, 0)..Point::new(1, 0), ""),
    ];
    for (selection, seed) in cases {
        select(&editor, selection, cx);
        cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
        cx.dispatch_action(crate::actions::FindInFiles);
        cx.run_until_parked();
        assert_eq!(current_picker_seed(&workspace, cx), seed);
        dispatch("menu::Cancel", cx);
        assert!(cx.read(|cx| registry::current(cx, workspace.entity_id()).is_none()));
    }
}

#[gpui::test]
async fn find_with_a_prefix_argument_stays_in_the_files_directory(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(
        cx,
        &[
            ("src/main/A.kt", "needle\n"),
            ("src/other/B.kt", "needle\n"),
            ("top.kt", "needle\n"),
        ],
    )
    .await;
    let cx = &mut cx;
    open_file(&workspace, "src/main/A.kt", cx).await;

    let picker = find(&workspace, true, cx);
    assert_eq!(picker.base_dir(), Some("src/main"));
    assert_eq!(picker.seed(), "needle");
    assert_eq!(rows(&picker, cx), ["src/main/A.kt:1"]);
    dispatch("menu::Cancel", cx);

    let picker = find(&workspace, false, cx);
    assert_eq!(picker.base_dir(), None);
    assert_eq!(rows(&picker, cx).len(), 3);
    dispatch("menu::Cancel", cx);
}

#[gpui::test]
async fn find_previews_the_match_in_the_resolved_pane_and_cancel_leaves_the_world(
    cx: &mut TestAppContext,
) {
    let long: String = (0..40)
        .map(|ix| {
            if ix == 29 {
                "the needle here\n".to_string()
            } else {
                format!("line {ix}\n")
            }
        })
        .collect();
    let (workspace, mut cx) = setup(cx, &[("a.txt", &long), ("b.txt", "\n")]).await;
    let cx = &mut cx;
    open_file(&workspace, "b.txt", cx).await;
    let pane = active_pane(&workspace, cx);
    let before = world(&workspace, cx);

    let picker = find(&workspace, false, cx);
    type_text("needle", cx);
    let slot = cx
        .read(|cx| pane.read(cx).preview().cloned())
        .expect("the resolved pane previews");
    let editor = slot.downcast::<Editor>().expect("a transient editor");
    assert_eq!(file_name(&editor, cx), "a.txt");
    assert_eq!(
        selections(&editor, cx),
        [Point::new(29, 0)..Point::new(29, 0)]
    );
    let highlights = editor.update_in(cx, |editor, window, cx| {
        editor
            .all_text_background_highlights(window, cx)
            .into_iter()
            .map(|(range, _)| {
                (
                    (range.start.row().0, range.start.column()),
                    (range.end.row().0, range.end.column()),
                )
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(highlights, [((29, 4), (29, 10))]);
    let scroll_top = editor.update(cx, |editor, cx| editor.scroll_position(cx).y);
    assert!(scroll_top > 0., "the match is scrolled to");

    dispatch("menu::Cancel", cx);
    assert!(picker.handle().is_none());
    assert_eq!(world(&workspace, cx), before);
}

#[gpui::test]
async fn find_confirm_opens_the_match_with_it_selected(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", "one\ntwo needle\n"), ("b.txt", "\n")]).await;
    let cx = &mut cx;
    open_file(&workspace, "b.txt", cx).await;

    find(&workspace, false, cx);
    type_text("needle", cx);
    dispatch("menu::Confirm", cx);

    let editor = active_editor(&workspace, cx);
    assert_eq!(file_name(&editor, cx), "a.txt");
    assert_eq!(
        selections(&editor, cx),
        [Point::new(1, 4)..Point::new(1, 10)]
    );
    assert!(cx.read(|cx| registry::current(cx, workspace.entity_id()).is_none()));
}

#[gpui::test]
async fn find_confirm_pushes_the_origin_mark_and_one_goto_entry(cx: &mut TestAppContext) {
    let origin_text: String = (0..10).map(|ix| format!("origin {ix}\n")).collect();
    let (workspace, mut cx) = setup(cx, &[("a.txt", "a needle\n"), ("b.txt", &origin_text)]).await;
    let cx = &mut cx;
    let origin = open_file(&workspace, "b.txt", cx).await;
    select(&origin, Point::new(5, 2)..Point::new(5, 2), cx);
    let pane = active_pane(&workspace, cx);
    let depth =
        |cx: &mut VisualTestContext| cx.read(|cx| pane.read(cx).nav_history().stack_depths().0);
    let before = depth(cx);

    find(&workspace, false, cx);
    type_text("needle", cx);
    dispatch("menu::Confirm", cx);
    assert_eq!(file_name(&active_editor(&workspace, cx), cx), "a.txt");

    let mark = cx.update(|_, cx| {
        let mark = mark::mark_for(&origin, cx)?;
        let snapshot = origin.read(cx).buffer().read(cx).snapshot(cx);
        Some(mark.to_point(&snapshot))
    });
    assert_eq!(mark, Some(Point::new(5, 2)));
    assert_eq!(depth(cx), before + 1);
    let newest = cx.read(|cx| {
        pane.read(cx)
            .nav_history()
            .entries_newest_first(cx)
            .into_iter()
            .next()
            .expect("an entry")
    });
    assert_eq!(newest.row, Some(5));
    assert_eq!(newest.item.id(), origin.entity_id());
}

#[gpui::test]
async fn find_status_reads_searching_then_clears(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", "a needle\n")]).await;
    let cx = &mut cx;
    find(&workspace, false, cx);
    let view = cx
        .read(|cx| workspace.read(cx).active_modal::<SessionView>(cx))
        .expect("the session is open");
    let statuses = Rc::new(RefCell::new(Vec::<String>::new()));
    let _observe = cx.update(|_, cx| {
        let statuses = statuses.clone();
        cx.observe(&view, move |view, cx| {
            let status = view.read(cx).status().to_string();
            let mut statuses = statuses.borrow_mut();
            if statuses.last() != Some(&status) {
                statuses.push(status);
            }
        })
    });

    type_text("needle", cx);
    let seen = statuses.borrow().clone();
    let searching = seen
        .iter()
        .position(|status| status == "searching")
        .expect("the status read searching");
    assert_eq!(&seen[searching..], ["searching", ""]);
    assert_eq!(cx.read(|cx| view.read(cx).status()), "");
    dispatch("menu::Cancel", cx);
}

#[gpui::test]
async fn find_typing_replaces_the_rows_with_the_new_querys(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(
        cx,
        &[("a.txt", "alpha\nalphabet\n"), ("b.txt", "alpha one\n")],
    )
    .await;
    let cx = &mut cx;
    let picker = find(&workspace, false, cx);
    type_text("alpha", cx);
    let mut found = rows(&picker, cx);
    found.sort();
    assert_eq!(found, ["a.txt:1", "a.txt:2", "b.txt:1"]);

    type_text("bet", cx);
    assert_eq!(rows(&picker, cx), ["a.txt:2"]);
    dispatch("menu::Cancel", cx);
}

#[gpui::test]
async fn find_split_right_opens_the_match_in_a_new_right_pane(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("a.txt", "one\ntwo needle\n"), ("b.txt", "\n")]).await;
    let cx = &mut cx;
    open_file(&workspace, "b.txt", cx).await;
    let first = active_pane(&workspace, cx);

    find(&workspace, false, cx);
    type_text("needle", cx);
    cx.dispatch_action(minibuffer::Actions);
    cx.run_until_parked();
    cx.simulate_keystrokes("r");
    cx.run_until_parked();

    let panes = cx.read(|cx| workspace.read(cx).center_panes());
    assert_eq!(panes.len(), 2);
    assert_eq!(panes[0], first);
    let right = panes[1].clone();
    let editor = cx
        .read(|cx| right.read(cx).active_item())
        .and_then(|item| item.downcast::<Editor>())
        .expect("the new pane shows an editor");
    assert_eq!(file_name(&editor, cx), "a.txt");
    assert_eq!(
        selections(&editor, cx),
        [Point::new(1, 4)..Point::new(1, 10)]
    );
}
