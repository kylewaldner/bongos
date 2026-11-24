//! The navigation keys: the Emacs rows that start the pickers resolve on both platforms, the keys
//! of an open session beat the minibuffer row and the Emacs `ctrl-k`, `alt-?` is find in files
//! in an editor, the `navigation` commands are in the command palette, a session opens in the
//! minibuffer strip, and the navigation settings are defined by default.

use gpui::{
    Focusable as _, KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext, VisualTestContext,
    WindowHandle,
};
use project::Project;
use settings::{KeybindSource, KeymapFile, Settings as _, SettingsStore};
use workspace::{AppState, MultiWorkspace};

use super::tests::init_keymap_test;

/// A platform's default keymap, its Emacs base keymap and its specific overrides.
struct Platform {
    default: &'static str,
    emacs: &'static str,
    overrides: &'static str,
}

const LINUX: Platform = Platform {
    default: "keymaps/default-linux.json",
    emacs: "keymaps/linux/emacs.json",
    overrides: "keymaps/specific-overrides.json",
};
const MACOS: Platform = Platform {
    default: "keymaps/default-macos.json",
    emacs: "keymaps/macos/emacs.json",
    overrides: "keymaps/specific-overrides-macos.json",
};

const WORKSPACE: [&str; 1] = ["Workspace"];
const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const SWITCHER: [&str; 5] = [
    "Workspace",
    "Minibuffer",
    "NavSwitcher",
    "Picker",
    "Editor mode=full",
];
const CENTERED_SWITCHER: [&str; 4] = ["Workspace", "NavSwitcher", "Picker", "Editor mode=full"];
const FIND: [&str; 5] = [
    "Workspace",
    "Minibuffer",
    "FindInFiles",
    "Picker",
    "Editor mode=full",
];

const CENTERED_FIND: [&str; 4] = ["Workspace", "FindInFiles", "Picker", "Editor mode=full"];

/// The keys that start a navigation picker, with the context they are pressed in.
const STARTERS: &[(&str, &[&str], &str)] = &[
    ("ctrl-x b", &WORKSPACE, "navigation::SwitchBuffer"),
    ("ctrl-x ctrl-b", &WORKSPACE, "tab_switcher::Toggle"),
    ("ctrl-x ctrl-f", &WORKSPACE, "navigation::GoToFile"),
    (
        "ctrl-x 4 ctrl-f",
        &WORKSPACE,
        "navigation::GoToFileOtherPane",
    ),
    ("alt-?", &WORKSPACE, "navigation::FindInFiles"),
    ("alt-s /", &EDITOR, "navigation::SearchBufferLines"),
];

/// The keys of a find in files session.
const FIND_KEYS: &[(&str, &str)] = &[
    ("alt-r", "navigation::ToggleRegex"),
    ("alt-c", "navigation::ToggleCaseSensitive"),
    ("alt-w", "navigation::ToggleWholeWord"),
    ("alt-b", "navigation::ScopeOpenBuffers"),
    ("alt-d", "navigation::ScopeDirectory"),
    ("alt-%", "navigation::ToggleReplace"),
    ("ctrl-c ctrl-o", "navigation::ExportToMultibuffer"),
    ("ctrl-c ctrl-r", "navigation::ReplaceAll"),
];

/// The keymaps in load order, with the sources the app loads them with (`load_default_keymap`).
fn load_all(platform: &Platform, cx: &TestAppContext) -> Vec<KeyBinding> {
    cx.update(|cx| {
        let mut all = Vec::new();
        for (path, source) in [
            (platform.default, KeybindSource::Default),
            (platform.emacs, KeybindSource::Base),
            (platform.overrides, KeybindSource::Default),
        ] {
            all.extend(
                KeymapFile::load_asset(path, Some(source), cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        all
    })
}

fn keys(input: &str) -> Vec<Keystroke> {
    input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect()
}

/// The names of the actions a key resolves to, deepest first, and whether it waits for more keys.
fn resolve(keymap: &Keymap, input: &str, contexts: &[&str]) -> (Vec<&'static str>, bool) {
    let keys: Vec<Keystroke> = input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect();
    let contexts: Vec<KeyContext> = contexts
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect();
    let (bindings, pending) = keymap.bindings_for_input(&keys, &contexts);
    (
        bindings
            .iter()
            .map(|binding| binding.action().name())
            .collect(),
        pending,
    )
}

#[gpui::test]
async fn navigation_keys_resolve_on_linux_and_macos(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, cx));
        for (input, contexts, action) in STARTERS {
            let (names, pending) = resolve(&keymap, input, contexts);
            assert_eq!(names.first(), Some(action), "{}: {input}", platform.emacs);
            assert!(!pending, "{}: {input}", platform.emacs);
        }
        for (input, action) in FIND_KEYS {
            let (names, pending) = resolve(&keymap, input, &FIND);
            assert_eq!(names.first(), Some(action), "{}: {input}", platform.emacs);
            assert!(!pending, "{}: {input}", platform.emacs);
        }
        let (names, pending) = resolve(&keymap, "ctrl-k", &SWITCHER);
        assert_eq!(names.first(), Some(&"navigation::KillBuffer"));
        assert!(!pending);
        // The multi-key prefixes keep waiting for their next key.
        for prefix in ["ctrl-x", "ctrl-x 4", "alt-s"] {
            assert!(
                resolve(&keymap, prefix, &EDITOR).1,
                "{}: {prefix}",
                platform.emacs
            );
        }
        // The terminal sends its own keys: the compound row is unbound there.
        let emacs = cx.update(|cx| KeymapFile::load_asset(platform.emacs, None, cx).unwrap());
        assert!(
            emacs.iter().any(|binding| {
                binding.match_keystrokes(&keys("ctrl-x 4 ctrl-f")) == Some(false)
                    && binding.action().name() == "zed::NoAction"
            }),
            "{}: ctrl-x 4 ctrl-f is not null for the terminal",
            platform.emacs
        );
    }
}

#[gpui::test]
async fn session_keys_beat_the_minibuffer_row_and_the_emacs_kill_line(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, cx));
        // In a plain editor `ctrl-k` is not the buffer kill.
        let plain = resolve(&keymap, "ctrl-k", &EDITOR).0;
        assert_ne!(plain.first(), Some(&"navigation::KillBuffer"));
        for contexts in [&SWITCHER[..], &CENTERED_SWITCHER[..]] {
            let names = resolve(&keymap, "ctrl-k", contexts).0;
            assert_eq!(
                names.first(),
                Some(&"navigation::KillBuffer"),
                "{}: {contexts:?}",
                platform.overrides
            );
            assert!(names.len() > 1, "a lower row is shadowed: {names:?}");
        }
        // In the strip and in the centered picker alike, the session's keys come first, ahead of
        // the `Minibuffer > Editor` row and the Emacs editor keys (`alt-c` is capitalize there).
        for contexts in [&FIND[..], &CENTERED_FIND[..]] {
            for (input, action) in FIND_KEYS {
                let names = resolve(&keymap, input, contexts).0;
                assert_eq!(
                    names.first(),
                    Some(action),
                    "{}: {input} in {contexts:?}",
                    platform.overrides
                );
            }
        }
        // Outside the session the same keys keep their Emacs meaning.
        for (input, action) in FIND_KEYS {
            let names = resolve(&keymap, input, &EDITOR).0;
            assert_ne!(names.first(), Some(action), "{}: {input}", platform.emacs);
        }
    }
}

#[gpui::test]
async fn alt_question_mark_is_find_in_files_in_an_editor(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, cx));
        let (names, pending) = resolve(&keymap, "alt-?", &EDITOR);
        assert_eq!(names.first(), Some(&"navigation::FindInFiles"));
        assert!(!pending);
        assert!(
            !names.contains(&"editor::FindAllReferences"),
            "{}: {names:?}",
            platform.emacs
        );
    }
}

#[gpui::test]
async fn navigation_commands_are_in_the_command_palette(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        command_palette_hooks::init(cx);
        bongos_navigation::init(cx);
        let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx)
            .expect("the command palette filter is set");
        let names: Vec<&str> = cx
            .all_action_names()
            .iter()
            .copied()
            .filter(|name| name.starts_with("navigation::"))
            .collect();
        for expected in [
            "SwitchBuffer",
            "KillBuffer",
            "OpenInPane",
            "GoToFile",
            "GoToFileOtherPane",
            "GoToFileSplitRight",
            "GoToFileSplitDown",
            "FindInFiles",
            "ToggleRegex",
            "ToggleCaseSensitive",
            "ToggleWholeWord",
            "ScopeOpenBuffers",
            "ScopeDirectory",
            "ToggleReplace",
            "ReplaceAll",
            "ExportToMultibuffer",
            "SearchBufferLines",
        ] {
            let name = format!("navigation::{expected}");
            assert!(names.contains(&name.as_str()), "{name} is not registered");
        }
        for name in names {
            let action = cx.build_action(name, None).unwrap();
            assert!(!filter.is_hidden(action.as_ref()), "{name} is hidden");
        }
    });
}

#[gpui::test]
async fn sessions_open_in_the_minibuffer_strip(cx: &mut TestAppContext) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        bongos_minibuffer::init(cx);
        bongos_navigation::init(cx);
        app_state
    });
    // The default `minibuffer.enabled_for` routes the pickers to the strip.
    cx.update(|cx| {
        for host in [
            "switch_buffer",
            "go_to_file",
            "find_in_files",
            "search_buffer_lines",
            "find_directory",
        ] {
            assert!(workspace::routes_to_minibuffer(host, cx), "{host}");
        }
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window: WindowHandle<MultiWorkspace> =
        cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    let in_strip = |cx: &mut VisualTestContext| {
        workspace.read_with(cx, |workspace, cx| {
            workspace.modal_layer().read(cx).bottom_modal().is_some()
        })
    };
    assert!(!in_strip(&mut cx));
    cx.dispatch_action(bongos_navigation::actions::SwitchBuffer);
    cx.run_until_parked();
    assert!(in_strip(&mut cx), "the switcher is in the strip");
    cx.dispatch_action(zed_actions::minibuffer::Cancel);
    cx.run_until_parked();
    assert!(!in_strip(&mut cx));
    cx.dispatch_action(bongos_navigation::actions::FindInFiles);
    cx.run_until_parked();
    assert!(in_strip(&mut cx), "find in files is in the strip");
    assert!(
        cx.read(|cx| bongos_navigation::registry::current(cx, workspace.entity_id()).is_some())
    );
}

#[gpui::test]
async fn the_navigation_settings_are_defined_by_default(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::new(cx, &settings::default_settings());
        cx.set_global(store);
        bongos_navigation::init(cx);
        let settings = bongos_navigation::NavigationSettings::get_global(cx);
        assert!(settings.include_recent_closed);
        assert_eq!(settings.preview_max_file_size, 2 * 1024 * 1024);
    });
}
