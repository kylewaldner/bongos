use std::path::Path;
use std::sync::Arc;

use bongos_project_repo::LandingResult;
use gpui::TestAppContext;
use tempfile::TempDir;

use crate::definitions::{CHECKS_VALID, ChecksValid, validate_folder};
use crate::test_support::{
    definition_toml, fixture_project, land, main_commit, new_copy, person, refusals,
    write_definition,
};

/// A named set with one good check and one with both `run` and `command`, the bad one on line 6.
const BAD_SET: &str = "[[check]]\nname = \"good\"\ncommand = [\"make\", \"test\"]\n\n\
                       [[check]]\nname = \"both\"\nrun = \"checks/a\"\ncommand = [\"b\"]\n";

const GOOD_SET: &str = "[[check]]\nname = \"tests\"\ncommand = [\"make\", \"test\"]\n";

/// The task `l1` with an own table of checks, the second of which has both `run` and `command`.
fn task_with(checks: &str) -> String {
    definition_toml("l1", "Backfill", checks)
}

const BAD_TASK_CHECKS: &str = "\n[[checks.check]]\nname = \"tests\"\ncommand = [\"make\"]\n\n\
                               [[checks.check]]\nname = \"both\"\nrun = \"checks/a\"\n\
                               command = [\"b\"]\n";

#[test]
fn checks_the_folder_validator_names_the_file_and_line_of_a_bad_set() {
    let problems = validate_folder(Path::new("checks/sets/rust.toml"), BAD_SET);
    assert_eq!(problems.len(), 1, "{problems:?}");
    let shown = problems[0].to_string();
    assert!(shown.starts_with("checks/sets/rust.toml: line "), "{shown}");
    assert!(shown.contains("check both"), "{shown}");
    assert!(shown.contains("both run and command"), "{shown}");
    // The bad check starts at its `[[check]]` header, the fifth line of the set.
    assert_eq!(
        problems[0].reason,
        "line 5, check both: both run and command are given; give exactly one"
    );
}

#[test]
fn checks_the_folder_validator_accepts_a_good_set_and_ignores_other_files() {
    assert_eq!(
        validate_folder(Path::new("checks/sets/rust.toml"), GOOD_SET),
        vec![]
    );
    // Scripts and files outside `checks/sets/` are not the validator's to judge.
    assert_eq!(
        validate_folder(Path::new("checks/run-tests"), "#!/bin/sh\nexit 0\n"),
        vec![]
    );
    assert_eq!(
        validate_folder(Path::new("checks/notes.toml"), "not toml ["),
        vec![]
    );
    assert_eq!(
        validate_folder(Path::new("tasks/sets/x.toml"), "not toml ["),
        vec![]
    );
}

#[test]
fn checks_the_folder_validator_reports_a_set_that_is_not_toml_with_its_line() {
    let problems = validate_folder(Path::new("checks/sets/rust.toml"), "[[check]]\nname = \n");
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].path, "checks/sets/rust.toml");
    assert!(problems[0].reason.starts_with("line 2"), "{problems:?}");
}

#[test]
fn checks_a_set_with_only_a_warning_is_accepted() {
    // A closing check is fine in a set (it applies where a group uses it).
    let set = "[[closing]]\nname = \"final\"\ncommand = [\"make\", \"all\"]\n";
    assert_eq!(
        validate_folder(Path::new("checks/sets/close.toml"), set),
        vec![]
    );
}

fn registered_repo(root: &TempDir) -> bongos_project_repo::ProjectRepo {
    let repo = fixture_project(root);
    repo.add_landing_check(Arc::new(ChecksValid));
    repo.add_definition_folder(crate::CHECKS_FOLDER, validate_folder);
    repo
}

#[test]
fn an_invalid_task_check_set_is_refused_at_landing_with_file_and_line() {
    let root = TempDir::new().unwrap();
    let repo = registered_repo(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    // The script the bad check names exists, so `both run and command` is the only fault.
    write_definition(&copy, "checks/a", "#!/bin/sh\nexit 0\n");
    write_definition(
        &copy,
        "tasks/backfill/task.toml",
        &task_with(BAD_TASK_CHECKS),
    );
    let old = main_commit(&repo);

    let reasons = refusals(land(&repo, &copy, &author));
    let text = task_with(BAD_TASK_CHECKS);
    // The bad check starts at its own `[[checks.check]]` header, the second one in the file.
    let expected_line = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains("[[checks.check]]"))
        .map(|(index, _)| index + 1)
        .nth(1)
        .unwrap();
    let reason = reasons
        .iter()
        .find(|reason| reason.starts_with(&format!("{CHECKS_VALID}:")))
        .unwrap_or_else(|| panic!("checks-valid refused nothing: {reasons:?}"));
    assert_eq!(
        reason,
        &format!(
            "{CHECKS_VALID}: tasks/backfill/task.toml, line {expected_line}, check both: \
             both run and command are given; give exactly one"
        )
    );
    assert_eq!(main_commit(&repo), old, "nothing landed");
}

#[test]
fn a_task_check_set_that_uses_a_missing_set_or_script_is_refused() {
    let root = TempDir::new().unwrap();
    let repo = registered_repo(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(
        &copy,
        "tasks/backfill/task.toml",
        &task_with(
            "\n[checks]\nuse = [\"nope\"]\n\n[[checks.check]]\nname = \"gone\"\n\
             run = \"checks/missing.sh\"\n",
        ),
    );
    let reasons = refusals(land(&repo, &copy, &author));
    let reason = reasons.join("\n");
    assert!(
        reason.contains("the set nope, which does not exist"),
        "{reason}"
    );
    assert!(reason.contains("tasks/backfill/task.toml"), "{reason}");
    assert!(reason.contains("check gone"), "{reason}");
}

#[test]
fn a_valid_task_check_set_lands_and_a_change_without_task_changes_is_not_checked() {
    let root = TempDir::new().unwrap();
    let repo = registered_repo(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(&copy, "checks/sets/rust.toml", GOOD_SET);
    write_definition(
        &copy,
        "tasks/backfill/task.toml",
        &task_with(
            "\n[checks]\nuse = [\"rust\"]\n\n[[checks.check]]\nname = \"lint\"\ncommand = [\"make\", \"lint\"]\n",
        ),
    );
    let landed = land(&repo, &copy, &author);
    assert!(matches!(landed, LandingResult::Landed { .. }), "{landed:?}");

    // A task already on master with a bad set, left untouched by the next change, is not judged.
    let second = new_copy(&repo, "bold-crane-0002", &author);
    write_definition(&second, "documents/note.md", "a note\n");
    let landed = land(&repo, &second, &author);
    assert!(matches!(landed, LandingResult::Landed { .. }), "{landed:?}");
}

#[test]
fn checks_an_invalid_named_set_is_refused_at_landing_with_file_and_line() {
    let root = TempDir::new().unwrap();
    let repo = registered_repo(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(&copy, "checks/sets/rust.toml", BAD_SET);
    let old = main_commit(&repo);
    let reasons = refusals(land(&repo, &copy, &author));
    let reason = reasons.join("\n");
    assert!(reason.contains("checks/sets/rust.toml"), "{reason}");
    assert!(reason.contains("line "), "{reason}");
    assert!(reason.contains("both run and command"), "{reason}");
    assert_eq!(main_commit(&repo), old);
}

#[gpui::test]
fn init_registers_the_folder_and_the_landing_check(cx: &mut TestAppContext) {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    cx.update(|cx| {
        crate::definitions::register(cx);
        repo.use_registered_checks(cx);
    });
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(&copy, "checks/a", "#!/bin/sh\nexit 0\n");
    write_definition(&copy, "checks/sets/rust.toml", BAD_SET);
    let reasons = refusals(land(&repo, &copy, &author)).join("\n");
    assert!(reasons.contains("checks/sets/rust.toml"), "{reasons}");

    // With the set fixed, the landing check judges the task's own checks.
    write_definition(&copy, "checks/sets/rust.toml", GOOD_SET);
    write_definition(
        &copy,
        "tasks/backfill/task.toml",
        &task_with(BAD_TASK_CHECKS),
    );
    let reasons = refusals(land(&repo, &copy, &author)).join("\n");
    assert!(
        reasons.contains("tasks/backfill/task.toml, line "),
        "{reasons}"
    );
}
