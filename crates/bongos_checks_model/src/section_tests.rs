use std::collections::BTreeMap;

use crate::decl::Inherit;
use crate::effective::{Applicability, Dropped, EffectiveSet, Row, Source};
use crate::evidence::Scope;
use crate::evidence_tests::{current, record, row, row_named};
use crate::section::{Live, Tone, TreeState, section, source_rows};
use crate::verdict::{NoVerdictReason, Verdict};

fn set(rows: Vec<Row>) -> EffectiveSet {
    EffectiveSet {
        task_id: "t1".into(),
        applicability: Applicability::Checks,
        is_group: false,
        inherit: Inherit::Parent,
        rows,
        closing: Vec::new(),
        dropped: Vec::new(),
        findings: Vec::new(),
    }
}

fn header(
    set: &EffectiveSet,
    tree: TreeState,
    live: &BTreeMap<String, Live>,
    recs: &[crate::evidence::EvidenceRecord],
) -> String {
    let cur = current(&["tests", "lint", "a", "b"]);
    section(set, recs, &cur, &tree, live).header
}

#[test]
fn headers() {
    let none = BTreeMap::new();
    let avail = || TreeState::Available;
    let mut s = set(vec![row("")]);
    assert_eq!(header(&s, avail(), &none, &[]), "not done: 1 check missing");
    assert_eq!(
        header(&s, TreeState::Unavailable, &none, &[]),
        "not done: no tree to check"
    );
    assert_eq!(
        header(&s, avail(), &none, &[record("tests", "1", Scope::Unscoped)]),
        "all required checks pass"
    );
    let mut fail = record("tests", "1", Scope::Unscoped);
    fail.verdict = Verdict::Fail;
    assert_eq!(
        header(&s, avail(), &none, &[fail]),
        "not done: 1 check failing"
    );
    let two = set(vec![row_named("a", ""), row_named("b", "")]);
    assert_eq!(
        header(&two, avail(), &none, &[]),
        "not done: 2 checks missing"
    );
    let live = BTreeMap::from([(
        "a".to_string(),
        Live::Waiting {
            pool: "gpu".into(),
            place: 2,
            secs: 180,
        },
    )]);
    assert_eq!(header(&two, avail(), &live, &[]), "checks waiting");
    assert_eq!(header(&set(Vec::new()), avail(), &none, &[]), "no checks");
    s.applicability = Applicability::NoChecksApply;
    assert_eq!(header(&s, avail(), &none, &[]), "no checks apply");
    let advisory = set(vec![row("required = false\n")]);
    assert_eq!(header(&advisory, avail(), &none, &[]), "no required checks");
}

fn text_of(
    rows: Vec<Row>,
    recs: &[crate::evidence::EvidenceRecord],
    live: &BTreeMap<String, Live>,
) -> Vec<(String, Tone, Vec<String>)> {
    let s = set(rows);
    let cur = current(&["tests", "lint"]);
    section(&s, recs, &cur, &TreeState::Available, live)
        .rows
        .into_iter()
        .map(|r| (r.text, r.tone, r.detail))
        .collect()
}

#[test]
fn a_row_shows_source_requirement_result_and_tree() {
    let mut r = row("");
    r.source = Source::Inherited { from: "g".into() };
    let mut rec = record("tests", "1", Scope::Unscoped);
    rec.duration_ms = Some(65_000);
    let rows = text_of(vec![r], &[rec], &BTreeMap::new());
    let (text, tone, detail) = &rows[0];
    assert_eq!(*tone, Tone::Success);
    assert!(
        text.starts_with(
            "tests | inherited from g | required | pass | scope unscoped | tree tree1"
        ),
        "{text}"
    );
    assert!(text.contains("1m 5s"), "{text}");
    assert!(detail.iter().any(|d| d == "output: /o"));
}

#[test]
fn sources_are_named() {
    let cases = [
        (Source::Own, "own"),
        (Source::Set { set: "rust".into() }, "from set rust"),
        (Source::Closing { group: "g".into() }, "closing check of g"),
        (
            Source::Overrides {
                from: Box::new(Source::Inherited { from: "g".into() }),
            },
            "overrides g",
        ),
    ];
    for (source, expected) in cases {
        let mut r = row("");
        r.source = source;
        let rows = text_of(vec![r], &[], &BTreeMap::new());
        assert!(
            rows[0].0.contains(&format!("| {expected} |")),
            "{}",
            rows[0].0
        );
        assert!(rows[0].0.contains("not run"));
    }
}

#[test]
fn advisory_and_accepted_are_shown() {
    let rows = vec![
        row("or_accepted = \"lint\"\n"),
        row_named("lint", "required = false\n"),
    ];
    let mut fail = record("tests", "1", Scope::Unscoped);
    fail.verdict = Verdict::Fail;
    fail.failing = vec!["t::a".into()];
    let lint = record("lint", "2", Scope::Unscoped);
    let out = text_of(rows, &[fail, lint], &BTreeMap::new());
    assert!(out[0].0.contains("accepted by lint"), "{}", out[0].0);
    assert!(out[0].0.contains("or accepted by lint"));
    assert!(out[1].0.contains("advisory"));
}

#[test]
fn fail_no_verdict_and_stale_rows() {
    let mut fail = record("tests", "1", Scope::Unscoped);
    fail.verdict = Verdict::Fail;
    fail.failing = vec!["a".into()];
    let out = text_of(vec![row("")], std::slice::from_ref(&fail), &BTreeMap::new());
    assert_eq!(out[0].1, Tone::Error);
    assert!(out[0].0.contains("fail (1 item)"));
    assert!(out[0].2.contains(&"failing: a".to_string()));

    let mut nv = record("tests", "2", Scope::Unscoped);
    nv.verdict = Verdict::NoVerdict(NoVerdictReason::Environment { code: 3 });
    let out = text_of(vec![row("")], &[fail, nv], &BTreeMap::new());
    assert_eq!(out[0].1, Tone::Warning);
    assert!(out[0].0.contains("no verdict: environment (exit 3)"));
    assert!(
        out[0].2.contains(&"last result: fail (1 item)".to_string()),
        "{:?}",
        out[0].2
    );

    let mut stale = record("tests", "3", Scope::Unscoped);
    stale.tree_key = "oldtree99".into();
    let out = text_of(vec![row("")], &[stale], &BTreeMap::new());
    assert_eq!(out[0].1, Tone::Muted);
    assert!(out[0].0.contains("stale: ran on an earlier tree"));
    assert!(out[0].2.contains(&"last result: pass".to_string()));
}

#[test]
fn live_state_and_dropped_rows() {
    let live = BTreeMap::from([(
        "tests".to_string(),
        Live::Running {
            part: 5,
            of: 8,
            kept: 4,
        },
    )]);
    let out = text_of(vec![row("")], &[], &live);
    assert!(out[0].0.contains("running part 5 of 8, 4 kept"));
    let waiting = BTreeMap::from([(
        "tests".to_string(),
        Live::Waiting {
            pool: "gpu".into(),
            place: 2,
            secs: 180,
        },
    )]);
    let out = text_of(vec![row("")], &[], &waiting);
    assert!(
        out[0].0.contains("waiting: gpu, place 2, 3m 0s"),
        "{}",
        out[0].0
    );

    let mut s = set(vec![row("")]);
    s.dropped.push(Dropped {
        by: "t1".into(),
        row: row_named("lint", ""),
    });
    let cur = current(&["tests"]);
    let sec = section(&s, &[], &cur, &TreeState::Available, &BTreeMap::new());
    assert_eq!(sec.rows.len(), 2);
    assert!(sec.rows[1].text.contains("dropped here"));
    assert_eq!(sec.title, "Checks");
}

#[test]
fn nothing_in_scope_and_narrow_pass_are_worded() {
    let none = record(
        "tests",
        "1",
        Scope::None {
            reason: "docs only".into(),
        },
    );
    let out = text_of(
        vec![row("scope = { script = \"s\" }\n")],
        &[none],
        &BTreeMap::new(),
    );
    assert!(out[0].0.contains("pass (nothing in scope)"), "{}", out[0].0);
    assert!(out[0].0.contains("scope nothing in scope: docs only"));
    let narrow = record(
        "tests",
        "2",
        Scope::Items {
            digest: "x".into(),
            count: 3,
        },
    );
    let out = text_of(vec![row("scope = \"all\"\n")], &[narrow], &BTreeMap::new());
    assert!(out[0].0.contains("narrower scope"));
}

#[test]
fn headers_count_every_kind_of_gap_and_no_checks_is_never_passing() {
    let none = BTreeMap::new();
    let avail = || TreeState::Available;
    let two = set(vec![row_named("a", ""), row_named("b", "")]);
    let mut fail = record("a", "1", Scope::Unscoped);
    fail.verdict = Verdict::Fail;
    let mut nv = record("b", "2", Scope::Unscoped);
    nv.verdict = Verdict::NoVerdict(NoVerdictReason::Environment { code: 3 });
    assert_eq!(
        header(&two, avail(), &none, &[fail, nv.clone()]),
        "not done: 1 check failing, 1 check with no verdict"
    );
    let mut stale = record("a", "3", Scope::Unscoped);
    stale.tree_key = "oldtree99".into();
    assert_eq!(
        header(&two, avail(), &none, &[stale, nv]),
        "not done: 1 check with no verdict, 1 check stale"
    );
    // Nothing to judge is never a pass, whatever the tree.
    for applicability in [Applicability::NoChecks, Applicability::UnknownTask] {
        let mut empty = set(Vec::new());
        empty.applicability = applicability;
        for tree in [TreeState::Available, TreeState::Unavailable] {
            assert_eq!(header(&empty, tree, &none, &[]), "no checks");
        }
    }
    // A group with no closing checks has nothing to judge either.
    let mut group = set(vec![row("")]);
    group.is_group = true;
    assert_eq!(
        header(
            &group,
            avail(),
            &none,
            &[record("tests", "1", Scope::Unscoped)]
        ),
        "no checks"
    );
}

#[test]
fn a_group_shows_its_closing_checks() {
    let mut closing = row("");
    closing.source = Source::Closing { group: "g".into() };
    let mut group = set(vec![row_named("down", "")]);
    group.is_group = true;
    group.closing = vec![closing];
    let cur = current(&["tests", "down"]);
    let sec = section(&group, &[], &cur, &TreeState::Available, &BTreeMap::new());
    assert_eq!(sec.header, "not done: 1 check missing");
    assert_eq!(sec.rows.len(), 1);
    assert!(
        sec.rows[0].text.contains("closing check of g"),
        "{}",
        sec.rows[0].text
    );
}

#[test]
fn the_sources_rows_show_each_layer_use_and_drop() {
    let mut inherited = row_named("lint", "");
    inherited.source = Source::Inherited { from: "g".into() };
    inherited.declared.file = "g/task.toml".into();
    inherited.declared.line = Some(7);
    let mut overriding = row("");
    overriding.source = Source::Overrides {
        from: Box::new(Source::Set { set: "rust".into() }),
    };
    let mut s = set(vec![overriding, inherited]);
    s.dropped.push(Dropped {
        by: "t1".into(),
        row: row_named("slow", ""),
    });
    let rows = source_rows(&s);
    let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "tests | uses: overrides set rust | declared in task.toml",
            "lint | uses: inherited from g | declared in g/task.toml:7",
            "slow | dropped by t1 | was own | declared in task.toml",
        ]
    );
    assert!(rows.iter().all(|r| r.id.starts_with("source/")));
    // A document task has no derivation to show.
    s.applicability = Applicability::NoChecksApply;
    assert!(source_rows(&s).is_empty());
}
