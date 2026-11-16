use std::fmt::{Display, Formatter};

use crate::{self as settings, settings_content::BaseKeymapContent};
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
    #[default]
    Emacs,
    None,
}

impl From<BaseKeymapContent> for BaseKeymap {
    fn from(value: BaseKeymapContent) -> Self {
        match value {
            BaseKeymapContent::VSCode => Self::VSCode,
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
            BaseKeymap::Emacs => BaseKeymapContent::Emacs,
            BaseKeymap::None => BaseKeymapContent::None,
        }
    }
}

impl Display for BaseKeymap {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BaseKeymap::VSCode => write!(f, "VS Code"),
            BaseKeymap::Emacs => write!(f, "Emacs"),
            BaseKeymap::None => write!(f, "None"),
        }
    }
}

impl BaseKeymap {
    pub const OPTIONS: [(&'static str, Self); 2] =
        [("VS Code", Self::VSCode), ("Emacs (Default)", Self::Emacs)];

    pub fn asset_path(&self) -> Option<&'static str> {
        #[cfg(target_os = "macos")]
        match self {
            BaseKeymap::Emacs => Some("keymaps/macos/emacs.json"),
            BaseKeymap::VSCode => None,
            BaseKeymap::None => None,
        }

        #[cfg(not(target_os = "macos"))]
        match self {
            BaseKeymap::Emacs => Some("keymaps/linux/emacs.json"),
            BaseKeymap::VSCode => None,
            BaseKeymap::None => None,
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
            BaseKeymapContent::VSCode,
            BaseKeymapContent::None,
        ] {
            assert!(!kept.is_retired());
        }
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
