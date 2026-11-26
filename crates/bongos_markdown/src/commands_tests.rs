//! Tests of the `markdown` block commands on a markdown editor in a workspace: block moves and
//! their undo, the prefix argument, insert block through the minibuffer, the checkbox toggle,
//! the table cell moves and their key context, list continuation on RET, and an empty buffer.

use std::sync::Arc;

use bongos_emacs::actions::UniversalArgument;
use bongos_minibuffer::minibuffer_for_window;
use editor::actions::{Newline, Undo};
use editor::{Editor, MultiBufferOffset, SelectionEffects};
use gpui::{
    Action, AppContext as _, Entity, Focusable as _, KeyBinding, TestAppContext,
    VisualContext as _, VisualTestContext, WindowHandle,
};
use language::{Buffer, Language, LanguageConfig};
use project::Project;
use workspace::{AppState, MultiWorkspace, Workspace};
use zed_actions::minibuffer;

use crate::{
    END_OF_TABLE, Mode, NO_BLOCK_ABOVE, NO_BLOCK_BELOW, NOT_A_TASK, NOT_MARKDOWN,
    TABLE_KEY_CONTEXT, markdown, set_mode,
};

/// The Markdown language with the app's own configuration (its list markers among them).
fn markdown_language() -> Arc<Language> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../grammars/src/markdown/config.toml"
    );
    let config = LanguageConfig::load(path).expect("the markdown config loads");
    Arc::new(Language::new(config, None))
}

struct Env {
    buffer: Entity<Buffer>,
    editor: Entity<Editor>,
    cx: VisualTestContext,
}

/// A workspace whose active item is an editor on a Markdown buffer holding `text`, in `mode`, and
/// focused. `ctrl-u` starts a prefix argument and `f2` moves the block down.
async fn setup(text: &str, mode: Mode, cx: &mut TestAppContext) -> Env {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        command_palette_hooks::init(cx);
        bongos_emacs::init(cx);
        bongos_minibuffer::init(cx);
        bongos_prompts::init(cx);
        crate::init(cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-u", UniversalArgument, Some("Editor")),
            KeyBinding::new("f2", markdown::MoveBlockDown, None),
        ]);
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace: Entity<Workspace> = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let text = text.to_string();
    let buffer = cx.new(|cx| {
        let mut buffer = Buffer::local(text, cx);
        buffer.set_language(Some(markdown_language()), cx);
        buffer
    });
    cx.update(|_, cx| set_mode(&buffer, mode, cx));
    let editor = add_editor(&workspace, &buffer, &mut cx);
    Env { buffer, editor, cx }
}

fn add_editor(
    workspace: &Entity<Workspace>,
    buffer: &Entity<Buffer>,
    cx: &mut VisualTestContext,
) -> Entity<Editor> {
    let buffer = buffer.clone();
    let editor = cx.new_window_entity(|window, cx| Editor::for_buffer(buffer, None, window, cx));
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    editor
}

impl Env {
    fn text(&mut self) -> String {
        self.buffer.read_with(&self.cx, |buffer, _| buffer.text())
    }

    fn put_cursor(&mut self, offset: usize) {
        self.editor.update_in(&mut self.cx, |editor, window, cx| {
            let at = MultiBufferOffset(offset);
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_ranges([at..at]);
            });
        });
        self.cx.run_until_parked();
    }

    /// Puts the cursor at the first `needle` in the text.
    fn put_cursor_at(&mut self, needle: &str) {
        let offset = self.text().find(needle).expect("needle in text");
        self.put_cursor(offset);
    }

    fn cursor(&mut self) -> usize {
        self.editor.update(&mut self.cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor
                .selections
                .newest::<MultiBufferOffset>(&snapshot)
                .head()
                .0
        })
    }

    fn run(&mut self, action: impl Action) {
        self.cx.dispatch_action(action);
        self.cx.run_until_parked();
    }

    fn undo(&mut self) {
        self.editor.update_in(&mut self.cx, |editor, window, cx| {
            editor.undo(&Undo, window, cx)
        });
        self.cx.run_until_parked();
    }

    /// The echo area's log, newest first.
    fn echoed(&mut self) -> Vec<String> {
        let strip = self
            .cx
            .update(|window, cx| minibuffer_for_window(window.window_handle(), cx))
            .expect("the window has a strip");
        strip.read_with(&self.cx, |strip, _| {
            strip
                .messages()
                .into_iter()
                .map(|line| line.text.to_string())
                .collect()
        })
    }

    fn last_echo(&mut self) -> Option<String> {
        self.echoed().into_iter().next()
    }

    fn in_table_context(&mut self) -> bool {
        let editor = self.editor.clone();
        let context = editor.update_in(&mut self.cx, |editor, window, cx| {
            editor.key_context(window, cx)
        });
        assert!(context.contains(crate::KEY_CONTEXT), "{context:?}");
        context.contains(TABLE_KEY_CONTEXT)
    }
}

#[gpui::test]
async fn a_moved_list_item_undoes_in_one_step(cx: &mut TestAppContext) {
    let text = "- one\n- two words\n  - nested\n- three\n";
    let mut env = setup(text, Mode::Source, cx).await;
    env.put_cursor_at("words");
    env.run(markdown::MoveBlockUp);
    assert_eq!(env.text(), "- two words\n  - nested\n- one\n- three\n");
    assert_eq!(
        env.cursor(),
        "- two ".len(),
        "point stays inside the moved item"
    );
    env.undo();
    assert_eq!(env.text(), text);
}

#[gpui::test]
async fn a_moved_heading_section_undoes_in_one_step(cx: &mut TestAppContext) {
    let text = "# Doc\n\n## A\n\nalpha\n\n## B\n\nbeta\n";
    let mut env = setup(text, Mode::Live, cx).await;
    env.put_cursor_at("# A");
    env.run(markdown::MoveBlockDown);
    assert_eq!(env.text(), "# Doc\n\n## B\n\nbeta\n\n## A\n\nalpha\n");
    let cursor = env.cursor();
    assert_eq!(&env.text()[cursor..cursor + 3], "# A");
    env.undo();
    assert_eq!(env.text(), text);
}

#[gpui::test]
async fn a_prefix_argument_moves_that_many_times_in_one_undo(cx: &mut TestAppContext) {
    let text = "- one\n- two\n- three\n- four\n";
    let mut env = setup(text, Mode::Source, cx).await;
    env.put_cursor_at("one");
    env.cx.simulate_keystrokes("ctrl-u 2 f2");
    env.cx.run_until_parked();
    assert_eq!(env.text(), "- two\n- three\n- one\n- four\n");
    env.undo();
    assert_eq!(env.text(), text);

    // More moves than siblings keep the ones made and say where they stopped.
    env.put_cursor_at("three");
    env.cx.simulate_keystrokes("ctrl-u 5 f2");
    env.cx.run_until_parked();
    assert_eq!(env.text(), "- one\n- two\n- four\n- three\n");
    assert_eq!(env.last_echo().as_deref(), Some(NO_BLOCK_BELOW));
    env.undo();
    assert_eq!(env.text(), text);
}

#[gpui::test]
async fn the_first_block_has_no_block_above(cx: &mut TestAppContext) {
    let text = "first\n\nsecond\n";
    let mut env = setup(text, Mode::Source, cx).await;
    env.put_cursor(0);
    env.run(markdown::MoveBlockUp);
    assert_eq!(env.text(), text);
    assert_eq!(env.last_echo().as_deref(), Some(NO_BLOCK_ABOVE));
}

#[gpui::test]
async fn insert_block_inserts_the_chosen_block_and_cancel_changes_nothing(cx: &mut TestAppContext) {
    let mut env = setup("intro\n", Mode::Source, cx).await;
    env.put_cursor_at("ro");
    env.run(markdown::InsertBlock);
    env.cx.simulate_input("table");
    env.cx.run_until_parked();
    env.run(minibuffer::Confirm);
    let expected = "intro\n\n| Column 1 | Column 2 |\n| --- | --- |\n|  |  |\n";
    assert_eq!(env.text(), expected);
    let point = "intro\n\n| Column 1 | Column 2 |\n| --- | --- |\n| ".len();
    assert_eq!(env.cursor(), point);
    env.undo();
    assert_eq!(env.text(), "intro\n", "one undo takes the block back");

    env.run(markdown::InsertBlock);
    env.cx.simulate_input("quote");
    env.cx.run_until_parked();
    env.run(minibuffer::Cancel);
    assert_eq!(env.text(), "intro\n", "C-g at the prompt changes nothing");
}

#[gpui::test]
async fn toggle_checkbox_flips_a_task_and_refuses_other_lines(cx: &mut TestAppContext) {
    let text = "- [ ] task\n- plain\n";
    let mut env = setup(text, Mode::Live, cx).await;
    env.put_cursor_at("task");
    env.run(markdown::ToggleCheckbox);
    assert_eq!(env.text(), "- [x] task\n- plain\n");
    env.run(markdown::ToggleCheckbox);
    assert_eq!(env.text(), text);
    env.undo();
    assert_eq!(
        env.text(),
        "- [x] task\n- plain\n",
        "each toggle is one undo"
    );

    env.put_cursor_at("plain");
    env.run(markdown::ToggleCheckbox);
    assert_eq!(env.text(), "- [x] task\n- plain\n");
    assert_eq!(env.last_echo().as_deref(), Some(NOT_A_TASK));
}

async fn check_table_cells(mode: Mode, cx: &mut TestAppContext) {
    let text = "intro\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n";
    let at = |needle: &str| text.find(needle).unwrap();
    let mut env = setup(text, mode, cx).await;
    env.put_cursor(0);
    assert!(!env.in_table_context(), "{mode:?}: off the table");
    env.put_cursor_at("a |");
    assert!(env.in_table_context(), "{mode:?}: on a table row");

    env.run(markdown::NextTableCell);
    assert_eq!(env.cursor(), at("b |"), "{mode:?}");
    env.run(markdown::NextTableCell);
    assert_eq!(
        env.cursor(),
        at("1 |"),
        "{mode:?}: the delimiter row is skipped"
    );
    env.run(markdown::NextTableCell);
    assert_eq!(env.cursor(), at("2 |"), "{mode:?}");
    env.run(markdown::NextTableCell);
    assert_eq!(env.cursor(), at("2 |"), "{mode:?}");
    assert_eq!(env.last_echo().as_deref(), Some(END_OF_TABLE), "{mode:?}");
    env.run(markdown::PreviousTableCell);
    assert_eq!(env.cursor(), at("1 |"), "{mode:?}");
    env.run(markdown::PreviousTableCell);
    assert_eq!(env.cursor(), at("b |"), "{mode:?}");
    assert_eq!(env.text(), text, "{mode:?}: cell moves edit nothing");
}

#[gpui::test]
async fn table_cell_moves_in_the_live_view(cx: &mut TestAppContext) {
    check_table_cells(Mode::Live, cx).await;
}

#[gpui::test]
async fn table_cell_moves_in_source(cx: &mut TestAppContext) {
    check_table_cells(Mode::Source, cx).await;
}

#[gpui::test]
async fn return_continues_tasks_and_numbers_and_ends_an_empty_item(cx: &mut TestAppContext) {
    let mut env = setup("- [x] done", Mode::Source, cx).await;
    let end = env.text().len();
    env.put_cursor(end);
    env.run(Newline);
    assert_eq!(env.text(), "- [x] done\n- [ ] ");
    // RET on the empty item ends the list.
    env.run(Newline);
    assert_eq!(env.text(), "- [x] done\n");

    env.editor.update_in(&mut env.cx, |editor, window, cx| {
        editor.set_text("3. three", window, cx)
    });
    env.put_cursor("3. three".len());
    env.run(Newline);
    assert_eq!(env.text(), "3. three\n4. ");
    env.run(Newline);
    assert_eq!(env.text(), "3. three\n");
}

#[gpui::test]
async fn commands_in_an_empty_buffer_edit_nothing(cx: &mut TestAppContext) {
    let mut env = setup("", Mode::Source, cx).await;
    for (name, echo) in [
        (markdown::MoveBlockUp::name_for_type(), NO_BLOCK_ABOVE),
        (markdown::MoveBlockDown::name_for_type(), NO_BLOCK_BELOW),
        (markdown::ToggleCheckbox::name_for_type(), NOT_A_TASK),
        (markdown::NextTableCell::name_for_type(), END_OF_TABLE),
        (markdown::PreviousTableCell::name_for_type(), END_OF_TABLE),
    ] {
        env.cx.update(|window, cx| {
            let action = cx.build_action(name, None).expect("a registered action");
            window.dispatch_action(action, cx);
        });
        env.cx.run_until_parked();
        assert_eq!(env.text(), "", "{name}");
        assert_eq!(env.last_echo().as_deref(), Some(echo), "{name}");
    }
    let dirty = env.buffer.read_with(&env.cx, |buffer, _| buffer.is_dirty());
    assert!(!dirty);
}

#[gpui::test]
async fn commands_outside_a_markdown_editor_say_so(cx: &mut TestAppContext) {
    let mut env = setup("- [ ] task\n", Mode::Source, cx).await;
    env.buffer.update(&mut env.cx, |buffer, cx| {
        buffer.set_language(None, cx);
    });
    env.cx.run_until_parked();
    env.put_cursor_at("task");
    env.run(markdown::ToggleCheckbox);
    assert_eq!(env.text(), "- [ ] task\n");
    assert_eq!(env.last_echo().as_deref(), Some(NOT_MARKDOWN));
}
