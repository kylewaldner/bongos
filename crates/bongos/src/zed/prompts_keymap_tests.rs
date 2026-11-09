//! The prompts key block of the Emacs base keymap (`Editor && prompts`), that it never reaches a
//! terminal, and that the mode attaches to a prompts file in a workspace with default settings.

use editor::Editor;
use gpui::{
    KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext, VisualContext as _,
    VisualTestContext,
};
use project::Project;
use settings::KeymapFile;
use workspace::MultiWorkspace;

use super::tests::{init_keymap_test, init_test};

const LINUX: (&str, &str) = ("keymaps/default-linux.json", "keymaps/linux/emacs.json");
const MACOS: (&str, &str) = ("keymaps/default-macos.json", "keymaps/macos/emacs.json");

/// Every key sequence of the D12 table and the action it runs.
const PROMPTS_KEYS: &[(&str, &str)] = &[
    ("ctrl-c ctrl-c", "prompts::Send"),
    ("ctrl-c ctrl-r", "prompts::SendRegion"),
    ("ctrl-c ctrl-n", "prompts::New"),
    ("ctrl-c ctrl-p", "prompts::Previous"),
    ("ctrl-c ctrl-f", "prompts::Next"),
    ("ctrl-c ctrl-j", "prompts::NextPending"),
    ("ctrl-c ctrl-w", "prompts::Copy"),
    ("ctrl-c ctrl-u", "prompts::UseFile"),
    ("ctrl-c ctrl-d", "prompts::UseDefault"),
    ("ctrl-c ctrl-a", "prompts::ToggleAutoSubmit"),
];

/// Commands that stay in the palette with no key.
const UNBOUND: &[&str] = &[
    "prompts::SendWithoutMarking",
    "prompts::Open",
    "prompts::Mode",
    "prompts::DumpView",
];

const PROMPTS_EDITOR: [&str; 4] = ["Workspace", "Pane", "Editor mode=full", "prompts"];
const PLAIN_EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const CHAT_TERMINAL: [&str; 5] = ["Workspace", "Dock", "ChatPanel", "ChatSession", "Terminal"];
const TERMINAL: [&str; 4] = ["Workspace", "Dock", "TerminalPanel", "Terminal"];

fn load(paths: &[&str], cx: &TestAppContext) -> (Keymap, Vec<KeyBinding>) {
    cx.update(|cx| {
        let mut all = Vec::new();
        for path in paths {
            all.extend(
                KeymapFile::load_asset(path, None, cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        (Keymap::new(all.clone()), all)
    })
}

/// The prompts editor's key context is one context: `Editor` with the `prompts` flag set.
fn stack(contexts: &[&str]) -> Vec<KeyContext> {
    let mut stack: Vec<KeyContext> = contexts
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect();
    // The `prompts` entry stands for a flag the editor adds to its own context.
    if let Some(last) = contexts.last().filter(|last| **last == "prompts") {
        stack.pop();
        let editor = stack.last_mut().expect("an editor context before prompts");
        editor.add(*last);
    }
    stack
}

fn keys(input: &str) -> Vec<Keystroke> {
    input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect()
}

/// The names of the actions a key resolves to, deepest first, and whether it waits for more keys.
fn resolve(keymap: &Keymap, input: &str, contexts: &[&str]) -> (Vec<&'static str>, bool) {
    let (bindings, pending) = keymap.bindings_for_input(&keys(input), &stack(contexts));
    (
        bindings
            .iter()
            .map(|binding| binding.action().name())
            .collect(),
        pending,
    )
}

#[gpui::test]
async fn emacs_binds_every_prompts_key_in_a_prompts_editor(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for (input, action) in PROMPTS_KEYS {
            let (names, pending) = resolve(&keymap, input, &PROMPTS_EDITOR);
            assert_eq!(names.first(), Some(action), "{emacs}: {input}");
            assert!(!pending, "{emacs}: {input}");
        }
        assert!(
            resolve(&keymap, "ctrl-c", &PROMPTS_EDITOR).1,
            "{emacs}: ctrl-c is a prefix"
        );
    }
}

#[gpui::test]
async fn prompts_keys_do_not_reach_an_editor_without_the_mode(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for (input, action) in PROMPTS_KEYS {
            assert_ne!(
                resolve(&keymap, input, &PLAIN_EDITOR).0.first(),
                Some(action),
                "{emacs}: {input}"
            );
        }
    }
}

#[gpui::test]
async fn prompts_keys_never_resolve_in_a_terminal(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for contexts in [&CHAT_TERMINAL[..], &TERMINAL[..]] {
            for (input, _) in PROMPTS_KEYS {
                for name in resolve(&keymap, input, contexts).0 {
                    assert!(
                        !name.starts_with("prompts::"),
                        "{emacs}: {input} in {contexts:?} runs {name}"
                    );
                }
            }
        }
        // Feature 04's interrupt is untouched.
        let (names, pending) = resolve(&keymap, "ctrl-c ctrl-c", &CHAT_TERMINAL);
        assert_eq!(names.first(), Some(&"terminal::SendKeystroke"), "{emacs}");
        assert!(!pending, "{emacs}");
    }
}

#[gpui::test]
async fn the_chat_keys_still_work_in_a_prompts_editor(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for (input, action) in [
            ("ctrl-c c e", "chat::FocusPrompts"),
            ("ctrl-c c n", "chat::New"),
            ("ctrl-c w w", "work::TogglePanel"),
        ] {
            let (names, pending) = resolve(&keymap, input, &PROMPTS_EDITOR);
            assert_eq!(names.first(), Some(&action), "{emacs}: {input}");
            assert!(!pending, "{emacs}: {input}");
        }
    }
}

/// Risk 7: a later `ctrl-c ctrl-<letter>` binding that matches a prompts editor must sit before
/// the prompts block, or it shadows it. Nothing but the prompts block matches one today.
#[gpui::test]
async fn no_other_ctrl_c_chord_matches_a_prompts_editor_after_the_block(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (_, bindings) = load(&[default, emacs], cx);
        let first_prompts = bindings
            .iter()
            .position(|binding| binding.action().name().starts_with("prompts::"))
            .expect("a prompts binding");
        let prompts_editor = stack(&PROMPTS_EDITOR);
        for binding in &bindings[first_prompts..] {
            let keystrokes = binding.keystrokes();
            let ctrl_c_chord = keystrokes.len() == 2
                && keystrokes[0].key() == "c"
                && keystrokes[0].modifiers().control
                && keystrokes[1].modifiers().control;
            let matches = binding
                .predicate()
                .is_none_or(|predicate| predicate.eval(&prompts_editor));
            let name = binding.action().name();
            assert!(
                !(ctrl_c_chord && matches && !name.starts_with("prompts::")),
                "{emacs}: {name} would shadow the prompts keys"
            );
        }
    }
}

#[gpui::test]
async fn emacs_leaves_the_other_prompts_commands_unbound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (_, bindings) = load(&[default, emacs], cx);
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!UNBOUND.contains(&name), "{emacs}: {name} is bound");
        }
        for (_, action) in PROMPTS_KEYS {
            assert!(
                bindings
                    .iter()
                    .any(|binding| binding.action().name() == *action),
                "{action}"
            );
        }
    }
}

#[gpui::test]
async fn default_keymaps_have_no_prompts_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, _) in [LINUX, MACOS] {
        let (keymap, bindings) = load(&[default], cx);
        for (input, action) in PROMPTS_KEYS {
            assert_ne!(
                resolve(&keymap, input, &PROMPTS_EDITOR).0.first(),
                Some(action),
                "{default}: {input}"
            );
        }
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!name.starts_with("prompts::"), "{default}: {name}");
        }
    }
}

/// A prompts file opened in a workspace with default settings (nothing sets `prompts.enabled`)
/// gets the mode, and its editor carries the `prompts` key context the block above matches.
#[gpui::test]
async fn the_mode_attaches_in_a_default_workspace(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    cx.update(bongos_prompts::init);
    app_state
        .fs
        .as_fake()
        .insert_tree(
            "/dir",
            serde_json::json!({
                "prompts.txt": "=== prompt\nfirst\n===\n",
                "notes.txt": "=== prompt\nfirst\n===\n",
            }),
        )
        .await;
    let project = Project::test(app_state.fs.clone(), ["/dir".as_ref()], cx).await;
    // The language that `main` registers, in the registry of this project.
    bongos_prompts::register_language(
        &project.read_with(cx, |project, _| project.languages().clone()),
    );
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let cx = &mut VisualTestContext::from_window(window.into(), cx);
    let open = |name: &str, cx: &mut VisualTestContext| {
        let buffer = project.update(cx, |project, cx| {
            project.open_local_buffer(format!("/dir/{name}"), cx)
        });
        cx.run_until_parked();
        buffer
    };
    let prompts = open("prompts.txt", cx).await.unwrap();
    let notes = open("notes.txt", cx).await.unwrap();
    let editors: Vec<_> = [prompts, notes]
        .into_iter()
        .map(|buffer| {
            cx.new_window_entity(|window, cx| {
                Editor::for_buffer(buffer, Some(project.clone()), window, cx)
            })
        })
        .collect();
    cx.run_until_parked();
    assert!(
        editors[0].read_with(cx, |editor, _| bongos_prompts::is_attached(editor)),
        "prompts.txt has the mode"
    );
    assert!(
        !editors[1].read_with(cx, |editor, _| bongos_prompts::is_attached(editor)),
        "notes.txt does not"
    );
    assert_eq!(bongos_prompts::KEY_CONTEXT, "prompts");
}
