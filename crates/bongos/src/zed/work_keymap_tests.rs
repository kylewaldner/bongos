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
    ("ctrl-c w c", "task::Complete"),
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
    ("ctrl-c w g h", "project::ShowHistory"),
    ("ctrl-c w g r", "project::Revert"),
    ("ctrl-c w g v", "project::ShowChanges"),
    ("ctrl-c w g s", "project::BackupStatus"),
    ("ctrl-c w g b", "project::BackupNow"),
    ("ctrl-c w g a", "project::AddExisting"),
    ("ctrl-c w g R", "project::Restore"),
    ("ctrl-c w g C", "project::Convert"),
    ("ctrl-c w g m", "task::MoveToProject"),
    ("ctrl-c w h", "artifact::Hide"),
    ("ctrl-c w u", "artifact::Share"),
    ("ctrl-c w H", "artifact::HideAll"),
    ("ctrl-c w U", "artifact::ShareAll"),
    ("ctrl-c w l", "artifact::Link"),
    ("ctrl-c w i", "task::ShowDetails"),
    ("ctrl-c w x s", "task::ShowChecks"),
    ("ctrl-c w x o", "task::ShowCheckOutput"),
    ("ctrl-c w x e", "task::EditChecks"),
    ("ctrl-c w x x", "task::RunChecks"),
    ("ctrl-c w x r", "task::RunCheck"),
    ("ctrl-c w x k", "task::CancelChecks"),
];

/// The checks actions that stay in the palette with no key.
const UNBOUND_CHECKS: &[&str] = &["task::RunChecksClean", "task::ShowCheckSources"];

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
    "project::ResolveSplit",
    "project::RestoreFrom",
    "project::ConvertAll",
];

fn load(paths: &[&str], cx: &TestAppContext) -> (Keymap, Vec<KeyBinding>) {
    cx.update(|cx| {
        let mut all = Vec::new();
        for path in paths {
            all.extend(
                KeymapFile::load_asset(path, None, cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
            // The panel keys live in the specific overrides, which every profile loads after
            // its base keymap.
            let overrides = match *path {
                "keymaps/linux/emacs.json" => Some("keymaps/specific-overrides.json"),
                "keymaps/macos/emacs.json" => Some("keymaps/specific-overrides-macos.json"),
                _ => None,
            };
            if let Some(overrides) = overrides {
                all.extend(
                    KeymapFile::load_asset(overrides, None, cx)
                        .unwrap_or_else(|error| panic!("{overrides}: {error}")),
                );
            }
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
            "ctrl-c w x",
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
async fn checks_keys_are_emacs_only_and_two_actions_stay_unbound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (emacs_keymap, all) = load(&[default, emacs], cx);
        let (default_keymap, _) = load(&[default], cx);
        let checks_keys: Vec<_> = WORK_KEYS
            .iter()
            .filter(|(input, _)| input.starts_with("ctrl-c w x "))
            .collect();
        assert_eq!(checks_keys.len(), 6, "{emacs}");
        for (input, action) in checks_keys {
            let (names, _) = resolve(&emacs_keymap, input, &["Workspace", "Pane"]);
            assert_eq!(names.first(), Some(action), "{emacs}: {input}");
            let (names, _) = resolve(&default_keymap, input, &["Workspace", "Pane"]);
            assert!(!names.contains(action), "{default}: {input}");
        }
        for binding in &all {
            let name = binding.action().name();
            assert!(!UNBOUND_CHECKS.contains(&name), "{emacs}: {name} is bound");
        }
    }
}

#[gpui::test]
async fn emacs_leaves_the_other_work_commands_unbound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (_, bindings) = load(&[default, emacs], cx);
        for binding in &bindings {
            let name = binding.action().name();
            // Resolve split is bound only inside the landing view; the next test checks that.
            if name == "project::ResolveSplit" {
                continue;
            }
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
async fn vscode_binds_the_work_keys_only_under_its_leader(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (macos, leader) in [(false, "ctrl-k b"), (true, "cmd-k b")] {
        let bindings = cx.update(|cx| {
            super::profile_key_bindings(settings::BaseKeymap::VSCode, false, macos, cx)
        });
        let keymap = Keymap::new(bindings.clone());
        let unparse = |keystrokes: Vec<String>| keystrokes.join(" ");
        for (input, action) in WORK_KEYS {
            let derived = format!("{leader}{}", input.strip_prefix("ctrl-c").unwrap());
            let expected = unparse(keys(&derived).iter().map(|key| key.unparse()).collect());
            // The work keys under VS Code are exactly the leader forms of the Emacs keys.
            let bound: Vec<String> = bindings
                .iter()
                .filter(|binding| binding.action().name() == *action)
                // Single-key bindings are panel keys from the overrides every profile loads.
                .filter(|binding| binding.keystrokes().len() > 1)
                .map(|binding| {
                    unparse(
                        binding
                            .keystrokes()
                            .iter()
                            .map(|key| key.inner().unparse())
                            .collect(),
                    )
                })
                .collect();
            assert_eq!(bound, vec![expected], "macos={macos}: {action}");
            let (names, pending) = resolve(&keymap, &derived, &workspace());
            assert_eq!(names.first(), Some(action), "macos={macos}: {derived}");
            assert!(!pending, "macos={macos}: {derived}");
        }
        for binding in &bindings {
            // The panel keys are in the overrides every profile loads.
            if binding.action().name() == "project::ResolveSplit" {
                continue;
            }
            assert!(
                !UNBOUND.contains(&binding.action().name()),
                "macos={macos}: {}",
                binding.action().name()
            );
        }
        assert!(
            resolve(&keymap, "ctrl-c w", &workspace()).0.is_empty(),
            "macos={macos}"
        );
    }
}

#[gpui::test]
async fn the_default_keymap_has_no_work_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let bases: &[(&str, &[&str])] = &[(LINUX.0, &[]), (MACOS.0, &[])];
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

/// The keys under `ctrl-c w g`, with the action each runs.
const REPOSITORY_KEYS: &[(&str, &str)] = &[
    ("o", "project::OpenRepository"),
    ("a", "project::AddExisting"),
    ("e", "project::Edit"),
    ("w", "project::OpenWorkingCopy"),
    ("l", "project::Land"),
    ("c", "project::Check"),
    ("v", "project::ShowChanges"),
    ("A", "project::Approve"),
    ("k", "project::Abandon"),
    ("h", "project::ShowHistory"),
    ("r", "project::Revert"),
    ("s", "project::BackupStatus"),
    ("b", "project::BackupNow"),
    ("R", "project::Restore"),
    ("C", "project::Convert"),
    ("m", "task::MoveToProject"),
];

#[gpui::test]
async fn repository_keys_sit_under_ctrl_c_w_g_and_the_graph_moved(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, bindings) = load(&[default, emacs], cx);
        let contexts = ["Workspace", "Pane"];
        for (key, action) in REPOSITORY_KEYS {
            let (names, pending) = resolve(&keymap, &format!("ctrl-c w g {key}"), &contexts);
            assert_eq!(names.first(), Some(action), "{emacs}: {key}");
            assert!(!pending, "{emacs}: {key}");
        }
        // The prefix only waits: nothing fires on `ctrl-c w g` alone, so nothing is shadowed.
        let (names, pending) = resolve(&keymap, "ctrl-c w g", &contexts);
        assert!(names.is_empty(), "{emacs}: {names:?}");
        assert!(pending, "{emacs}");
        let (names, pending) = resolve(&keymap, "ctrl-c w G", &contexts);
        assert_eq!(names.first(), Some(&"work::Graph"), "{emacs}");
        assert!(!pending, "{emacs}");
        // Every other binding under `ctrl-c w g` is one of the table.
        let under_prefix = bindings
            .iter()
            .filter(|binding| {
                let keystrokes = binding.keystrokes();
                keystrokes.len() == 4
                    && keystrokes[..3]
                        .iter()
                        .map(|k| k.unparse())
                        .eq(["ctrl-c", "w", "g"])
            })
            .count();
        assert_eq!(under_prefix, REPOSITORY_KEYS.len(), "{emacs}");
    }
}

#[gpui::test]
async fn resolve_split_is_bound_only_in_the_landing_view(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        let (names, pending) = resolve(&keymap, "r", &["Workspace", "LandingView"]);
        assert_eq!(names.first(), Some(&"project::ResolveSplit"), "{emacs}");
        assert!(!pending, "{emacs}");
        for contexts in [
            &["Workspace", "Pane"][..],
            &["Workspace", "Pane", "Editor mode=full"][..],
        ] {
            let (names, _) = resolve(&keymap, "r", contexts);
            assert!(
                !names.contains(&"project::ResolveSplit"),
                "{emacs}: {contexts:?}"
            );
        }
    }
}

#[gpui::test]
async fn task_detail_view_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, _) = load(&[default, emacs], cx);
        let view = ["Workspace", "Pane", "TaskDetailView"];
        for (input, action) in [
            ("g", "task_detail::Refresh"),
            ("s", "task_detail::ToggleSources"),
            ("x", "task::RunChecks"),
            ("r", "task::RunCheck"),
            ("k", "task::CancelChecks"),
            ("c", "task::Complete"),
        ] {
            let (names, pending) = resolve(&keymap, input, &view);
            assert_eq!(names.first(), Some(&action), "{emacs}: {input}");
            assert!(!pending, "{emacs}: {input}");
        }
        // Outside the view they bind nothing.
        assert_ne!(
            resolve(&keymap, "x", &["Workspace", "Pane"]).0.first(),
            Some(&"task::RunChecks")
        );
    }
}
