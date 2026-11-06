use collections::HashMap;
use extension::{
    DownloadFileCapability, ExtensionCapability, NpmInstallPackageCapability, ProcessExecCapability,
};
use settings::{RegisterSetting, Settings};
use std::sync::Arc;

#[derive(Debug, Default, Clone, RegisterSetting)]
pub struct ExtensionSettings {
    /// Kept so imported Zed settings still parse. Nothing installs extensions automatically.
    pub auto_install_extensions: HashMap<Arc<str>, bool>,
    pub auto_update_extensions: HashMap<Arc<str>, bool>,
    pub granted_capabilities: Vec<ExtensionCapability>,
}

impl Settings for ExtensionSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            auto_install_extensions: content.extension.auto_install_extensions.clone(),
            auto_update_extensions: content.extension.auto_update_extensions.clone(),
            granted_capabilities: content
                .extension
                .granted_extension_capabilities
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|capability| match capability {
                    settings::ExtensionCapabilityContent::ProcessExec { command, args } => {
                        ExtensionCapability::ProcessExec(ProcessExecCapability { command, args })
                    }
                    settings::ExtensionCapabilityContent::DownloadFile { host, path } => {
                        ExtensionCapability::DownloadFile(DownloadFileCapability { host, path })
                    }
                    settings::ExtensionCapabilityContent::NpmInstallPackage { package } => {
                        ExtensionCapability::NpmInstallPackage(NpmInstallPackageCapability {
                            package,
                        })
                    }
                })
                .collect(),
        }
    }
}
