//! Leader rows derived from the Emacs keymap for the other keymap profiles.
//!
//! The Emacs keymap binds bongos's own commands under `ctrl-c`. Other profiles reach the same
//! commands under a leader of their own, derived row by row from the platform's Emacs keymap, so
//! a command bound there is bound under every profile without a second table to keep in sync.

use anyhow::Result;
use gpui::{App, KeyBinding};
use serde_json::{Map, Value};
use util::asset_str;

use crate::{KeybindSource, KeymapFile, KeymapFileLoadResult, SettingsAssets};

/// The Emacs prefix of a bongos command row.
const EMACS_PREFIX: &str = "ctrl-c";

/// How a profile spells the leading `ctrl-c` of a bongos command row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leader {
    /// `ctrl-k b` on Linux and `cmd-k b` on macOS, in the row's own context.
    VsCode,
    /// `space` in Vim's normal and visual modes.
    VimSpace,
}

impl Leader {
    fn keys(self, macos: bool) -> &'static str {
        match self {
            Leader::VsCode if macos => "cmd-k b",
            Leader::VsCode => "ctrl-k b",
            Leader::VimSpace => "space",
        }
    }

    fn context(self, context: &str) -> String {
        match self {
            Leader::VsCode => context.to_string(),
            Leader::VimSpace if context.trim() == "Workspace" => "VimControl && !menu".to_string(),
            Leader::VimSpace => format!("({context}) && VimControl && !menu"),
        }
    }
}

/// The name of a keymap action value, or `None` for `null` and malformed values.
fn action_name(action: &Value) -> Option<&str> {
    match action {
        Value::String(name) => Some(name),
        Value::Array(items) => items.first().and_then(Value::as_str),
        _ => None,
    }
}

/// Whether a binding is a bongos command row: two or more keys starting with `ctrl-c`, an action
/// that is not `null` and not in the `emacs` or `terminal` namespace, in a context that does not
/// mention `Terminal`.
fn is_command_row(context: &str, keystrokes: &str, action: &Value) -> bool {
    let mut keys = keystrokes.split_whitespace();
    if keys.next() != Some(EMACS_PREFIX) || keys.next().is_none() {
        return false;
    }
    if context.contains("Terminal") {
        return false;
    }
    let Some(name) = action_name(action) else {
        return false;
    };
    let namespace = name.split("::").next().unwrap_or_default();
    namespace != "emacs" && namespace != "terminal"
}

/// The keymap JSON that binds every bongos command row of `emacs_source` under `leader` for the
/// given platform (macOS when `macos` is true, Linux otherwise). There is one derived row per
/// source row, in source order, grouped by the source sections; action arguments are kept as is.
pub fn leader_bindings(emacs_source: &str, leader: Leader, macos: bool) -> Result<String> {
    let keymap = KeymapFile::parse(emacs_source)?;
    let leader_keys = leader.keys(macos);
    let mut sections = Vec::new();
    for section in keymap.sections() {
        let mut bindings = Map::new();
        for (keystrokes, action) in section.bindings() {
            if !is_command_row(&section.context, keystrokes, action.value()) {
                continue;
            }
            let rest = keystrokes
                .trim_start()
                .strip_prefix(EMACS_PREFIX)
                .unwrap_or_default()
                .trim_start();
            bindings.insert(format!("{leader_keys} {rest}"), action.value().clone());
        }
        if !bindings.is_empty() {
            let mut derived = Map::new();
            derived.insert(
                "context".into(),
                Value::String(leader.context(&section.context)),
            );
            derived.insert("bindings".into(), Value::Object(bindings));
            sections.push(Value::Object(derived));
        }
    }
    Ok(serde_json::to_string_pretty(&Value::Array(sections))?)
}

/// Loads the leader rows derived from the bundled Emacs keymap at `emacs_path`, bound with
/// `source`. The Emacs keymap is parsed once per call.
pub fn load_leader_asset(
    emacs_path: &str,
    leader: Leader,
    macos: bool,
    source: KeybindSource,
    cx: &App,
) -> Result<Vec<KeyBinding>> {
    let json = leader_bindings(
        asset_str::<SettingsAssets>(emacs_path).as_ref(),
        leader,
        macos,
    )?;
    match KeymapFile::load(&json, cx) {
        KeymapFileLoadResult::Success { mut key_bindings } => {
            for key_binding in &mut key_bindings {
                key_binding.set_meta(source.meta());
            }
            Ok(key_bindings)
        }
        KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
            anyhow::bail!(
                "Error loading the leader rows derived from \"{emacs_path}\": {error_message}"
            )
        }
        KeymapFileLoadResult::JsonParseFailure { error } => {
            anyhow::bail!(
                "JSON parse error in the leader rows derived from \"{emacs_path}\": {error}"
            )
        }
    }
}
