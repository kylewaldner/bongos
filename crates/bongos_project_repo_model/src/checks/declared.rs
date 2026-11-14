//! The part of the `schema` check that reads the project's `[[landing.checks]]` declarations: an
//! entry may not make a check amending, may only name an amending check to disable it, and may not
//! disable the checks that are always on.

use bongos_work_model::ProjectFile;

use super::outcome::CheckPhase;
use crate::definitions::Problem;
use crate::layout::PROJECT_FILE;

pub const APPROVAL_CHECK: &str = "approval";
pub const NO_STATE_CHECK: &str = "no-state";
pub const NO_CYCLE_CHECK: &str = "no-cycle";
pub const REFERENCES_CHECK: &str = "references";
pub const STARTED_UNCHANGED_CHECK: &str = "started-unchanged";
pub const SIZE_AND_KIND_CHECK: &str = "size-and-kind";
pub const SECRETS_CHECK: &str = "secrets";

/// The checks a `[[landing.checks]]` entry cannot disable.
pub const CANNOT_DISABLE: [&str; 3] = [APPROVAL_CHECK, NO_STATE_CHECK, super::SCHEMA_CHECK];

/// The keys a `[[landing.checks]]` entry may hold.
const ENTRY_KEYS: [&str; 5] = ["name", "applies", "phase", "run", "enabled"];

/// The problems of `project`'s `[[landing.checks]]` entries. `amending` names the registered
/// amending checks: an entry naming one may hold only `name` and `enabled = false`.
pub fn landing_declaration_problems(project: &ProjectFile, amending: &[&str]) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut refuse = |reason: String| {
        problems.push(Problem {
            path: PROJECT_FILE.to_string(),
            reason,
        })
    };
    let Some(landing) = project.extra.get("landing") else {
        return problems;
    };
    let Some(checks) = landing.get("checks") else {
        return problems;
    };
    let Some(entries) = checks.as_array() else {
        refuse("landing.checks must be a list of tables".to_string());
        return problems;
    };
    for (index, entry) in entries.iter().enumerate() {
        let at = format!("landing.checks entry {}", index + 1);
        let Some(table) = entry.as_table() else {
            refuse(format!("{at} must be a table"));
            continue;
        };
        let Some(name) = table.get("name").and_then(|name| name.as_str()) else {
            refuse(format!("{at} needs a name"));
            continue;
        };
        let at = format!("landing check {name}");
        for key in table.keys() {
            if !ENTRY_KEYS.contains(&key.as_str()) {
                refuse(format!("{at}: unknown key {key}"));
            }
        }
        let enabled = table.get("enabled");
        if enabled.is_some_and(|value| value.as_bool().is_none()) {
            refuse(format!("{at}: enabled must be true or false"));
        }
        let disabled = enabled.and_then(|value| value.as_bool()) == Some(false);
        match table.get("phase") {
            Some(phase) if phase.as_str() == Some("amending") => refuse(format!(
                "{at}: phase \"amending\" is not allowed; only checks bongos ships can amend"
            )),
            Some(phase)
                if !amending.contains(&name)
                    && phase.as_str().and_then(CheckPhase::parse).is_none() =>
            {
                refuse(format!(
                    "{at}: phase must be \"before-queue\" or \"in-queue\""
                ))
            }
            _ => {}
        }
        if amending.contains(&name) {
            let other_keys = table
                .keys()
                .any(|key| !["name", "enabled"].contains(&key.as_str()));
            if other_keys || !disabled {
                refuse(format!(
                    "{at} is an amending check; it may only be named to set enabled = false"
                ));
            }
        }
        if disabled && CANNOT_DISABLE.contains(&name) {
            refuse(format!("{at} cannot be disabled"));
        }
        if let Some(applies) = table.get("applies")
            && !applies_is_valid(applies)
        {
            refuse(format!(
                "{at}: applies must be \"always\", \"tasks\" or {{ paths = [...] }}"
            ));
        }
    }
    problems.dedup();
    problems
}

fn applies_is_valid(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(word) => word == "always" || word == "tasks",
        toml::Value::Table(table) => {
            table.len() == 1
                && table
                    .get("paths")
                    .and_then(|paths| paths.as_array())
                    .is_some_and(|paths| paths.iter().all(toml::Value::is_str))
        }
        _ => false,
    }
}
