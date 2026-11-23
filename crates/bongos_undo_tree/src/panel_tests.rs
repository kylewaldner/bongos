use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use bongos_undo_tree_model::{NO_OTHER_BRANCH, NodeMark, Now};
use editor::Editor;
use gpui::{AppContext as _, Entity, Focusable as _, TestAppContext};
use language::{Buffer, Capability};
use settings::KeymapFile;
use text::UndoNodeId;
use workspace::{MinibufferRouting, Pane, SaveIntent, item::Item as _};

use crate::{
    actions::Vundo,
    open::open_beside,
    panel::{KEY_CONTEXT, UndoTreeClock, UndoTreePanel},
    preview::PreviewSession,
    test_support::{Fixture, fixture},
};

struct FakeNow(Rc<Cell<Instant>>);

impl Now for FakeNow {
    fn now(&self) -> Instant {
        self.0.get()
    }
}

struct Setup {
    fixture: Fixture,
    clock: Rc<Cell<Instant>>,
    messages: Rc<RefCell<Vec<String>>>,
}

/// The fixture with `undo_tree::Vundo` registered, the default Linux keymap bound, a fake clock
/// (set from the history's own times by each test) and the echo area captured.
async fn setup(cx: &mut TestAppContext) -> Setup {
    let mut fixture = fixture(cx).await;
    fixture
        .cx
        .update_workspace(|workspace, _, _| crate::register(workspace));
    let clock = Rc::new(Cell::new(fixture.cx.update(|_, cx| UndoTreeClock::now(cx))));
    let messages = Rc::new(RefCell::new(Vec::<String>::new()));
    fixture.cx.update(|_, cx| {
        let bindings =
            KeymapFile::load_asset_allow_partial_failure("keymaps/default-linux.json", cx)
                .expect("the default keymap loads");
        cx.bind_keys(bindings);
        UndoTreeClock::set(cx, Rc::new(FakeNow(clock.clone())));
        let messages = messages.clone();
        cx.default_global::<MinibufferRouting>()
            .set_message_hook(Some(Rc::new(move |_, _, text, _| {
                messages.borrow_mut().push(text.to_string());
                true
            })));
    });
    Setup {
        fixture,
        clock,
        messages,
    }
}

impl Setup {
    fn vundo(&mut self) -> Entity<UndoTreePanel> {
        self.fixture.cx.cx.cx.dispatch_action(Vundo);
        self.fixture.cx.run_until_parked();
        self.panels()
            .into_iter()
            .next()
            .expect("an undo tree panel is open")
    }

    fn keys(&mut self, keys: &str) {
        self.fixture.cx.cx.cx.simulate_keystrokes(keys);
        self.fixture.cx.run_until_parked();
    }

    fn panels(&mut self) -> Vec<Entity<UndoTreePanel>> {
        self.fixture.cx.update_workspace(|workspace, _, cx| {
            workspace
                .panes()
                .iter()
                .flat_map(|pane| {
                    pane.read(cx)
                        .items_of_type::<UndoTreePanel>()
                        .collect::<Vec<_>>()
                })
                .collect()
        })
    }

    fn center_panes(&mut self) -> Vec<Entity<Pane>> {
        self.fixture
            .cx
            .update_workspace(|workspace, _, _| workspace.center_panes())
    }

    fn pane_of(&mut self, panel: &Entity<UndoTreePanel>) -> Entity<Pane> {
        self.fixture
            .cx
            .update_workspace(|workspace, _, _| workspace.pane_for(panel))
            .expect("the panel is in a pane")
    }

    fn capability(&mut self, buffer: &Entity<Buffer>) -> Capability {
        buffer.read_with(&self.fixture.cx.cx.cx, |buffer, _| buffer.capability())
    }

    fn has_override(&mut self) -> bool {
        self.fixture
            .pane
            .read_with(&self.fixture.cx.cx.cx, |pane, _| {
                pane.content_override().is_some()
            })
    }

    fn preview_text(&mut self, panel: &Entity<UndoTreePanel>) -> String {
        let session = panel.read_with(&self.fixture.cx.cx.cx, |panel, _| panel.session().clone());
        self.fixture.preview_text(&session)
    }

    fn cursor_node(&mut self, panel: &Entity<UndoTreePanel>) -> Option<UndoNodeId> {
        panel.read_with(&self.fixture.cx.cx.cx, |panel, _| {
            panel.tree().id(panel.cursor())
        })
    }

    fn row_texts(&mut self, panel: &Entity<UndoTreePanel>) -> Vec<(String, Option<String>)> {
        panel.read_with(&self.fixture.cx.cx.cx, |panel, _| {
            (0..panel.rows().len())
                .filter_map(|index| panel.row_text(index))
                .collect()
        })
    }

    fn last_edit_at(&mut self, node: UndoNodeId) -> Instant {
        self.fixture
            .buffer
            .read_with(&self.fixture.cx.cx.cx, |buffer, _| {
                buffer
                    .undo_tree()
                    .node(node)
                    .and_then(|node| node.entry())
                    .expect("an edited node")
                    .last_edit_at()
            })
    }

    /// Asserts that the panel and its pane are gone, the preview is closed and the real buffer
    /// is writable and drawn again.
    fn assert_closed(&mut self, buffer: &Entity<Buffer>) {
        assert!(self.panels().is_empty(), "no undo tree panel is left");
        assert_eq!(self.center_panes(), vec![self.fixture.pane.clone()]);
        assert_eq!(self.capability(buffer), Capability::ReadWrite);
        assert!(!self.has_override());
        let buffer = buffer.clone();
        assert!(
            self.fixture
                .cx
                .update(|_, cx| PreviewSession::for_buffer(&buffer, cx))
                .is_none()
        );
    }

    /// A plain buffer with `count` undo nodes in a chain, and an editor over it shown as the
    /// active item of the fixture's pane.
    fn chain_in_pane(&mut self, count: usize) -> (Entity<Editor>, Entity<Buffer>) {
        let (editor, buffer) = self.fixture.seeded_chain(count);
        let pane = self.fixture.pane.clone();
        self.fixture.cx.update(|window, cx| {
            pane.update(cx, |pane, cx| {
                pane.add_item(Box::new(editor.clone()), true, true, None, window, cx)
            })
        });
        self.fixture.cx.run_until_parked();
        (editor, buffer)
    }

    fn open_beside(&mut self, editor: &Entity<Editor>) -> Entity<UndoTreePanel> {
        let pane = self.fixture.pane.clone();
        let panel = self
            .fixture
            .cx
            .update_workspace(|workspace, window, cx| {
                open_beside(workspace, editor, &pane, window, cx)
            })
            .expect("the undo tree opens");
        self.fixture.cx.run_until_parked();
        panel
    }
}

/// Types `one `, `two `, undoes `two ` and types `three `, so `two ` is an undone sibling
/// branch. Returns the nodes of `one `, `two ` and `three `.
fn branch_history(fixture: &mut Fixture) -> [UndoNodeId; 3] {
    let one = fixture.type_step("one ");
    let two = fixture.type_step("two ");
    fixture.undo();
    let three = fixture.type_step("three ");
    [one, two, three]
}

#[gpui::test]
async fn vundo_splits_a_dedicated_pane_right_of_the_buffer_and_focuses_it(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    setup.fixture.type_step("one ");
    let panel = setup.vundo();

    let editor_pane = setup.fixture.pane.clone();
    let panel_pane = setup.pane_of(&panel);
    assert_eq!(
        setup.center_panes(),
        vec![editor_pane.clone(), panel_pane.clone()]
    );
    let (dedicated, editor_pane_dedicated) = setup.fixture.cx.update(|_, cx| {
        (
            panel_pane.read(cx).is_dedicated(),
            editor_pane.read(cx).is_dedicated(),
        )
    });
    assert!(dedicated);
    assert!(!editor_pane_dedicated);
    let (focused, title) = setup.fixture.cx.update(|window, cx| {
        (
            panel.focus_handle(cx).is_focused(window),
            panel.read(cx).tab_content_text(0, cx),
        )
    });
    assert!(focused, "the panel has focus");
    assert_eq!(title, "Undo tree: file.rs");
    assert!(setup.has_override(), "the buffer's pane shows the preview");
    let buffer = setup.fixture.buffer.clone();
    assert_eq!(setup.capability(&buffer), Capability::ReadOnly);

    let editor = setup.fixture.editor.clone();
    setup
        .fixture
        .cx
        .update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    setup.fixture.cx.run_until_parked();
    let again = setup.vundo();
    assert_eq!(again.entity_id(), panel.entity_id());
    assert_eq!(setup.panels().len(), 1);
    assert_eq!(setup.center_panes().len(), 2);
    let focused = setup
        .fixture
        .cx
        .update(|window, cx| panel.focus_handle(cx).is_focused(window));
    assert!(focused, "a second Vundo focuses the open panel");
}

#[gpui::test]
async fn rows_show_relative_times_from_the_injected_clock(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let one = setup.fixture.type_step("one ");
    let two = setup.fixture.type_step("two ");
    let newest = setup.last_edit_at(two);
    assert!(setup.last_edit_at(one) <= newest);
    setup
        .clock
        .set(newest + Duration::from_secs(2 * 60 * 60 + 30));
    let panel = setup.vundo();

    assert_eq!(
        setup.row_texts(&panel),
        vec![
            ("○".to_string(), None),
            ("○".to_string(), Some("2 h ago".to_string())),
            ("◉".to_string(), Some("2 h ago".to_string())),
        ]
    );

    setup.clock.set(newest + Duration::from_secs(25 * 60 * 60));
    setup.keys("b");
    assert_eq!(
        setup.row_texts(&panel),
        vec![
            ("○".to_string(), None),
            ("●".to_string(), Some("yesterday".to_string())),
            ("◆".to_string(), Some("yesterday".to_string())),
        ]
    );

    setup
        .clock
        .set(newest + Duration::from_secs(3 * 24 * 60 * 60 + 5));
    setup.keys("f");
    assert_eq!(
        setup.row_texts(&panel)[2],
        ("◉".to_string(), Some("3 days ago".to_string()))
    );
}

#[gpui::test]
async fn keys_move_the_preview_and_set_and_clear_the_mark(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let [one, two, three] = branch_history(&mut setup.fixture);
    let panel = setup.vundo();
    assert_eq!(setup.cursor_node(&panel), Some(three));
    assert_eq!(setup.preview_text(&panel), "one three ");

    setup.keys("n");
    assert_eq!(setup.messages.borrow().as_slice(), [NO_OTHER_BRANCH]);
    assert_eq!(setup.cursor_node(&panel), Some(three));

    setup.keys("p");
    assert_eq!(setup.cursor_node(&panel), Some(two));
    assert_eq!(setup.preview_text(&panel), "one two ");
    setup.keys("n");
    assert_eq!(setup.cursor_node(&panel), Some(three));
    setup.keys("p");

    setup.keys("b");
    assert_eq!(setup.cursor_node(&panel), Some(one));
    assert_eq!(setup.preview_text(&panel), "one ");
    setup.keys("f");
    assert_eq!(
        setup.cursor_node(&panel),
        Some(three),
        "forward follows the path to the state the buffer is in"
    );
    assert_eq!(setup.preview_text(&panel), "one three ");

    setup.keys("b b");
    assert_eq!(setup.cursor_node(&panel), Some(UndoNodeId::ROOT));
    assert_eq!(setup.preview_text(&panel), "");
    setup.keys("b");
    assert_eq!(setup.cursor_node(&panel), Some(UndoNodeId::ROOT));

    setup.keys("m");
    assert_eq!(
        panel.read_with(&setup.fixture.cx.cx.cx, |panel, _| panel.mark()),
        Some(0)
    );
    setup.keys("f");
    assert_eq!(setup.cursor_node(&panel), Some(one));
    let root_mark = panel.read_with(&setup.fixture.cx.cx.cx, |panel, _| panel.rows()[0].mark);
    assert_eq!(root_mark, NodeMark::Marked);
    setup.keys("u");
    assert_eq!(
        panel.read_with(&setup.fixture.cx.cx.cx, |panel, _| panel.mark()),
        Some(0),
        "u clears the mark only on the marked state"
    );
    setup.keys("b u");
    assert_eq!(
        panel.read_with(&setup.fixture.cx.cx.cx, |panel, _| panel.mark()),
        None
    );

    assert_eq!(setup.fixture.text(), "one three ");
}

#[gpui::test]
async fn typing_in_the_panel_inserts_nothing(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    setup.fixture.type_step("one ");
    let panel = setup.vundo();
    let in_context = setup.fixture.cx.update(|window, cx| {
        panel.focus_handle(cx).is_focused(window)
            && window
                .context_stack()
                .iter()
                .any(|context| context.contains(KEY_CONTEXT))
    });
    assert!(in_context, "keys reach the panel in its key context");

    setup.keys("x y z");
    assert_eq!(setup.fixture.text(), "one ");
    assert_eq!(setup.preview_text(&panel), "one ");
    assert_eq!(setup.panels().len(), 1);
}

#[gpui::test]
async fn q_and_ctrl_g_close_the_pane_and_restore_the_buffer(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    setup.fixture.type_step("one ");
    let buffer = setup.fixture.buffer.clone();

    setup.vundo();
    setup.keys("b q");
    setup.assert_closed(&buffer);
    assert_eq!(setup.fixture.text(), "one ");

    let editor = setup.fixture.editor.clone();
    setup
        .fixture
        .cx
        .update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    setup.vundo();
    setup.keys("ctrl-g");
    setup.assert_closed(&buffer);
}

#[gpui::test]
async fn deleting_the_panel_window_restores_and_leaves_no_panel_tab(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    setup.fixture.type_step("one ");
    let buffer = setup.fixture.buffer.clone();
    let panel = setup.vundo();
    let panel_pane = setup.pane_of(&panel);
    let editor_pane = setup.fixture.pane.clone();

    setup.fixture.cx.update_workspace(|workspace, window, cx| {
        workspace
            .close_pane_keeping_tabs(&panel_pane, &editor_pane, window, cx)
            .expect("the pane closes into its neighbor");
        panel_pane.update(cx, |pane, cx| pane.set_dedicated(false, cx));
    });
    setup.fixture.cx.run_until_parked();

    setup.assert_closed(&buffer);
    let editor_id = setup.fixture.editor.entity_id();
    let active = editor_pane.read_with(&setup.fixture.cx.cx.cx, |pane, _| {
        pane.active_item().map(|item| item.item_id())
    });
    assert_eq!(active, Some(editor_id));
}

#[gpui::test]
async fn a_pruned_history_starts_with_a_note_row(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let buffer = setup.fixture.cx.update(|_, cx| {
        cx.new(|cx| {
            let mut text = text::Buffer::new(
                clock::ReplicaId::LOCAL,
                cx.entity_id().as_non_zero_u64().into(),
                String::new(),
            );
            text.set_undo_node_limit(100);
            let mut buffer = Buffer::build(text, None, Capability::ReadWrite);
            buffer.set_group_interval(Duration::ZERO);
            for _ in 0..110 {
                buffer.edit([(0..0, "x")], None, cx);
                buffer.undo(cx);
            }
            buffer
        })
    });
    let editor = setup
        .fixture
        .cx
        .update(|window, cx| cx.new(|cx| Editor::for_buffer(buffer.clone(), None, window, cx)));
    let pane = setup.fixture.pane.clone();
    setup.fixture.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(editor.clone()), true, true, None, window, cx)
        })
    });
    let (len, pruned) = buffer.read_with(&setup.fixture.cx.cx.cx, |buffer, _| {
        (buffer.undo_tree().len(), buffer.undo_tree().pruned())
    });
    assert!(pruned > 0, "the history was pruned");

    let panel = setup.open_beside(&editor);
    let rows = setup.row_texts(&panel);
    assert_eq!(rows.len(), len + 1);
    assert_eq!(
        rows[0],
        (format!("Older history discarded ({pruned} nodes)"), None)
    );
    let first_state = panel.read_with(&setup.fixture.cx.cx.cx, |panel, _| panel.rows()[1].node);
    assert_eq!(first_state, Some(0));
}

#[gpui::test]
async fn opening_ten_thousand_nodes_lays_out_once(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let (editor, buffer) = setup.chain_in_pane(9_999);
    let (len, pruned) = buffer.read_with(&setup.fixture.cx.cx.cx, |buffer, _| {
        (buffer.undo_tree().len(), buffer.undo_tree().pruned())
    });
    assert_eq!(len, 10_000);

    let panel = setup.open_beside(&editor);
    let (rows, state_rows, passes, nodes) = panel.read_with(&setup.fixture.cx.cx.cx, |panel, _| {
        (
            panel.rows().len(),
            panel.rows().iter().filter(|row| row.node.is_some()).count(),
            panel.layout_passes(),
            panel.tree().shape.nodes.len(),
        )
    });
    assert_eq!(nodes, len);
    assert_eq!(state_rows, len, "one row per state");
    assert_eq!(rows, len + usize::from(pruned > 0));
    assert_eq!(passes, 1, "the rows are laid out in a single pass on open");
}

#[gpui::test]
async fn the_buffer_pane_showing_another_item_closes_the_panel(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    setup.fixture.type_step("one ");
    let buffer = setup.fixture.buffer.clone();
    setup.vundo();

    setup.chain_in_pane(1);
    setup.assert_closed(&buffer);
}

#[gpui::test]
async fn closing_the_panel_tab_closes_the_panel(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    setup.fixture.type_step("one ");
    let buffer = setup.fixture.buffer.clone();
    let panel = setup.vundo();
    let panel_pane = setup.pane_of(&panel);
    let panel_id = panel.entity_id();
    drop(panel);
    let closed = setup.fixture.cx.update(|window, cx| {
        panel_pane.update(cx, |pane, cx| {
            pane.close_item_by_id(panel_id, SaveIntent::Close, window, cx)
        })
    });
    closed.await.expect("the tab closes");
    setup.fixture.cx.run_until_parked();
    setup.assert_closed(&buffer);
}

#[gpui::test]
async fn closing_the_previewed_editor_closes_the_panel(cx: &mut TestAppContext) {
    let mut setup = setup(cx).await;
    let (editor, other_buffer) = setup.chain_in_pane(3);
    let panel = setup.open_beside(&editor);
    assert_eq!(setup.capability(&other_buffer), Capability::ReadOnly);
    let session = panel.read_with(&setup.fixture.cx.cx.cx, |panel, _| panel.session().clone());
    drop(panel);

    let pane = setup.fixture.pane.clone();
    let editor_id = editor.entity_id();
    let closed = setup.fixture.cx.update(|window, cx| {
        pane.update(cx, |pane, cx| {
            pane.close_item_by_id(editor_id, SaveIntent::Close, window, cx)
        })
    });
    closed.await.expect("the editor closes");
    setup.fixture.cx.run_until_parked();

    assert!(setup.panels().is_empty());
    assert_eq!(setup.center_panes(), vec![setup.fixture.pane.clone()]);
    assert_eq!(setup.capability(&other_buffer), Capability::ReadWrite);
    assert!(!setup.has_override());
    assert!(
        session
            .read_with(&setup.fixture.cx.cx.cx, |session, _| session
                .closed_reason())
            .is_some()
    );
}

#[gpui::test]
fn both_default_keymaps_bind_the_panel_keys(cx: &mut TestAppContext) {
    cx.update(|cx| {
        for path in ["keymaps/default-linux.json", "keymaps/default-macos.json"] {
            let bindings = KeymapFile::load_asset_allow_partial_failure(path, cx)
                .expect("the default keymap loads");
            let mut panel_keys = bindings
                .iter()
                .filter(|binding| binding.action().name().starts_with("undo_tree::"))
                .map(|binding| {
                    let keys = binding
                        .keystrokes()
                        .iter()
                        .map(|keystroke| keystroke.unparse())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let context = binding
                        .predicate()
                        .map(|predicate| predicate.to_string())
                        .unwrap_or_default();
                    (keys, binding.action().name().to_string(), context)
                })
                .collect::<Vec<_>>();
            panel_keys.sort();
            let expected = [
                ("b", "undo_tree::VundoBackward"),
                ("ctrl-g", "undo_tree::VundoQuit"),
                ("d", "undo_tree::VundoDiff"),
                ("enter", "undo_tree::VundoConfirm"),
                ("f", "undo_tree::VundoForward"),
                ("m", "undo_tree::VundoMark"),
                ("n", "undo_tree::VundoNext"),
                ("p", "undo_tree::VundoPrevious"),
                ("q", "undo_tree::VundoQuit"),
                ("u", "undo_tree::VundoUnmark"),
            ]
            .map(|(keys, action)| {
                (
                    keys.to_string(),
                    action.to_string(),
                    KEY_CONTEXT.to_string(),
                )
            });
            assert_eq!(panel_keys, expected, "{path}");
        }
    });
}
