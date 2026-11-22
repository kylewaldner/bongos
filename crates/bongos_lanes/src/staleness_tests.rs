use crate::staleness::{Staleness, staleness_answer};

// The first output line decides; anything else names that line or the exit code.
#[test]
fn the_first_line_of_a_staleness_script_decides() {
    let cases = [
        ("stale\n", Some(0), Ok(Staleness::Stale)),
        ("fresh\nmore lines\n", Some(0), Ok(Staleness::Fresh)),
        ("  stale  \n", Some(1), Ok(Staleness::Stale)),
        ("maybe\nstale\n", Some(0), Err("maybe".to_string())),
        ("", Some(2), Err("exit code 2".to_string())),
        ("", None, Err("killed by a signal".to_string())),
    ];
    for (stdout, code, expected) in cases {
        assert_eq!(
            staleness_answer(stdout, code),
            expected,
            "{stdout:?} {code:?}"
        );
    }
}
