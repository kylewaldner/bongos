//! The keys of the Lanes view (key context `LanesView`): every action the view declares is bound,
//! and each key resolves to its action on the view's focus path.

use gpui::{KeyContext, Keymap, Keystroke, TestAppContext};
use settings::KeymapFile;

use super::tests::init_keymap_test;

const PROFILES: [[&str; 3]; 2] = [
    [
        "keymaps/default-linux.json",
        "keymaps/linux/emacs.json",
        "keymaps/specific-overrides.json",
    ],
    [
        "keymaps/default-macos.json",
        "keymaps/macos/emacs.json",
        "keymaps/specific-overrides-macos.json",
    ],
];

/// The keys of the view and what each asks for.
const VIEW_KEYS: &[(&str, &str)] = &[
    ("enter", "lanes_view::OpenTask"),
    ("p", "lanes_view::PauseLane"),
    ("c", "lanes_view::ContinueLane"),
    ("S", "lanes_view::StopLane"),
    ("o", "lanes_view::OpenRun"),
    ("d", "lanes_view::DryRun"),
    ("g", "lanes_view::Refresh"),
    ("a", "lanes::Adopt"),
];

fn resolve(keymap: &Keymap, input: &str) -> Vec<&'static str> {
    let keys = [Keystroke::parse(input).unwrap()];
    let contexts: Vec<KeyContext> = ["Workspace", "Pane", "LanesView"]
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect();
    let (bindings, _) = keymap.bindings_for_input(&keys, &contexts);
    bindings
        .iter()
        .map(|binding| binding.action().name())
        .collect()
}

#[gpui::test]
async fn every_action_the_lanes_view_declares_is_bound(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let declared: Vec<&'static str> = cx.update(|cx| {
        cx.all_action_names()
            .iter()
            .copied()
            .filter(|name| name.starts_with("lanes_view::"))
            .collect()
    });
    assert!(!declared.is_empty(), "the view declares no actions");
    for paths in PROFILES {
        let bindings = cx.update(|cx| {
            let mut all = Vec::new();
            for path in paths {
                all.extend(
                    KeymapFile::load_asset(path, None, cx)
                        .unwrap_or_else(|error| panic!("{path}: {error}")),
                );
            }
            all
        });
        let keymap = Keymap::new(bindings);
        for name in &declared {
            assert!(
                VIEW_KEYS.iter().any(|(_, action)| action == name),
                "{}: {name} is declared but missing from the key table of this test",
                paths[1]
            );
        }
        for (input, action) in VIEW_KEYS {
            assert_eq!(
                resolve(&keymap, input).first(),
                Some(action),
                "{}: {input}",
                paths[1]
            );
        }
    }
}
