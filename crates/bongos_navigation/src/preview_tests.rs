//! Tests of the preview session: the one pane slot it owns, the transient editor over a file, and
//! that ending the session leaves the workspace as it found it.

use std::{cell::RefCell, rc::Rc};

use editor::Editor;
use futures::channel::oneshot;
use gpui::{
    Action as _, App, AppContext as _, Context, DismissEvent, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, Render, Styled as _, Task,
    TestAppContext, VisualTestContext, Window, WindowHandle, div,
};
use language::Point;
use project::{Fs as _, Project, ProjectPath};
use settings::Settings as _;
use workspace::{
    AppState, ItemHandle, ModalView, MultiWorkspace, Pane, SplitDirection, Workspace,
    WorkspaceHandle as _,
    dock::{DockPosition, test::TestPanel},
    item::test::TestItem,
    pane,
};

use crate::NavigationSettings;
use crate::preview::{NO_PREVIEW, Preview, PreviewMessage, PreviewSession, PreviewTarget};
use crate::test_support::{WorldSnapshot, world_snapshot};

/// A workspace on `/root` holding `files`, with nothing open yet.
async fn setup(
    cx: &mut TestAppContext,
    files: &[(&str, Vec<u8>)],
) -> (Entity<Workspace>, VisualTestContext) {
    let params = cx.update(|cx| {
        let params = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        NavigationSettings::register(cx);
        params
    });
    let fake = params.fs.as_fake();
    fake.create_dir("/root".as_ref())
        .await
        .expect("the root directory is created");
    for (name, content) in files {
        fake.insert_file(format!("/root/{name}"), content.clone())
            .await;
    }
    let project = Project::test(params.fs.clone(), ["/root".as_ref()], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .expect("the window holds a workspace");
    let cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    (workspace, cx)
}

fn lines(name: &str, count: usize) -> Vec<u8> {
    (0..count)
        .map(|ix| format!("{name} line {ix}\n"))
        .collect::<String>()
        .into_bytes()
}

fn project_path(
    workspace: &Entity<Workspace>,
    name: &str,
    cx: &mut VisualTestContext,
) -> ProjectPath {
    cx.read(|cx| workspace.file_project_paths(cx))
        .into_iter()
        .find(|path| path.path.file_name() == Some(name))
        .expect("the file is in the worktree")
}

async fn open(
    workspace: &Entity<Workspace>,
    name: &str,
    cx: &mut VisualTestContext,
) -> Box<dyn ItemHandle> {
    let path = project_path(workspace, name, cx);
    let task = workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_path(path, None, true, window, cx)
    });
    let item = task.await.expect("the file opens");
    cx.run_until_parked();
    item
}

fn new_session(
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> Entity<PreviewSession> {
    cx.update(|_, cx| cx.new(|cx| PreviewSession::new(workspace, cx)))
}

fn send(session: &Entity<PreviewSession>, preview: Preview, cx: &mut VisualTestContext) {
    session.update_in(cx, |session, window, cx| {
        session.update(preview, window, cx)
    });
}

/// A modal standing in for the picker, which holds the focus while it previews.
struct PickerStandIn {
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for PickerStandIn {}

impl Focusable for PickerStandIn {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for PickerStandIn {}

impl Render for PickerStandIn {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().id("picker").track_focus(&self.focus_handle).size_10()
    }
}

/// Opens the stand-in picker, so no pane is focused while the session previews.
fn picker_takes_focus(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |_, cx| PickerStandIn {
            focus_handle: cx.focus_handle(),
        })
    });
    cx.run_until_parked();
}

fn snapshot(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> WorldSnapshot {
    cx.update(|_, cx| world_snapshot(workspace, cx))
}

fn active_pane(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<Pane> {
    cx.read(|cx| workspace.read(cx).active_pane().clone())
}

fn slot(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<gpui::AnyView> {
    cx.read(|cx| pane.read(cx).preview().cloned())
}

fn slot_id(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<EntityId> {
    slot(pane, cx).map(|view| view.entity_id())
}

fn slot_message(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<String> {
    let message = slot(pane, cx)?.downcast::<PreviewMessage>().ok()?;
    Some(cx.read(|cx| message.read(cx).text().to_string()))
}

fn slot_editor(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> Option<Entity<Editor>> {
    slot(pane, cx)?.downcast::<Editor>().ok()
}

fn path_target(path: ProjectPath, row: Option<u32>, pane: &Entity<Pane>) -> Preview {
    Preview::Highlight(PreviewTarget::Path {
        path,
        row,
        ranges: Vec::new(),
        pane: pane.clone(),
    })
}

#[gpui::test]
async fn an_inactive_item_previews_in_its_pane_and_confirm_or_cancel_clear_the_slot_first(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(
        cx,
        &[("one.txt", lines("one", 40)), ("two.txt", lines("two", 40))],
    )
    .await;
    let cx = &mut cx;
    let one = open(&workspace, "one.txt", cx).await;
    open(&workspace, "two.txt", cx).await;
    let pane = active_pane(&workspace, cx);
    picker_takes_focus(&workspace, cx);
    let before = snapshot(&workspace, cx);
    let session = new_session(&workspace, cx);

    let highlight_one = |cx: &mut VisualTestContext| {
        send(
            &session,
            Preview::Highlight(PreviewTarget::Item {
                pane: pane.clone(),
                item: one.boxed_clone(),
            }),
            cx,
        );
        cx.run_until_parked();
    };

    highlight_one(cx);
    assert_eq!(slot_id(&pane, cx), Some(one.item_id()));
    assert_eq!(snapshot(&workspace, cx).without_previews(), before);
    send(&session, Preview::Confirmed, cx);
    assert_eq!(slot_id(&pane, cx), None, "confirm clears the slot first");

    highlight_one(cx);
    assert_eq!(slot_id(&pane, cx), Some(one.item_id()));
    send(&session, Preview::Cancelled, cx);
    assert_eq!(slot_id(&pane, cx), None, "cancel clears the slot first");
    cx.run_until_parked();
    assert_eq!(snapshot(&workspace, cx), before);

    // The active item needs no slot.
    let two = cx.read(|cx| pane.read(cx).active_item().expect("two is active"));
    send(
        &session,
        Preview::Highlight(PreviewTarget::Item {
            pane: pane.clone(),
            item: two,
        }),
        cx,
    );
    assert_eq!(slot_id(&pane, cx), None);
}

#[gpui::test]
async fn an_item_in_a_closed_dock_previews_in_the_active_center_pane_and_the_dock_stays_closed(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx, &[("one.txt", lines("one", 40))]).await;
    let cx = &mut cx;
    open(&workspace, "one.txt", cx).await;
    let center = active_pane(&workspace, cx);
    let docked = cx.update(|_, cx| cx.new(TestItem::new));
    let dock_pane = workspace.update_in(cx, |workspace, window, cx| {
        let dock_pane = cx.new(|cx| {
            Pane::new(
                workspace.weak_handle(),
                workspace.project().clone(),
                Default::default(),
                None,
                gpui::NoAction.boxed_clone(),
                false,
                window,
                cx,
            )
        });
        dock_pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(docked.clone()), false, false, None, window, cx);
        });
        let panel = cx.new(|cx| {
            let mut panel = TestPanel::new(DockPosition::Bottom, 100, cx);
            panel.pane = Some(dock_pane.clone());
            panel
        });
        workspace.add_panel(panel, window, cx);
        dock_pane
    });
    cx.run_until_parked();
    let dock_is_open = |cx: &mut VisualTestContext| {
        cx.read(|cx| workspace.read(cx).bottom_dock().read(cx).is_open())
    };
    assert!(!dock_is_open(cx));
    picker_takes_focus(&workspace, cx);
    let before = snapshot(&workspace, cx);
    let session = new_session(&workspace, cx);

    send(
        &session,
        Preview::Highlight(PreviewTarget::Item {
            pane: dock_pane.clone(),
            item: Box::new(docked.clone()),
        }),
        cx,
    );
    cx.run_until_parked();
    assert_eq!(slot_id(&center, cx), Some(docked.entity_id()));
    assert_eq!(slot_id(&dock_pane, cx), None);
    assert!(!dock_is_open(cx), "no dock opens");

    send(&session, Preview::Cancelled, cx);
    cx.run_until_parked();
    assert_eq!(snapshot(&workspace, cx), before);
    assert!(!dock_is_open(cx));
}

#[gpui::test]
async fn a_path_previews_in_a_transient_editor_that_is_not_an_item_nor_in_recent_history(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(
        cx,
        &[("one.txt", lines("one", 40)), ("two.txt", lines("two", 40))],
    )
    .await;
    let cx = &mut cx;
    open(&workspace, "one.txt", cx).await;
    let pane = active_pane(&workspace, cx);
    picker_takes_focus(&workspace, cx);
    let before = snapshot(&workspace, cx);
    let session = new_session(&workspace, cx);

    send(
        &session,
        path_target(project_path(&workspace, "two.txt", cx), Some(3), &pane),
        cx,
    );
    cx.run_until_parked();

    let editor = slot_editor(&pane, cx).expect("the slot shows an editor");
    assert_eq!(
        cx.read(|cx| session.read(cx).path_editor()),
        Some(editor.clone())
    );
    cx.read(|cx| {
        let editor = editor.read(cx);
        assert!(editor.read_only(cx));
        assert!(editor.text(cx).starts_with("two line 0\n"));
    });
    let item_ids = cx.read(|cx| {
        workspace
            .read(cx)
            .item_panes(cx)
            .iter()
            .flat_map(|pane| {
                pane.read(cx)
                    .items()
                    .map(|item| item.item_id())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    });
    assert!(
        !item_ids.contains(&editor.entity_id()),
        "not a workspace item"
    );
    let during = snapshot(&workspace, cx);
    assert_eq!(
        during.recent_navigation_history,
        before.recent_navigation_history
    );
    assert_eq!(during.without_previews(), before);

    send(&session, Preview::Cancelled, cx);
    cx.run_until_parked();
    assert_eq!(snapshot(&workspace, cx), before);
}

#[gpui::test]
async fn a_row_is_centered_with_ranges_painted_and_another_row_reuses_the_editor(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = setup(cx, &[("long.txt", lines("long", 200))]).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    let path = project_path(&workspace, "long.txt", cx);
    let session = new_session(&workspace, cx);

    let centered_row = |editor: &Entity<Editor>, cx: &mut VisualTestContext| {
        editor.update_in(cx, |editor, _, cx| {
            let visible = editor
                .visible_line_count()
                .expect("the editor was laid out");
            editor.scroll_position(cx).y + visible / 2.
        })
    };

    send(
        &session,
        Preview::Highlight(PreviewTarget::Path {
            path: path.clone(),
            row: Some(100),
            ranges: vec![Point::new(100, 5)..Point::new(100, 9)],
            pane: pane.clone(),
        }),
        cx,
    );
    cx.run_until_parked();
    let editor = slot_editor(&pane, cx).expect("the slot shows an editor");
    let center = centered_row(&editor, cx);
    assert!(
        (center - 100.).abs() <= 1.5,
        "row 100 is centered, center is {center}"
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
    assert_eq!(highlights, [((100, 5), (100, 9))]);

    send(&session, path_target(path, Some(150), &pane), cx);
    cx.run_until_parked();
    let reused = slot_editor(&pane, cx).expect("the slot shows an editor");
    assert_eq!(
        reused.entity_id(),
        editor.entity_id(),
        "the editor is reused"
    );
    let center = centered_row(&reused, cx);
    assert!(
        (center - 150.).abs() <= 1.5,
        "row 150 is centered, center is {center}"
    );
}

#[gpui::test]
async fn a_file_above_the_size_limit_shows_the_skipped_line(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("big.txt", vec![b'a'; 3_400_000])]).await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    let session = new_session(&workspace, cx);

    send(
        &session,
        path_target(project_path(&workspace, "big.txt", cx), None, &pane),
        cx,
    );
    cx.run_until_parked();
    assert_eq!(
        slot_message(&pane, cx).as_deref(),
        Some("Preview skipped: 3.4 MB")
    );
    assert_eq!(cx.read(|cx| session.read(cx).path_editor()), None);
}

#[gpui::test]
async fn the_previewed_pane_closing_drops_the_slot_without_panic(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("one.txt", lines("one", 40))]).await;
    let cx = &mut cx;
    let left = active_pane(&workspace, cx);
    let right = workspace.update_in(cx, |workspace, window, cx| {
        workspace.split_pane(left.clone(), SplitDirection::Right, window, cx)
    });
    cx.run_until_parked();
    let path = project_path(&workspace, "one.txt", cx);
    let session = new_session(&workspace, cx);

    send(&session, path_target(path.clone(), Some(5), &right), cx);
    cx.run_until_parked();
    assert!(slot_editor(&right, cx).is_some());
    assert_eq!(
        cx.read(|cx| session.read(cx).slot_pane()),
        Some(right.clone())
    );

    right.update(cx, |_, cx| {
        cx.emit(pane::Event::Remove {
            focus_on_pane: None,
        })
    });
    cx.run_until_parked();
    assert_eq!(
        cx.read(|cx| workspace.read(cx).center_panes()),
        vec![left.clone()]
    );
    assert_eq!(cx.read(|cx| session.read(cx).slot_pane()), None);
    assert_eq!(slot_id(&right, cx), None);
    drop(right);
    cx.run_until_parked();

    send(&session, Preview::Cancelled, cx);
    send(&session, path_target(path, Some(5), &left), cx);
    cx.run_until_parked();
    assert!(slot_editor(&left, cx).is_some(), "the session goes on");
}

#[gpui::test]
async fn a_load_for_an_older_highlight_that_finishes_last_is_dropped(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(
        cx,
        &[("one.txt", lines("one", 40)), ("two.txt", lines("two", 40))],
    )
    .await;
    let cx = &mut cx;
    let pane = active_pane(&workspace, cx);
    let one = project_path(&workspace, "one.txt", cx);
    let two = project_path(&workspace, "two.txt", cx);
    let session = new_session(&workspace, cx);

    let (release_one, gate) = oneshot::channel::<()>();
    let gate = Rc::new(RefCell::new(Some(gate)));
    let gated_path = one.clone();
    session.update(cx, |session, _| {
        session.set_load_buffer(Rc::new(move |project, path, cx| {
            let open = project.update(cx, |project, cx| project.open_buffer(path.clone(), cx));
            let gate = (path == gated_path)
                .then(|| gate.borrow_mut().take())
                .flatten();
            cx.spawn(async move |_| {
                if let Some(gate) = gate {
                    gate.await.ok();
                }
                open.await
            })
        }));
    });

    send(&session, path_target(one, Some(1), &pane), cx);
    cx.run_until_parked();
    assert_eq!(slot_id(&pane, cx), None, "the load of one is held");

    send(&session, path_target(two, Some(1), &pane), cx);
    cx.run_until_parked();
    let editor = slot_editor(&pane, cx).expect("two is shown");
    assert!(cx.read(|cx| editor.read(cx).text(cx).starts_with("two")));

    release_one.send(()).ok();
    cx.run_until_parked();
    let shown = slot_editor(&pane, cx).expect("an editor is shown");
    assert_eq!(
        shown.entity_id(),
        editor.entity_id(),
        "the late load of one is dropped"
    );
    assert!(cx.read(|cx| shown.read(cx).text(cx).starts_with("two")));
}

#[gpui::test]
async fn a_failing_open_shows_no_preview_and_changes_nothing_else(cx: &mut TestAppContext) {
    let (workspace, mut cx) = setup(cx, &[("one.txt", lines("one", 40))]).await;
    let cx = &mut cx;
    open(&workspace, "one.txt", cx).await;
    let pane = active_pane(&workspace, cx);
    picker_takes_focus(&workspace, cx);
    let before = snapshot(&workspace, cx);
    let session = new_session(&workspace, cx);
    session.update(cx, |session, _| {
        session.set_load_buffer(Rc::new(|_, _, _| {
            Task::ready(Err(anyhow::anyhow!("the file cannot be read")))
        }));
    });

    send(
        &session,
        path_target(project_path(&workspace, "one.txt", cx), Some(2), &pane),
        cx,
    );
    cx.run_until_parked();
    assert_eq!(slot_message(&pane, cx).as_deref(), Some(NO_PREVIEW));
    assert_eq!(snapshot(&workspace, cx).without_previews(), before);

    send(&session, Preview::Cancelled, cx);
    cx.run_until_parked();
    assert_eq!(snapshot(&workspace, cx), before);
}
