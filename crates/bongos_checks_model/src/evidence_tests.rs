use std::collections::BTreeMap;

use crate::decl::CheckDecl;
use crate::effective::{Declared, Row, Source};
use crate::evidence::{
    CheckState, Current, EvidenceRecord, Freshness, Output, Params, PartRecord, RowStatus, Scope,
    Stale, StartedBy, TreeKind, Variant, freshness, or_accepted, records_of_task, row_status,
    satisfied,
};
use crate::verdict::{NoVerdictReason, Verdict};

pub(crate) fn row(extra: &str) -> Row {
    row_named("tests", extra)
}

pub(crate) fn row_named(name: &str, extra: &str) -> Row {
    let command = if extra.contains("command =") || extra.contains("run =") {
        ""
    } else {
        "command = [\"t\"]\n"
    };
    let text = format!("name = \"{name}\"\n{command}{extra}");
    let decl: CheckDecl = toml::from_str(&text).unwrap();
    Row {
        declared: Declared {
            file: "task.toml".to_string(),
            check: name.to_string(),
            line: None,
        },
        decl,
        source: Source::Own,
    }
}

pub(crate) fn record(check: &str, id: &str, scope: Scope) -> EvidenceRecord {
    EvidenceRecord {
        id: id.to_string(),
        task_id: "t1".to_string(),
        check: check.to_string(),
        digest: "d1".to_string(),
        tree_key: "tree1".to_string(),
        tree: TreeKind::Tip,
        base: None,
        key_inputs: "k1".to_string(),
        variant: Variant {
            scope,
            parts: 1,
            params: None,
        },
        pin_commit: "c".to_string(),
        master_path: "/m".to_string(),
        command: vec!["t".to_string()],
        started: "s".to_string(),
        ended: Some("e".to_string()),
        duration_ms: Some(1500),
        verdict: Verdict::Pass,
        reason: None,
        exit: None,
        summary: None,
        failing: Vec::new(),
        scope_reason: None,
        started_by: StartedBy::Person,
        parts: Vec::new(),
        output: Output::Path("/o".to_string()),
        cut: 0,
    }
}

pub(crate) fn state(tree: &str) -> CheckState {
    CheckState {
        tree_key: tree.to_string(),
        key_inputs: "k1".to_string(),
        digest: "d1".to_string(),
    }
}

pub(crate) fn current(checks: &[&str]) -> Current {
    Current {
        checks: checks
            .iter()
            .map(|name| (name.to_string(), state("tree1")))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn status(row: &Row, records: &[EvidenceRecord]) -> RowStatus {
    row_status(
        row,
        records,
        &current(&["tests", "flake"]),
        std::slice::from_ref(row),
        "t1",
    )
}

#[test]
fn freshness_by_tree_digest_and_key_input() {
    let r = record("tests", "1", Scope::Unscoped);
    assert_eq!(freshness(&r, Some(&state("tree1"))), Freshness::Fresh);
    assert_eq!(
        freshness(&r, Some(&state("tree2"))),
        Freshness::Stale(Stale::EarlierTree)
    );
    let mut s = state("tree1");
    s.digest = "d2".into();
    assert_eq!(
        freshness(&r, Some(&s)),
        Freshness::Stale(Stale::CheckChanged)
    );
    let mut s = state("tree1");
    s.key_inputs = "k2".into();
    assert_eq!(
        freshness(&r, Some(&s)),
        Freshness::Stale(Stale::KeyInputChanged)
    );
    assert!(matches!(freshness(&r, None), Freshness::Stale(_)));
    // Going back to the old tree makes the old pass fresh again.
    assert_eq!(freshness(&r, Some(&state("tree1"))), Freshness::Fresh);
    assert_eq!(Stale::EarlierTree.text(), "ran on an earlier tree");
}

#[test]
fn a_fresh_pass_satisfies_and_a_stale_one_does_not() {
    let row = row("");
    let rec = record("tests", "1", Scope::Unscoped);
    assert!(matches!(
        status(&row, std::slice::from_ref(&rec)),
        RowStatus::Satisfied { .. }
    ));
    let moved = Current {
        checks: BTreeMap::from([("tests".to_string(), state("tree2"))]),
    };
    let sat = satisfied(
        std::slice::from_ref(&row),
        std::slice::from_ref(&rec),
        &moved,
        "t1",
    );
    assert_eq!(sat.stale, ["tests"]);
    assert!(!sat.is_satisfied());
    let mut changed = state("tree1");
    changed.digest = "dX".into();
    let moved = Current {
        checks: BTreeMap::from([("tests".to_string(), changed)]),
    };
    assert_eq!(
        row_status(&row, &[rec], &moved, std::slice::from_ref(&row), "t1"),
        RowStatus::Stale(Stale::CheckChanged)
    );
}

#[test]
fn no_records_is_missing_and_no_state_satisfies_nothing() {
    let row = row("");
    assert_eq!(status(&row, &[]), RowStatus::Missing);
    let rec = record("tests", "1", Scope::Unscoped);
    let none = Current::default();
    assert!(!satisfied(std::slice::from_ref(&row), &[rec], &none, "t1").is_satisfied());
}

#[test]
fn an_unfinished_or_non_pass_record_never_satisfies() {
    let row = row("");
    let mut open = record("tests", "1", Scope::Unscoped);
    open.ended = None;
    assert_eq!(status(&row, &[open]), RowStatus::Missing);
    let mut fail = record("tests", "2", Scope::Unscoped);
    fail.verdict = Verdict::Fail;
    assert_eq!(status(&row, &[fail]), RowStatus::Failing);
    let mut nv = record("tests", "3", Scope::Unscoped);
    nv.verdict = Verdict::NoVerdict(NoVerdictReason::Cancelled);
    assert_eq!(status(&row, &[nv]), RowStatus::NoVerdict);
}

#[test]
fn a_narrow_pass_does_not_satisfy_scope_all() {
    let row = row("scope = \"all\"\n");
    let items = record(
        "tests",
        "1",
        Scope::Items {
            digest: "x".into(),
            count: 2,
        },
    );
    assert_eq!(status(&row, &[items]), RowStatus::Narrow);
    let none = record(
        "tests",
        "2",
        Scope::None {
            reason: "docs".into(),
        },
    );
    assert_eq!(status(&row, &[none]), RowStatus::Narrow);
    let unscoped = record("tests", "3", Scope::Unscoped);
    assert_eq!(status(&row, &[unscoped]), RowStatus::Narrow);
    let all = record("tests", "4", Scope::All);
    assert!(matches!(status(&row, &[all]), RowStatus::Satisfied { .. }));
    let sat = satisfied(
        std::slice::from_ref(&row),
        &[record(
            "tests",
            "1",
            Scope::Items {
                digest: "x".into(),
                count: 2,
            },
        )],
        &current(&["tests"]),
        "t1",
    );
    assert_eq!(sat.missing, ["tests"]);
}

#[test]
fn an_all_pass_satisfies_a_scoped_check_and_items_and_none_do_too() {
    let row = row("scope = { script = \"scopes/changed\" }\n");
    for scope in [
        Scope::All,
        Scope::Items {
            digest: "x".into(),
            count: 1,
        },
        Scope::None {
            reason: "nothing".into(),
        },
    ] {
        assert!(matches!(
            status(&row, &[record("tests", "1", scope)]),
            RowStatus::Satisfied { .. }
        ));
    }
    let empty_items = Scope::Items {
        digest: "x".into(),
        count: 0,
    };
    assert_eq!(
        status(&row, &[record("tests", "1", empty_items)]),
        RowStatus::Narrow
    );
    let no_reason = Scope::None { reason: " ".into() };
    assert_eq!(
        status(&row, &[record("tests", "1", no_reason)]),
        RowStatus::Narrow
    );
    assert_eq!(
        status(&row, &[record("tests", "1", Scope::Unscoped)]),
        RowStatus::Narrow
    );
}

#[test]
fn an_unscoped_check_takes_only_unscoped() {
    let row = row("");
    assert_eq!(
        status(&row, &[record("tests", "1", Scope::All)]),
        RowStatus::Narrow
    );
}

#[test]
fn an_invalid_scope_declaration_is_satisfied_by_nothing() {
    let row = row("scope = \"some\"\n");
    assert_eq!(
        status(&row, &[record("tests", "1", Scope::All)]),
        RowStatus::Narrow
    );
}

#[test]
fn a_parameterized_pass_satisfies_nothing() {
    let row = row("");
    let mut rec = record("tests", "1", Scope::Unscoped);
    rec.variant.params = Some(Params {
        items: vec!["a".into()],
        repeat_index: None,
        tree: Some(TreeKind::Base),
    });
    assert_eq!(status(&row, std::slice::from_ref(&rec)), RowStatus::Missing);
    let sat = satisfied(
        std::slice::from_ref(&row),
        &[rec],
        &current(&["tests"]),
        "t1",
    );
    assert_eq!(sat.missing, ["tests"]);
    // And a parameterized fail does not count against a real pass.
    let pass = record("tests", "0", Scope::Unscoped);
    let mut failed = record("tests", "2", Scope::Unscoped);
    failed.verdict = Verdict::Fail;
    failed.variant.params = Some(Params {
        items: vec![],
        repeat_index: Some(3),
        tree: None,
    });
    assert!(matches!(
        status(&row, &[pass, failed]),
        RowStatus::Satisfied { .. }
    ));
}

fn part(n: u32, verdict: Verdict) -> PartRecord {
    PartRecord {
        n,
        verdict,
        reason: None,
        exit: None,
        started: "s".into(),
        ended: Some("e".into()),
        pid: None,
        pid_start_time: None,
        token: None,
        duration_ms: None,
        summary: None,
        failing: Vec::new(),
    }
}

#[test]
fn parts_must_all_be_there_and_pass() {
    let row = row("parts = 3\n");
    let mut rec = record("tests", "1", Scope::Unscoped);
    rec.variant.parts = 3;
    rec.parts = vec![
        part(1, Verdict::Pass),
        part(2, Verdict::Pass),
        part(3, Verdict::Pass),
    ];
    assert!(matches!(
        status(&row, std::slice::from_ref(&rec)),
        RowStatus::Satisfied { .. }
    ));
    let mut missing = rec.clone();
    missing.parts.pop();
    assert_eq!(status(&row, &[missing]), RowStatus::Narrow);
    let mut dup = rec.clone();
    dup.parts[2].n = 2;
    assert_eq!(status(&row, &[dup]), RowStatus::Narrow);
    let mut failed = rec.clone();
    failed.parts[1].verdict = Verdict::Fail;
    assert_eq!(status(&row, &[failed]), RowStatus::Narrow);
    let mut empty = rec.clone();
    empty.parts.clear();
    assert_eq!(status(&row, &[empty]), RowStatus::Narrow);
    let mut zero = rec;
    zero.variant.parts = 0;
    assert_eq!(status(&row, &[zero]), RowStatus::Narrow);
}

fn accepted_rows() -> Vec<Row> {
    vec![
        row("or_accepted = \"flake\"\n"),
        row_named("flake", "required = false\n"),
    ]
}

#[test]
fn accepted_by_b_on_the_same_tree_only() {
    let rows = accepted_rows();
    let cur = current(&["tests", "flake"]);
    let mut tests_fail = record("tests", "1", Scope::Unscoped);
    tests_fail.verdict = Verdict::Fail;
    let flake_pass = record("flake", "2", Scope::Unscoped);
    let records = [tests_fail, flake_pass.clone()];
    let accepted = or_accepted(&rows[0], &records, &cur, &rows).unwrap();
    assert_eq!(accepted.by, "flake");
    assert_eq!(accepted.record_id, "2");
    assert_eq!(
        row_status(&rows[0], &records, &cur, &rows, "t1"),
        RowStatus::Satisfied {
            record_id: "2".into(),
            accepted_by: Some("flake".into())
        }
    );
    assert!(satisfied(&rows, &records, &cur, "t1").is_satisfied());

    // B passed on another tree: accepts nothing, even if B itself is fresh there.
    let mut other_tree = flake_pass.clone();
    other_tree.tree_key = "tree0".into();
    let cur_b_old = Current {
        checks: BTreeMap::from([
            ("tests".to_string(), state("tree1")),
            ("flake".to_string(), state("tree0")),
        ]),
    };
    assert_eq!(
        or_accepted(&rows[0], &[other_tree.clone()], &cur_b_old, &rows),
        None
    );
    // Stale B, parameterized B, failed B, unfinished B.
    assert_eq!(or_accepted(&rows[0], &[other_tree], &cur, &rows), None);
    let mut param = flake_pass.clone();
    param.variant.params = Some(Params {
        items: vec![],
        repeat_index: None,
        tree: None,
    });
    assert_eq!(or_accepted(&rows[0], &[param], &cur, &rows), None);
    let mut failed = flake_pass.clone();
    failed.verdict = Verdict::Fail;
    assert_eq!(or_accepted(&rows[0], &[failed], &cur, &rows), None);
    let mut open = flake_pass;
    open.ended = None;
    assert_eq!(or_accepted(&rows[0], &[open], &cur, &rows), None);
}

#[test]
fn accepting_check_must_be_in_the_set_and_not_itself() {
    let rows = accepted_rows();
    let cur = current(&["tests", "flake"]);
    let flake_pass = record("flake", "2", Scope::Unscoped);
    assert_eq!(
        or_accepted(
            &rows[0],
            std::slice::from_ref(&flake_pass),
            &cur,
            &rows[..1]
        ),
        None
    );
    let own = row("or_accepted = \"tests\"\n");
    let own_pass = record("tests", "1", Scope::Unscoped);
    assert_eq!(
        or_accepted(&own, &[own_pass], &cur, std::slice::from_ref(&own)),
        None
    );
    // B's pass must meet B's own scope declaration.
    let strict = vec![
        row("or_accepted = \"flake\"\n"),
        row_named("flake", "scope = \"all\"\n"),
    ];
    assert_eq!(or_accepted(&strict[0], &[flake_pass], &cur, &strict), None);
}

#[test]
fn advisory_rows_are_never_counted() {
    let rows = vec![row("required = false\n")];
    let sat = satisfied(&rows, &[], &current(&["tests"]), "t1");
    assert!(sat.is_satisfied());
    assert_eq!(sat.unsatisfied_count(), 0);
}

#[test]
fn satisfaction_lists_each_kind_by_name() {
    let rows = vec![
        row_named("a", ""),
        row_named("b", ""),
        row_named("c", ""),
        row_named("d", ""),
        row_named("e", ""),
    ];
    let cur = current(&["a", "b", "c", "d", "e"]);
    let mut b = record("b", "2", Scope::Unscoped);
    b.verdict = Verdict::Fail;
    let mut c = record("c", "3", Scope::Unscoped);
    c.verdict = Verdict::NoVerdict(NoVerdictReason::TimeBudgetReached);
    let mut d = record("d", "4", Scope::Unscoped);
    d.tree_key = "old".into();
    let a = record("a", "1", Scope::Unscoped);
    let sat = satisfied(&rows, &[a, b, c, d], &cur, "t1");
    assert_eq!(sat.failing, ["b"]);
    assert_eq!(sat.no_verdict, ["c"]);
    assert_eq!(sat.stale, ["d"]);
    assert_eq!(sat.missing, ["e"]);
    assert_eq!(sat.unsatisfied_count(), 4);
}

#[test]
fn the_newest_fresh_record_decides_pass_then_fail_and_fail_then_pass() {
    let row = row("");
    let pass = record("tests", "1", Scope::Unscoped);
    let mut fail = record("tests", "2", Scope::Unscoped);
    fail.verdict = Verdict::Fail;
    let mut none = record("tests", "3", Scope::Unscoped);
    none.verdict = Verdict::NoVerdict(NoVerdictReason::Environment { code: 1 });
    assert_eq!(
        status(&row, &[pass.clone(), fail.clone()]),
        RowStatus::Failing
    );
    assert_eq!(
        status(&row, &[pass.clone(), none.clone()]),
        RowStatus::NoVerdict
    );
    let mut pass_again = pass;
    pass_again.id = "4".into();
    assert!(matches!(
        status(&row, &[fail, none, pass_again]),
        RowStatus::Satisfied { record_id, .. } if record_id == "4"
    ));
    // A newer narrow pass is no verdict against an older whole pass.
    let scoped = row_named("tests", "scope = \"all\"\n");
    let whole = record("tests", "5", Scope::All);
    let narrow = record(
        "tests",
        "6",
        Scope::Items {
            digest: "x".into(),
            count: 1,
        },
    );
    assert!(matches!(
        status(&scoped, &[whole, narrow]),
        RowStatus::Satisfied { record_id, .. } if record_id == "5"
    ));
}

#[test]
fn a_later_failure_of_b_stops_it_accepting() {
    let rows = accepted_rows();
    let cur = current(&["tests", "flake"]);
    let mut fail = record("flake", "3", Scope::Unscoped);
    fail.verdict = Verdict::Fail;
    let records = [record("flake", "2", Scope::Unscoped), fail];
    assert_eq!(or_accepted(&rows[0], &records, &cur, &rows), None);
}

#[test]
fn accepting_uses_the_tree_of_the_row_even_when_b_has_another_current_key() {
    let rows = accepted_rows();
    // A's current tree is tree1; B's current state is tree2, so B's tree1 pass is stale and
    // B's tree2 pass is on another tree than A's: neither accepts.
    let cur = Current {
        checks: BTreeMap::from([
            ("tests".to_string(), state("tree1")),
            ("flake".to_string(), state("tree2")),
        ]),
    };
    let on_tree1 = record("flake", "1", Scope::Unscoped);
    let mut on_tree2 = record("flake", "2", Scope::Unscoped);
    on_tree2.tree_key = "tree2".into();
    assert_eq!(
        or_accepted(&rows[0], &[on_tree1, on_tree2], &cur, &rows),
        None
    );
}

#[test]
fn records_of_other_tasks_and_landings_do_not_count() {
    let mine = record("tests", "1", Scope::Unscoped);
    let mut other = record("tests", "2", Scope::Unscoped);
    other.task_id = "t2".into();
    let mut landing = record("tests", "3", Scope::Unscoped);
    landing.started_by = StartedBy::Landing { tree: "x".into() };
    let kept = records_of_task(&[mine, other, landing], "t1");
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id, "1");
}

#[test]
fn another_tasks_or_a_landings_records_neither_satisfy_nor_block() {
    let row = row("");
    let mut other_pass = record("tests", "2", Scope::Unscoped);
    other_pass.task_id = "t2".into();
    assert_eq!(status(&row, &[other_pass.clone()]), RowStatus::Missing);
    assert!(
        !satisfied(
            std::slice::from_ref(&row),
            &[other_pass],
            &current(&["tests"]),
            "t1"
        )
        .is_satisfied()
    );
    let mine = record("tests", "1", Scope::Unscoped);
    let mut other_fail = record("tests", "3", Scope::Unscoped);
    other_fail.task_id = "t2".into();
    other_fail.verdict = Verdict::Fail;
    let mut landing_fail = record("tests", "4", Scope::Unscoped);
    landing_fail.verdict = Verdict::Fail;
    landing_fail.started_by = StartedBy::Landing { tree: "x".into() };
    assert!(matches!(
        status(&row, &[mine, other_fail, landing_fail]),
        RowStatus::Satisfied { record_id, .. } if record_id == "1"
    ));
}

#[test]
fn newest_is_decided_by_id_not_by_position() {
    let row = row("");
    let pass = record("tests", "1", Scope::Unscoped);
    let mut fail = record("tests", "2", Scope::Unscoped);
    fail.verdict = Verdict::Fail;
    assert_eq!(
        status(&row, &[fail.clone(), pass.clone()]),
        RowStatus::Failing
    );
    assert!(matches!(
        status(&row, &[pass.clone(), fail.clone()]),
        RowStatus::Failing
    ));
    let mut pass_new = pass;
    pass_new.id = "3".into();
    assert!(matches!(
        status(&row, &[pass_new, fail]),
        RowStatus::Satisfied { record_id, .. } if record_id == "3"
    ));
}
