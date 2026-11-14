//! Project values: a key of a project's `project.toml` read through, in order, a registered
//! [`ProjectValueOverride`] (only for [`OVERRIDABLE_PROJECT_KEYS`]), the `project.toml` of the
//! verified commit, and the user's default from the settings. `[approval]`, `[[landing.checks]]`
//! and every other key are never overridden: an override offered for one is ignored and logged.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::create::ProjectRepo;

pub const SNAPSHOT_INTERVAL_KEY: &str = "repository.snapshot_interval_minutes";
pub const PUSH_RETRY_MIN_KEY: &str = "repository.push_retry_min_minutes";
pub const PUSH_RETRY_MAX_KEY: &str = "repository.push_retry_max_minutes";
pub const RUN_RECORD_COPY_INTERVAL_KEY: &str = "repository.run_record_copy_interval_minutes";
pub const QUEUE_LIMIT_KEY: &str = "landing.queue_limit_seconds";

/// The only keys an override may change.
pub const OVERRIDABLE_PROJECT_KEYS: [&str; 5] = [
    SNAPSHOT_INTERVAL_KEY,
    PUSH_RETRY_MIN_KEY,
    PUSH_RETRY_MAX_KEY,
    RUN_RECORD_COPY_INTERVAL_KEY,
    QUEUE_LIMIT_KEY,
];

/// Gives a value of a project's key in place of its `project.toml` (a feature that runs the
/// project differently, such as a lane).
pub trait ProjectValueOverride: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// The value of `key` for the project `project_id`, `None` to leave it alone.
    fn value(&self, project_id: &str, key: &str) -> Option<toml::Value>;
}

/// The registered overrides.
pub struct ProjectValueOverrides {
    overrides: Mutex<Vec<Arc<dyn ProjectValueOverride>>>,
}

static GLOBAL_OVERRIDES: ProjectValueOverrides = ProjectValueOverrides::new();

impl Default for ProjectValueOverrides {
    fn default() -> Self {
        Self::new()
    }
}

impl ProjectValueOverrides {
    pub const fn new() -> Self {
        Self {
            overrides: Mutex::new(Vec::new()),
        }
    }

    pub fn global() -> &'static Self {
        &GLOBAL_OVERRIDES
    }

    /// Adds `value_override`, unless one with its name is registered already.
    pub fn register(&self, value_override: Arc<dyn ProjectValueOverride>) {
        let mut overrides = self.overrides.lock();
        if overrides
            .iter()
            .any(|known| known.name() == value_override.name())
        {
            return;
        }
        overrides.push(value_override);
    }

    pub fn all(&self) -> Vec<Arc<dyn ProjectValueOverride>> {
        self.overrides.lock().clone()
    }
}

/// Where a project value came from.
#[derive(Clone, Debug, PartialEq)]
pub enum ValueSource {
    /// The override with this name.
    Override(String),
    ProjectFile,
    UserDefault,
}

/// A project value and where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectValue {
    pub value: Option<toml::Value>,
    pub source: ValueSource,
    /// Overrides that offered a value for a key no override may change; they were ignored.
    pub ignored_overrides: Vec<String>,
}

impl ProjectValue {
    /// The value as a whole number of at least `minimum`, `fallback` when it is not one.
    pub fn as_u32(&self, fallback: u32, minimum: u32) -> u32 {
        self.value
            .as_ref()
            .and_then(toml::Value::as_integer)
            .and_then(|number| u32::try_from(number).ok())
            .unwrap_or(fallback)
            .max(minimum)
    }
}

/// `key` (dotted) in `table`.
fn lookup(table: &toml::Table, key: &str) -> Option<toml::Value> {
    let mut parts = key.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.as_table()?.get(part)?;
    }
    Some(value.clone())
}

impl ProjectRepo {
    /// The value of `key` for this project, with the overrides registered in the app.
    pub fn project_value(&self, key: &str) -> ProjectValue {
        match self.value_overrides() {
            Some(overrides) => self.project_value_with(key, &overrides),
            None => self.project_value_with(key, ProjectValueOverrides::global()),
        }
    }

    /// [`Self::project_value`] with `overrides`.
    pub fn project_value_with(&self, key: &str, overrides: &ProjectValueOverrides) -> ProjectValue {
        let project_id = &self.location.id;
        let overridable = OVERRIDABLE_PROJECT_KEYS.contains(&key);
        let mut ignored_overrides = Vec::new();
        for value_override in overrides.all() {
            let Some(value) = value_override.value(project_id, key) else {
                continue;
            };
            if !overridable {
                log::warn!(
                    "{} tried to override {key} of project {project_id}, which no override may change",
                    value_override.name()
                );
                ignored_overrides.push(value_override.name().to_string());
                continue;
            }
            return ProjectValue {
                value: Some(value),
                source: ValueSource::Override(value_override.name().to_string()),
                ignored_overrides,
            };
        }
        if let Some(value) = self.project_file_value(key) {
            return ProjectValue {
                value: Some(value),
                source: ValueSource::ProjectFile,
                ignored_overrides,
            };
        }
        ProjectValue {
            value: self.user_default(key),
            source: ValueSource::UserDefault,
            ignored_overrides,
        }
    }

    /// `key` in the `project.toml` of the definitions the writer holds (the verified commit's).
    fn project_file_value(&self, key: &str) -> Option<toml::Value> {
        let writer = self.state.as_ref()?.writer()?;
        let project = writer.definitions().project.clone()?;
        let table = match toml::Value::try_from(&project).ok()? {
            toml::Value::Table(table) => table,
            _ => return None,
        };
        lookup(&table, key)
    }

    fn user_default(&self, key: &str) -> Option<toml::Value> {
        let options = self.backup_options();
        let number = match key {
            SNAPSHOT_INTERVAL_KEY => options.default_snapshot_interval_minutes,
            PUSH_RETRY_MIN_KEY => options.push_retry_min_minutes,
            PUSH_RETRY_MAX_KEY => options.push_retry_max_minutes,
            RUN_RECORD_COPY_INTERVAL_KEY => options.run_record_copy_interval_minutes,
            QUEUE_LIMIT_KEY => {
                u32::try_from(self.land_options().queue_limit.as_secs()).unwrap_or(u32::MAX)
            }
            _ => return None,
        };
        Some(toml::Value::Integer(i64::from(number)))
    }
}
