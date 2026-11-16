use std::fmt::{Display, Formatter};

use crate::{
    self as settings, KeybindSource, KeymapFile, Leader, VIM_KEYMAP_PATH,
    settings_content::BaseKeymapContent,
};
use gpui::{App, KeyBinding};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, Settings};

/// Base key bindings scheme. Base keymaps can be overridden with user keymaps.
///
/// Default: Emacs
#[derive(
    Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default, RegisterSetting,
)]
pub enum BaseKeymap {
    VSCode,
    Vim,
    #[default]
    Emacs,
    None,
}

impl From<BaseKeymapContent> for BaseKeymap {
    fn from(value: BaseKeymapContent) -> Self {
        match value {
            BaseKeymapContent::VSCode => Self::VSCode,
            BaseKeymapContent::Vim => Self::Vim,
            BaseKeymapContent::Emacs => Self::Emacs,
            // Retired keymaps fall back to Emacs; the stored value is left alone.
            BaseKeymapContent::JetBrains
            | BaseKeymapContent::SublimeText
            | BaseKeymapContent::Atom
            | BaseKeymapContent::TextMate
            | BaseKeymapContent::Cursor => Self::Emacs,
            BaseKeymapContent::None => Self::None,
        }
    }
}
impl Into<BaseKeymapContent> for BaseKeymap {
    fn into(self) -> BaseKeymapContent {
        match self {
            BaseKeymap::VSCode => BaseKeymapContent::VSCode,
            BaseKeymap::Vim => BaseKeymapContent::Vim,
            BaseKeymap::Emacs => BaseKeymapContent::Emacs,
            BaseKeymap::None => BaseKeymapContent::None,
        }
    }
}

impl Display for BaseKeymap {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BaseKeymap::VSCode => write!(f, "VS Code"),
            BaseKeymap::Vim => write!(f, "Vim"),
            BaseKeymap::Emacs => write!(f, "Emacs"),
            BaseKeymap::None => write!(f, "None"),
        }
    }
}

impl BaseKeymap {
    pub const OPTIONS: [(&'static str, Self); 4] = [
        ("VS Code", Self::VSCode),
        ("Vim", Self::Vim),
        ("Emacs (Default)", Self::Emacs),
        ("None", Self::None),
    ];

    /// The Emacs keymap asset for this platform, if the base keymap has one.
    pub fn asset_path(&self) -> Option<&'static str> {
        match self {
            BaseKeymap::Emacs => {
                profile_keymap(*self, false)
                    .into_iter()
                    .find_map(|part| match part {
                        KeymapPart::Asset { path, .. } if path.contains("emacs") => Some(path),
                        _ => None,
                    })
            }
            BaseKeymap::VSCode | BaseKeymap::Vim | BaseKeymap::None => None,
        }
    }

    pub fn names() -> impl Iterator<Item = &'static str> {
        Self::OPTIONS.iter().map(|(name, _)| *name)
    }

    pub fn from_names(option: &str) -> BaseKeymap {
        Self::OPTIONS
            .iter()
            .copied()
            .find_map(|(name, value)| (name == option).then_some(value))
            .unwrap_or_default()
    }
}

/// One piece of the keymap a base keymap loads, in load order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeymapPart {
    /// A bundled keymap asset, bound with the given source.
    Asset {
        path: &'static str,
        source: KeybindSource,
    },
    /// The leader rows derived from the bundled Emacs keymap at `emacs`, for the platform the
    /// part was built for (macOS when `macos` is true), bound with the given source.
    Leader {
        emacs: &'static str,
        leader: Leader,
        macos: bool,
        source: KeybindSource,
    },
}

impl KeymapPart {
    const fn asset(path: &'static str, source: KeybindSource) -> Self {
        Self::Asset { path, source }
    }

    /// The key bindings of this part, with their source set.
    pub fn load(&self, cx: &App) -> anyhow::Result<Vec<KeyBinding>> {
        match *self {
            KeymapPart::Asset { path, source } => KeymapFile::load_asset(path, Some(source), cx),
            KeymapPart::Leader {
                emacs,
                leader,
                macos,
                source,
            } => settings::load_leader_asset(emacs, leader, macos, source, cx),
        }
    }
}

/// The keymap parts to load for a base keymap, in load order. User keymaps load after all of
/// them. `vim_enabled` is true when vim mode or helix mode is on.
pub fn profile_keymap(base: BaseKeymap, vim_enabled: bool) -> Vec<KeymapPart> {
    profile_keymap_for_platform(base, vim_enabled, cfg!(target_os = "macos"))
}

/// `profile_keymap` for an explicit platform: macOS when `macos` is true, Linux otherwise.
pub fn profile_keymap_for_platform(
    base: BaseKeymap,
    vim_enabled: bool,
    macos: bool,
) -> Vec<KeymapPart> {
    let overrides = if macos {
        "keymaps/specific-overrides-macos.json"
    } else {
        "keymaps/specific-overrides.json"
    };
    let overrides = KeymapPart::asset(overrides, KeybindSource::Default);
    let vim = KeymapPart::asset(VIM_KEYMAP_PATH, KeybindSource::Vim);
    let default_path = if macos {
        "keymaps/default-macos.json"
    } else {
        "keymaps/default-linux.json"
    };
    let default = KeymapPart::asset(default_path, KeybindSource::Default);
    let emacs = if macos {
        "keymaps/macos/emacs.json"
    } else {
        "keymaps/linux/emacs.json"
    };

    let mut parts = Vec::new();
    match base {
        BaseKeymap::None => {}
        BaseKeymap::Emacs => {
            parts.push(default);
            parts.push(KeymapPart::asset(emacs, KeybindSource::Base));
            if vim_enabled {
                parts.push(vim);
            }
        }
        BaseKeymap::VSCode | BaseKeymap::Vim => {
            parts.push(default);
            let (vscode, tail) = if macos {
                (
                    "keymaps/macos/vscode.json",
                    "keymaps/macos/vscode-tail.json",
                )
            } else {
                (
                    "keymaps/linux/vscode.json",
                    "keymaps/linux/vscode-tail.json",
                )
            };
            parts.push(KeymapPart::asset(vscode, KeybindSource::Base));
            // The bongos commands under `ctrl-k b` (`cmd-k b` on macOS), before the tail, which
            // binds again every single `ctrl-k` (`cmd-k`) that must not wait for the chord.
            parts.push(KeymapPart::Leader {
                emacs,
                leader: Leader::VsCode,
                macos,
                source: KeybindSource::Base,
            });
            if base == BaseKeymap::Vim {
                // vim.json, then the `space` chords after it so they outrank vim's own `space`,
                // then the tail.
                parts.push(vim);
                parts.push(KeymapPart::Leader {
                    emacs,
                    leader: Leader::VimSpace,
                    macos,
                    source: KeybindSource::Base,
                });
                parts.push(KeymapPart::asset(tail, KeybindSource::Base));
            } else {
                parts.push(KeymapPart::asset(tail, KeybindSource::Base));
                if vim_enabled {
                    parts.push(vim);
                }
            }
        }
    }
    parts.push(overrides);
    parts
}

impl Settings for BaseKeymap {
    fn from_settings(s: &crate::settings_content::SettingsContent) -> Self {
        s.base_keymap.unwrap().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETIRED: [&str; 5] = ["JetBrains", "Atom", "SublimeText", "TextMate", "Cursor"];

    #[test]
    fn retired_values_load_as_emacs() {
        for name in RETIRED {
            let content: BaseKeymapContent = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert!(content.is_retired(), "{name}");
            assert_eq!(BaseKeymap::from(content), BaseKeymap::Emacs, "{name}");
        }
        for kept in [
            BaseKeymapContent::Emacs,
            BaseKeymapContent::Vim,
            BaseKeymapContent::VSCode,
            BaseKeymapContent::None,
        ] {
            assert!(!kept.is_retired());
        }
    }

    #[test]
    fn profile_keymap_table() {
        use KeybindSource::{Base, Default, Vim};
        // A derived leader part is listed as `leader:<its Emacs asset>`.
        let paths = |base, vim, macos: bool| -> Vec<(String, KeybindSource)> {
            profile_keymap_for_platform(base, vim, macos)
                .into_iter()
                .map(|part| match part {
                    KeymapPart::Asset { path, source } => (path.to_string(), source),
                    KeymapPart::Leader {
                        emacs,
                        leader,
                        macos: part_macos,
                        source,
                    } => {
                        assert_eq!(part_macos, macos);
                        let kind = match leader {
                            Leader::VsCode => "leader",
                            Leader::VimSpace => "space-leader",
                        };
                        (format!("{kind}:{emacs}"), source)
                    }
                })
                .collect()
        };
        let owned = |parts: Vec<(&str, KeybindSource)>| -> Vec<(String, KeybindSource)> {
            parts
                .into_iter()
                .map(|(path, source)| (path.to_string(), source))
                .collect()
        };
        for (macos, dir, default, overrides) in [
            (
                false,
                "linux",
                "keymaps/default-linux.json",
                "keymaps/specific-overrides.json",
            ),
            (
                true,
                "macos",
                "keymaps/default-macos.json",
                "keymaps/specific-overrides-macos.json",
            ),
        ] {
            let emacs = format!("keymaps/{dir}/emacs.json");
            let vscode = format!("keymaps/{dir}/vscode.json");
            let tail = format!("keymaps/{dir}/vscode-tail.json");
            let leader = format!("leader:{emacs}");
            let space_leader = format!("space-leader:{emacs}");
            for vim_enabled in [false, true] {
                let vim_part: Vec<_> = vim_enabled
                    .then_some(("keymaps/vim.json", Vim))
                    .into_iter()
                    .collect();
                let mut expected = vec![(default, Default), (emacs.as_str(), Base)];
                expected.extend(vim_part.clone());
                expected.push((overrides, Default));
                assert_eq!(
                    paths(BaseKeymap::Emacs, vim_enabled, macos),
                    owned(expected)
                );

                let mut expected = vec![
                    (default, Default),
                    (vscode.as_str(), Base),
                    (leader.as_str(), Base),
                    (tail.as_str(), Base),
                ];
                expected.extend(vim_part);
                expected.push((overrides, Default));
                assert_eq!(
                    paths(BaseKeymap::VSCode, vim_enabled, macos),
                    owned(expected)
                );

                // Under `Vim` vim.json always loads, whatever `vim_enabled` says.
                assert_eq!(
                    paths(BaseKeymap::Vim, vim_enabled, macos),
                    owned(vec![
                        (default, Default),
                        (vscode.as_str(), Base),
                        (leader.as_str(), Base),
                        ("keymaps/vim.json", Vim),
                        (space_leader.as_str(), Base),
                        (tail.as_str(), Base),
                        (overrides, Default),
                    ])
                );

                assert_eq!(
                    paths(BaseKeymap::None, vim_enabled, macos),
                    owned(vec![(overrides, Default)])
                );
            }
        }
        assert_eq!(
            profile_keymap(BaseKeymap::Emacs, false),
            profile_keymap_for_platform(BaseKeymap::Emacs, false, cfg!(target_os = "macos"))
        );
    }

    #[test]
    fn lowercase_aliases_parse() {
        for (name, expected) in [
            ("emacs", BaseKeymapContent::Emacs),
            ("vscode", BaseKeymapContent::VSCode),
            ("none", BaseKeymapContent::None),
            ("vim", BaseKeymapContent::Vim),
            ("Vim", BaseKeymapContent::Vim),
            ("Emacs", BaseKeymapContent::Emacs),
            ("VSCode", BaseKeymapContent::VSCode),
            ("None", BaseKeymapContent::None),
        ] {
            let content: BaseKeymapContent = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(content, expected, "{name}");
        }
        assert_eq!(
            serde_json::to_string(&BaseKeymapContent::VSCode).unwrap(),
            "\"VSCode\""
        );
    }

    #[test]
    fn retired_values_are_not_options() {
        for name in RETIRED {
            assert!(
                BaseKeymap::OPTIONS
                    .iter()
                    .all(|(label, _)| !label.contains(name)),
                "{name}"
            );
        }
    }

    #[test]
    fn options_have_assets_only_where_needed() {
        for (_, keymap) in BaseKeymap::OPTIONS {
            if let Some(path) = keymap.asset_path() {
                assert!(path.contains("emacs"), "{path}");
            }
        }
    }
}
