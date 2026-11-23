use std::time::Duration;

use settings::{RegisterSetting, Settings, SettingsContent};

pub use settings::settings_content::{LanesOnInterruption, LanesOnLastWindowClose};

/// The `lanes` settings (user level only). Project values (kinds, counts, rules) live in the
/// project's `lanes.toml`; machine values come from the project profile.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct LanesSettings {
    /// Whether lanes are on (on by default). While false, no engine host starts, no lane takes
    /// work, and every lane command is hidden and only says "Lanes are off (lanes.enabled)".
    pub enabled: bool,
    /// The time between two looks at the project for work a lane could take.
    pub reevaluate_interval: Duration,
    /// Whether the engine host exits once every lane is idle and nothing is left to take.
    pub exit_when_idle: bool,
    /// What happens to a lane run that an interruption stopped, unless `lanes.toml` says.
    pub on_interruption: LanesOnInterruption,
    /// What closing the last window does while lanes run in it.
    pub on_last_window_close: LanesOnLastWindowClose,
    /// Whether a lane run waiting on a person shows a desktop notification.
    pub desktop_notifications: bool,
    /// The most picks `lanes: dry run` lists.
    pub dry_run_limit: u32,
    /// How long a staleness script may run before it counts as failed.
    pub staleness_timeout: Duration,
    /// The largest file kept when a lane tree is saved before a reset.
    pub save_max_file_bytes: u64,
    /// How the engine host behaves.
    pub host: LanesHostSettings,
}

/// The `lanes.host` settings.
#[derive(Clone, Debug, PartialEq)]
pub struct LanesHostSettings {
    /// How long an engine host with no lane running and no client waits before it exits.
    pub idle_exit: Duration,
    /// How long a window waits for a starting engine host to answer.
    pub start_timeout: Duration,
    /// How long a handover between a window and an engine host may take.
    pub handover_timeout: Duration,
    /// The largest the engine host's log grows before it is rotated.
    pub log_max_bytes: u64,
}

const DEFINED: &str = "default.json defines every `lanes` setting";

impl Settings for LanesSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let lanes = content.lanes.clone().expect(DEFINED);
        let host = lanes.host.expect(DEFINED);
        Self {
            enabled: lanes.enabled.expect(DEFINED),
            reevaluate_interval: Duration::from_secs(lanes.reevaluate_interval.expect(DEFINED)),
            exit_when_idle: lanes.exit_when_idle.expect(DEFINED),
            on_interruption: lanes.on_interruption.expect(DEFINED),
            on_last_window_close: lanes.on_last_window_close.expect(DEFINED),
            desktop_notifications: lanes.desktop_notifications.expect(DEFINED),
            dry_run_limit: lanes.dry_run_limit.expect(DEFINED),
            staleness_timeout: Duration::from_secs(lanes.staleness_timeout_seconds.expect(DEFINED)),
            save_max_file_bytes: lanes.save_max_file_bytes.expect(DEFINED),
            host: LanesHostSettings {
                idle_exit: Duration::from_secs(host.idle_exit_seconds.expect(DEFINED)),
                start_timeout: Duration::from_secs(host.start_timeout_seconds.expect(DEFINED)),
                handover_timeout: Duration::from_secs(
                    host.handover_timeout_seconds.expect(DEFINED),
                ),
                log_max_bytes: host.log_max_bytes.expect(DEFINED),
            },
        }
    }
}
