//! The `extensions` actions of language extensions and the `language_extensions` settings. While
//! `language_extensions.enabled` is false the six actions are hidden from the command palette and
//! each only says "language extensions are off" in the echo area, and nothing on disk is read or
//! touched.

pub mod build_log;
pub mod git;
pub mod host;
pub mod installer;
pub mod reconcile_run;

use std::any::TypeId;

use bongos_minibuffer::message;
use command_palette_hooks::CommandPaletteFilter;
use gpui::{App, Global, TaskExt as _};
use schemars::JsonSchema;
use serde::Deserialize;
use settings::{RegisterSetting, Settings, SettingsContent, SettingsStore};
use workspace::Workspace;

/// What the echo area says while the setting is off.
pub const OFF_MESSAGE: &str = "language extensions are off";

/// The `language_extensions` settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct LanguageExtensionsSettings {
    /// Whether installing language extensions from git is on.
    pub enabled: bool,
    /// Whether to offer an extension for a file of an unknown language.
    pub offer: bool,
    /// Languages never offered an extension.
    pub never_offer: Vec<String>,
}

impl Settings for LanguageExtensionsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let settings = content
            .extension
            .language_extensions
            .clone()
            .expect("default.json defines `language_extensions`");
        Self {
            enabled: settings
                .enabled
                .expect("default.json defines `language_extensions.enabled`"),
            offer: settings
                .offer
                .expect("default.json defines `language_extensions.offer`"),
            never_offer: settings
                .never_offer
                .expect("default.json defines `language_extensions.never_offer`"),
        }
    }
}

/// Installs a language extension from a git repository.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = extensions)]
#[serde(deny_unknown_fields)]
pub struct InstallFromGit {
    /// The repository URL. Default: asks for it.
    #[serde(default)]
    pub url: Option<String>,
    /// The branch, tag or commit to install. Default: the default branch.
    #[serde(default)]
    pub rev: Option<String>,
    /// The subdirectory of the repository that holds the extension.
    #[serde(default)]
    pub path: Option<String>,
}

/// Installs the language extension for a language from the curated list.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = extensions)]
#[serde(deny_unknown_fields)]
pub struct InstallLanguage {
    /// The language name. Default: asks for it.
    #[serde(default)]
    pub name: Option<String>,
}

/// Updates an installed language extension.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = extensions)]
#[serde(deny_unknown_fields)]
pub struct Update {
    /// The extension id. Default: asks for it.
    #[serde(default)]
    pub id: Option<String>,
    /// Whether to move to the latest revision instead of the pinned one.
    #[serde(default)]
    pub latest: Option<bool>,
}

/// Rebuilds an installed language extension.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = extensions)]
#[serde(deny_unknown_fields)]
pub struct Rebuild {
    /// The extension id. Default: asks for it.
    #[serde(default)]
    pub id: Option<String>,
}

/// Removes an installed language extension.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = extensions)]
#[serde(deny_unknown_fields)]
pub struct Remove {
    /// The extension id. Default: asks for it.
    #[serde(default)]
    pub id: Option<String>,
}

/// Shows the build log of a language extension.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = extensions)]
#[serde(deny_unknown_fields)]
pub struct ShowBuildLog {
    /// The extension id. Default: asks for it.
    #[serde(default)]
    pub id: Option<String>,
}

/// Which of the six actions ran.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionKind {
    InstallFromGit(InstallFromGit),
    InstallLanguage(InstallLanguage),
    Update(Update),
    Rebuild(Rebuild),
    Remove(Remove),
    ShowBuildLog(ShowBuildLog),
}

/// Every action of language extensions, for hiding them from the command palette.
pub fn action_types() -> Vec<TypeId> {
    vec![
        TypeId::of::<InstallFromGit>(),
        TypeId::of::<InstallLanguage>(),
        TypeId::of::<Update>(),
        TypeId::of::<Rebuild>(),
        TypeId::of::<Remove>(),
        TypeId::of::<ShowBuildLog>(),
    ]
}

/// Runs an action that got past the setting check. Nothing happens here yet.
fn dispatch(_kind: ActionKind, _window: &mut gpui::Window, _cx: &mut App) {}

fn handle(kind: ActionKind, window: &mut gpui::Window, cx: &mut App) {
    if !LanguageExtensionsSettings::get_global(cx).enabled {
        message(window, cx, OFF_MESSAGE);
        return;
    }
    dispatch(kind, window, cx);
}

/// Registers the settings and the handlers of the six actions on every workspace.
pub fn init(cx: &mut App) {
    LanguageExtensionsSettings::register(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        macro_rules! register {
            ($($action:ident),*) => {$(
                workspace.register_action(|_, action: &$action, window, cx| {
                    handle(ActionKind::$action(action.clone()), window, cx);
                });
            )*};
        }
        register!(
            InstallFromGit,
            InstallLanguage,
            Update,
            Rebuild,
            Remove,
            ShowBuildLog
        );
    })
    .detach();
}

/// Hides the actions from the command palette while `language_extensions.enabled` is false and
/// shows them while it is true. Call after `command_palette_hooks::init` and [`init`].
pub fn init_palette(cx: &mut App) {
    sync_palette(cx);
    cx.observe_global::<SettingsStore>(sync_palette).detach();
}

/// Repairs what a crash left in the extensions directory, once, as soon as
/// `language_extensions.enabled` is true: at start, or when it is turned on later. Does nothing
/// while it is off. Call after `extension_host::init` and [`init`].
pub fn init_reconcile(cx: &mut App) {
    reconcile_once_enabled(cx);
    cx.observe_global::<SettingsStore>(reconcile_once_enabled)
        .detach();
}

struct Reconciled;

impl Global for Reconciled {}

fn reconcile_once_enabled(cx: &mut App) {
    if !LanguageExtensionsSettings::get_global(cx).enabled || cx.has_global::<Reconciled>() {
        return;
    }
    cx.set_global(Reconciled);
    let installer = installer::Installer::global(cx);
    installer
        .update(cx, |installer, cx| installer.reconcile(cx))
        .detach_and_log_err(cx);
}

fn sync_palette(cx: &mut App) {
    let enabled = LanguageExtensionsSettings::get_global(cx).enabled;
    let types = action_types();
    CommandPaletteFilter::update_global(cx, |filter, _| {
        if enabled {
            filter.show_action_types(&types);
        } else {
            filter.hide_action_types(&types);
        }
    });
}

#[cfg(test)]
mod tests;
