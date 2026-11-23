use crate::binding::{TokenError, TokenValues, check_tokens, expand_tokens};

fn values(cpus: u32, live_lanes: u32) -> TokenValues {
    TokenValues {
        lane: "build-2".into(),
        kind: "build".into(),
        lane_index: 2,
        project: "0123456789ab".into(),
        repo: "app".into(),
        live_lanes,
        cpus,
        dirs: [("target".to_string(), "/x/target".to_string())].into(),
    }
}

#[test]
fn expands_every_token() {
    let text = "{lane}|{kind}|{lane_index}|{project}|{repo}|{live_lanes}|{cpus}|{dir.target}";
    assert_eq!(
        expand_tokens(text, &values(24, 3)).unwrap(),
        "build-2|build|2|0123456789ab|app|3|24|/x/target"
    );
    assert_eq!(expand_tokens("plain", &values(1, 1)).unwrap(), "plain");
}

#[test]
fn division_floors_and_is_at_least_one() {
    assert_eq!(
        expand_tokens("{cpus/live_lanes}", &values(24, 3)).unwrap(),
        "8"
    );
    assert_eq!(
        expand_tokens("-j{cpus/live_lanes}", &values(25, 3)).unwrap(),
        "-j8"
    );
    assert_eq!(
        expand_tokens("{cpus/live_lanes}", &values(2, 3)).unwrap(),
        "1"
    );
    assert_eq!(expand_tokens("{cpus/2}", &values(9, 1)).unwrap(), "4");
}

#[test]
fn unknown_tokens_are_errors() {
    assert_eq!(
        expand_tokens("{nope}", &values(1, 1)),
        Err(TokenError::Unknown("nope".into()))
    );
    assert!(expand_tokens("{dir.missing}", &values(1, 1)).is_err());
    assert!(expand_tokens("{cpus/nope}", &values(1, 1)).is_err());
    assert!(matches!(
        expand_tokens("{cpus", &values(1, 1)),
        Err(TokenError::Unterminated(_))
    ));
}

#[test]
fn bad_divisions_are_errors() {
    assert!(matches!(
        expand_tokens("{cpus/0}", &values(4, 1)),
        Err(TokenError::Zero { .. })
    ));
    assert!(matches!(
        expand_tokens("{cpus/live_lanes}", &values(4, 0)),
        Err(TokenError::Zero { .. })
    ));
    assert!(matches!(
        expand_tokens("{0/2}", &values(4, 1)),
        Err(TokenError::Zero { .. })
    ));
    assert!(matches!(
        expand_tokens("{lane/2}", &values(4, 1)),
        Err(TokenError::NonNumeric { .. })
    ));
    assert!(matches!(
        expand_tokens("{cpus/dir.target}", &values(4, 1)),
        Err(TokenError::NonNumeric { .. })
    ));
}

#[test]
fn check_tokens_uses_declared_directories() {
    assert!(check_tokens("{dir.target} {cpus/live_lanes}", &["target"]).is_ok());
    assert!(check_tokens("{dir.logs}", &["target"]).is_err());
    assert!(check_tokens("{bogus}", &[]).is_err());
}
