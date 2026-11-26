use settings::{RegisterSetting, Settings, SettingsContent};

/// The `inbox` settings (user level only: the inbox is per machine).
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct InboxSettings {
    /// While false, nothing under `~/.bongos/inbox` is created, read or watched.
    pub enabled: bool,
}

impl Settings for InboxSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let inbox = content.inbox.clone().expect("default.json defines `inbox`");
        Self {
            enabled: inbox.enabled.expect("default.json defines `inbox.enabled`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use settings::SettingsStore;

    #[gpui::test]
    fn the_inbox_is_on_by_default(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            InboxSettings::register(cx);
            assert!(InboxSettings::get_global(cx).enabled);
        });
    }
}
