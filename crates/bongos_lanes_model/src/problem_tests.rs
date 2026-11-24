use std::path::PathBuf;

use crate::{EnvEdit, Problem};

#[test]
fn problem_round_trips_through_serde() {
    let problem = Problem {
        requirement: "drive for target is mounted".into(),
        message: "/mnt/build is missing".into(),
        hint: Some("mount the drive".into()),
    };
    let json = serde_json::to_string(&problem).unwrap();
    assert_eq!(serde_json::from_str::<Problem>(&json).unwrap(), problem);

    let without_hint = Problem {
        hint: None,
        ..problem
    };
    let json = serde_json::to_string(&without_hint).unwrap();
    assert_eq!(
        serde_json::from_str::<Problem>(&json).unwrap(),
        without_hint
    );
}

#[test]
fn env_edit_round_trips_through_serde() {
    let edits = vec![
        EnvEdit::Set {
            name: "A".into(),
            value: "1".into(),
        },
        EnvEdit::Unset { name: "B".into() },
        EnvEdit::PrependPath {
            dirs: vec![PathBuf::from("/opt/bin"), PathBuf::from("tools")],
        },
    ];
    let json = serde_json::to_string(&edits).unwrap();
    assert_eq!(serde_json::from_str::<Vec<EnvEdit>>(&json).unwrap(), edits);
}
