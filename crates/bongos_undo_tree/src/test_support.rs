//! Test helpers: a workspace with one Rust file open in an editor, a fake file system and a
//! fake language server, plus ways to build undo histories in it.

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use bongos_undo_tree_model::Now;
use editor::{Editor, actions::Undo, test::editor_lsp_test_context::EditorLspTestContext};
use gpui::{AppContext as _, Entity, TestAppContext};
use language::{Buffer, Capability};
use settings::KeymapFile;
use text::UndoNodeId;
use workspace::{MinibufferRouting, Pane};

use crate::{
    actions::Vundo,
    open::open_beside,
    panel::{UndoTreeClock, UndoTreePanel},
    preview::PreviewSession,
};

/// An editor over one file in a workspace, with a fake file system and a fake language server
/// that asks for incremental document sync.
pub(crate) struct Fixture {
    pub cx: EditorLspTestContext,
    pub editor: Entity<Editor>,
    pub buffer: Entity<Buffer>,
    pub pane: Entity<Pane>,
    pub path: PathBuf,
}

/// Opens an empty Rust file in a workspace. Every edit of its buffer is its own undo node.
pub(crate) async fn fixture(cx: &mut TestAppContext) -> Fixture {
    let mut cx = EditorLspTestContext::new_rust(
        lsp::ServerCapabilities {
            text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                lsp::TextDocumentSyncKind::INCREMENTAL,
            )),
            ..Default::default()
        },
        cx,
    )
    .await;
    let editor = cx.editor.clone();
    let buffer = editor.read_with(&cx.cx.cx, |editor, cx| {
        editor
            .buffer()
            .read(cx)
            .as_singleton()
            .expect("a singleton buffer")
    });
    buffer.update(&mut cx.cx.cx, |buffer, _| {
        buffer.set_group_interval(Duration::ZERO)
    });
    let pane = cx
        .workspace
        .read_with(&cx.cx.cx, |workspace, _| workspace.active_pane().clone());
    let path = EditorLspTestContext::root_path()
        .join("dir")
        .join("file.rs");
    Fixture {
        cx,
        editor,
        buffer,
        pane,
        path,
    }
}

impl Fixture {
    /// Types `text` in the editor as one undo node and returns that node.
    pub fn type_step(&mut self, text: &str) -> UndoNodeId {
        self.cx.update_editor(|editor, window, cx| {
            editor.handle_input(text, window, cx);
        });
        self.current()
    }

    /// Undoes once in the editor.
    pub fn undo(&mut self) {
        self.cx
            .update_editor(|editor, window, cx| editor.undo(&Undo, window, cx));
    }

    /// A plain buffer outside the project, with `count` undo nodes in a chain, each appending
    /// one character, and an editor over it with no project. Nothing parses or syncs it, so a
    /// long history is quick to build.
    pub fn seeded_chain(&mut self, count: usize) -> (Entity<Editor>, Entity<Buffer>) {
        let buffer = self.cx.update(|_, cx| {
            cx.new(|cx| {
                let mut buffer = Buffer::local("", cx);
                buffer.set_group_interval(Duration::ZERO);
                for _ in 0..count {
                    let end = buffer.len();
                    buffer.edit([(end..end, "x")], None, cx);
                }
                buffer
            })
        });
        let editor = self
            .cx
            .update(|window, cx| cx.new(|cx| Editor::for_buffer(buffer.clone(), None, window, cx)));
        (editor, buffer)
    }

    /// The undo node the real buffer is in.
    pub fn current(&mut self) -> UndoNodeId {
        self.buffer
            .read_with(&self.cx.cx.cx, |buffer, _| buffer.undo_tree().current())
    }

    /// The real buffer's text.
    pub fn text(&mut self) -> String {
        self.buffer
            .read_with(&self.cx.cx.cx, |buffer, _| buffer.text())
    }

    /// Opens a preview of the editor's buffer in its pane.
    pub fn open(&mut self) -> Entity<PreviewSession> {
        let editor = self.editor.clone();
        let pane = self.pane.clone();
        self.cx
            .update(|window, cx| PreviewSession::open(&editor, &pane, window, cx))
            .expect("the preview opens")
    }

    /// Moves the preview to `node`.
    pub fn move_to(&mut self, session: &Entity<PreviewSession>, node: UndoNodeId) {
        session
            .update_in(&mut self.cx.cx.cx, |session, window, cx| {
                session.move_to(node, window, cx)
            })
            .expect("the move succeeds");
    }

    /// Closes the preview.
    pub fn close(&mut self, session: &Entity<PreviewSession>) {
        session.update(&mut self.cx.cx.cx, |session, cx| session.close(cx));
    }

    /// The text the preview editor shows.
    pub fn preview_text(&mut self, session: &Entity<PreviewSession>) -> String {
        self.cx.update(|_, cx| {
            let preview_editor = session.read(cx).preview_editor().clone();
            preview_editor.read(cx).text(cx)
        })
    }
}

pub(crate) struct FakeNow(pub Rc<Cell<Instant>>);

impl Now for FakeNow {
    fn now(&self) -> Instant {
        self.0.get()
    }
}

pub(crate) struct Setup {
    pub fixture: Fixture,
    pub clock: Rc<Cell<Instant>>,
    pub messages: Rc<RefCell<Vec<String>>>,
}

/// The fixture with `undo_tree::Vundo` registered, the default Linux keymap bound, a fake clock
/// (set from the history's own times by each test) and the echo area captured.
pub(crate) async fn setup(cx: &mut TestAppContext) -> Setup {
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
    pub fn vundo(&mut self) -> Entity<UndoTreePanel> {
        self.fixture.cx.cx.cx.dispatch_action(Vundo);
        self.fixture.cx.run_until_parked();
        self.panels()
            .into_iter()
            .next()
            .expect("an undo tree panel is open")
    }

    pub fn keys(&mut self, keys: &str) {
        self.fixture.cx.cx.cx.simulate_keystrokes(keys);
        self.fixture.cx.run_until_parked();
    }

    pub fn panels(&mut self) -> Vec<Entity<UndoTreePanel>> {
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

    pub fn center_panes(&mut self) -> Vec<Entity<Pane>> {
        self.fixture
            .cx
            .update_workspace(|workspace, _, _| workspace.center_panes())
    }

    pub fn pane_of(&mut self, panel: &Entity<UndoTreePanel>) -> Entity<Pane> {
        self.fixture
            .cx
            .update_workspace(|workspace, _, _| workspace.pane_for(panel))
            .expect("the panel is in a pane")
    }

    pub fn capability(&mut self, buffer: &Entity<Buffer>) -> Capability {
        buffer.read_with(&self.fixture.cx.cx.cx, |buffer, _| buffer.capability())
    }

    pub fn has_override(&mut self) -> bool {
        self.fixture
            .pane
            .read_with(&self.fixture.cx.cx.cx, |pane, _| {
                pane.content_override().is_some()
            })
    }

    pub fn preview_text(&mut self, panel: &Entity<UndoTreePanel>) -> String {
        let session = panel.read_with(&self.fixture.cx.cx.cx, |panel, _| panel.session().clone());
        self.fixture.preview_text(&session)
    }

    pub fn cursor_node(&mut self, panel: &Entity<UndoTreePanel>) -> Option<UndoNodeId> {
        panel.read_with(&self.fixture.cx.cx.cx, |panel, _| {
            panel.tree().id(panel.cursor())
        })
    }

    pub fn row_texts(&mut self, panel: &Entity<UndoTreePanel>) -> Vec<(String, Option<String>)> {
        panel.read_with(&self.fixture.cx.cx.cx, |panel, _| {
            (0..panel.rows().len())
                .filter_map(|index| panel.row_text(index))
                .collect()
        })
    }

    pub fn last_edit_at(&mut self, node: UndoNodeId) -> Instant {
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
    pub fn assert_closed(&mut self, buffer: &Entity<Buffer>) {
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
    pub fn chain_in_pane(&mut self, count: usize) -> (Entity<Editor>, Entity<Buffer>) {
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

    pub fn open_beside(&mut self, editor: &Entity<Editor>) -> Entity<UndoTreePanel> {
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
pub(crate) fn branch_history(fixture: &mut Fixture) -> [UndoNodeId; 3] {
    let one = fixture.type_step("one ");
    let two = fixture.type_step("two ");
    fixture.undo();
    let three = fixture.type_step("three ");
    [one, two, three]
}
