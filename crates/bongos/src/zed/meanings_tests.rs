//! What the prefix argument does with the commands of the other features, in an editor with the
//! app keymap loaded: every declared name is a real action, a redirect runs only its target, a
//! count field reads the argument (a negative one swaps to the opposite action), and the starters
//! of a search or a replace clear it and run once.

use std::any::{Any, TypeId};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use bongos_emacs::actions::UniversalArgument;
use bongos_emacs::meanings::{COUNT_FIELDS, IGNORED, REDIRECTS};
use bongos_emacs::prefix_arg;
use editor::{Addon, Editor};
use gpui::{
    Action, DispatchPhase, DummyKeyboardMapper, Entity, Focusable as _, KeyBinding,
    KeyBindingContextPredicate, TestAppContext, VisualTestContext,
};
use settings::{KeybindSource, KeymapFile};

use super::tests::init_keymap_test;

/// The test-only key of the action at `index` of the list of every declared name.
fn stand_in_key(index: usize) -> String {
    format!("f{}", index + 1)
}

/// Every declared name, in the order their stand-in keys are numbered.
fn all_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    for (name, target) in REDIRECTS {
        names.push(name);
        names.push(target);
    }
    for (name, _) in COUNT_FIELDS {
        names.push(name);
    }
    names.extend(IGNORED);
    names.sort_unstable();
    names.dedup();
    names
}

fn key_of(name: &str) -> String {
    let index = all_names().iter().position(|n| *n == name).unwrap();
    stand_in_key(index)
}

/// Adds the `QueryReplaceBatch` key context, as an editor running a batch has.
struct BatchContext;

impl Addon for BatchContext {
    fn extend_key_context(&self, context: &mut gpui::KeyContext, _: &gpui::App) {
        context.add("QueryReplaceBatch");
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

/// An editor in a window with the Linux app keymap (with its Emacs base) loaded, `C-u` bound to
/// the universal argument and a stand-in key to every declared name.
async fn editor_with_keymap(cx: &mut TestAppContext) -> (Entity<Editor>, &mut VisualTestContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        editor::init(cx);
        bongos_emacs::init(cx);
        let mut all = Vec::new();
        for (path, source) in [
            ("keymaps/default-linux.json", KeybindSource::Default),
            ("keymaps/linux/emacs.json", KeybindSource::Base),
            ("keymaps/specific-overrides.json", KeybindSource::Default),
        ] {
            all.extend(
                KeymapFile::load_asset(path, Some(source), cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        cx.bind_keys(all);
        let mut own = vec![KeyBinding::new("ctrl-u", UniversalArgument, Some("Editor"))];
        for name in all_names() {
            let action = cx
                .build_action(name, None)
                .unwrap_or_else(|_| panic!("{name} is registered"));
            own.push(
                KeyBinding::load(
                    &key_of(name),
                    action,
                    Some(KeyBindingContextPredicate::parse("Editor").unwrap().into()),
                    false,
                    None,
                    &DummyKeyboardMapper,
                )
                .unwrap(),
            );
        }
        cx.bind_keys(own);
    });
    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text("one two three", window, cx);
        editor
    });
    let editor = window.root(cx).unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    (editor, cx)
}

/// Counts the runs of the action called `name` in `editor`.
fn runs(name: &str, editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Rc<Cell<usize>> {
    let count = Rc::new(Cell::new(0));
    let type_id: TypeId = cx.update(|_, cx| Any::type_id(&*cx.build_action(name, None).unwrap()));
    let counter = count.clone();
    editor.update(cx, |editor, _| {
        editor
            .register_action_renderer(move |_, window, _| {
                let counter = counter.clone();
                window.on_action(type_id, move |_, phase, _, _| {
                    if phase == DispatchPhase::Bubble {
                        counter.set(counter.get() + 1);
                    }
                });
            })
            .detach();
    });
    redraw(editor, cx);
    count
}

/// Draws the window again, so what was just registered on `editor` is on its dispatch path.
fn redraw(editor: &Entity<Editor>, cx: &mut VisualTestContext) {
    editor.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

const ABORT: &str = "query_replace::Abort";

/// Makes the abort clear the argument the way the real one does, through the quit hooks, and
/// counts its runs.
fn abort_runs(editor: &Entity<Editor>, cx: &mut VisualTestContext) -> Rc<Cell<usize>> {
    let count = Rc::new(Cell::new(0));
    let counter = count.clone();
    editor.update(cx, |editor, _| {
        editor
            .register_action(
                move |_: &bongos_query_replace::actions::Abort, window, cx| {
                    counter.set(counter.get() + 1);
                    bongos_emacs::quit::run_hooks_once(window, cx);
                },
            )
            .detach();
    });
    redraw(editor, cx);
    count
}

fn pending(cx: &mut VisualTestContext) -> bool {
    cx.update(|window, cx| prefix_arg::pending(window, cx).is_some())
}

#[gpui::test]
async fn every_declared_name_is_a_registered_action(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        for name in all_names() {
            assert!(
                cx.build_action(name, None).is_ok(),
                "{name} is not an action"
            );
        }
        for (_, opposite) in COUNT_FIELDS {
            assert!(cx.build_action(opposite, None).is_ok(), "{opposite}");
        }
    });
}

#[gpui::test]
async fn the_declarations_are_made_when_the_emacs_commands_start(cx: &mut TestAppContext) {
    use bongos_emacs::prefix_arg::{Meaning, meaning};

    init_keymap_test(cx);
    cx.update(|cx| {
        bongos_emacs::init(cx);
        for (name, target) in REDIRECTS {
            assert!(
                matches!(meaning(cx, name), Some(Meaning::Redirect(t)) if t == target),
                "{name}"
            );
        }
        for (name, opposite) in COUNT_FIELDS {
            assert!(
                matches!(meaning(cx, name), Some(Meaning::CountFieldSwap(o)) if o == opposite),
                "{name}"
            );
        }
        for name in IGNORED {
            assert!(
                matches!(meaning(cx, name), Some(Meaning::Ignored)),
                "{name}"
            );
        }
    });
}

#[gpui::test]
async fn an_argument_redirects_to_the_stand_in(cx: &mut TestAppContext) {
    let (editor, cx) = editor_with_keymap(cx).await;
    for (name, target) in REDIRECTS {
        let plain = runs(name, &editor, cx);
        let stand_in = runs(target, &editor, cx);
        cx.simulate_keystrokes(&format!("ctrl-u {}", key_of(name)));
        cx.run_until_parked();
        assert_eq!(stand_in.get(), 1, "{name}");
        assert_eq!(plain.get(), 0, "{name}");
        assert!(!pending(cx), "{name}");
        // Without an argument the key runs the plain action.
        cx.simulate_keystrokes(&key_of(name));
        assert_eq!(plain.get(), 1, "{name}");
        assert_eq!(stand_in.get(), 1, "{name}");
    }
}

#[gpui::test]
async fn any_argument_redirects(cx: &mut TestAppContext) {
    let (editor, cx) = editor_with_keymap(cx).await;
    let plain = runs("task::Advance", &editor, cx);
    let stand_in = runs("task::AdvanceOverride", &editor, cx);
    cx.simulate_keystrokes(&format!("ctrl-u 5 {}", key_of("task::Advance")));
    cx.simulate_keystrokes(&format!("ctrl-u - {}", key_of("task::Advance")));
    assert_eq!((stand_in.get(), plain.get()), (2, 0));
}

#[gpui::test]
async fn the_starters_run_once_and_clear_the_argument(cx: &mut TestAppContext) {
    let (editor, cx) = editor_with_keymap(cx).await;
    for name in IGNORED.into_iter().filter(|name| *name != ABORT) {
        let count = runs(name, &editor, cx);
        cx.simulate_keystrokes(&format!("ctrl-u {}", key_of(name)));
        cx.run_until_parked();
        assert_eq!(count.get(), 1, "{name}");
        assert!(!pending(cx), "{name}");
    }
}

#[gpui::test]
async fn abort_runs_once_and_clears_the_argument(cx: &mut TestAppContext) {
    let (editor, cx) = editor_with_keymap(cx).await;
    let aborts = abort_runs(&editor, cx);
    cx.simulate_keystrokes(&format!("ctrl-u {}", key_of(ABORT)));
    cx.run_until_parked();
    assert_eq!(aborts.get(), 1);
    assert!(!pending(cx));
}

#[gpui::test]
async fn the_app_keys_of_the_starters_run_once_and_clear_the_argument(cx: &mut TestAppContext) {
    let (editor, cx) = editor_with_keymap(cx).await;
    for (keys, name) in [
        ("ctrl-s", "isearch::Forward"),
        ("ctrl-r", "isearch::Backward"),
        ("ctrl-alt-s", "isearch::ForwardRegexp"),
        ("ctrl-alt-r", "isearch::BackwardRegexp"),
        ("alt-%", "query_replace::QueryReplace"),
        ("ctrl-alt-%", "query_replace::QueryReplaceRegexp"),
    ] {
        let count = runs(name, &editor, cx);
        cx.simulate_keystrokes(&format!("ctrl-u {keys}"));
        cx.run_until_parked();
        assert_eq!(count.get(), 1, "{keys}");
        assert!(!pending(cx), "{keys}");
    }
}

#[gpui::test]
async fn the_prompt_moves_read_the_count(cx: &mut TestAppContext) {
    use bongos_prompts::prompts::{Next, Previous};

    let (editor, cx) = editor_with_keymap(cx).await;
    let nexts = Rc::new(RefCell::new(Vec::<Option<u32>>::new()));
    let previous = Rc::new(RefCell::new(Vec::<Option<u32>>::new()));
    let (n, p) = (nexts.clone(), previous.clone());
    editor.update(cx, |editor, _| {
        editor
            .register_action(move |action: &Next, _, _| n.borrow_mut().push(action.count))
            .detach();
        editor
            .register_action(move |action: &Previous, _, _| p.borrow_mut().push(action.count))
            .detach();
    });
    let next = key_of(Next::name_for_type());
    let back = key_of(Previous::name_for_type());
    cx.simulate_keystrokes(&format!("ctrl-u 2 {next}"));
    assert_eq!(*nexts.borrow(), vec![Some(2)]);
    cx.simulate_keystrokes(&format!("ctrl-u 3 {back}"));
    assert_eq!(*previous.borrow(), vec![Some(3)]);
    // A negative argument swaps to the other action.
    cx.simulate_keystrokes(&format!("ctrl-u - {back}"));
    assert_eq!(*nexts.borrow(), vec![Some(2), Some(4)]);
    cx.simulate_keystrokes(&format!("ctrl-u - 2 {next}"));
    assert_eq!(*previous.borrow(), vec![Some(3), Some(2)]);
    // Without an argument the key runs as it is.
    cx.simulate_keystrokes(&next);
    assert_eq!(*nexts.borrow(), vec![Some(2), Some(4), None]);
    assert!(!pending(cx));
}

#[gpui::test]
async fn quit_in_a_running_batch_clears_the_argument_and_aborts_once(cx: &mut TestAppContext) {
    let (editor, cx) = editor_with_keymap(cx).await;
    editor.update(cx, |editor, _| editor.register_addon(BatchContext));
    redraw(&editor, cx);
    let aborts = abort_runs(&editor, cx);
    cx.simulate_keystrokes("ctrl-u ctrl-g");
    cx.run_until_parked();
    assert_eq!(aborts.get(), 1);
    assert!(!pending(cx));
}
