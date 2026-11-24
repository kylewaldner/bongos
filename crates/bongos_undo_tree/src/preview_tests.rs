use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use editor::Editor;
use fs::Fs as _;
use gpui::{AppContext as _, BorrowAppContext as _, Point, TestAppContext};
use language::Capability;
use settings::SettingsStore;
use text::UndoNodeId;
use workspace::MinibufferRouting;

use crate::{
    adapter::shape_of,
    preview::{BUFFER_CHANGED_MESSAGE, CloseReason, PreviewSession},
    test_support::{Fixture, fixture},
};

/// Types `one `, `two `, undoes `two ` and types `three `, so `two ` is an undone sibling
/// branch. Returns the nodes of `one `, `two ` and `three `.
fn branch_history(fixture: &mut Fixture) -> [UndoNodeId; 3] {
    let start_len = fixture.text().len();
    let one = fixture.type_step("one ");
    let two = fixture.type_step("two ");
    fixture.undo();
    let three = fixture.type_step("three ");
    assert_eq!(fixture.text().len(), start_len + "one three ".len());
    [one, two, three]
}

fn capability(fixture: &mut Fixture) -> Capability {
    fixture
        .buffer
        .read_with(&fixture.cx.cx.cx, |buffer, _| buffer.capability())
}

fn has_override(fixture: &mut Fixture) -> bool {
    fixture.pane.read_with(&fixture.cx.cx.cx, |pane, _| {
        pane.content_override().is_some()
    })
}

#[gpui::test]
async fn a_move_shows_the_node_text_at_once(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    let [one, two, three] = branch_history(&mut fixture);
    let session = fixture.open();
    assert_eq!(fixture.preview_text(&session), "one three ");
    let has_project = fixture.cx.update(|_, cx| {
        let preview_editor = session.read(cx).preview_editor().clone();
        preview_editor.read(cx).project().is_some()
    });
    assert!(!has_project, "the preview editor has no project");
    assert!(has_override(&mut fixture));

    fixture.move_to(&session, two);
    assert_eq!(fixture.preview_text(&session), "one two ");
    fixture.move_to(&session, one);
    assert_eq!(fixture.preview_text(&session), "one ");
    fixture.move_to(&session, UndoNodeId::ROOT);
    assert_eq!(fixture.preview_text(&session), "");
    fixture.move_to(&session, three);
    assert_eq!(fixture.preview_text(&session), "one three ");
    assert_eq!(fixture.text(), "one three ");
}

#[gpui::test]
async fn twenty_moves_send_no_did_change(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    let changes = Arc::new(AtomicUsize::new(0));
    let _handler = fixture
        .cx
        .lsp
        .handle_notification::<lsp::notification::DidChangeTextDocument, _>({
            let changes = changes.clone();
            move |_, _| {
                changes.fetch_add(1, Ordering::SeqCst);
            }
        });
    let [one, two, three] = branch_history(&mut fixture);
    fixture.cx.run_until_parked();
    assert!(
        changes.load(Ordering::SeqCst) > 0,
        "real edits reach the language server"
    );
    let before = changes.load(Ordering::SeqCst);

    let session = fixture.open();
    let targets = [one, two, UndoNodeId::ROOT, three];
    for target in targets.iter().cycle().take(20) {
        fixture.move_to(&session, *target);
        fixture.cx.run_until_parked();
    }
    fixture.cx.executor().advance_clock(Duration::from_secs(5));
    fixture.cx.run_until_parked();
    assert_eq!(changes.load(Ordering::SeqCst), before);
}

#[gpui::test]
async fn autosave_after_delay_writes_nothing(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    let [one, two, three] = branch_history(&mut fixture);
    fixture.cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.workspace.autosave = Some(settings::AutosaveSetting::AfterDelay {
                    milliseconds: 100.try_into().unwrap(),
                });
            });
        });
    });
    fixture.cx.run_until_parked();
    let fs = fixture
        .cx
        .update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().as_fake());
    let writes_before = fs.write_count_for_path(&fixture.path);
    let disk_before = fs.load(&fixture.path).await.unwrap();

    let session = fixture.open();
    for target in [one, two, UndoNodeId::ROOT, three].iter().cycle().take(20) {
        fixture.move_to(&session, *target);
    }
    fixture.cx.executor().advance_clock(Duration::from_secs(10));
    fixture.cx.run_until_parked();
    assert_eq!(fs.write_count_for_path(&fixture.path), writes_before);
    assert_eq!(fs.load(&fixture.path).await.unwrap(), disk_before);

    fixture.close(&session);
    fixture.type_step("four ");
    fixture.cx.executor().advance_clock(Duration::from_secs(10));
    fixture.cx.run_until_parked();
    assert!(
        fs.write_count_for_path(&fixture.path) > writes_before,
        "autosave writes a real edit once the preview is closed"
    );
}

#[gpui::test]
async fn the_real_buffer_tree_gains_no_node(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    let [one, two, three] = branch_history(&mut fixture);
    let root_time = fixture.buffer.read_with(&fixture.cx.cx.cx, |buffer, _| {
        let node = buffer.undo_tree().node(one).unwrap();
        node.entry().unwrap().last_edit_at()
    });
    let shape_and_version = |fixture: &mut Fixture| {
        fixture.buffer.read_with(&fixture.cx.cx.cx, |buffer, _| {
            (
                shape_of(buffer.undo_tree(), three, root_time).shape,
                buffer.version(),
            )
        })
    };
    let before = shape_and_version(&mut fixture);
    assert_eq!(before.0.nodes.len(), 4);

    let session = fixture.open();
    for target in [two, one, UndoNodeId::ROOT, two, three] {
        fixture.move_to(&session, target);
    }
    fixture.cx.run_until_parked();
    assert_eq!(shape_and_version(&mut fixture), before);
    fixture.close(&session);
    assert_eq!(shape_and_version(&mut fixture), before);
}

#[gpui::test]
async fn other_editors_are_read_only_while_open(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    let [one, _, _] = branch_history(&mut fixture);
    let buffer = fixture.buffer.clone();
    let project = fixture
        .cx
        .update_workspace(|workspace, _, _| workspace.project().clone());
    let other = fixture
        .cx
        .update(|window, cx| cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx)));
    let read_only = |fixture: &mut Fixture| {
        let other = other.clone();
        let editor = fixture.editor.clone();
        fixture
            .cx
            .update(|_, cx| (editor.read(cx).read_only(cx), other.read(cx).read_only(cx)))
    };
    assert_eq!(read_only(&mut fixture), (false, false));

    let session = fixture.open();
    fixture.move_to(&session, one);
    assert_eq!(read_only(&mut fixture), (true, true));
    other.update_in(&mut fixture.cx.cx.cx, |other, window, cx| {
        other.handle_input("typed ", window, cx);
        other.undo(&editor::actions::Undo, window, cx);
    });
    fixture.cx.run_until_parked();
    assert_eq!(fixture.text(), "one three ");
    assert_eq!(
        session.read_with(&fixture.cx.cx.cx, |session, _| session.closed_reason()),
        None
    );

    fixture.close(&session);
    assert_eq!(read_only(&mut fixture), (false, false));
}

#[gpui::test]
async fn close_restores_text_selections_scroll_and_capability(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    let lines = (0..200)
        .map(|row| format!("line {row}\n"))
        .collect::<String>();
    fixture.cx.update_editor(|editor, window, cx| {
        editor.set_text(lines.clone(), window, cx);
    });
    let [one, two, three] = branch_history(&mut fixture);
    fixture.cx.update_editor(|editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([editor::MultiBufferOffset(30)..editor::MultiBufferOffset(42)])
        });
        editor.set_scroll_position(Point::new(0., 120.), window, cx);
    });
    fixture.cx.run_until_parked();
    let state = |fixture: &mut Fixture| {
        fixture.cx.update_editor(|editor, _, cx| {
            (
                editor.text(cx),
                editor.selections.disjoint_anchors_arc(),
                editor.scroll_position(cx),
            )
        })
    };
    let before = state(&mut fixture);
    assert!(before.2.y > 0.);
    assert_eq!(capability(&mut fixture), Capability::ReadWrite);

    let session = fixture.open();
    assert_eq!(capability(&mut fixture), Capability::ReadOnly);
    for target in [one, UndoNodeId::ROOT, two, three, one] {
        fixture.move_to(&session, target);
        fixture.cx.run_until_parked();
    }
    fixture.close(&session);
    fixture.cx.run_until_parked();

    assert_eq!(state(&mut fixture), before);
    assert_eq!(capability(&mut fixture), Capability::ReadWrite);
    assert!(!has_override(&mut fixture));
    assert_eq!(
        session.read_with(&fixture.cx.cx.cx, |session, _| session.closed_reason()),
        Some(CloseReason::Quit)
    );
}

#[gpui::test]
async fn an_outside_edit_closes_the_session(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    let messages = Rc::new(RefCell::new(Vec::<String>::new()));
    fixture.cx.update(|_, cx| {
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });
    let [one, _, _] = branch_history(&mut fixture);
    let session = fixture.open();
    fixture.move_to(&session, one);

    fixture.buffer.update(&mut fixture.cx.cx.cx, |buffer, cx| {
        buffer.edit([(0..0, "outside ")], None, cx)
    });
    fixture.cx.run_until_parked();

    assert_eq!(
        session.read_with(&fixture.cx.cx.cx, |session, _| session.closed_reason()),
        Some(CloseReason::OutsideEdit)
    );
    assert_eq!(messages.borrow().as_slice(), [BUFFER_CHANGED_MESSAGE]);
    assert_eq!(capability(&mut fixture), Capability::ReadWrite);
    assert!(!has_override(&mut fixture));
    assert_eq!(fixture.text(), "outside one three ");
    let buffer = fixture.buffer.clone();
    assert!(
        fixture
            .cx
            .update(|_, cx| PreviewSession::for_buffer(&buffer, cx))
            .is_none()
    );
}

#[gpui::test]
async fn a_move_on_a_ten_thousand_node_history_is_bounded(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    let (editor, buffer) = fixture.seeded_chain(9_999);
    let (len, tip, parent) = buffer.read_with(&fixture.cx.cx.cx, |buffer, _| {
        let tree = buffer.undo_tree();
        let tip = tree.current();
        (tree.len(), tip, tree.parent(tip).unwrap())
    });
    assert_eq!(len, 10_000);

    let pane = fixture.pane.clone();
    let session = fixture
        .cx
        .update(|window, cx| PreviewSession::open(&editor, &pane, window, cx))
        .unwrap();
    let counts = |fixture: &mut Fixture| {
        session.read_with(&fixture.cx.cx.cx, |session, _| {
            (session.last_move_toggles(), session.undo_operation_calls())
        })
    };

    fixture.move_to(&session, UndoNodeId::ROOT);
    assert_eq!(counts(&mut fixture), (9_999, 1));
    assert_eq!(fixture.preview_text(&session), "");

    fixture.move_to(&session, tip);
    assert_eq!(counts(&mut fixture), (9_999, 2));
    assert_eq!(fixture.preview_text(&session).len(), 9_999);

    fixture.move_to(&session, parent);
    assert_eq!(counts(&mut fixture), (1, 3));
    assert_eq!(fixture.preview_text(&session).len(), 9_998);
    assert_eq!(
        buffer.read_with(&fixture.cx.cx.cx, |buffer, _| buffer.len()),
        9_999
    );
}

#[gpui::test]
async fn a_second_open_returns_the_first_session(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    assert_eq!(fixture.current(), UndoNodeId::ROOT);
    let first = fixture.open();
    assert_eq!(
        first.read_with(&fixture.cx.cx.cx, |session, _| session.preview_node()),
        UndoNodeId::ROOT
    );
    fixture.move_to(&first, UndoNodeId::ROOT);
    assert_eq!(fixture.preview_text(&first), "");
    let second = fixture.open();
    assert_eq!(first.entity_id(), second.entity_id());

    fixture.close(&first);
    let third = fixture.open();
    assert_ne!(first.entity_id(), third.entity_id());
}

#[gpui::test]
async fn releasing_the_session_or_the_editor_restores_the_buffer(cx: &mut TestAppContext) {
    let mut fixture = fixture(cx).await;
    let session = fixture.open();
    assert_eq!(capability(&mut fixture), Capability::ReadOnly);
    drop(session);
    fixture.cx.run_until_parked();
    assert_eq!(capability(&mut fixture), Capability::ReadWrite);
    assert!(!has_override(&mut fixture));

    let buffer = fixture.buffer.clone();
    let pane = fixture.pane.clone();
    let (editor, session) = fixture.cx.update(|window, cx| {
        let editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
        let session = PreviewSession::open(&editor, &pane, window, cx).unwrap();
        (editor, session)
    });
    assert_eq!(capability(&mut fixture), Capability::ReadOnly);
    assert!(has_override(&mut fixture));
    drop(editor);
    fixture.cx.run_until_parked();
    assert_eq!(
        session.read_with(&fixture.cx.cx.cx, |session, _| session.closed_reason()),
        Some(CloseReason::Released)
    );
    assert_eq!(capability(&mut fixture), Capability::ReadWrite);
    assert!(!has_override(&mut fixture));
}
