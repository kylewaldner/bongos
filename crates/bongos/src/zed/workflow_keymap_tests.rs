//! The workflow key table of the Emacs base keymap: bound in the Emacs base, absent from the
//! default keymaps, and the run view and runs panel keys.

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::KeymapFile;

use super::tests::init_keymap_test;

const LINUX: (&str, &str) = ("keymaps/default-linux.json", "keymaps/linux/emacs.json");
const MACOS: (&str, &str) = ("keymaps/default-macos.json", "keymaps/macos/emacs.json");

const WORKFLOW_KEYS: &[(&str, &str)] = &[
    ("ctrl-c f r", "workflow::Run"),
    ("ctrl-c f p", "workflow::Pause"),
    ("ctrl-c f h", "workflow::Halt"),
    ("ctrl-c f c", "workflow::Resume"),
    ("ctrl-c f t", "workflow::RetryStep"),
    ("ctrl-c f R", "workflow::Restart"),
    ("ctrl-c f k", "workflow::Cancel"),
    ("ctrl-c f a", "workflow::Approve"),
    ("ctrl-c f v", "workflow::Revise"),
    ("ctrl-c f l", "workflow::OpenLogs"),
    ("ctrl-c f w", "workflow::ShowRuns"),
    ("ctrl-c f s", "workflow::ShowRun"),
    ("ctrl-c f o", "workflow::OpenRunWorktree"),
    ("ctrl-c f d", "workflow::ShowRunDiff"),
    ("ctrl-c f x", "workflow::ClearFinished"),
    ("ctrl-c f V", "workflow::Validate"),
    ("ctrl-c f n", "workflow::New"),
    ("ctrl-c f e", "workflow::EditDefinition"),
];

/// Commands that stay in the palette with no key.
const UNBOUND: &[&str] = &[
    "workflow::RetryStepFresh",
    "workflow::CopyResumeCommand",
    "workflow::DumpRunView",
    "workflow::DumpEngine",
];

/// Keys of the `RunView` and `RunsPanel` contexts.
const VIEW_KEYS: &[(&str, &str)] = &[
    ("enter", "workflow::OpenLogs"),
    ("a", "workflow::Approve"),
    ("v", "workflow::Revise"),
    ("l", "workflow::OpenLogs"),
];

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

fn stack(contexts: &[&str]) -> Vec<KeyContext> {
    contexts
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect()
}

fn resolve(keymap: &Keymap, input: &str, contexts: &[&str]) -> (Vec<&'static str>, bool) {
    let keys: Vec<Keystroke> = input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect();
    let (bindings, pending) = keymap.bindings_for_input(&keys, &stack(contexts));
    (
        bindings
            .iter()
            .map(|binding| binding.action().name())
            .collect(),
        pending,
    )
}

#[gpui::test]
async fn emacs_binds_every_workflow_key(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, bindings) = load(&[default, emacs], cx);
        for (input, action) in WORKFLOW_KEYS {
            let (names, pending) = resolve(&keymap, input, &["Workspace", "Pane"]);
            assert_eq!(names.first(), Some(action), "{emacs}: {input}");
            assert!(!pending, "{emacs}: {input}");
        }
        assert!(
            resolve(&keymap, "ctrl-c f", &["Workspace", "Pane"]).1,
            "{emacs}"
        );
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!UNBOUND.contains(&name), "{emacs}: {name} is bound");
        }
    }
}

#[gpui::test]
async fn default_base_has_no_workflow_binding(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, _) in [LINUX, MACOS] {
        let (keymap, bindings) = load(&[default], cx);
        for binding in &bindings {
            let name = binding.action().name();
            assert!(
                !name.starts_with("workflow::"),
                "{default}: {name} is bound"
            );
        }
        for (input, _) in WORKFLOW_KEYS {
            let (names, _) = resolve(&keymap, input, &["Workspace", "Pane"]);
            assert!(
                names.iter().all(|name| !name.starts_with("workflow::")),
                "{default}: {input}"
            );
        }
    }
}

#[gpui::test]
async fn run_view_and_runs_panel_keys_resolve(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for context in ["RunView", "RunsPanel"] {
            for (input, action) in VIEW_KEYS {
                let (names, _) = resolve(&keymap, input, &["Workspace", context]);
                assert_eq!(names.first(), Some(action), "{emacs}: {context} {input}");
            }
        }
        let (names, _) = resolve(&keymap, "g", &["Workspace", "RunView"]);
        assert_eq!(names.first(), Some(&"workflow::ToggleGraph"), "{emacs}");
    }
}
