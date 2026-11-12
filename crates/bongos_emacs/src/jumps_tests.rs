use std::cell::Cell;
use std::rc::Rc;

use editor::Editor;
use editor::test::editor_test_context::EditorTestContext;
use futures::StreamExt as _;
use gpui::{App, Entity, Focusable as _, TestAppContext, VisualTestContext, WindowHandle};
use language::{FakeLspAdapter, rust_lang};
use multi_buffer::ToOffset as _;
use project::{Fs as _, Project, ProjectPath};
use text::Point;
use workspace::{AppState, MultiWorkspace, Workspace, WorkspaceHandle as _};

use crate::actions::{
    BeginningOfBuffer, EndOfBuffer, PopGlobalMark, PopMark, SetMarkCommand, XrefFindDefinitions,
    XrefFindImplementations,
};
use crate::apply_tests::init_test;
use crate::mark::{mark_for, ring_for};

/// The mark and the ring (newest first) of the editor's buffer, as offsets.
fn marks_of(editor: &Entity<Editor>, cx: &mut App) -> (Option<usize>, Vec<usize>) {
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let mark = mark_for(editor, cx).map(|anchor| anchor.to_offset(&snapshot).0);
    let ring = ring_for(editor, cx)
        .into_iter()
        .map(|anchor| anchor.to_offset(&snapshot).0)
        .collect();
    (mark, ring)
}

fn marks(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> (Option<usize>, Vec<usize>) {
    cx.update(|_, cx| marks_of(editor, cx))
}

async fn text_marks(cx: &mut TestAppContext, marked: &str) -> EditorTestContext {
    init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.set_state(marked);
    cx
}

fn buffer_marks(cx: &mut EditorTestContext) -> (Option<usize>, Vec<usize>) {
    let editor = cx.editor.clone();
    cx.update(|_, cx| marks_of(&editor, cx))
}

#[gpui::test]
async fn end_of_buffer_pushes_the_mark_and_pop_mark_returns(cx: &mut TestAppContext) {
    let mut cx = text_marks(cx, "one ˇtwo three").await;
    cx.dispatch_action(EndOfBuffer);
    cx.assert_editor_state("one two threeˇ");
    assert_eq!(buffer_marks(&mut cx), (Some(4), vec![]));

    cx.dispatch_action(PopMark);
    cx.assert_editor_state("one ˇtwo three");
}

#[gpui::test]
async fn beginning_of_buffer_pushes_the_mark_and_pop_mark_returns(cx: &mut TestAppContext) {
    let mut cx = text_marks(cx, "one two thˇree").await;
    cx.dispatch_action(BeginningOfBuffer);
    cx.assert_editor_state("ˇone two three");
    assert_eq!(buffer_marks(&mut cx), (Some(10), vec![]));

    cx.dispatch_action(PopMark);
    cx.assert_editor_state("one two thˇree");
}

#[gpui::test]
async fn two_jumps_stack_two_marks(cx: &mut TestAppContext) {
    let mut cx = text_marks(cx, "one ˇtwo three").await;
    cx.dispatch_action(EndOfBuffer);
    cx.dispatch_action(BeginningOfBuffer);
    cx.assert_editor_state("ˇone two three");
    assert_eq!(buffer_marks(&mut cx), (Some(13), vec![4]));
}

#[gpui::test]
async fn end_of_buffer_with_a_region_extends_it_and_pushes_nothing(cx: &mut TestAppContext) {
    let mut cx = text_marks(cx, "one ˇtwo three").await;
    cx.dispatch_action(SetMarkCommand { count: None });
    assert_eq!(buffer_marks(&mut cx), (Some(4), vec![]));

    cx.dispatch_action(EndOfBuffer);
    cx.assert_editor_state("one «two threeˇ»");
    assert_eq!(buffer_marks(&mut cx), (Some(4), vec![]));
}

#[gpui::test]
async fn a_single_line_editor_propagates_the_jumps(cx: &mut TestAppContext) {
    init_test(cx);
    let passed = Rc::new(Cell::new(0));
    cx.update(|cx| {
        let count = passed.clone();
        cx.on_action(move |_: &BeginningOfBuffer, _| count.set(count.get() + 1));
        let count = passed.clone();
        cx.on_action(move |_: &EndOfBuffer, _| count.set(count.get() + 1));
        let count = passed.clone();
        cx.on_action(move |_: &XrefFindDefinitions, _| count.set(count.get() + 1));
        let count = passed.clone();
        cx.on_action(move |_: &XrefFindImplementations, _| count.set(count.get() + 1));
    });
    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text("one two", window, cx);
        editor
    });
    let editor = window.root(cx).unwrap();
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));

    cx.dispatch_action(BeginningOfBuffer);
    cx.dispatch_action(EndOfBuffer);
    cx.dispatch_action(XrefFindDefinitions);
    cx.dispatch_action(XrefFindImplementations);
    assert_eq!(passed.get(), 4);
    assert_eq!(marks(&editor, cx), (None, vec![]));
}

const CALLER: &str = "fn caller() {\n    target();\n}\nfn target() {}\n";
const TARGET: &str = "fn other() {}\nfn target() {}\n";

/// Two Rust files, `a.rs` (open, caret on the call to `target` in row 1) and `b.rs`, served by
/// a fake language server.
struct Scene {
    workspace: Entity<Workspace>,
    editor: Entity<Editor>,
    paths: [ProjectPath; 2],
    server: lsp::FakeLanguageServer,
    cx: VisualTestContext,
}

async fn scene(cx: &mut TestAppContext) -> Scene {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        app_state
    });
    let fake = app_state.fs.as_fake();
    fake.create_dir("/root".as_ref()).await.unwrap();
    fake.insert_file("/root/a.rs", CALLER.as_bytes().to_vec())
        .await;
    fake.insert_file("/root/b.rs", TARGET.as_bytes().to_vec())
        .await;
    let project = Project::test(app_state.fs.clone(), ["/root".as_ref()], cx).await;
    let languages = project.read_with(cx, |project, _| project.languages().clone());
    languages.add(rust_lang());
    let mut servers = languages.register_fake_lsp(
        "Rust",
        FakeLspAdapter {
            capabilities: lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                implementation_provider: Some(lsp::ImplementationProviderCapability::Simple(true)),
                ..Default::default()
            },
            ..FakeLspAdapter::default()
        },
    );
    let _ = languages.language_for_name("Rust").await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    cx.run_until_parked();
    let all = cx.read(|cx| workspace.file_project_paths(cx));
    let path = |name: &str| {
        all.iter()
            .find(|path| path.path.file_name() == Some(name))
            .unwrap()
            .clone()
    };
    let paths = [path("a.rs"), path("b.rs")];
    let editor = open(&workspace, &paths[0], &mut cx).await;
    let server = servers.next().await.expect("the fake server starts");
    cx.run_until_parked();
    editor.update_in(&mut cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |s| {
            s.select_ranges([Point::new(1, 6)..Point::new(1, 6)])
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    Scene {
        workspace,
        editor,
        paths,
        server,
        cx,
    }
}

async fn open(
    workspace: &Entity<Workspace>,
    path: &ProjectPath,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let editor = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.open_path(path.clone(), None, true, window, cx)
        })
        .await
        .unwrap()
        .downcast::<Editor>()
        .unwrap();
    cx.run_until_parked();
    editor
}

impl Scene {
    /// Makes the server answer both lookups with `row` of `file`.
    fn answer(&self, file: &str, row: u32) {
        let location = lsp::Location {
            uri: lsp::Uri::from_file_path(format!("/root/{file}")).unwrap(),
            range: lsp::Range::new(lsp::Position::new(row, 3), lsp::Position::new(row, 9)),
        };
        let definition = location.clone();
        self.server
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(move |_, _| {
                let location = definition.clone();
                async move { Ok(Some(lsp::GotoDefinitionResponse::Scalar(location))) }
            });
        self.server
            .set_request_handler::<lsp::request::GotoImplementation, _, _>(move |_, _| {
                let location = location.clone();
                async move { Ok(Some(lsp::GotoDefinitionResponse::Scalar(location))) }
            });
    }

    /// The active file and caret row.
    fn place(&mut self) -> (ProjectPath, u32) {
        self.workspace.update(&mut self.cx, |workspace, cx| {
            let item = workspace.active_item(cx).unwrap();
            let path = item.project_path(cx).unwrap();
            let editor = item.downcast::<Editor>().unwrap();
            editor.update(cx, |editor, cx| {
                let display = editor.display_snapshot(cx);
                (
                    path,
                    editor.selections.newest_display(&display).head().row().0,
                )
            })
        })
    }
}

#[gpui::test]
async fn a_definition_in_the_same_file_pushes_the_mark_and_pop_mark_returns(
    cx: &mut TestAppContext,
) {
    let mut scene = scene(cx).await;
    scene.answer("a.rs", 3);
    // "fn caller() {\n" is 14 bytes; the caret is 6 columns into row 1.
    assert_eq!(marks(&scene.editor, &mut scene.cx), (None, vec![]));

    scene.cx.dispatch_action(XrefFindDefinitions);
    scene.cx.run_until_parked();
    assert_eq!(scene.place(), (scene.paths[0].clone(), 3));
    assert_eq!(marks(&scene.editor, &mut scene.cx), (Some(20), vec![]));

    scene.cx.dispatch_action(PopMark);
    assert_eq!(scene.place(), (scene.paths[0].clone(), 1));
}

#[gpui::test]
async fn an_implementation_in_the_same_file_pushes_the_mark(cx: &mut TestAppContext) {
    let mut scene = scene(cx).await;
    scene.answer("a.rs", 3);

    scene.cx.dispatch_action(XrefFindImplementations);
    scene.cx.run_until_parked();
    assert_eq!(scene.place(), (scene.paths[0].clone(), 3));
    assert_eq!(marks(&scene.editor, &mut scene.cx), (Some(20), vec![]));

    scene.cx.dispatch_action(PopMark);
    assert_eq!(scene.place(), (scene.paths[0].clone(), 1));
}

#[gpui::test]
async fn a_definition_in_another_file_returns_with_pop_global_mark(cx: &mut TestAppContext) {
    let mut scene = scene(cx).await;
    scene.answer("b.rs", 1);

    scene.cx.dispatch_action(XrefFindDefinitions);
    scene.cx.run_until_parked();
    assert_eq!(scene.place(), (scene.paths[1].clone(), 1));
    assert_eq!(marks(&scene.editor, &mut scene.cx), (Some(20), vec![]));

    scene.cx.dispatch_action(PopGlobalMark);
    scene.cx.run_until_parked();
    assert_eq!(scene.place(), (scene.paths[0].clone(), 1));
}
