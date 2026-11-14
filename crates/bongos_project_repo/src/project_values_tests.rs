use std::sync::Arc;

use bongos_project_repo_model::DefinitionTree;
use bongos_work_model::ProjectFile;
use tempfile::TempDir;

use crate::project_values::{PUSH_RETRY_MIN_KEY, QUEUE_LIMIT_KEY};
use crate::snapshot_tests::{project, writer};
use crate::{
    OVERRIDABLE_PROJECT_KEYS, ProjectValueOverride, ProjectValueOverrides, SNAPSHOT_INTERVAL_KEY,
    ValueSource,
};

/// An override named `name` that gives `value` for `key` of every project.
pub(crate) struct Fixed {
    name: String,
    key: String,
    value: toml::Value,
}

pub(crate) fn fixed(name: &str, key: &str, value: toml::Value) -> Arc<dyn ProjectValueOverride> {
    Arc::new(Fixed {
        name: name.to_string(),
        key: key.to_string(),
        value,
    })
}

impl ProjectValueOverride for Fixed {
    fn name(&self) -> &str {
        &self.name
    }

    fn value(&self, _project_id: &str, key: &str) -> Option<toml::Value> {
        (key == self.key).then(|| self.value.clone())
    }
}

/// Gives the project's `project.toml` the `[repository]` table `table`.
fn project_file(repo: &crate::ProjectRepo, table: toml::Table) {
    let mut project = ProjectFile::new("Invoicing");
    project
        .extra
        .insert("repository".to_string(), toml::Value::Table(table));
    project.extra.insert(
        "approval".to_string(),
        toml::Value::Table(toml::toml! { policy = "person" }),
    );
    writer(repo).set_definitions(Arc::new(DefinitionTree {
        project: Some(project),
        ..DefinitionTree::default()
    }));
}

#[test]
fn a_project_value_comes_from_an_override_then_project_toml_then_the_user_default() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    let overrides = Arc::new(ProjectValueOverrides::new());
    repo.use_value_overrides(overrides.clone());

    let default = repo.project_value(SNAPSHOT_INTERVAL_KEY);
    assert_eq!(default.source, ValueSource::UserDefault);
    assert_eq!(default.value, Some(toml::Value::Integer(15)));

    project_file(&repo, toml::toml! { snapshot_interval_minutes = 30 });
    let from_file = repo.project_value(SNAPSHOT_INTERVAL_KEY);
    assert_eq!(from_file.source, ValueSource::ProjectFile);
    assert_eq!(repo.snapshot_interval_minutes(), 30);

    overrides.register(fixed(
        "lane",
        SNAPSHOT_INTERVAL_KEY,
        toml::Value::Integer(5),
    ));
    let overridden = repo.project_value(SNAPSHOT_INTERVAL_KEY);
    assert_eq!(overridden.source, ValueSource::Override("lane".to_string()));
    assert_eq!(repo.snapshot_interval_minutes(), 5);
    // Other keys keep their own order.
    assert_eq!(
        repo.project_value(PUSH_RETRY_MIN_KEY).source,
        ValueSource::UserDefault
    );
    assert_eq!(
        repo.project_value(QUEUE_LIMIT_KEY).value,
        Some(toml::Value::Integer(120))
    );
}

#[test]
fn an_override_registered_twice_by_name_is_kept_once() {
    let overrides = ProjectValueOverrides::new();
    overrides.register(fixed(
        "lane",
        SNAPSHOT_INTERVAL_KEY,
        toml::Value::Integer(5),
    ));
    overrides.register(fixed(
        "lane",
        SNAPSHOT_INTERVAL_KEY,
        toml::Value::Integer(9),
    ));
    overrides.register(fixed(
        "other",
        SNAPSHOT_INTERVAL_KEY,
        toml::Value::Integer(7),
    ));
    let names: Vec<String> = overrides
        .all()
        .iter()
        .map(|value_override| value_override.name().to_string())
        .collect();
    assert_eq!(names, ["lane", "other"]);
}

#[test]
fn approval_landing_checks_and_other_keys_are_never_overridden() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    project_file(&repo, toml::Table::new());
    let overrides = Arc::new(ProjectValueOverrides::new());
    for key in [
        "approval.policy",
        "approval",
        "landing.checks",
        "title",
        "repository.state_branch",
    ] {
        overrides.register(fixed(
            &format!("sneaky {key}"),
            key,
            toml::Value::String("anything".to_string()),
        ));
    }
    repo.use_value_overrides(overrides);

    let policy = repo.project_value("approval.policy");
    assert_eq!(policy.source, ValueSource::ProjectFile);
    assert_eq!(
        policy.value,
        Some(toml::Value::String("person".to_string()))
    );
    assert_eq!(policy.ignored_overrides, ["sneaky approval.policy"]);
    for key in ["landing.checks", "title", "repository.state_branch"] {
        let value = repo.project_value(key);
        assert!(!matches!(value.source, ValueSource::Override(_)), "{key}");
        assert_eq!(value.ignored_overrides.len(), 1, "{key}");
    }
    assert!(!OVERRIDABLE_PROJECT_KEYS.contains(&"approval.policy"));
    assert!(OVERRIDABLE_PROJECT_KEYS.contains(&SNAPSHOT_INTERVAL_KEY));
}

#[test]
fn a_nonsense_interval_falls_back_to_at_least_a_minute() {
    let root = TempDir::new().unwrap();
    let repo = project(&root);
    for (value, expected) in [
        (toml::Value::Integer(0), 1),
        (toml::Value::Integer(-5), 15),
        (toml::Value::String("often".to_string()), 15),
        (toml::Value::Integer(i64::MAX), 15),
    ] {
        let mut table = toml::Table::new();
        table.insert("snapshot_interval_minutes".to_string(), value.clone());
        project_file(&repo, table);
        assert_eq!(repo.snapshot_interval_minutes(), expected, "{value}");
    }
}
