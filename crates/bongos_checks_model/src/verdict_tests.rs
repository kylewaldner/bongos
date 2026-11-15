use std::collections::BTreeMap;

use crate::decl::CheckDecl;
use crate::verdict::{
    Exit, NoVerdictReason as R, ResultFile, ScopeAnswer, Verdict, combine, judge,
    parse_plan_answer, parse_result_file, parse_scope_answer, union_failing,
};

fn decl(extra: &str) -> CheckDecl {
    let text = format!("name = \"tests\"\ncommand = [\"t\"]\n{extra}");
    toml::from_str(&text).unwrap()
}

fn result() -> Option<Result<ResultFile, String>> {
    Some(Ok(ResultFile {
        summary: "ok".to_string(),
        failing: Vec::new(),
        counts: BTreeMap::new(),
    }))
}

fn no(reason: R) -> Verdict {
    Verdict::NoVerdict(reason)
}

#[test]
fn a_pass_code_alone_passes() {
    assert_eq!(judge(Exit::Code(0), &decl(""), false, None), Verdict::Pass);
    let wide = decl("pass = [0, 2]\nfail = [1]\n");
    assert_eq!(judge(Exit::Code(2), &wide, false, None), Verdict::Pass);
}

#[test]
fn a_pass_needs_the_summary_line_when_declared() {
    let d = decl("summary = \"^ok\"\n");
    assert_eq!(judge(Exit::Code(0), &d, true, None), Verdict::Pass);
    assert_eq!(judge(Exit::Code(0), &d, false, None), no(R::MissingSummary));
}

#[test]
fn a_pass_needs_a_valid_result_when_declared() {
    let d = decl("result_file = true\n");
    assert_eq!(judge(Exit::Code(0), &d, false, result()), Verdict::Pass);
    assert!(matches!(
        judge(Exit::Code(0), &d, false, None),
        Verdict::NoVerdict(R::InvalidResult(_))
    ));
    assert_eq!(
        judge(Exit::Code(0), &d, false, Some(Err("bad".to_string()))),
        no(R::InvalidResult("bad".to_string()))
    );
    let empty = Some(Ok(ResultFile::default()));
    assert!(matches!(
        judge(Exit::Code(0), &d, false, empty),
        Verdict::NoVerdict(R::InvalidResult(_))
    ));
}

#[test]
fn pass_needs_both_summary_and_result_when_both_are_declared() {
    let d = decl("summary = \"ok\"\nresult_file = true\n");
    assert_eq!(judge(Exit::Code(0), &d, true, result()), Verdict::Pass);
    assert_eq!(
        judge(Exit::Code(0), &d, false, result()),
        no(R::MissingSummary)
    );
    assert!(!judge(Exit::Code(0), &d, true, None).is_pass());
}

#[test]
fn a_fail_code_fails_and_an_invalid_result_makes_it_no_verdict() {
    assert_eq!(judge(Exit::Code(1), &decl(""), false, None), Verdict::Fail);
    let d = decl("result_file = true\n");
    assert_eq!(judge(Exit::Code(1), &d, false, result()), Verdict::Fail);
    assert!(matches!(
        judge(Exit::Code(1), &d, false, None),
        Verdict::NoVerdict(R::InvalidResult(_))
    ));
    assert!(matches!(
        judge(Exit::Code(1), &d, false, Some(Err("x".to_string()))),
        Verdict::NoVerdict(R::InvalidResult(_))
    ));
}

#[test]
fn every_no_verdict_exit_reason_never_passes_or_fails() {
    let d = decl("environment = [3]\n");
    assert_eq!(
        judge(Exit::Code(3), &d, true, result()),
        no(R::Environment { code: 3 })
    );
    assert_eq!(
        judge(Exit::Code(7), &d, true, result()),
        no(R::UndeclaredExit { code: 7 })
    );
    assert_eq!(
        judge(Exit::Code(-1), &d, true, result()),
        no(R::UndeclaredExit { code: -1 })
    );
    assert_eq!(
        judge(Exit::Signal(9), &d, true, result()),
        no(R::Killed { signal: 9 })
    );
}

#[test]
fn a_code_in_two_lists_is_no_verdict() {
    let both = decl("pass = [0, 1]\nfail = [1]\n");
    assert_eq!(
        judge(Exit::Code(1), &both, true, None),
        no(R::UndeclaredExit { code: 1 })
    );
    let env_pass = decl("pass = [0, 3]\nenvironment = [3]\n");
    assert_eq!(
        judge(Exit::Code(3), &env_pass, true, None),
        no(R::Environment { code: 3 })
    );
}

#[test]
fn reasons_have_text_for_every_kind() {
    let reasons = [
        R::Environment { code: 3 },
        R::UndeclaredExit { code: 4 },
        R::Killed { signal: 9 },
        R::MissingSummary,
        R::InvalidResult("x".into()),
        R::ToolNotFound("cargo".into()),
        R::StartFailed("x".into()),
        R::TimeBudgetReached,
        R::PoolWaitLimit { pool: "gpu".into() },
        R::Cancelled,
        R::Interrupted,
        R::InvalidScope("x".into()),
        R::InvalidPlan("x".into()),
        R::TreeUnavailable("x".into()),
    ];
    for reason in reasons {
        assert!(!reason.text().is_empty());
    }
    assert!(
        R::PoolWaitLimit { pool: "gpu".into() }
            .text()
            .contains("never became available")
    );
}

#[test]
fn result_files_are_strict() {
    let ok = parse_result_file(
        br#"{"summary":"1 failed","failing":["a"],"counts":{"tests":3}}"#,
        1000,
    )
    .unwrap();
    assert_eq!(ok.failing, ["a"]);
    assert_eq!(ok.counts["tests"], 3);
    assert!(parse_result_file(br#"{"summary":"x"}"#, 1000).is_ok());
    for bad in [
        &br#"{}"#[..],
        br#"{"summary":""}"#,
        br#"{"summary":"  "}"#,
        br#"{"summary":"a\nb"}"#,
        br#"{"summary":"x","extra":1}"#,
        br#"{"summary":"x","counts":{"a":1.5}}"#,
        br#"{"summary":"x","failing":"a"}"#,
        br#"{"summary":"x","summary":"y"}"#,
        br#"not json"#,
        &[0xff, 0xfe],
    ] {
        assert!(parse_result_file(bad, 1000).is_err(), "{bad:?}");
    }
    assert!(parse_result_file(br#"{"summary":"x"}"#, 5).is_err());
}

fn scope(file: &str) -> Result<ScopeAnswer, R> {
    parse_scope_answer(Exit::Code(0), Some(file.as_bytes()), 1000)
}

#[test]
fn scope_answers() {
    assert_eq!(
        scope(r#"{"scope":"items","items":["a","b"],"reason":"changed"}"#),
        Ok(ScopeAnswer::Items {
            items: vec!["a".into(), "b".into()],
            reason: "changed".into()
        })
    );
    assert!(matches!(
        scope(r#"{"scope":"all","reason":"toolchain"}"#),
        Ok(ScopeAnswer::All { .. })
    ));
    assert!(matches!(
        scope(r#"{"scope":"none","reason":"docs only"}"#),
        Ok(ScopeAnswer::None { .. })
    ));
}

#[test]
fn bad_scope_answers_are_invalid_scope() {
    for bad in [
        r#"{"scope":"items","items":[],"reason":"r"}"#,
        r#"{"scope":"items","reason":"r"}"#,
        r#"{"scope":"items","items":[""],"reason":"r"}"#,
        r#"{"scope":"all","items":["a"],"reason":"r"}"#,
        r#"{"scope":"all"}"#,
        r#"{"scope":"all","reason":" "}"#,
        r#"{"scope":"some","reason":"r"}"#,
        r#"{"scope":"all","reason":"r","extra":1}"#,
        "garbage",
    ] {
        assert!(matches!(scope(bad), Err(R::InvalidScope(_))), "{bad}");
    }
    let ok = br#"{"scope":"all","reason":"r"}"#;
    assert!(matches!(
        parse_scope_answer(Exit::Code(1), Some(ok), 1000),
        Err(R::InvalidScope(_))
    ));
    assert!(matches!(
        parse_scope_answer(Exit::Signal(9), Some(ok), 1000),
        Err(R::InvalidScope(_))
    ));
    assert!(matches!(
        parse_scope_answer(Exit::Code(0), None, 1000),
        Err(R::InvalidScope(_))
    ));
    assert!(matches!(
        parse_scope_answer(Exit::Code(0), Some(ok), 3),
        Err(R::InvalidScope(_))
    ));
}

#[test]
fn plan_answers() {
    assert_eq!(parse_plan_answer(Exit::Code(0), "8\n"), Ok(8));
    assert_eq!(parse_plan_answer(Exit::Code(0), "1000"), Ok(1000));
    for bad in [
        "0",
        "1001",
        "",
        "abc",
        "-3",
        "+3",
        "3 4",
        "2.5",
        "99999999999",
    ] {
        assert!(
            matches!(
                parse_plan_answer(Exit::Code(0), bad),
                Err(R::InvalidPlan(_))
            ),
            "{bad}"
        );
    }
    assert!(parse_plan_answer(Exit::Code(2), "8").is_err());
    assert!(parse_plan_answer(Exit::Signal(9), "8").is_err());
}

#[test]
fn a_part_fail_beside_a_part_no_verdict_is_fail() {
    let parts = [
        (1, Verdict::Pass),
        (2, no(R::Cancelled)),
        (3, Verdict::Fail),
    ];
    assert_eq!(combine(&parts), Verdict::Fail);
}

#[test]
fn no_verdict_is_the_first_by_part_number_and_all_pass_passes() {
    let parts = [
        (4, no(R::Interrupted)),
        (2, no(R::Cancelled)),
        (1, Verdict::Pass),
    ];
    assert_eq!(combine(&parts), no(R::Cancelled));
    assert_eq!(
        combine(&[(1, Verdict::Pass), (2, Verdict::Pass)]),
        Verdict::Pass
    );
    assert!(!combine(&[]).is_pass());
}

#[test]
fn failing_items_are_the_union_over_parts() {
    let a = vec!["b".to_string(), "a".to_string()];
    let b = vec!["c".to_string(), "a".to_string()];
    assert_eq!(union_failing([a.as_slice(), b.as_slice()]), ["a", "b", "c"]);
}
