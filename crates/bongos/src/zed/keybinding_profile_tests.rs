//! The keymap profile loader: what each base keymap binds on each platform, that `ctrl-g` and
//! `escape` leave prompts under every profile, and that the user's `keymap.json` is never written
//! and always wins.

use gpui::{BorrowAppContext, KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::{
    BaseKeymap, BaseKeymapContent, KeybindSource, KeymapFile, KeymapFileLoadResult, KeymapPart,
};
use std::path::PathBuf;

use super::tests::init_keymap_test;

pub(super) const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
pub(super) const WORKSPACE: [&str; 1] = ["Workspace"];
const PICKER: [&str; 3] = ["Workspace", "Picker", "Editor mode=single_line"];
const MINIBUFFER: [&str; 4] = ["Workspace", "Minibuffer", "Picker", "Editor mode=full"];
pub(super) const PANE_TERMINAL: [&str; 3] = ["Workspace", "Pane", "Terminal"];
pub(super) const TERMINAL: [&str; 4] = ["Workspace", "Dock", "TerminalPanel", "Terminal"];
const CHAT_TERMINAL: [&str; 4] = ["Workspace", "Dock", "ChatPanel", "Terminal"];
pub(super) const PROMPTS_EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full prompts"];

/// The bindings a profile loads, in load order, with the sources the app loads them with.
pub(super) fn load_profile(
    base: BaseKeymap,
    vim_enabled: bool,
    macos: bool,
    cx: &TestAppContext,
) -> Vec<KeyBinding> {
    cx.update(|cx| super::profile_key_bindings(base, vim_enabled, macos, cx))
}

pub(super) fn stack(contexts: &[&str]) -> Vec<KeyContext> {
    contexts
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect()
}

/// The names of the actions a key resolves to, deepest first, and whether it waits for more keys.
pub(super) fn resolve(
    keymap: &Keymap,
    input: &str,
    contexts: &[&str],
) -> (Vec<&'static str>, bool) {
    let keystrokes: Vec<Keystroke> = input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect();
    let (bindings, pending) = keymap.bindings_for_input(&keystrokes, &stack(contexts));
    (
        bindings
            .iter()
            .map(|binding| binding.action().name())
            .collect(),
        pending,
    )
}

pub(super) fn first(keymap: &Keymap, input: &str, contexts: &[&str]) -> Option<&'static str> {
    resolve(keymap, input, contexts).0.first().copied()
}

fn user_bindings(json: &str, cx: &TestAppContext) -> Vec<KeyBinding> {
    cx.update(|cx| match KeymapFile::load(json, cx) {
        KeymapFileLoadResult::Success { key_bindings } => key_bindings
            .into_iter()
            .map(|mut binding| {
                binding.set_meta(KeybindSource::User.meta());
                binding
            })
            .collect(),
        _ => panic!("the user keymap does not load"),
    })
}

#[gpui::test]
async fn requirements_9_and_10_resolve_under_vscode(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (macos, modifier) in [(false, "ctrl"), (true, "cmd")] {
        let keymap = Keymap::new(load_profile(BaseKeymap::VSCode, false, macos, cx));
        let label = format!("macos={macos}");
        for (input, contexts, action) in [
            (
                format!("{modifier}-p"),
                &WORKSPACE[..],
                "file_finder::Toggle",
            ),
            (
                format!("{modifier}-shift-p"),
                &WORKSPACE[..],
                "command_palette::Toggle",
            ),
            (format!("{modifier}-d"), &EDITOR[..], "editor::SelectNext"),
            ("alt-up".to_string(), &EDITOR[..], "editor::MoveLineUp"),
            ("alt-down".to_string(), &EDITOR[..], "editor::MoveLineDown"),
            (
                format!("{modifier}-/"),
                &EDITOR[..],
                "editor::ToggleComments",
            ),
        ] {
            assert_eq!(
                first(&keymap, &input, contexts),
                Some(action),
                "{label}: {input}"
            );
        }
    }
}

#[gpui::test]
async fn ctrl_g_and_escape_leave_prompts_under_every_profile(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        for vim_enabled in [false, true] {
            for base in [BaseKeymap::Emacs, BaseKeymap::VSCode, BaseKeymap::None] {
                let keymap = Keymap::new(load_profile(base, vim_enabled, macos, cx));
                let label = format!("{base:?} vim={vim_enabled} macos={macos}");
                for input in ["ctrl-g", "escape"] {
                    // A minibuffer session keeps its own keys, even with no base keymap.
                    let (names, pending) = resolve(&keymap, input, &MINIBUFFER);
                    assert_eq!(
                        names.first(),
                        Some(&"minibuffer::Cancel"),
                        "{label}: {input}"
                    );
                    assert!(!pending, "{label}: {input}");
                    // A centered picker is closed by the default and base keymaps, which `None`
                    // does not load.
                    if base != BaseKeymap::None {
                        // Emacs's `ctrl-g` in an editor propagates to `menu::Cancel` when there
                        // is nothing to cancel, so the picker's cancel only has to be reachable.
                        assert!(
                            resolve(&keymap, input, &PICKER).0.contains(&"menu::Cancel"),
                            "{label}: {input}"
                        );
                    }
                }
            }
        }
    }
}

#[gpui::test]
async fn a_user_binding_on_ctrl_p_wins_under_each_profile(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    // The profile binds `ctrl-p` in `Editor` under Emacs and in `Workspace` under VS Code.
    let user = user_bindings(
        r#"[
            {"context": "Editor", "bindings": {"ctrl-p": "menu::Confirm"}},
            {"context": "Workspace", "bindings": {"ctrl-p": "menu::Confirm"}}
        ]"#,
        cx,
    );
    for macos in [false, true] {
        // Under VS Code, `ctrl-p` is the file finder in `Workspace` on Linux and `editor::MoveUp`
        // in `Editor` on macOS.
        let (vscode_contexts, vscode_profile) = if macos {
            (&EDITOR[..], Some("editor::MoveUp"))
        } else {
            (&WORKSPACE[..], Some("file_finder::Toggle"))
        };
        for (base, contexts, profile) in [
            (BaseKeymap::Emacs, &EDITOR[..], Some("editor::MoveUp")),
            (BaseKeymap::VSCode, vscode_contexts, vscode_profile),
            (BaseKeymap::None, &WORKSPACE[..], None),
        ] {
            let mut all = load_profile(base, false, macos, cx);
            let profile_only = Keymap::new(all.clone());
            all.extend(user.clone());
            let keymap = Keymap::new(all);
            // `ctrl-p` is `cmd-p` for the VS Code file finder on macOS; the user block is on
            // `ctrl-p` either way, and Emacs binds `ctrl-p` on both platforms.
            assert_eq!(
                first(&keymap, "ctrl-p", contexts),
                Some("menu::Confirm"),
                "{base:?} macos={macos}"
            );
            if base != BaseKeymap::None {
                assert_eq!(
                    first(&profile_only, "ctrl-p", contexts),
                    profile,
                    "{base:?} macos={macos} profile only"
                );
            }
        }
    }
}

#[gpui::test]
async fn switching_profiles_never_writes_the_user_keymap(cx: &mut TestAppContext) {
    let executor = cx.executor();
    let app_state = init_keymap_test(cx);
    let user_json = r#"[
        {"context": "Editor", "bindings": {"ctrl-p": "menu::Confirm"}},
        {"context": "Workspace", "bindings": {"ctrl-p": "menu::Confirm"}}
    ]"#;
    app_state
        .fs
        .save(
            paths::settings_file(),
            &r#"{"base_keymap": "Emacs"}"#.into(),
            Default::default(),
        )
        .await
        .unwrap();
    app_state
        .fs
        .save(
            "/keymap.json".as_ref(),
            &user_json.into(),
            Default::default(),
        )
        .await
        .unwrap();
    executor.run_until_parked();
    cx.update(|cx| {
        let (keymap_rx, keymap_watcher) = settings::watch_config_file(
            &executor,
            app_state.fs.clone(),
            PathBuf::from("/keymap.json"),
        );
        super::watch_settings_files(app_state.fs.clone(), cx);
        super::handle_keymap_file_changes(keymap_rx, keymap_watcher, cx);
    });
    executor.run_until_parked();

    let live_first = |cx: &mut TestAppContext, contexts: &[&str]| -> Option<&'static str> {
        cx.update(|cx| {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let (bindings, _) =
                keymap.bindings_for_input(&[Keystroke::parse("ctrl-p").unwrap()], &stack(contexts));
            bindings.first().map(|binding| binding.action().name())
        })
    };

    for (name, contexts) in [
        ("Emacs", &EDITOR[..]),
        ("VSCode", &WORKSPACE[..]),
        ("None", &WORKSPACE[..]),
        ("Emacs", &EDITOR[..]),
    ] {
        app_state
            .fs
            .save(
                paths::settings_file(),
                &format!(r#"{{"base_keymap": "{name}"}}"#).into(),
                Default::default(),
            )
            .await
            .unwrap();
        executor.run_until_parked();
        assert_eq!(live_first(cx, contexts), Some("menu::Confirm"), "{name}");
        let bytes = app_state.fs.load("/keymap.json".as_ref()).await.unwrap();
        assert_eq!(bytes, user_json, "{name}");
    }

    // A fresh load of the same files.
    cx.update(|cx| {
        let user = match KeymapFile::load(user_json, cx) {
            KeymapFileLoadResult::Success { key_bindings } => key_bindings,
            _ => panic!("the user keymap does not load"),
        };
        super::reload_keymaps(cx, user);
    });
    assert_eq!(live_first(cx, &EDITOR), Some("menu::Confirm"));
    assert_eq!(
        app_state.fs.load("/keymap.json".as_ref()).await.unwrap(),
        user_json
    );
}

#[gpui::test]
async fn the_linux_terminal_forwards_ctrl_k_and_alt_1_under_vscode(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let keymap = Keymap::new(load_profile(BaseKeymap::VSCode, false, false, cx));
    for contexts in [&TERMINAL[..], &PANE_TERMINAL[..]] {
        let (names, pending) = resolve(&keymap, "ctrl-k", contexts);
        assert_eq!(
            names.first(),
            Some(&"terminal::SendKeystroke"),
            "{contexts:?}"
        );
        assert!(!pending, "{contexts:?}");
        assert_eq!(
            first(&keymap, "alt-1", contexts),
            Some("workspace::ActivatePane"),
            "{contexts:?}"
        );
    }
    // Outside a terminal `ctrl-k` still starts the Workspace chords.
    assert!(resolve(&keymap, "ctrl-k", &WORKSPACE).1);
}

#[gpui::test]
async fn vscode_loads_vim_json_when_vim_or_helix_is_on(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let vim = |vim_enabled| {
        settings::profile_keymap(BaseKeymap::VSCode, vim_enabled)
            .iter()
            .any(|part| {
                matches!(part, KeymapPart::Asset { path, .. } if *path == settings::VIM_KEYMAP_PATH)
            })
    };
    assert!(vim(true));
    assert!(!vim(false));
}

#[gpui::test]
async fn load_default_keymap_binds_vim_json_for_vim_or_helix_mode_under_vscode(
    cx: &mut TestAppContext,
) {
    init_keymap_test(cx);
    // vim.json adds bindings no other part of the profile has, so the total grows.
    let count = |cx: &mut TestAppContext, settings_json: &str| -> usize {
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(settings_json, cx)
                    .expect("settings parse");
            });
            cx.clear_key_bindings();
            super::load_default_keymap(cx);
            let keymap = cx.key_bindings();
            let bindings = keymap.borrow().bindings().count();
            bindings
        })
    };
    let off = count(cx, r#"{"base_keymap": "VSCode"}"#);
    let vim = count(cx, r#"{"base_keymap": "VSCode", "vim_mode": true}"#);
    let helix = count(cx, r#"{"base_keymap": "VSCode", "helix_mode": true}"#);
    assert!(vim > off, "vim_mode adds vim.json: {vim} vs {off}");
    assert!(helix > off, "helix_mode adds vim.json: {helix} vs {off}");
}

#[test]
fn the_lowercase_aliases_parse_and_written_values_stay() {
    for (alias, value, written) in [
        ("emacs", BaseKeymapContent::Emacs, "Emacs"),
        ("vim", BaseKeymapContent::Vim, "Vim"),
        ("vscode", BaseKeymapContent::VSCode, "VSCode"),
        ("none", BaseKeymapContent::None, "None"),
    ] {
        assert_eq!(
            serde_json::from_str::<BaseKeymapContent>(&format!("\"{alias}\"")).unwrap(),
            value
        );
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            format!("\"{written}\"")
        );
    }
}

/// The keys of a binding, one string per keystroke.
pub(super) fn keys_of(binding: &KeyBinding) -> Vec<String> {
    binding
        .keystrokes()
        .iter()
        .map(|keystroke| keystroke.inner().unparse())
        .collect()
}

pub(super) fn context_of(binding: &KeyBinding) -> String {
    binding
        .predicate()
        .map(|predicate| predicate.to_string())
        .unwrap_or_default()
}

/// The VS Code leader's keys, spelled the way `keys_of` spells them (`cmd` prints as the
/// platform's own name).
fn leader_keys(macos: bool) -> Vec<String> {
    let leader = if macos { "cmd-k b" } else { "ctrl-k b" };
    leader
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap().unparse())
        .collect()
}

/// The bongos command rows among some bindings of an Emacs keymap: two or more keys starting with
/// `ctrl-c`, an action that is not `null`, `emacs::` or `terminal::`, in a context without
/// `Terminal`.
pub(super) fn command_rows(bindings: Vec<KeyBinding>) -> Vec<KeyBinding> {
    bindings
        .into_iter()
        .filter(|binding| {
            let keys = keys_of(binding);
            let name = binding.action().name();
            keys.len() >= 2
                && keys[0] == "ctrl-c"
                && !gpui::is_no_action(binding.action())
                && !name.starts_with("emacs::")
                && !name.starts_with("terminal::")
                && !context_of(binding).contains("Terminal")
        })
        .collect()
}

/// The bongos command rows of a platform's Emacs keymap.
fn emacs_command_rows(macos: bool, cx: &TestAppContext) -> Vec<KeyBinding> {
    let path = if macos {
        "keymaps/macos/emacs.json"
    } else {
        "keymaps/linux/emacs.json"
    };
    command_rows(
        cx.update(|cx| KeymapFile::load_asset(path, Some(KeybindSource::Base), cx).unwrap()),
    )
}

/// The derived leader part of the VS Code profile, loaded the way the loader loads it.
fn vscode_leader_rows(macos: bool, cx: &TestAppContext) -> Vec<KeyBinding> {
    let parts = settings::profile_keymap_for_platform(BaseKeymap::VSCode, false, macos);
    let leaders: Vec<_> = parts
        .iter()
        .filter(|part| matches!(part, KeymapPart::Leader { .. }))
        .collect();
    assert_eq!(leaders.len(), 1, "macos={macos}");
    cx.update(|cx| leaders[0].load(cx).unwrap())
}

/// The context stacks a derived row is checked in.
fn stacks_for(context: &str) -> Vec<&'static [&'static str]> {
    match context {
        "Workspace" => vec![&WORKSPACE[..], &EDITOR[..]],
        "Editor && prompts" => vec![&PROMPTS_EDITOR[..]],
        other => panic!("no context stack for derived rows in `{other}`"),
    }
}

#[gpui::test]
async fn every_emacs_command_row_has_exactly_one_vscode_leader_row(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let rows = emacs_command_rows(macos, cx);
        let derived = vscode_leader_rows(macos, cx);
        assert!(!rows.is_empty(), "macos={macos}");
        assert_eq!(derived.len(), rows.len(), "macos={macos}");
        for row in &rows {
            let mut expected_keys = leader_keys(macos);
            expected_keys.extend(keys_of(row).into_iter().skip(1));
            let matching = derived
                .iter()
                .filter(|binding| {
                    keys_of(binding) == expected_keys
                        && context_of(binding) == context_of(row)
                        && binding.action().partial_eq(row.action())
                })
                .count();
            assert_eq!(
                matching,
                1,
                "macos={macos}: {:?} in {}",
                keys_of(row),
                context_of(row)
            );
        }
        for binding in &derived {
            assert_eq!(
                binding.meta(),
                Some(KeybindSource::Base.meta()),
                "macos={macos}"
            );
            let name = binding.action().name();
            assert!(
                !name.starts_with("emacs::") && !name.starts_with("terminal::"),
                "macos={macos}: {name}"
            );
            assert!(
                !context_of(binding).contains("Terminal"),
                "macos={macos}: {}",
                context_of(binding)
            );
        }
    }
}

#[gpui::test]
async fn the_leader_reaches_work_chat_workflow_and_prompts_under_vscode(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let keymap = Keymap::new(load_profile(BaseKeymap::VSCode, false, macos, cx));
        let leader = leader_keys(macos).join(" ");
        for (rest, contexts, action) in [
            ("w w", &WORKSPACE[..], "work::TogglePanel"),
            ("c n", &WORKSPACE[..], "chat::New"),
            ("f r", &WORKSPACE[..], "workflow::Run"),
            ("ctrl-c", &PROMPTS_EDITOR[..], "prompts::Send"),
        ] {
            let input = format!("{leader} {rest}");
            let (names, pending) = resolve(&keymap, &input, contexts);
            assert_eq!(names.first(), Some(&action), "macos={macos}: {input}");
            assert!(!pending, "macos={macos}: {input}");
        }
    }
}

#[gpui::test]
async fn no_default_binding_or_prefix_outranks_a_derived_chord(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let keymap = Keymap::new(load_profile(BaseKeymap::VSCode, false, macos, cx));
        for binding in vscode_leader_rows(macos, cx) {
            let keys = keys_of(&binding);
            let input = keys.join(" ");
            for contexts in stacks_for(&context_of(&binding)) {
                let label = format!("macos={macos}: {input} in {contexts:?}");
                let (names, pending) = resolve(&keymap, &input, contexts);
                assert_eq!(names.first(), Some(&binding.action().name()), "{label}");
                assert!(!pending, "{label}");
                // Every shorter prefix waits for more keys instead of running something.
                for len in 1..keys.len() {
                    let prefix = keys[..len].join(" ");
                    assert!(
                        resolve(&keymap, &prefix, contexts).1,
                        "{label}: {prefix} does not wait"
                    );
                }
            }
        }
    }
}

#[gpui::test]
async fn the_terminal_still_forwards_ctrl_k_with_the_leader_loaded(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    // Linux forwards `ctrl-k` to the shell; macOS clears the terminal on `cmd-k`.
    for (macos, action) in [
        (false, "terminal::SendKeystroke"),
        (true, "terminal::Clear"),
    ] {
        let leader = leader_keys(macos);
        let bindings = load_profile(BaseKeymap::VSCode, false, macos, cx);
        assert!(
            bindings
                .iter()
                .any(|binding| keys_of(binding).starts_with(&leader)),
            "macos={macos}: the leader block is loaded"
        );
        let keymap = Keymap::new(bindings);
        let input = if macos { "cmd-k" } else { "ctrl-k" };
        for contexts in [&TERMINAL[..], &PANE_TERMINAL[..]] {
            let (names, pending) = resolve(&keymap, input, contexts);
            assert_eq!(names.first(), Some(&action), "macos={macos}: {contexts:?}");
            assert!(!pending, "macos={macos}: {contexts:?}");
        }
    }
}

/// Context stacks a default single-key binding is checked in; each binding is checked in every
/// stack its context matches.
const SINGLE_KEY_STACKS: [&[&str]; 5] = [
    &TERMINAL,
    &PANE_TERMINAL,
    &["Workspace", "RecentProjects"],
    &[
        "Workspace",
        "RecentProjects",
        "Picker",
        "Editor mode=single_line",
    ],
    &["Workspace", "Pane", "KeymapEditor"],
];

#[gpui::test]
async fn every_default_single_leader_key_still_fires_under_vscode(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let leader = leader_keys(macos);
        let default_path = if macos {
            "keymaps/default-macos.json"
        } else {
            "keymaps/default-linux.json"
        };
        let defaults: Vec<KeyBinding> = cx
            .update(|cx| KeymapFile::load_asset(default_path, Some(KeybindSource::Default), cx))
            .unwrap()
            .into_iter()
            .filter(|binding| {
                keys_of(binding) == leader[..1] && !gpui::is_no_action(binding.action())
            })
            .collect();
        assert!(
            !defaults.is_empty(),
            "macos={macos}: no single {}",
            leader[0]
        );
        let keymap = Keymap::new(load_profile(BaseKeymap::VSCode, false, macos, cx));
        let input = if macos { "cmd-k" } else { "ctrl-k" };
        for binding in &defaults {
            let predicate = binding.predicate();
            let stacks: Vec<_> = SINGLE_KEY_STACKS
                .iter()
                .filter(|contexts| {
                    predicate
                        .as_ref()
                        .is_none_or(|predicate| predicate.eval(&stack(contexts)))
                })
                .collect();
            assert!(
                !stacks.is_empty(),
                "macos={macos}: add a context stack for `{}`",
                context_of(binding)
            );
            for contexts in stacks {
                let label = format!("macos={macos}: {input} in {contexts:?}");
                let (names, pending) = resolve(&keymap, input, contexts);
                assert_eq!(names.first(), Some(&binding.action().name()), "{label}");
                assert!(!pending, "{label}");
            }
        }
    }
}

/// Panel contexts that are not editor-like: a bare context name, none of the contexts the
/// Emacs file binds Emacs-only keys in.
const NOT_PANEL_CONTEXTS: &[&str] = &["Editor", "Workspace", "Pane", "Terminal", "Markdown"];

/// Panel blocks that may stay in `emacs.json` because they are Emacs-only. Empty today. Keys of
/// later features' panels go to the specific overrides file, not here.
const EMACS_ONLY_PANEL_KEYS: &[(&str, &str)] = &[];

/// A panel key: a binding whose context is one bare name (no operators, not an editor, workspace,
/// pane, terminal or markdown context) and whose keys are a single keystroke with no modifier.
/// Emacs-only keys (`ctrl-` re-declarations) always carry a modifier, so they are never panel keys.
pub(super) fn is_panel_key(binding: &KeyBinding) -> bool {
    let context = context_of(binding);
    let bare = !context.is_empty() && context.chars().all(|c| c.is_alphanumeric() || c == '_');
    let keystrokes = binding.keystrokes();
    bare && !NOT_PANEL_CONTEXTS.contains(&context.as_str())
        && keystrokes.len() == 1
        && !keystrokes[0].inner().modifiers.modified()
}

#[gpui::test]
async fn emacs_json_keeps_no_single_key_panel_blocks(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for path in ["keymaps/linux/emacs.json", "keymaps/macos/emacs.json"] {
        let bindings =
            cx.update(|cx| KeymapFile::load_asset(path, Some(KeybindSource::Base), cx).unwrap());
        for binding in bindings.iter().filter(|binding| is_panel_key(binding)) {
            let entry = (context_of(binding), keys_of(binding).join(" "));
            assert!(
                EMACS_ONLY_PANEL_KEYS
                    .iter()
                    .any(|(context, key)| entry.0 == *context && entry.1 == *key),
                "{path}: panel key `{}` in `{}` belongs in the specific overrides file",
                entry.1,
                entry.0
            );
        }
    }
}

/// One key of each moved panel block, with the context stack it resolves in.
const PANEL_KEYS: &[(&str, &[&str], &str)] = &[
    ("r", &["Workspace", "ChatList"], "chat::Rename"),
    ("/", &["Workspace", "ChatList"], "chat::Filter"),
    ("tab", &["Workspace", "WorkPanel"], "work::ToggleRow"),
    ("^", &["Workspace", "WorkPanel"], "work::Up"),
    ("a", &["Workspace", "RunsPanel"], "workflow::Approve"),
    ("enter", &["Workspace", "RunsPanel"], "workflow::OpenLogs"),
    ("g", &["Workspace", "RunView"], "workflow::ToggleGraph"),
    ("v", &["Workspace", "RunView"], "workflow::Revise"),
    ("r", &["Workspace", "LandingView"], "project::ResolveSplit"),
    ("s", &["Workspace", "TaskDetailView"], "task_detail::ToggleSources"),
    ("x", &["Workspace", "TaskDetailView"], "task::RunChecks"),
    ("enter", &["Workspace", "ProposalView"], "proposal::Descend"),
    ("^", &["Workspace", "ProposalView"], "proposal::Up"),
];

#[gpui::test]
async fn panel_keys_resolve_under_emacs_vscode_and_vim(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        for (base, vim) in [
            (BaseKeymap::Emacs, false),
            (BaseKeymap::Emacs, true),
            (BaseKeymap::VSCode, false),
            (BaseKeymap::VSCode, true),
        ] {
            let keymap = Keymap::new(load_profile(base, vim, macos, cx));
            for (key, contexts, action) in PANEL_KEYS {
                let (names, pending) = resolve(&keymap, key, contexts);
                assert_eq!(
                    names.first(),
                    Some(action),
                    "macos={macos} {base:?} vim={vim}: {key} in {contexts:?}"
                );
                assert!(!pending, "macos={macos} {base:?} vim={vim}: {key}");
            }
        }
    }
}

pub(super) const VIM_NORMAL: [&str; 3] = [
    "Workspace",
    "Pane",
    "Editor mode=full vim_mode=normal vim_operator=none VimControl",
];

fn set_base_keymap_json(cx: &mut TestAppContext, base_keymap: &str) {
    cx.update(|cx| {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store
                .set_user_settings(&format!(r#"{{"base_keymap": "{base_keymap}"}}"#), cx)
                .expect("settings parse");
        });
    });
}

/// The bindings `load_default_keymap` binds for the current settings.
fn loaded_keymap(cx: &mut TestAppContext) -> Vec<KeyBinding> {
    cx.update(|cx| {
        cx.clear_key_bindings();
        super::load_default_keymap(cx);
        let bindings: Vec<KeyBinding> = cx.key_bindings().borrow().bindings().cloned().collect();
        bindings
    })
}

fn has_action_prefix(bindings: &[KeyBinding], prefix: &str) -> bool {
    bindings
        .iter()
        .any(|binding| binding.action().name().starts_with(prefix))
}

#[gpui::test]
async fn the_vim_profile_resolves_colon_and_the_space_leader(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let keymap = Keymap::new(load_profile(BaseKeymap::Vim, true, macos, cx));
        assert_eq!(
            first(&keymap, ":", &VIM_NORMAL),
            Some("vim::CommandLine"),
            "macos={macos}"
        );
        let (names, pending) = resolve(&keymap, "space w w", &VIM_NORMAL);
        assert_eq!(names.first(), Some(&"work::TogglePanel"), "macos={macos}");
        assert!(!pending, "macos={macos}");
        // `space` alone is a prefix now, and the timeout falls back to vim's own `space`.
        let (names, pending) = resolve(&keymap, "space", &VIM_NORMAL);
        assert_eq!(names.first(), Some(&"vim::WrappingRight"), "macos={macos}");
        assert!(pending, "macos={macos}");
        // The VS Code leader and the Terminal tail stay.
        let modifier = if macos { "cmd" } else { "ctrl" };
        assert_eq!(
            first(&keymap, &format!("{modifier}-k b w w"), &WORKSPACE),
            Some("work::TogglePanel"),
            "macos={macos}"
        );
        // The space leader does not leak into insert mode.
        let insert = ["Workspace", "Pane", "Editor mode=full vim_mode=insert"];
        assert_ne!(
            first(&keymap, "space w w", &insert),
            Some("work::TogglePanel"),
            "macos={macos}"
        );
    }
    let keymap = Keymap::new(load_profile(BaseKeymap::Vim, true, false, cx));
    assert_eq!(
        first(&keymap, "ctrl-k", &TERMINAL),
        Some("terminal::SendKeystroke")
    );
}

#[gpui::test]
async fn a_user_binding_on_ctrl_p_wins_under_vim(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let user = user_bindings(
        r#"[
            {"context": "Editor", "bindings": {"ctrl-p": "menu::Confirm"}},
            {"context": "Workspace", "bindings": {"ctrl-p": "menu::Confirm"}}
        ]"#,
        cx,
    );
    for macos in [false, true] {
        let mut all = load_profile(BaseKeymap::Vim, true, macos, cx);
        all.extend(user.clone());
        let keymap = Keymap::new(all);
        assert_eq!(
            first(&keymap, "ctrl-p", &VIM_NORMAL),
            Some("menu::Confirm"),
            "macos={macos}"
        );
        assert_eq!(
            first(&keymap, "ctrl-p", &WORKSPACE),
            Some("menu::Confirm"),
            "macos={macos}"
        );
    }
}

#[gpui::test]
async fn switching_through_vim_binds_and_unbinds_vim_and_the_space_rows(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let space_ww = |bindings: &[KeyBinding]| {
        let keymap = Keymap::new(bindings.to_vec());
        first(&keymap, "space w w", &VIM_NORMAL) == Some("work::TogglePanel")
    };
    let colon = |bindings: &[KeyBinding]| {
        let keymap = Keymap::new(bindings.to_vec());
        first(&keymap, ":", &VIM_NORMAL) == Some("vim::CommandLine")
    };
    for (base, vim, emacs) in [
        ("Emacs", false, true),
        ("Vim", true, false),
        ("VSCode", false, false),
        ("Vim", true, false),
        ("Emacs", false, true),
    ] {
        set_base_keymap_json(cx, base);
        assert_eq!(
            cx.update(|cx| {
                use settings::Settings as _;
                vim_mode_setting::VimModeSetting::get_global(cx).0
            }),
            vim,
            "{base}: vim mode"
        );
        let bindings = loaded_keymap(cx);
        assert_eq!(has_action_prefix(&bindings, "vim::"), vim, "{base}: vim::");
        assert_eq!(
            has_action_prefix(&bindings, "emacs::"),
            emacs,
            "{base}: emacs::"
        );
        assert_eq!(space_ww(&bindings), vim, "{base}: space w w");
        assert_eq!(colon(&bindings), vim, "{base}: colon");
        if vim {
            assert!(
                bindings
                    .iter()
                    .any(|binding| binding.meta() == Some(KeybindSource::Vim.meta())),
                "{base}: vim source"
            );
        } else {
            assert!(
                bindings
                    .iter()
                    .all(|binding| binding.meta() != Some(KeybindSource::Vim.meta())),
                "{base}: stray vim source"
            );
        }
    }
}

#[gpui::test]
async fn a_new_full_editor_opens_in_normal_mode_under_vim_and_i_inserts(cx: &mut TestAppContext) {
    use editor::{Editor, EditorMode, MultiBuffer};
    use gpui::{AppContext as _, Focusable as _, VisualTestContext};

    init_keymap_test(cx);
    cx.update(|cx| {
        editor::init(cx);
        vim::init(cx);
    });
    set_base_keymap_json(cx, "Vim");
    let bindings = loaded_keymap(cx);
    cx.update(|cx| {
        cx.clear_key_bindings();
        cx.bind_keys(bindings);
    });

    // The same shape the agent prompt editor has: a full multi-line editor.
    let (editor, cx) = cx.add_window_view(|window, cx| {
        let buffer = cx.new(|cx| language::Buffer::local("", cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        Editor::new(EditorMode::full(), buffer, None, window, cx)
    });
    let vim_mode = |cx: &mut VisualTestContext| {
        editor.update_in(cx, |editor, window, cx| {
            editor
                .key_context(window, cx)
                .get("vim_mode")
                .map(|mode| mode.to_string())
        })
    };
    editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx)
    });
    assert_eq!(vim_mode(cx).as_deref(), Some("normal"));
    cx.simulate_keystrokes("i");
    assert_eq!(vim_mode(cx).as_deref(), Some("insert"));
}

#[gpui::test]
async fn terminal_keys_forward_and_never_wait_under_vscode_and_vim(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        for (base, vim_enabled) in [(BaseKeymap::VSCode, false), (BaseKeymap::Vim, true)] {
            let keymap = Keymap::new(load_profile(base, vim_enabled, macos, cx));
            let pane = if macos { "cmd-1" } else { "alt-1" };
            for contexts in [&TERMINAL[..], &PANE_TERMINAL[..], &CHAT_TERMINAL[..]] {
                let label = format!("{base:?} macos={macos}: {contexts:?}");
                for input in [
                    "ctrl-c", "ctrl-d", "ctrl-k", "ctrl-w", "ctrl-r", "escape", "a", "j", "q",
                ] {
                    let (names, pending) = resolve(&keymap, input, contexts);
                    assert!(
                        matches!(
                            names.first().copied(),
                            None | Some("terminal::SendKeystroke")
                        ),
                        "{label}: {input} -> {names:?}"
                    );
                    assert!(!pending, "{label}: {input}");
                }
                assert_eq!(
                    first(&keymap, pane, contexts),
                    Some("workspace::ActivatePane"),
                    "{label}: {pane}"
                );
            }
        }
    }
}

#[gpui::test]
async fn the_prompt_editor_keys_resolve_under_vim_and_vscode(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for macos in [false, true] {
        let vscode = Keymap::new(load_profile(BaseKeymap::VSCode, false, macos, cx));
        assert_eq!(
            first(
                &vscode,
                if macos { "cmd-c" } else { "ctrl-c" },
                &PROMPTS_EDITOR
            ),
            Some("editor::Copy"),
            "macos={macos}"
        );
        let vim = Keymap::new(load_profile(BaseKeymap::Vim, true, macos, cx));
        let (names, pending) = resolve(
            &vim,
            if macos {
                "cmd-k b ctrl-c"
            } else {
                "ctrl-k b ctrl-c"
            },
            &PROMPTS_EDITOR,
        );
        assert_eq!(names.first(), Some(&"prompts::Send"), "macos={macos}");
        assert!(!pending, "macos={macos}");
    }
}

#[gpui::test]
async fn the_prompt_editor_leaves_insert_mode_with_escape_under_vim(cx: &mut TestAppContext) {
    use editor::{Editor, EditorMode, MultiBuffer};
    use gpui::{AppContext as _, Focusable as _, VisualTestContext};

    init_keymap_test(cx);
    cx.update(|cx| {
        editor::init(cx);
        vim::init(cx);
    });
    set_base_keymap_json(cx, "Vim");
    let bindings = loaded_keymap(cx);
    cx.update(|cx| {
        cx.clear_key_bindings();
        cx.bind_keys(bindings);
    });
    let (editor, cx) = cx.add_window_view(|window, cx| {
        let buffer = cx.new(|cx| language::Buffer::local("", cx));
        let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        Editor::new(EditorMode::full(), buffer, None, window, cx)
    });
    let vim_mode = |cx: &mut VisualTestContext| {
        editor.update_in(cx, |editor, window, cx| {
            editor
                .key_context(window, cx)
                .get("vim_mode")
                .map(|mode| mode.to_string())
        })
    };
    editor.update_in(cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx)
    });
    cx.simulate_keystrokes("i");
    assert_eq!(vim_mode(cx).as_deref(), Some("insert"));
    cx.simulate_keystrokes("escape");
    assert_eq!(vim_mode(cx).as_deref(), Some("normal"));
}
