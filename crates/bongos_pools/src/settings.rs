use std::path::PathBuf;
use std::time::Duration;

use settings::{RegisterSetting, Settings, SettingsContent};

const DEFINED: &str =
    "default.json defines every `pools`, `queues`, `triggers` and `hooks` setting";

/// The `pools` settings (user level only). Pools themselves, their sizes and guards are declared
/// by a project; with nothing declared nothing waits.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct PoolsSettings {
    /// Whether pools, queues, triggers and lane job hooks are on (off by default). While false,
    /// nothing of this feature is registered and nothing waits on a pool.
    pub enabled: bool,
    /// The machine-wide pool store's directory; `None` is `pools` in the state directory.
    pub machine_store_dir: Option<PathBuf>,
    pub poll_interval: Duration,
    pub lock_timeout: Duration,
    pub mixed_head: Duration,
    pub guard_interval: Duration,
    pub guard_settle: Duration,
    pub guard_script_timeout: Duration,
    /// How long a hold may last when its pool sets no `max_hold`; `None` is no limit.
    pub max_hold: Option<Duration>,
}

impl Settings for PoolsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let pools = content.pools.clone().expect(DEFINED);
        Self {
            enabled: pools.enabled.expect(DEFINED),
            machine_store_dir: pools
                .machine_store_dir
                .filter(|dir| !dir.trim().is_empty())
                .map(PathBuf::from),
            poll_interval: Duration::from_millis(pools.poll_interval_ms.expect(DEFINED)),
            lock_timeout: Duration::from_secs(pools.lock_timeout_seconds.expect(DEFINED)),
            mixed_head: Duration::from_secs(pools.mixed_head_seconds.expect(DEFINED)),
            guard_interval: Duration::from_secs(pools.guard_interval_seconds.expect(DEFINED)),
            guard_settle: Duration::from_secs(pools.guard_settle_seconds.expect(DEFINED)),
            guard_script_timeout: Duration::from_secs(
                pools.guard_script_timeout_seconds.expect(DEFINED),
            ),
            max_hold: pools
                .max_hold
                .map(|minutes| Duration::from_secs(minutes.saturating_mul(60))),
        }
    }
}

/// The `queues` settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct QueuesSettings {
    /// Attempts at an item when its queue sets no `max_attempts`.
    pub max_attempts: u32,
    /// Failures kept with each item.
    pub failures_kept: u32,
}

impl Settings for QueuesSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let queues = content.queues.clone().expect(DEFINED);
        Self {
            max_attempts: queues.max_attempts.expect(DEFINED),
            failures_kept: queues.failures_kept.expect(DEFINED),
        }
    }
}

/// The `triggers` settings.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct TriggersSettings {
    /// Whether the engine host keeps running while a trigger may still fire.
    pub keep_host_alive: bool,
    /// How late a missed timer may still fire.
    pub scheduled_tolerance: Duration,
    /// Firings kept in each trigger's history.
    pub history_limit: u32,
    /// How long the checks before a scheduled firing may take.
    pub preflight_timeout: Duration,
}

impl Settings for TriggersSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let triggers = content.triggers.clone().expect(DEFINED);
        Self {
            keep_host_alive: triggers.keep_host_alive.expect(DEFINED),
            scheduled_tolerance: Duration::from_secs(
                u64::from(triggers.scheduled_tolerance_minutes.expect(DEFINED)) * 60,
            ),
            history_limit: triggers.history_limit.expect(DEFINED),
            preflight_timeout: Duration::from_secs(
                triggers.preflight_timeout_seconds.expect(DEFINED),
            ),
        }
    }
}

/// The `hooks` settings of the lane job hooks.
#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct HooksSettings {
    /// How long a hook may run when it sets no `timeout`.
    pub timeout: Duration,
    /// How much of a hook's output is kept at its end, in bytes.
    pub output_tail_bytes: u64,
}

impl Settings for HooksSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let hooks = content.hooks.clone().expect(DEFINED);
        Self {
            timeout: Duration::from_secs(hooks.timeout_seconds.expect(DEFINED)),
            output_tail_bytes: hooks.output_tail_kb.expect(DEFINED).saturating_mul(1024),
        }
    }
}
