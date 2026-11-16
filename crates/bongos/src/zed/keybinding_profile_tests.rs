//! The keymap profile loader: what each base keymap binds on each platform, that `ctrl-g` and
//! `escape` leave prompts under every profile, and that the user's `keymap.json` is never written
//! and always wins.

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::{
    BaseKeymap, BaseKeymapContent, KeybindSource, KeymapFile, KeymapFileLoadResult, KeymapPart,
};
use std::path::PathBuf;

use super::tests::init_keymap_test;

const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const WORKSPACE: [&str; 1] = ["Workspace"];
const PICKER: [&str; 3] = ["Workspace", "Picker", "Editor mode=single_line"];
const MINIBUFFER: [&str; 4] = ["Workspace", "Minibuffer", "Picker", "Editor mode=full"];
const PANE_TERMINAL: [&str; 3] = ["Workspace", "Pane", "Terminal"];
const TERMINAL: [&str; 4] = ["Workspace", "Dock", "TerminalPanel", "Terminal"];

/// The bindings a profile loads, in load order, with the sources the app loads them with.
fn load_profile(
    base: BaseKeymap,
    vim_enabled: bool,
    macos: bool,
    cx: &TestAppContext,
) -> Vec<KeyBinding> {
    cx.update(|cx| {
        let mut all = Vec::new();
        for part in settings::profile_keymap_for_platform(base, vim_enabled, macos) {
            let KeymapPart::Asset { path, source } = part;
            all.extend(
                KeymapFile::load_asset(path, Some(source), cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        all
    })
}

fn stack(contexts: &[&str]) -> Vec<KeyContext> {
    contexts
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect()
}

/// The names of the actions a key resolves to, deepest first, and whether it waits for more keys.
fn resolve(keymap: &Keymap, input: &str, contexts: &[&str]) -> (Vec<&'static str>, bool) {
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

fn first(keymap: &Keymap, input: &str, contexts: &[&str]) -> Option<&'static str> {
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
        for (base, contexts) in [
            (BaseKeymap::Emacs, &EDITOR[..]),
            (BaseKeymap::VSCode, &WORKSPACE[..]),
            (BaseKeymap::None, &WORKSPACE[..]),
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
            if base == BaseKeymap::Emacs {
                assert_eq!(
                    first(&profile_only, "ctrl-p", contexts),
                    Some("editor::MoveUp")
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
            .any(|KeymapPart::Asset { path, .. }| *path == settings::VIM_KEYMAP_PATH)
    };
    assert!(vim(true));
    assert!(!vim(false));
}

#[test]
fn the_lowercase_aliases_parse_and_written_values_stay() {
    for (alias, value, written) in [
        ("emacs", BaseKeymapContent::Emacs, "Emacs"),
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
