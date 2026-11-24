use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use bongos_project_repo::{ApprovalHook as _, ApprovalInput, ApprovalOpinion, LiveFacts};
use bongos_project_repo_model::{Author, ChangeMeta, ChangeSummary, DefinitionTree};
use bongos_workflow_model::unsandboxed::Mode;
use tempfile::TempDir;

use super::{SandboxSwitch, refusing_runs};
use crate::dispatch::RunnerSnapshot;
use crate::runner_program::ProgramSettings;

pub(crate) const PROJECT: &str = "title = \"Shop\"\n";
pub(crate) const REFUSING: &str = "title = \"Shop\"\n\n[runs]\nunsandboxed = \"refuse\"\n";
const WORKFLOW: &str = "workflows/ship/workflow.toml";

/// A `wait` node, with `with.env` when `env` is not empty, waiting on `command`.
fn wait(command: &str, env: &str) -> String {
    let env = if env.is_empty() {
        String::new()
    } else {
        format!("env = {env}\n")
    };
    format!(
        "name = \"ship\"\n\n[nodes.deploy]\nuse = \"wait\"\n\n[nodes.deploy.with]\n\
         condition = \"command\"\ncommand = [\"{command}\"]\n{env}"
    )
}

fn write(definitions: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        let path = definitions.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

/// What the hook says of a healer's change from `master` to `change`, both relative to the
/// definitions folder.
pub(crate) fn judge(master: &[(&str, &str)], change: &[(&str, &str)]) -> ApprovalOpinion {
    let folder = TempDir::new().unwrap();
    let master_root = folder.path().join("master");
    let change_root = folder.path().join("change");
    write(&master_root.join("definitions"), master);
    write(&change_root.join("definitions"), change);
    judge_roots(&master_root, &change_root)
}

/// What the hook says of a healer's change from the checkout `master_root` to `change_root`.
fn judge_roots(master_root: &Path, change_root: &Path) -> ApprovalOpinion {
    let tree = DefinitionTree::default();
    SandboxSwitch.judge(&ApprovalInput {
        change_id: "c1",
        project_id: "project-1",
        summary: &ChangeSummary::default(),
        meta: &ChangeMeta::new("edit", Author::healer("healer")),
        live: &LiveFacts::default(),
        master: &tree,
        change: &tree,
        master_root,
        change_root,
    })
}

pub(crate) fn needs(reason: &str) -> ApprovalOpinion {
    ApprovalOpinion::NeedsPerson(reason.to_string())
}

const ENV_CHANGED: &str =
    "changes the workflow sandbox switch nodes.deploy.with.env in workflows/ship/workflow.toml";

#[test]
fn a_misspelled_project_value_refuses_and_warns_once() {
    let values = |value: &str| [("run-1".to_string(), Some(toml::Value::from(value)))];
    let refusing = refusing_runs(values("refuze"), &BTreeSet::new());
    assert_eq!(refusing.runs, BTreeSet::from(["run-1".to_string()]));
    assert_eq!(refusing.misread, refusing.runs);
    assert_eq!(refusing.warnings.len(), 1);
    assert!(
        refusing.warnings[0].contains("\"refuze\""),
        "{:?}",
        refusing.warnings
    );
    let snapshot = RunnerSnapshot {
        programs: ProgramSettings {
            unsandboxed: Mode::Allow,
            ..ProgramSettings::default()
        },
        refusing_runs: refusing.runs.clone(),
        ..RunnerSnapshot::default()
    };
    assert_eq!(snapshot.unsandboxed_mode("run-1"), Mode::Refuse);
    assert_eq!(snapshot.unsandboxed_mode("run-2"), Mode::Allow);
    // The next refresh refuses again without warning again; a run with no value has no override.
    let again = refusing_runs(values("refuze"), &refusing.misread);
    assert_eq!(again.runs, refusing.runs);
    assert!(again.warnings.is_empty());
    let none = refusing_runs([("run-1".to_string(), None)], &BTreeSet::new());
    assert_eq!(none, super::Refusing::default());
}

#[test]
fn a_refusing_value_that_turns_bad_warns() {
    let values = |value: &str| [("run-1".to_string(), Some(toml::Value::from(value)))];
    let refusing = refusing_runs(values("refuse"), &BTreeSet::new());
    assert!(refusing.warnings.is_empty() && refusing.misread.is_empty());
    let bad = refusing_runs(values("refuze"), &refusing.misread);
    assert_eq!(bad.runs, refusing.runs);
    assert_eq!(bad.warnings.len(), 1, "{:?}", bad.warnings);
}

#[test]
fn an_unreadable_master_project_file_is_held() {
    let folder = TempDir::new().unwrap();
    let master_root = folder.path().join("master");
    let change_root = folder.path().join("change");
    write(
        &change_root.join("definitions"),
        &[("project.toml", PROJECT)],
    );
    let master = master_root.join("definitions");
    fs::create_dir_all(&master).unwrap();
    for bytes in [
        &b"title = \"Shop\n[runs"[..],
        &b"title = \"Sh\xffop\"\n"[..],
    ] {
        fs::write(master.join("project.toml"), bytes).unwrap();
        assert_eq!(
            judge_roots(&master_root, &change_root),
            needs("cannot read runs.unsandboxed")
        );
    }
}

#[cfg(unix)]
#[test]
fn a_workflow_in_a_folder_with_a_non_utf8_name_is_checked() {
    use std::os::unix::ffi::OsStrExt as _;
    let folder = TempDir::new().unwrap();
    let master_root = folder.path().join("master");
    let change_root = folder.path().join("change");
    let name = std::ffi::OsStr::from_bytes(b"sh\xffip");
    let file = Path::new("definitions/workflows")
        .join(name)
        .join("workflow.toml");
    for (root, text) in [
        (&master_root, wait("deploy-ready", "")),
        (&change_root, wait("deploy-ready", "[\"DEPLOY_TOKEN\"]")),
    ] {
        let path = root.join(&file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    assert_eq!(
        judge_roots(&master_root, &change_root),
        needs(
            "changes the workflow sandbox switch nodes.deploy.with.env in \
             workflows/sh\u{FFFD}ip/workflow.toml"
        )
    );
}

#[test]
fn any_change_of_runs_unsandboxed_needs_a_person() {
    let changed = needs("changes the workflow sandbox switch runs.unsandboxed");
    let other = "title = \"Shop\"\n\n[runs]\nunsandboxed = \"allow\"\n";
    for (before, after) in [(PROJECT, REFUSING), (REFUSING, PROJECT), (REFUSING, other)] {
        assert_eq!(
            judge(&[("project.toml", before)], &[("project.toml", after)]),
            changed,
            "{before} -> {after}"
        );
    }
}

#[test]
fn a_change_that_leaves_runs_unsandboxed_alone_is_not_held() {
    let retitled = REFUSING.replace("Shop", "Store");
    assert_eq!(
        judge(
            &[("project.toml", REFUSING)],
            &[("project.toml", &retitled)]
        ),
        ApprovalOpinion::NoOpinion
    );
    assert_eq!(
        judge(&[("project.toml", REFUSING)], &[("project.toml", REFUSING)]),
        ApprovalOpinion::NoOpinion
    );
}

#[test]
fn a_malformed_project_file_after_the_change_is_held() {
    assert_eq!(
        judge(
            &[("project.toml", PROJECT)],
            &[("project.toml", "title = \"Shop\n[runs")]
        ),
        needs("cannot read runs.unsandboxed")
    );
}

#[test]
fn an_added_or_changed_with_env_is_held() {
    let bare = wait("deploy-ready", "");
    let one = wait("deploy-ready", "[\"DEPLOY_TOKEN\"]");
    let two = wait("deploy-ready", "[\"DEPLOY_TOKEN\", \"AWS_SECRET\"]");
    assert_eq!(
        judge(&[(WORKFLOW, &bare)], &[(WORKFLOW, &one)]),
        needs(ENV_CHANGED)
    );
    assert_eq!(
        judge(&[(WORKFLOW, &one)], &[(WORKFLOW, &two)]),
        needs(ENV_CHANGED)
    );
    // A new workflow that sets it is an addition too.
    assert_eq!(judge(&[], &[(WORKFLOW, &one)]), needs(ENV_CHANGED));
}

#[test]
fn a_removed_with_env_is_held() {
    let one = wait("deploy-ready", "[\"DEPLOY_TOKEN\"]");
    let bare = wait("deploy-ready", "");
    assert_eq!(
        judge(&[(WORKFLOW, &one)], &[(WORKFLOW, &bare)]),
        needs(ENV_CHANGED)
    );
}

#[test]
fn a_definition_change_that_keeps_with_env_is_not_held() {
    let before = wait("deploy-ready", "[\"DEPLOY_TOKEN\"]");
    let after = wait("deploy-done", "[\"DEPLOY_TOKEN\"]");
    assert_eq!(
        judge(&[(WORKFLOW, &before)], &[(WORKFLOW, &after)]),
        ApprovalOpinion::NoOpinion
    );
}

#[test]
fn an_unparsable_definition_after_the_change_is_held() {
    let before = wait("deploy-ready", "");
    assert_eq!(
        judge(
            &[(WORKFLOW, &before)],
            &[(WORKFLOW, "[nodes.deploy\nuse = ")]
        ),
        needs("cannot read nodes.*.with.env in workflows/ship/workflow.toml")
    );
}
