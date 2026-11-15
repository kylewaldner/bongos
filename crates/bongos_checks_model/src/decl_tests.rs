use std::collections::BTreeMap;

use crate::decl::{
    CheckDecl, Digest, DigestError, Inherit, NeedEntry, Parts, Scope, Severity, digest, parse,
    parse_file, validate,
};

const ALL_KEYS: &str = include_str!("../test-fixtures/all_keys.toml");
const INVALID: &str = include_str!("../test-fixtures/invalid.toml");

fn exists(path: &str) -> bool {
    path != "checks/gone"
}

#[test]
fn every_key_parses_with_its_line() {
    let table = parse_file("checks/sets/all.toml", ALL_KEYS).unwrap();
    assert!(table.findings.is_empty(), "{:?}", table.findings);
    assert_eq!(table.check.len(), 2);
    assert_eq!(table.closing.len(), 1);

    let tests = &table.check[0];
    assert_eq!(tests.line, Some(1));
    assert_eq!(tests.name, "tests");
    assert_eq!(tests.run.as_deref(), Some("checks/run-tests"));
    assert_eq!(tests.command, None);
    assert_eq!(tests.repos, Some(vec!["app".to_string()]));
    assert_eq!(tests.pass, Some(vec![0]));
    assert_eq!(tests.fail, Some(vec![1]));
    assert_eq!(tests.environment, Some(vec![3]));
    assert_eq!(tests.summary.as_deref(), Some("^test result: ok"));
    assert_eq!(tests.result_file, Some(true));
    assert_eq!(tests.parts, Some(Parts::Count(8)));
    assert_eq!(tests.parallel, Some(true));
    assert_eq!(tests.stop_at_first_failure, Some(false));
    assert_eq!(
        tests.scope,
        Some(Scope::Script {
            script: "checks/scope".to_string()
        })
    );
    assert_eq!(tests.unset, vec!["TEST_FILTER".to_string()]);
    assert_eq!(
        tests.key_inputs,
        vec!["{home}/.config/tool/pin".to_string()]
    );
    assert_eq!(tests.pinned, vec!["registry/flaky-tests.md".to_string()]);
    let params = tests.params.as_ref().unwrap();
    assert_eq!(params["first_flake_cap"].as_integer(), Some(2));
    assert_eq!(tests.required, Some(true));
    assert_eq!(tests.or_accepted.as_deref(), Some("flake-verdict"));
    assert_eq!(tests.budget_minutes, Some(90));
    assert!(tests.unknown.is_empty());

    // Both forms of `needs`: a pool name, and a table whose other keys are kept unparsed.
    assert_eq!(tests.needs.len(), 2);
    assert_eq!(tests.needs[0], NeedEntry::Pool("test-slots".to_string()));
    let NeedEntry::Request { pool, rest } = &tests.needs[1] else {
        panic!("not a request: {:?}", tests.needs[1]);
    };
    assert_eq!(pool, "jobs");
    assert_eq!(rest["mode"].as_str(), Some("shared"));

    let verdict = &table.check[1];
    assert_eq!(verdict.line, Some(23));
    assert_eq!(
        verdict.command,
        Some(vec!["make".to_string(), "test".to_string()])
    );
    assert_eq!(
        verdict.parts,
        Some(Parts::Plan {
            plan: "checks/plan-parts".to_string()
        })
    );
    assert_eq!(verdict.scope, Some(Scope::Word("all".to_string())));
    let NeedEntry::Request { pool, rest } = &verdict.needs[0] else {
        panic!("not a request");
    };
    assert_eq!(pool, "jobs");
    assert_eq!(rest["count"].as_integer(), Some(2));
    assert!(!verdict.is_required());

    assert_eq!(table.closing[0].line, Some(31));
    assert_eq!(table.closing[0].name, "tests-full");
}

#[test]
fn defaults_apply_to_a_bare_declaration() {
    let table = parse_file("s.toml", "[[check]]\nname = \"x\"\ncommand = [\"true\"]\n").unwrap();
    let decl = &table.check[0];
    assert_eq!(decl.pass_codes(), vec![0]);
    assert_eq!(decl.fail_codes(), vec![1]);
    assert!(decl.environment_codes().is_empty());
    assert!(decl.is_required());
    assert!(!decl.is_parallel());
    assert!(!decl.stops_at_first_failure());
    assert!(!decl.writes_result_file());
}

#[test]
fn a_task_toml_is_read_through_its_checks_table() {
    let text = "id = \"t\"\n\n[checks]\ninherit = \"none\"\nuse = [\"rust\"]\ndrop = [\"lint\"]\n\n[[checks.check]]\nname = \"a\"\ncommand = [\"x\"]\n\n[[checks.closing]]\nname = \"c\"\ncommand = [\"x\"]\n";
    let table = parse_file("tasks/t/task.toml", text).unwrap();
    assert_eq!(table.inherit, Inherit::None);
    assert_eq!(table.uses, vec!["rust".to_string()]);
    assert_eq!(table.drop, vec!["lint".to_string()]);
    assert_eq!(table.check[0].line, Some(8));
    assert_eq!(table.closing[0].line, Some(12));
}

#[test]
fn a_table_read_without_spans_has_no_lines() {
    let value: toml::Value = toml::from_str(
        "[[check]]\nname = \"a\"\ncommand = [\"x\"]\nneeds = [\"p\", { pool = \"q\" }]\n",
    )
    .unwrap();
    let table = parse("tasks/t/task.toml", &value);
    assert!(table.findings.is_empty());
    assert_eq!(table.check[0].line, None);
    assert_eq!(table.check[0].needs.len(), 2);
}

#[test]
fn text_that_is_not_toml_names_the_file_and_line() {
    let finding = parse_file("sets/bad.toml", "[[check]]\nname = \n").unwrap_err();
    assert_eq!(finding.file, "sets/bad.toml");
    assert_eq!(finding.line, Some(2));
    assert!(finding.is_error());
}

#[test]
fn a_bad_inherit_and_a_nameless_entry_are_findings() {
    let table = parse_file(
        "s.toml",
        "[checks]\ninherit = \"all\"\n\n[[checks.check]]\ncommand = [\"x\"]\n",
    )
    .unwrap();
    assert_eq!(table.inherit, Inherit::Parent);
    assert!(table.check.is_empty());
    let texts: Vec<String> = table.findings.iter().map(ToString::to_string).collect();
    assert!(
        texts.iter().any(|text| text.contains("inherit must be")),
        "{texts:?}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("line 4") && text.contains("(unnamed)")),
        "{texts:?}"
    );
}

/// Every message of `findings` for the check `name`, with its line.
fn findings_of(
    findings: &[crate::decl::Finding],
    name: &str,
) -> Vec<(Option<usize>, Severity, String)> {
    findings
        .iter()
        .filter(|finding| finding.check.as_deref() == Some(name))
        .map(|finding| (finding.line, finding.severity, finding.message.clone()))
        .collect()
}

#[test]
fn each_validation_error_names_file_line_and_check() {
    let table = parse_file("checks/sets/invalid.toml", INVALID).unwrap();
    let mut findings = table.findings.clone();
    findings.extend(validate("checks/sets/invalid.toml", &table.check, &exists));
    assert!(
        findings
            .iter()
            .all(|finding| finding.file == "checks/sets/invalid.toml")
    );

    let expect = |name: &str, line: usize, severity: Severity, text: &str| {
        let found = findings_of(&findings, name);
        assert!(
            found.iter().any(|(l, s, message)| *l == Some(line)
                && *s == severity
                && message.contains(text)),
            "{name}: wanted line {line} {text:?}, found {found:?}"
        );
    };
    expect(
        "dup",
        5,
        Severity::Error,
        "duplicate check name (first at line 1)",
    );
    expect("both", 9, Severity::Error, "both run and command");
    expect("neither", 14, Severity::Error, "neither run nor command");
    expect(
        "codes",
        17,
        Severity::Error,
        "exit code 2 is in both pass and fail",
    );
    expect("regex", 23, Severity::Error, "not a valid regex");
    expect("zero-parts", 28, Severity::Error, "parts is 0");
    expect(
        "self",
        33,
        Severity::Error,
        "or_accepted names the check itself",
    );
    expect(
        "escape",
        38,
        Severity::Error,
        "../outside is not a path inside definitions/",
    );
    expect(
        "absolute",
        42,
        Severity::Error,
        "/bin/sh is not a path inside definitions/",
    );
    expect("gone", 46, Severity::Error, "checks/gone does not exist");
    expect("bad-scope", 50, Severity::Error, "scope \"some\"");
    expect("bad-scope-type", 55, Severity::Error, "scope must be");
    expect("bad-needs", 60, Severity::Error, "a needs entry must be");
    expect("unknown-key", 65, Severity::Warning, "unknown key retries");
    expect("(unnamed)", 70, Severity::Error, "missing field `name`");

    // The first `dup` is not reported as a duplicate, and a warning never counts as an error.
    assert!(
        findings_of(&findings, "dup")
            .iter()
            .all(|(line, ..)| *line != Some(1))
    );
    let unknown = findings_of(&findings, "unknown-key");
    assert!(
        unknown
            .iter()
            .all(|(_, severity, _)| *severity == Severity::Warning)
    );

    // The text a user reads names file, line and check.
    let text = findings
        .iter()
        .find(|finding| finding.check.as_deref() == Some("regex"))
        .unwrap()
        .to_string();
    assert!(
        text.starts_with("checks/sets/invalid.toml, line 23, check regex: "),
        "{text}"
    );
}

#[test]
fn a_finding_without_a_line_says_line_unknown() {
    let decl = CheckDecl {
        name: "a".to_string(),
        ..CheckDecl::default()
    };
    let findings = validate("tasks/t/task.toml", &[decl], &exists);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].line, None);
    assert_eq!(
        findings[0].to_string(),
        "tasks/t/task.toml, line unknown, check a: neither run nor command is given; give exactly one"
    );
}

#[test]
fn a_valid_set_has_no_findings() {
    let table = parse_file("checks/sets/all.toml", ALL_KEYS).unwrap();
    let mut decls = table.check.clone();
    decls.extend(table.closing);
    assert_eq!(validate("checks/sets/all.toml", &decls, &exists), vec![]);
}

fn decl(text: &str) -> CheckDecl {
    parse_file("s.toml", text).unwrap().check.remove(0)
}

fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    pairs
        .iter()
        .map(|(path, bytes)| (path.to_string(), bytes.as_bytes().to_vec()))
        .collect()
}

const SCRIPTED: &str = "[[check]]\nname = \"t\"\nrun = \"checks/run\"\nscope = { script = \"checks/scope\" }\nparts = { plan = \"checks/plan\" }\npinned = [\"registry/flaky.md\"]\nparams = { b = 1, a = { y = 1, x = 2 } }\n";

fn scripted_files() -> BTreeMap<String, Vec<u8>> {
    files(&[
        ("checks/run", "run v1"),
        ("checks/scope", "scope v1"),
        ("checks/plan", "plan v1"),
        ("registry/flaky.md", "flaky v1"),
        ("unrelated", "unrelated"),
    ])
}

fn digest_of(decl: &CheckDecl, files: &BTreeMap<String, Vec<u8>>) -> Digest {
    digest(decl, files).unwrap()
}

#[test]
fn the_digest_changes_with_the_declaration_and_every_named_file() {
    let base = decl(SCRIPTED);
    let before = digest_of(&base, &scripted_files());
    assert_eq!(before, digest_of(&base, &scripted_files()));
    assert_eq!(before.as_str().len(), 64);

    // A file the check does not name changes nothing.
    let mut unrelated = scripted_files();
    unrelated.insert("unrelated".to_string(), b"changed".to_vec());
    assert_eq!(before, digest_of(&base, &unrelated));

    for path in [
        "checks/run",
        "checks/scope",
        "checks/plan",
        "registry/flaky.md",
    ] {
        let mut changed = scripted_files();
        changed.insert(path.to_string(), b"edited".to_vec());
        assert_ne!(before, digest_of(&base, &changed), "{path}");
    }

    let edited = decl(&SCRIPTED.replace(
        "run = \"checks/run\"",
        "run = \"checks/run\"\nrequired = false",
    ));
    assert_ne!(before, digest_of(&edited, &scripted_files()));
}

#[test]
fn the_digest_ignores_key_order_and_written_defaults_but_not_line() {
    let base = decl(SCRIPTED);
    let before = digest_of(&base, &scripted_files());

    // `params` keys in another order, and defaults written out.
    let reordered = SCRIPTED.replace(
        "params = { b = 1, a = { y = 1, x = 2 } }",
        "params = { a = { x = 2, y = 1 }, b = 1 }\npass = [0]\nfail = [1]\nrequired = true\nparallel = false",
    );
    assert_eq!(before, digest_of(&decl(&reordered), &scripted_files()));

    // Where the declaration sits in its file is not part of it.
    let moved = format!("\n\n{SCRIPTED}");
    assert_eq!(before, digest_of(&decl(&moved), &scripted_files()));
}

#[test]
fn a_named_file_that_was_not_given_is_an_error() {
    let mut given = scripted_files();
    given.remove("checks/plan");
    assert_eq!(
        digest(&decl(SCRIPTED), &given),
        Err(DigestError::MissingFile {
            check: "t".to_string(),
            path: "checks/plan".to_string()
        })
    );
}

#[test]
fn a_command_check_needs_no_files() {
    let command = decl("[[check]]\nname = \"c\"\ncommand = [\"make\", \"test\"]\n");
    let none = BTreeMap::new();
    assert_eq!(
        digest_of(&command, &none),
        digest_of(&command, &scripted_files())
    );
}
