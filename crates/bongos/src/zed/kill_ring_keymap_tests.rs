//! The kill ring keys of the Emacs base keymap: yank, yank-pop, browse-kill-ring,
//! kill-whole-line and append-next-kill resolve to their `emacs::` actions in editors, win over
//! Zed's git and agent keys only inside editors, and leave terminals, isearch and the kill ring
//! browser's own keys alone.

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::{KeybindSource, KeymapFile};

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

/// The kill ring keys of the `Editor` block and the actions they run.
const EDITOR_KEYS: &[(&str, &str)] = &[
    ("ctrl-y", "emacs::Yank"),
    ("alt-y", "emacs::YankPop"),
    ("alt-shift-y", "emacs::BrowseKillRing"),
    ("ctrl-alt-k", "emacs::KillWholeLine"),
    ("ctrl-alt-w", "emacs::AppendNextKill"),
];

const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];
const AGENT_DIFF_EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full editor_agent_diff"];
const AGENT_DIFF: [&str; 3] = ["Workspace", "Pane", "AgentDiff"];
const AGENT_EDITOR: [&str; 4] = ["Workspace", "Dock", "AcpThread", "Editor mode=full"];
const TERMINAL: [&str; 4] = ["Workspace", "Dock", "TerminalPanel", "Terminal"];
const CHAT_TERMINAL: [&str; 5] = ["Workspace", "Dock", "ChatPanel", "ChatSession", "Terminal"];
const ISEARCH: [&str; 3] = ["Workspace", "Isearch", "Editor mode=full"];
const STRIP_ISEARCH: [&str; 4] = ["Workspace", "Minibuffer", "Isearch", "Editor mode=full"];
const KILL_RING: [&str; 3] = ["Workspace", "KillRing", "Editor"];
const STRIP_KILL_RING: [&str; 4] = ["Workspace", "Minibuffer", "KillRing", "Editor"];

/// The keymaps in load order, with the sources the app loads them with (`load_default_keymap`).
fn load_all(platform: &Platform, emacs: bool, cx: &TestAppContext) -> Vec<KeyBinding> {
    cx.update(|cx| {
        let mut paths = vec![(platform.default, KeybindSource::Default)];
        if emacs {
            paths.push((platform.emacs, KeybindSource::Base));
        }
        paths.push((platform.overrides, KeybindSource::Default));
        let mut all = Vec::new();
        for (path, source) in paths {
            all.extend(
                KeymapFile::load_asset(path, Some(source), cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        all
    })
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

fn first(keymap: &Keymap, input: &str, contexts: &[&str]) -> Option<&'static str> {
    resolve(keymap, input, contexts).0.first().copied()
}

#[gpui::test]
async fn kill_ring_keys_resolve_to_the_emacs_actions_in_editors(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for (input, action) in EDITOR_KEYS {
            let (names, pending) = resolve(&keymap, input, &EDITOR);
            assert_eq!(names.first(), Some(action), "{}: {input}", platform.emacs);
            assert!(!pending, "{}: {input}", platform.emacs);
        }
    }
}

#[gpui::test]
async fn yank_keys_shadow_the_git_and_agent_keys_only_inside_editors(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    // Zed binds these in editors; the Emacs base takes them over there. The agent's message
    // editor gets its keep-all key back, so alt-shift-y is lost only in the editor-agent-diff
    // context (`Editor && editor_agent_diff`), which is accepted.
    let zed = Keymap::new(load_all(&LINUX, false, cx));
    assert_eq!(first(&zed, "alt-y", &EDITOR), Some("git::StageAndNext"));
    assert_eq!(
        first(&zed, "alt-y", &AGENT_DIFF_EDITOR),
        Some("agent::Keep")
    );
    for platform in [LINUX, MACOS] {
        let zed = Keymap::new(load_all(&platform, false, cx));
        assert_eq!(
            first(&zed, "alt-shift-y", &AGENT_DIFF_EDITOR),
            Some("agent::KeepAll"),
            "{}",
            platform.default
        );
        let keymap = Keymap::new(load_all(&platform, true, cx));
        assert_eq!(
            first(&keymap, "alt-shift-y", &AGENT_DIFF_EDITOR),
            Some("emacs::BrowseKillRing"),
            "{}",
            platform.emacs
        );
        assert_eq!(
            first(&keymap, "alt-y", &AGENT_DIFF_EDITOR),
            Some("emacs::YankPop"),
            "{}",
            platform.emacs
        );
        // Outside editors the agent diff keeps its keys.
        assert_eq!(
            first(&keymap, "alt-shift-y", &AGENT_DIFF),
            Some("agent::KeepAll"),
            "{}",
            platform.emacs
        );
    }
    let keymap = Keymap::new(load_all(&LINUX, true, cx));
    assert_eq!(first(&keymap, "alt-y", &AGENT_DIFF), Some("agent::Keep"));
    // ctrl-alt-y keeps Zed's bindings in editors.
    assert_eq!(
        first(&keymap, "ctrl-alt-y", &EDITOR),
        Some("git::ToggleStaged")
    );
    assert_eq!(
        first(&keymap, "ctrl-alt-y", &AGENT_DIFF_EDITOR),
        Some("agent::Keep")
    );
    assert_eq!(
        first(&keymap, "ctrl-alt-y", &AGENT_DIFF),
        Some("agent::Keep")
    );
}

#[gpui::test]
async fn the_agent_editor_keeps_its_keep_all_key(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        assert_eq!(
            first(&keymap, "alt-shift-y", &AGENT_EDITOR),
            Some("agent::KeepAll"),
            "{}",
            platform.emacs
        );
        assert_eq!(
            first(&keymap, "alt-shift-y", &EDITOR),
            Some("emacs::BrowseKillRing"),
            "{}",
            platform.emacs
        );
    }
}

#[gpui::test]
async fn the_agent_editor_keeps_its_thinking_mode_key(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let keymap = Keymap::new(load_all(&LINUX, true, cx));
    assert_eq!(
        first(&keymap, "ctrl-alt-k", &AGENT_EDITOR),
        Some("agent::ToggleThinkingMode")
    );
    // macOS puts thinking mode on cmd-alt-k, so ctrl-alt-k kills the whole line there.
    let keymap = Keymap::new(load_all(&MACOS, true, cx));
    assert_eq!(
        first(&keymap, "cmd-alt-k", &AGENT_EDITOR),
        Some("agent::ToggleThinkingMode")
    );
    assert_eq!(
        first(&keymap, "ctrl-alt-k", &AGENT_EDITOR),
        Some("emacs::KillWholeLine")
    );
}

#[gpui::test]
async fn terminals_keep_receiving_the_kill_ring_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let emacs = Keymap::new(load_all(&platform, true, cx));
        let zed = Keymap::new(load_all(&platform, false, cx));
        for contexts in [&TERMINAL[..], &CHAT_TERMINAL[..]] {
            for (input, _) in EDITOR_KEYS {
                let (names, _) = resolve(&emacs, input, contexts);
                assert!(
                    !names.iter().any(|name| name.starts_with("emacs::")),
                    "{}: {input} in {contexts:?}: {names:?}",
                    platform.emacs
                );
                assert_eq!(
                    names,
                    resolve(&zed, input, contexts).0,
                    "{}: {input} in {contexts:?}",
                    platform.emacs
                );
            }
        }
    }
}

#[gpui::test]
async fn isearch_and_the_kill_ring_browser_keep_their_session_keys(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for platform in [LINUX, MACOS] {
        let keymap = Keymap::new(load_all(&platform, true, cx));
        for contexts in [&ISEARCH[..], &STRIP_ISEARCH[..]] {
            assert_eq!(
                first(&keymap, "ctrl-y", contexts),
                Some("isearch::YankKill"),
                "{}: {contexts:?}",
                platform.emacs
            );
        }
        for contexts in [&KILL_RING[..], &STRIP_KILL_RING[..]] {
            for (input, action) in [
                ("alt-n", "minibuffer::SelectNext"),
                ("alt-p", "minibuffer::SelectPrev"),
            ] {
                assert_eq!(
                    first(&keymap, input, contexts),
                    Some(action),
                    "{}: {input} in {contexts:?}",
                    platform.emacs
                );
            }
        }
    }
}

#[gpui::test]
async fn the_default_settings_open_the_kill_ring_browser_in_the_strip(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    cx.update(|cx| {
        bongos_minibuffer::init(cx);
        assert!(workspace::routes_to_minibuffer("kill_ring", cx));
    });
}
