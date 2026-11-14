//! The Work key table of the Emacs base keymap and the `ctrl-c` re-declarations that keep it
//! from changing what `ctrl-c` does elsewhere.

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::KeymapFile;

use super::tests::init_keymap_test;

const LINUX: (&str, &str) = ("keymaps/default-linux.json", "keymaps/linux/emacs.json");
const MACOS: (&str, &str) = ("keymaps/default-macos.json", "keymaps/macos/emacs.json");

/// Every key sequence of the D17 table and the action it runs.
const WORK_KEYS: &[(&str, &str)] = &[
    ("ctrl-c w w", "work::TogglePanel"),
    ("ctrl-c w G", "work::Graph"),
    ("ctrl-c w n", "task::New"),
    ("ctrl-c w N", "task::NewChild"),
    ("ctrl-c w o", "task::NewOneOff"),
    ("ctrl-c w s", "task::Start"),
    ("ctrl-c w a", "task::Advance"),
    ("ctrl-c w c", "task::Close"),
    ("ctrl-c w t", "task::Think"),
    ("ctrl-c w m", "task::Mode"),
    ("ctrl-c w f", "task::Focus"),
    ("ctrl-c w e", "task::AddEdge"),
    ("ctrl-c w E", "task::RemoveEdge"),
    ("ctrl-c w d a", "todo::Add"),
    ("ctrl-c w d k", "todo::Remove"),
    ("ctrl-c w d s", "todo::Skip"),
    ("ctrl-c w d r", "todo::Reopen"),
    ("ctrl-c w d m", "todo::Move"),
    ("ctrl-c w d w", "todo::SetWorkflow"),
    ("ctrl-c w d o", "todo::SetRole"),
    ("ctrl-c w d b", "todo::Start"),
    ("ctrl-c w p n", "project::New"),
    ("ctrl-c w P", "project::Focus"),
    ("ctrl-c w g e", "project::Edit"),
    ("ctrl-c w g w", "project::OpenWorkingCopy"),
    ("ctrl-c w g k", "project::Abandon"),
    ("ctrl-c w g o", "project::OpenRepository"),
    ("ctrl-c w g l", "project::Land"),
    ("ctrl-c w g c", "project::Check"),
    ("ctrl-c w g A", "project::Approve"),
    ("ctrl-c w h", "artifact::Hide"),
    ("ctrl-c w u", "artifact::Share"),
    ("ctrl-c w H", "artifact::HideAll"),
    ("ctrl-c w U", "artifact::ShareAll"),
    ("ctrl-c w l", "artifact::Link"),
];

/// Commands that stay in the palette with no key.
const UNBOUND: &[&str] = &[
    "work::FilterTodo",
    "work::FilterKind",
    "work::ShowStale",
    "work::ShowClosed",
    "work::AddRepository",
    "work::EditWorktreeRoot",
    "task::AdvanceOverride",
    "task::Drop",
    "task::Reopen",
    "task::Unfocus",
    "task::Split",
    "task::Edit",
    "task::Move",
    "task::MoveToProject",
    "task::MakeOneOff",
    "task::SetResult",
    "task::ClearResult",
    "task::Retarget",
    "task::ShowBlockers",
    "task::SetUnblockPoint",
    "task::SetMergeShip",
    "task::ClearRebase",
    "task::SetDefaultVisibility",
    "task::OpenWorktree",
    "task::OpenPr",
    "task::CopyPath",
    "project::Archive",
    "project::Unarchive",
    "project::OpenFolder",
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

fn workspace() -> [&'static str; 3] {
    ["Workspace", "Pane", "Editor mode=full"]
}

#[gpui::test]
async fn emacs_binds_every_work_key(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        for (input, action) in WORK_KEYS {
            let (names, pending) = resolve(&keymap, input, &["Workspace", "Pane"]);
            assert_eq!(names.first(), Some(action), "{emacs}: {input}");
            assert!(!pending, "{emacs}: {input}");
        }
        // Each prefix waits for the next key.
        for prefix in [
            "ctrl-c",
            "ctrl-c w",
            "ctrl-c w d",
            "ctrl-c w p",
            "ctrl-c w g",
        ] {
            assert!(
                resolve(&keymap, prefix, &["Workspace", "Pane"]).1,
                "{emacs}: {prefix}"
            );
        }
        // The Editor sits under the Workspace, and ctrl-c waits there too.
        let editor = workspace();
        assert!(resolve(&keymap, "ctrl-c", &editor).1, "{emacs}");
        assert_eq!(
            resolve(&keymap, "ctrl-c w n", &editor).0.first(),
            Some(&"task::New"),
            "{emacs}"
        );
    }
}

#[gpui::test]
async fn emacs_leaves_the_other_work_commands_unbound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (_, bindings) = load(&[default, emacs], cx);
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!UNBOUND.contains(&name), "{emacs}: {name} is bound");
        }
        for (_, action) in WORK_KEYS {
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
async fn emacs_keeps_ctrl_c_where_it_was_bound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        let (names, pending) = resolve(&keymap, "ctrl-c", &["Workspace", "Pane", "Terminal"]);
        assert_eq!(names.first(), Some(&"terminal::SendKeystroke"), "{emacs}");
        assert!(!pending, "{emacs}: terminal");

        for menu in [["Workspace", "menu"], ["Workspace", "Picker"]] {
            let (names, pending) = resolve(&keymap, "ctrl-c", &menu);
            assert_eq!(names.first(), Some(&"menu::Cancel"), "{emacs}: {menu:?}");
            assert!(!pending, "{emacs}: {menu:?}");
        }
    }

    let (keymap, _) = load(&[LINUX.0, LINUX.1], cx);
    for (contexts, action) in [
        (
            &["Workspace", "Dock", "ProjectPanel"][..],
            "project_panel::Copy",
        ),
        (&["Workspace", "Pane", "Markdown"][..], "markdown::Copy"),
        (
            &["Workspace", "AgentPanel", "Markdown"][..],
            "markdown::CopyAsMarkdown",
        ),
        (
            &["Workspace", "VariableList"][..],
            "variable_list::CopyVariableValue",
        ),
        (
            &["Workspace", "KeymapEditor"][..],
            "keymap_editor::CopyAction",
        ),
        (
            &["Workspace", "NotebookEditor"][..],
            "notebook::InterruptKernel",
        ),
    ] {
        let (names, pending) = resolve(&keymap, "ctrl-c", contexts);
        assert_eq!(names.first(), Some(&action), "{contexts:?}");
        assert!(!pending, "{contexts:?}");
    }
}

#[gpui::test]
async fn picker_query_editor_keeps_ctrl_c_pending(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    // The exact match is what ctrl-c did before this change (Copy on Linux, where macOS copies
    // with cmd-c and ctrl-c is the global cancel); the prefix still waits.
    for ((default, emacs), exact) in [(LINUX, "editor::Copy"), (MACOS, "menu::Cancel")] {
        let (keymap, _) = load(&[default, emacs], cx);
        let (names, pending) = resolve(
            &keymap,
            "ctrl-c",
            &["Workspace", "Picker", "Editor mode=full"],
        );
        assert!(pending, "{emacs}");
        assert_eq!(names.first(), Some(&exact), "{emacs}");
    }
}

#[gpui::test]
async fn work_panel_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        let panel = ["Workspace", "Dock", "WorkPanel"];
        let (names, pending) = resolve(&keymap, "tab", &panel);
        assert_eq!(names.first(), Some(&"work::ToggleRow"), "{emacs}");
        assert!(!pending);
        let (names, pending) = resolve(&keymap, "^", &panel);
        assert_eq!(names.first(), Some(&"work::Up"), "{emacs}");
        assert!(!pending);
        // Outside the panel they do nothing new.
        assert_ne!(
            resolve(&keymap, "tab", &["Workspace", "Pane"]).0.first(),
            Some(&"work::ToggleRow")
        );
    }
}

#[gpui::test]
async fn other_base_keymaps_have_no_work_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let bases: &[(&str, &[&str])] = &[
        (
            LINUX.0,
            &[
                "keymaps/linux/atom.json",
                "keymaps/linux/cursor.json",
                "keymaps/linux/jetbrains.json",
                "keymaps/linux/sublime_text.json",
            ],
        ),
        (
            MACOS.0,
            &[
                "keymaps/macos/atom.json",
                "keymaps/macos/cursor.json",
                "keymaps/macos/jetbrains.json",
                "keymaps/macos/sublime_text.json",
                "keymaps/macos/textmate.json",
            ],
        ),
    ];
    for (default, others) in bases {
        // The first key of each pair is the base; its set is the default keymap alone.
        let mut sets: Vec<Vec<&str>> = vec![vec![default]];
        sets.extend(others.iter().map(|other| vec![*default, *other]));
        for set in sets {
            let (keymap, bindings) = load(&set, cx);
            let contexts = ["Workspace", "Pane"];
            assert!(!resolve(&keymap, "ctrl-c", &contexts).1, "{set:?}");
            assert!(
                resolve(&keymap, "ctrl-c w", &contexts).0.is_empty(),
                "{set:?}"
            );
            for binding in &bindings {
                let name = binding.action().name();
                assert!(
                    !["work::", "todo::", "project::", "artifact::", "task::New"]
                        .iter()
                        .any(|prefix| name.starts_with(prefix)),
                    "{set:?}: {name}"
                );
            }
        }
    }
}
