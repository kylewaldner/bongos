//! A lane job's environment: the binding's `env` (each a `Set`, its tokens expanded), then the
//! project profile's edits in order, so the profile has the last word.

use bongos_lanes_model::table::Binding;
use bongos_lanes_model::{EnvEdit, TokenError, TokenValues, expand_tokens};

use crate::locations::LaneLocations;

/// The edits of a job in the tip tree: the binding's `env` with `values` (whose `dirs` are the
/// lane's named directories), then `profile`'s edits.
pub fn tip_env(
    binding: &Binding,
    values: &TokenValues,
    profile: &[EnvEdit],
) -> Result<Vec<EnvEdit>, TokenError> {
    edits(binding, values, profile)
}

/// The edits of a job in `repo`'s second tree: as [`tip_env`], except that `{dir.<name>}` is the
/// `<name>-base` directory for each name in that tree's `base_tree.dirs`.
pub fn second_tree_env(
    binding: &Binding,
    repo: &str,
    locations: &LaneLocations,
    values: &TokenValues,
    profile: &[EnvEdit],
) -> Result<Vec<EnvEdit>, TokenError> {
    let base_dirs: Vec<String> = binding
        .trees
        .iter()
        .filter(|tree| tree.repo == repo)
        .filter_map(|tree| tree.base_tree.as_ref())
        .flat_map(|base| base.dirs.iter().map(|dir| dir.value.clone()))
        .collect();
    let values = TokenValues {
        dirs: locations.dir_values(&base_dirs),
        ..values.clone()
    };
    edits(binding, &values, profile)
}

fn edits(
    binding: &Binding,
    values: &TokenValues,
    profile: &[EnvEdit],
) -> Result<Vec<EnvEdit>, TokenError> {
    let mut edits = Vec::with_capacity(binding.env.len() + profile.len());
    for (name, value) in &binding.env {
        edits.push(EnvEdit::Set {
            name: name.clone(),
            value: expand_tokens(&value.value, values)?,
        });
    }
    edits.extend(profile.iter().cloned());
    Ok(edits)
}

/// `base` with `edits` applied in order: `Set` replaces, `Unset` removes, `PrependPath` puts its
/// directories before the current `PATH`. An unset variable is absent from the result.
pub fn apply_env(
    base: impl IntoIterator<Item = (String, String)>,
    edits: &[EnvEdit],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = base.into_iter().collect();
    let position = |env: &[(String, String)], name: &str| {
        env.iter().position(|(existing, _)| existing == name)
    };
    for edit in edits {
        match edit {
            EnvEdit::Set { name, value } => match position(&env, name) {
                Some(index) => env[index].1 = value.clone(),
                None => env.push((name.clone(), value.clone())),
            },
            EnvEdit::Unset { name } => env.retain(|(existing, _)| existing != name),
            EnvEdit::PrependPath { dirs } => {
                let mut parts: Vec<String> = dirs
                    .iter()
                    .map(|dir| dir.to_string_lossy().into_owned())
                    .collect();
                match position(&env, "PATH") {
                    Some(index) => {
                        if !env[index].1.is_empty() {
                            parts.push(env[index].1.clone());
                        }
                        env[index].1 = parts.join(":");
                    }
                    None => env.push(("PATH".to_string(), parts.join(":"))),
                }
            }
        }
    }
    env
}
