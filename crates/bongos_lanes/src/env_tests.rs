//! A lane job's environment: the binding's `env`, then the profile's edits.

use std::path::{Path, PathBuf};

use bongos_lanes_model::{EnvEdit, LaneId, TokenValues};

use crate::env::{apply_env, second_tree_env, tip_env};
use crate::locations::{binding_of, resolve_lane};
use crate::locations_tests::{BUILD_TABLE, FakeProfile, inputs, table};

fn values() -> TokenValues {
    TokenValues {
        lane: "build-1".to_string(),
        kind: "build".to_string(),
        lane_index: 1,
        project: "0123456789ab".to_string(),
        repo: "app".to_string(),
        live_lanes: 2,
        cpus: 8,
        dirs: Default::default(),
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

#[test]
fn the_profile_edits_apply_after_the_binding_env() {
    let table = table(BUILD_TABLE);
    let lane = LaneId::new("build", 1);
    let profile = FakeProfile {
        env: vec![
            EnvEdit::Set {
                name: "BUILD_DIR".to_string(),
                value: "/profile/target".to_string(),
            },
            EnvEdit::Unset {
                name: "SCRATCH".to_string(),
            },
            EnvEdit::Unset {
                name: "SECRET".to_string(),
            },
            EnvEdit::PrependPath {
                dirs: vec![PathBuf::from("/opt/tools/bin"), PathBuf::from("/opt/more")],
            },
        ],
        ..FakeProfile::default()
    };
    let locations = resolve_lane(
        &inputs(Path::new("/w")),
        &table.locations,
        binding_of(&table, &lane),
        &lane,
        Some(&profile),
    )
    .unwrap();
    let binding = binding_of(&table, &lane).unwrap();
    let values = TokenValues {
        dirs: locations.dir_values(&[]),
        ..values()
    };
    let edits = tip_env(binding, &values, &profile.env).unwrap();
    assert_eq!(
        edits[..2],
        [
            EnvEdit::Set {
                name: "BUILD_DIR".to_string(),
                value: "/w/project/lanes/build-1/target".to_string(),
            },
            EnvEdit::Set {
                name: "SCRATCH".to_string(),
                value: "/w/project/lanes/build-1/scratch".to_string(),
            },
        ]
    );
    assert_eq!(edits[2..], profile.env[..]);

    let base = pairs(&[("HOME", "/home/u"), ("PATH", "/usr/bin"), ("SECRET", "s")]);
    assert_eq!(
        apply_env(base, &edits),
        pairs(&[
            ("HOME", "/home/u"),
            ("PATH", "/opt/tools/bin:/opt/more:/usr/bin"),
            ("BUILD_DIR", "/profile/target"),
        ])
    );
}

#[test]
fn prepend_path_starts_a_missing_path() {
    let edits = [EnvEdit::PrependPath {
        dirs: vec![PathBuf::from("/opt/bin")],
    }];
    assert_eq!(
        apply_env(Vec::new(), &edits),
        pairs(&[("PATH", "/opt/bin")])
    );
}

#[test]
fn the_second_tree_env_uses_the_base_directories() {
    let table = table(BUILD_TABLE);
    let lane = LaneId::new("build", 1);
    let locations = resolve_lane(
        &inputs(Path::new("/w")),
        &table.locations,
        binding_of(&table, &lane),
        &lane,
        None,
    )
    .unwrap();
    let binding = binding_of(&table, &lane).unwrap();
    let edits = second_tree_env(binding, "app", &locations, &values(), &[]).unwrap();
    assert_eq!(
        edits,
        [
            EnvEdit::Set {
                name: "BUILD_DIR".to_string(),
                value: "/w/project/lanes/build-1/target-base".to_string(),
            },
            // scratch is not in base_tree.dirs, so the second tree shares the tip's.
            EnvEdit::Set {
                name: "SCRATCH".to_string(),
                value: "/w/project/lanes/build-1/scratch".to_string(),
            },
        ]
    );
}
