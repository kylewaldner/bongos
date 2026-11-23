//! The lanes key table of the Emacs base keymap: every `lanes::` action bound under `C-c f L`,
//! nothing bound on the prefix itself, and no clash with another binding.

use gpui::{KeyBinding, KeyContext, Keymap, Keystroke, TestAppContext};
use settings::KeymapFile;

use super::tests::init_keymap_test;

const LINUX: (&str, &str) = ("keymaps/default-linux.json", "keymaps/linux/emacs.json");
const MACOS: (&str, &str) = ("keymaps/default-macos.json", "keymaps/macos/emacs.json");

const LANES_KEYS: &[(&str, &str)] = &[
    ("ctrl-c f L L", "lanes::Show"),
    ("ctrl-c f L s", "lanes::Start"),
    ("ctrl-c f L S", "lanes::Stop"),
    ("ctrl-c f L p", "lanes::Pause"),
    ("ctrl-c f L c", "lanes::Resume"),
    ("ctrl-c f L d", "lanes::DryRun"),
    ("ctrl-c f L j", "lanes::Pick"),
    ("ctrl-c f L o", "lanes::Adopt"),
    ("ctrl-c f L a", "lanes::AcceptGraph"),
];

/// Commands that stay in the palette with no key.
const UNBOUND: &[&str] = &["task::Run", "lanes::Dump"];

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
async fn emacs_binds_every_lanes_key_without_a_clash(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, emacs) in [LINUX, MACOS] {
        let (keymap, bindings) = load(&[default, emacs], cx);
        for (input, action) in LANES_KEYS {
            let (names, pending) = resolve(&keymap, input, &["Workspace", "Pane"]);
            assert_eq!(names, [*action], "{emacs}: {input}");
            assert!(!pending, "{emacs}: {input}");
            // Every longer prefix (a lone `ctrl-c` is a copy key in places) only waits for more keys: nothing is bound on it.
            let keys: Vec<&str> = input.split(' ').collect();
            for length in 2..keys.len() {
                let (names, pending) =
                    resolve(&keymap, &keys[..length].join(" "), &["Workspace", "Pane"]);
                assert!(
                    names.is_empty(),
                    "{emacs}: {} is bound",
                    keys[..length].join(" ")
                );
                assert!(pending, "{emacs}: {}", keys[..length].join(" "));
            }
        }
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!UNBOUND.contains(&name), "{emacs}: {name} is bound");
            if name.starts_with("lanes::") {
                assert!(
                    LANES_KEYS.iter().any(|(_, action)| *action == name),
                    "{emacs}: {name} is bound but not in the table"
                );
            }
        }
    }
}

#[gpui::test]
async fn default_base_has_no_lanes_binding(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (default, _) in [LINUX, MACOS] {
        let (_, bindings) = load(&[default], cx);
        for binding in &bindings {
            let name = binding.action().name();
            assert!(!name.starts_with("lanes::"), "{default}: {name} is bound");
        }
    }
}
