use std::sync::Arc;

use bongos_project_repo_model::{Author, ChangeMeta, Problem};
use bongos_work_model::TodoState;
use futures::channel::oneshot;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::check_fakes::FakeCheck;
use crate::test_support::{
    PROJECT_TOML, add_task, fixture_project, land, land_as, landed, main_commit, new_copy, person,
    request, run_git, start, task_with_todos, write_definition,
};
use crate::{CheckOutcome, CheckPhase, LandMode, LandRequest, LandingResult};

fn check_only(repo: &crate::ProjectRepo, copy: &crate::WorkingCopy) -> LandingResult {
    block_on(land_as(
        &repo,
        &copy.change_id,
        LandRequest {
            meta: ChangeMeta::new("check", copy_author()),
            mode: LandMode::CheckOnly,
        },
        start(),
    ))
    .unwrap()
}

fn copy_author() -> Author {
    Author::agent("chat 1")
}

fn refusals(result: LandingResult) -> Vec<String> {
    match result {
        LandingResult::Refused { reasons } => reasons,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn waiting(result: LandingResult) -> String {
    match result {
        LandingResult::NoVerdict { reason } => reason,
        other => panic!("expected no verdict, got {other:?}"),
    }
}

#[test]
fn a_landing_checks_entry_with_phase_amending_is_refused_by_the_schema_check() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = copy_author();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(
        &copy,
        "project.toml",
        &format!(
            "{PROJECT_TOML}\n[[landing.checks]]\nname = \"rebalance-mine\"\nphase = \"amending\"\n\
             run = {{ workflow = \"rebalance\" }}\n"
        ),
    );
    let old = main_commit(&repo);
    let reasons = refusals(land(&repo, &copy, &author));
    assert!(
        reasons
            .iter()
            .any(|reason| reason.starts_with("schema:") && reason.contains("phase \"amending\"")),
        "{reasons:?}"
    );
    assert_eq!(main_commit(&repo), old);
}

#[test]
fn a_slow_check_on_one_change_does_not_hold_up_another_landing() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let slow = Arc::new(FakeCheck::new("slow", CheckPhase::BeforeQueue));
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    *slow.gate.lock() = Some((started_tx, release_rx));
    repo.add_landing_check(slow.clone());
    let author = copy_author();
    let first = new_copy(&repo, "amber-otter-0001", &author);
    let second = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&first, "alpha", "a1");
    add_task(&second, "beta", "b1");

    let land_first = land_as(&repo, &first.change_id, request("first", &author), start());
    let land_second = async {
        started_rx.await.unwrap();
        // The first change's slow check is running: the second lands meanwhile.
        let result = land_as(
            &repo,
            &second.change_id,
            request("second", &author),
            start(),
        )
        .await;
        release_tx.send(()).unwrap();
        result
    };
    let (first_result, second_result) = block_on(async { futures::join!(land_first, land_second) });
    let second_commit = landed(second_result.unwrap());
    let first_commit = landed(first_result.unwrap());
    assert_eq!(main_commit(&repo), first_commit);
    assert_eq!(
        run_git(&repo, &["rev-parse", &format!("{first_commit}^")]),
        second_commit
    );
    // The first change's tree moved under its slow check, which ran again on the rebased tree.
    let seen = slow.seen.lock().clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(
        seen.last().unwrap().tree,
        run_git(&repo, &["rev-parse", "main^{tree}"])
    );
}

#[test]
fn a_result_for_another_tree_is_never_counted_and_no_verdict_is_retried() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let counting = Arc::new(FakeCheck::new("counting", CheckPhase::BeforeQueue));
    repo.add_landing_check(counting.clone());
    let unsure = Arc::new(
        FakeCheck::new("unsure", CheckPhase::BeforeQueue)
            .answering(vec![CheckOutcome::no_verdict("not sure yet")]),
    );
    repo.add_landing_check(unsure.clone());
    let author = copy_author();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");

    let LandingResult::Checked {
        tree: first_tree,
        checks,
    } = check_only(&repo, &copy)
    else {
        panic!("expected check results");
    };
    assert!(checks.iter().any(|result| result.check == "unsure"
        && matches!(result.outcome, CheckOutcome::NoVerdict { .. })));
    // The same tree again: the stored pass is used, the no verdict is tried again.
    let LandingResult::Checked { tree, .. } = check_only(&repo, &copy) else {
        panic!("expected check results");
    };
    assert_eq!(tree, first_tree);
    assert_eq!(counting.calls(), 1);
    assert_eq!(unsure.calls(), 2);

    // Another tree: the result stored for the first tree is not counted for it.
    // Committed by the check's first step.
    add_task(&copy, "gamma", "c1");
    let LandingResult::Checked {
        tree: second_tree, ..
    } = check_only(&repo, &copy)
    else {
        panic!("expected check results");
    };
    assert_ne!(second_tree, first_tree);
    assert_eq!(counting.calls(), 2);
    let on_first = repo.check_results_on(&copy.change_id, &first_tree).unwrap();
    let on_second = repo
        .check_results_on(&copy.change_id, &second_tree)
        .unwrap();
    assert!(on_first.iter().any(|result| result.check == "counting"));
    assert!(on_second.iter().any(|result| result.check == "counting"));
    assert!(on_second.iter().all(|result| result.tree == second_tree));
}

#[test]
fn a_task_started_while_the_change_waits_is_refused() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let unsure = Arc::new(
        FakeCheck::new("unsure", CheckPhase::BeforeQueue)
            .answering(vec![CheckOutcome::no_verdict("not sure yet")]),
    );
    repo.add_landing_check(unsure);
    let author = copy_author();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    // Retags `backfill`, which nobody has started yet.
    write_definition(
        &copy,
        "tasks/backfill/task.toml",
        &crate::test_support::definition_toml(
            "l1",
            "Backfill",
            "other",
            &[("t1", "implement"), ("t2", "review"), ("t3", "check")],
        )
        .replace(
            "kind = \"other\"\n",
            "kind = \"other\"\nareas = [\"billing\"]\n",
        ),
    );
    assert_eq!(waiting(land(&repo, &copy, &author)), "unsure: not sure yet");
    let passed = repo.check_results(&copy.change_id).unwrap();
    assert!(
        passed
            .iter()
            .any(|result| result.check == "started-unchanged"
                && result.outcome == CheckOutcome::Pass)
    );

    // While it waits, the task is started.
    repo.writer_or_error()
        .unwrap()
        .transact(|tx| tx.set_task("l1", task_with_todos(&[("t1", TodoState::Done)])))
        .unwrap();
    let reasons = refusals(land(&repo, &copy, &author));
    assert!(
        reasons
            .iter()
            .any(|reason| reason.starts_with("started-unchanged:") && reason.contains("backfill")),
        "{reasons:?}"
    );
}

#[test]
fn a_secret_pattern_in_changed_text_is_refused_without_quoting_it() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    let secret = "AKIAABCDEFGHIJKLMNOP";
    write_definition(
        &copy,
        "documents/notes.md",
        &format!("deploy with {secret}\n"),
    );
    let reasons = refusals(land(&repo, &copy, &author));
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(reasons[0].starts_with("secrets: definitions/documents/notes.md:1"));
    assert!(!reasons[0].contains(secret));
}

#[test]
fn a_disabled_built_in_does_not_run_and_the_always_on_ones_cannot_be_disabled() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let setup = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(
        &setup,
        "project.toml",
        &format!("{PROJECT_TOML}\n[[landing.checks]]\nname = \"secrets\"\nenabled = false\n"),
    );
    landed(land(&repo, &setup, &author));
    let copy = new_copy(&repo, "calm-heron-0002", &author);
    write_definition(&copy, "documents/notes.md", "AKIAABCDEFGHIJKLMNOP\n");
    landed(land(&repo, &copy, &author));

    let refused = new_copy(&repo, "dark-ibis-0003", &author);
    write_definition(
        &refused,
        "project.toml",
        &format!("{PROJECT_TOML}\n[[landing.checks]]\nname = \"schema\"\nenabled = false\n"),
    );
    let reasons = refusals(land(&repo, &refused, &author));
    assert!(reasons[0].contains("cannot be disabled"), "{reasons:?}");
}

#[test]
fn a_landing_without_a_subject_takes_one_generated_from_the_change() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let commit = landed(
        block_on(land_as(
            &repo,
            &copy.change_id,
            request("", &author),
            start(),
        ))
        .unwrap(),
    );
    assert_eq!(
        run_git(&repo, &["log", "-1", "--format=%s", &commit]),
        "Add 1 task"
    );
}

fn accept_any(_path: &std::path::Path, _text: &str) -> Vec<Problem> {
    Vec::new()
}

#[cfg(unix)]
fn write_script(copy: &crate::WorkingCopy, relative: &str, text: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    write_definition(copy, relative, text);
    let path = copy.path.join("definitions").join(relative);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_script_check_passes_on_exit_0_fails_on_1_and_gives_no_verdict_otherwise() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.add_definition_folder("checks", accept_any);
    let author = person();
    let setup = new_copy(&repo, "amber-otter-0001", &author);
    write_script(
        &setup,
        "checks/verdict.sh",
        "#!/bin/sh\ncode=$(cat \"$1/definitions/documents/exit.txt\" 2>/dev/null || echo 0)\n\
         case \"$2\" in *tasks_added*) ;; *) echo \"no summary\"; exit 4;; esac\n\
         echo \"verdict $code\"\nexit \"$code\"\n",
    );
    write_script(
        &setup,
        "checks/broken.sh",
        "#!/nonexistent/interpreter\nexit 0\n",
    );
    write_definition(
        &setup,
        "project.toml",
        &format!(
            "{PROJECT_TOML}\n[[landing.checks]]\nname = \"verdict\"\n\
             run = {{ script = \"checks/verdict.sh\" }}\n\n[[landing.checks]]\nname = \"broken\"\n\
             applies = {{ paths = [\"definitions/documents/broken/**\"] }}\n\
             run = {{ script = \"checks/broken.sh\" }}\n"
        ),
    );
    // Master has no declarations yet, so the change that adds them lands unchecked by them.
    landed(land(&repo, &setup, &author));

    let with_exit = |name: &str, code: &str| {
        let copy = new_copy(&repo, name, &author);
        write_definition(&copy, "documents/exit.txt", code);
        copy
    };
    let pass = with_exit("bold-crane-0002", "0\n");
    landed(land(&repo, &pass, &author));
    let fail = with_exit("calm-heron-0003", "1\n");
    assert_eq!(
        refusals(land(&repo, &fail, &author)),
        vec!["verdict: checks/verdict.sh: verdict 1".to_string()]
    );
    let other = with_exit("dark-ibis-0004", "3\n");
    let reason = waiting(land(&repo, &other, &author));
    assert!(
        reason.starts_with("verdict: checks/verdict.sh exited with 3"),
        "{reason}"
    );
    let broken = with_exit("eager-lark-0005", "0\n");
    write_definition(&broken, "documents/broken/x.md", "x\n");
    let reason = waiting(land(&repo, &broken, &author));
    assert!(
        reason.starts_with("broken: checks/broken.sh could not run"),
        "{reason}"
    );
}

#[cfg(unix)]
#[test]
fn a_hung_script_check_is_stopped_with_no_verdict_and_runs_again_on_the_next_try() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.add_definition_folder("checks", accept_any);
    let author = person();
    let ready = root.path().join("ready");
    let setup = new_copy(&repo, "amber-otter-0001", &author);
    write_script(
        &setup,
        "checks/slow.sh",
        &format!(
            "#!/bin/sh\n[ -f '{}' ] && exit 0\nexec sleep 600\n",
            ready.display()
        ),
    );
    write_definition(
        &setup,
        "project.toml",
        &format!(
            "{PROJECT_TOML}\n[[landing.checks]]\nname = \"slow\"\n\
             run = {{ script = \"checks/slow.sh\" }}\n"
        ),
    );
    landed(land(&repo, &setup, &author));

    let copy = new_copy(&repo, "bold-crane-0002", &author);
    add_task(&copy, "alpha", "a1");
    // The script hangs until its time is up.
    repo.set_timer(crate::check_fakes::elapsed_timer());
    let reason = waiting(land(&repo, &copy, &author));
    assert!(
        reason.starts_with("slow: checks/slow.sh ran longer than 600 seconds and was stopped"),
        "{reason}"
    );
    // The no verdict is not kept: the same tree runs the script again, which now ends.
    repo.set_timer(crate::check_fakes::ManualTimer::default().timer());
    std::fs::write(&ready, "").unwrap();
    landed(land(&repo, &copy, &author));
}
