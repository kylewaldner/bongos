//! Validation of check declarations: pure, each finding with its file, line and check.

use std::collections::BTreeMap;

use bongos_project_repo_model::RelPath;

use super::{CheckDecl, Finding, NeedEntry, Parts, Scope};

/// The findings for the declarations of one set in `file`. `exists` says whether a path in the
/// definitions exists. `or_accepted` naming a check missing from the effective set is checked on
/// the effective set (`effective::effective_set`), not here; naming itself is checked here.
pub fn validate(file: &str, decls: &[CheckDecl], exists: &dyn Fn(&str) -> bool) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut first_seen: BTreeMap<&str, Option<usize>> = BTreeMap::new();
    for decl in decls {
        if let Some(first) = first_seen.insert(&decl.name, decl.line) {
            let also = first.map_or_else(String::new, |line| format!(" (first at line {line})"));
            findings.push(error(file, decl, format!("duplicate check name{also}")));
        }
        validate_one(file, decl, exists, &mut findings);
    }
    findings
}

fn error(file: &str, decl: &CheckDecl, message: impl Into<String>) -> Finding {
    Finding::error(file, decl.line, Some(&decl.name), message)
}

fn validate_one(
    file: &str,
    decl: &CheckDecl,
    exists: &dyn Fn(&str) -> bool,
    findings: &mut Vec<Finding>,
) {
    let mut push = |message: String| findings.push(error(file, decl, message));
    if decl.name.is_empty() {
        push("the check has an empty name".to_string());
    }
    match (&decl.run, &decl.command) {
        (Some(_), Some(_)) => push("both run and command are given; give exactly one".to_string()),
        (None, None) => push("neither run nor command is given; give exactly one".to_string()),
        (None, Some(command)) if command.is_empty() => push("command is empty".to_string()),
        _ => {}
    }
    for (a, b, codes_a, codes_b) in [
        ("pass", "fail", decl.pass_codes(), decl.fail_codes()),
        (
            "pass",
            "environment",
            decl.pass_codes(),
            decl.environment_codes(),
        ),
        (
            "fail",
            "environment",
            decl.fail_codes(),
            decl.environment_codes(),
        ),
    ] {
        for code in &codes_a {
            if codes_b.contains(code) {
                push(format!("exit code {code} is in both {a} and {b}"));
            }
        }
    }
    if let Some(summary) = &decl.summary
        && let Err(error) = regex::Regex::new(summary)
    {
        push(format!("summary is not a valid regex: {error}"));
    }
    match &decl.parts {
        Some(Parts::Count(count)) if *count < 1 => {
            push(format!("parts is {count}; it must be at least 1"));
        }
        Some(Parts::Invalid(_)) => {
            push("parts must be a positive integer or a table with a plan script".to_string())
        }
        _ => {}
    }
    match &decl.scope {
        Some(Scope::Word(word)) if word != "all" => push(format!(
            "scope \"{word}\" is not \"all\" or a table with a script"
        )),
        Some(Scope::Invalid(_)) => {
            push("scope must be \"all\" or a table with a script".to_string());
        }
        _ => {}
    }
    for need in &decl.needs {
        if matches!(need, NeedEntry::Invalid(_)) {
            push("a needs entry must be a pool name or a table with a string pool".to_string());
        }
    }
    if decl.or_accepted.as_deref() == Some(decl.name.as_str()) {
        push("or_accepted names the check itself".to_string());
    }
    for path in decl.named_files() {
        if RelPath::new(path).is_none() {
            push(format!(
                "{path} is not a path inside definitions/ (no absolute path, no .. part)"
            ));
        } else if !exists(path) {
            push(format!("{path} does not exist in the definitions"));
        }
    }
    for key in decl.unknown.keys() {
        findings.push(Finding::warning(
            file,
            decl.line,
            Some(&decl.name),
            format!("unknown key {key}"),
        ));
    }
}
