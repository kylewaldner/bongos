use std::sync::Arc;

use collections::HashMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings_macros::{MergeFrom, with_fallible_options};

#[with_fallible_options]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct ExtensionSettingsContent {
    /// The extensions that should be automatically installed by bongos.
    ///
    /// This is used to make functionality provided by extensions (e.g., language support)
    /// available out-of-the-box.
    ///
    /// Default: { "html": true }
    #[serde(default)]
    pub auto_install_extensions: HashMap<Arc<str>, bool>,
    #[serde(default)]
    pub auto_update_extensions: HashMap<Arc<str>, bool>,
    /// The capabilities granted to extensions.
    pub granted_extension_capabilities: Option<Vec<ExtensionCapabilityContent>>,
    /// Installing language extensions from git.
    pub language_extensions: Option<LanguageExtensionsSettingsContent>,
}

/// The `language_extensions` settings.
#[with_fallible_options]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize, JsonSchema, MergeFrom)]
pub struct LanguageExtensionsSettingsContent {
    /// Whether installing language extensions from git is on. While false, the
    /// `extensions:` commands are hidden and only say that language extensions are off.
    ///
    /// Default: true
    pub enabled: Option<bool>,
    /// Whether bongos offers to install a language extension when you open a file of a
    /// language it does not know.
    ///
    /// Default: true
    pub offer: Option<bool>,
    /// Languages bongos never offers an extension for.
    ///
    /// Default: []
    pub never_offer: Option<Vec<String>>,
}

/// A capability for an extension.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionCapabilityContent {
    #[serde(rename = "process:exec")]
    ProcessExec {
        /// The command to execute.
        command: String,
        /// The arguments to pass to the command. Use `*` for a single wildcard argument.
        /// If the last element is `**`, then any trailing arguments are allowed.
        args: Vec<String>,
    },
    DownloadFile {
        host: String,
        path: Vec<String>,
    },
    #[serde(rename = "npm:install")]
    NpmInstallPackage {
        package: String,
    },
}
