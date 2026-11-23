//! Test helpers: a workspace with one Rust file open in an editor, a fake file system and a
//! fake language server, plus ways to build undo histories in it.

use std::{path::PathBuf, time::Duration};

use editor::{Editor, actions::Undo, test::editor_lsp_test_context::EditorLspTestContext};
use gpui::{AppContext as _, Entity, TestAppContext};
use language::Buffer;
use text::UndoNodeId;
use workspace::Pane;

use crate::preview::PreviewSession;

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
