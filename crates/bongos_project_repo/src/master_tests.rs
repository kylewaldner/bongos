use std::path::PathBuf;
use std::sync::Arc;

use bongos_project_repo_model::{Author, AuthorKind, PersonAtKeyboard};
use futures::FutureExt as _;
use futures::StreamExt as _;
use futures::executor::block_on;
use tempfile::TempDir;

use crate::landing::ProbePoint;
use crate::test_support::{
    CountingHook, add_task, fixture_project_with_hooks, land, landed, main_commit, new_copy,
    outside_commit, outside_document, person, reopen, request, run_git, start, verified,
};
use crate::{
    HistoryKind, LandingEvent, LandingResult, MASTER_IS_SPLIT, MasterStatus, NOT_A_LANDING,
    ProjectRepo, Recovery, RepoEvent, SplitResolution, StateApplyHooks, UNAPPROVED_OUTSIDE_CHANGE,
    WAITING_MASTER_SPLIT, landed_ref, outside_ref,
};

/// A project whose first change has landed, so there is a verified commit, pushing to a bare
/// remote.
fn landed_project(root: &TempDir, hooks: &StateApplyHooks) -> (ProjectRepo, PathBuf) {
    let mut repo = fixture_project_with_hooks(root, hooks);
    let remote = root.path().join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    block_on(
        repo.git()
            .run(&remote, &["init", "-q", "--bare", "-b", "main"]),
    )
    .unwrap();
    repo.location.remote = Some(remote.to_string_lossy().into_owned());
    let author = person();
    let copy = new_copy(&repo, "first-change-0000", &author);
    add_task(&copy, "first", "f1");
    landed(land(&repo, &copy, &author));
    (repo, remote)
}

fn remote_main(repo: &ProjectRepo, remote: &PathBuf) -> String {
    block_on(repo.git().rev_parse(remote, "main")).unwrap()
}

fn forged_message(change_id: &str) -> String {
    format!(
        "Outside edit\n\nBongos-Change: {change_id}\nBongos-Author-Kind: person\nBongos-Author: someone\nBongos-Checks: schema=pass"
    )
}

fn landing_commits(repo: &ProjectRepo) -> Vec<String> {
    repo.history()
        .unwrap()
        .into_iter()
        .filter(|row| matches!(row.kind, HistoryKind::Landing { .. }))
        .map(|row| row.commit)
        .collect()
}

#[test]
fn an_outside_commit_is_checked_kept_out_until_approved_and_its_adoption_runs_the_hooks() {
    let root = TempDir::new().unwrap();
    let hook = CountingHook::new("counting-hook", false);
    let (repo, _) = landed_project(&root, &CountingHook::registry(&hook));
    let before = verified(&repo).unwrap();
    let calls = hook.call_count();

    let outside = outside_document(&repo, "note.md", "an edit made with git");
    let MasterStatus::Outside(seen) = block_on(repo.check_master()).unwrap() else {
        panic!("expected an outside change");
    };
    assert_eq!(seen.commit, outside);
    assert!(seen.passed, "{:?}", seen.problems);
    assert_eq!(verified(&repo).as_deref(), Some(before.as_str()));
    assert_eq!(repo.outside_change().unwrap().unwrap().commit, outside);

    // Every landing refuses meanwhile.
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    assert_eq!(
        land(&repo, &copy, &author),
        LandingResult::Refused {
            reasons: vec![UNAPPROVED_OUTSIDE_CHANGE.to_string()]
        }
    );
    assert_eq!(verified(&repo).as_deref(), Some(before.as_str()));

    // Approved by a person, it becomes the verified commit, and the hooks see it.
    let adopted = block_on(repo.approve_outside(PersonAtKeyboard::for_test())).unwrap();
    assert_eq!(adopted, outside);
    assert_eq!(verified(&repo).as_deref(), Some(outside.as_str()));
    assert_eq!(hook.call_count(), calls + 1);
    assert!(repo.outside_change().unwrap().is_none());
    assert!(!landing_commits(&repo).contains(&outside));
    // Landings go on from it.
    let commit = landed(land(&repo, &copy, &author));
    assert_eq!(
        run_git(&repo, &["rev-parse", &format!("{commit}^")]),
        outside
    );
}

#[test]
fn an_outside_commit_that_fails_the_checks_cannot_be_approved() {
    let root = TempDir::new().unwrap();
    let (repo, _) = landed_project(&root, &StateApplyHooks::new());
    let before = verified(&repo).unwrap();
    block_on(outside_commit(
        repo.git().clone(),
        repo.location().repository.clone(),
        vec![("definitions/junk.txt".to_string(), "junk\n".to_string())],
        "junk".to_string(),
    ));
    let MasterStatus::Outside(seen) = block_on(repo.check_master()).unwrap() else {
        panic!("expected an outside change");
    };
    assert!(!seen.passed);
    assert!(seen.problems[0].contains("junk.txt"));
    assert!(block_on(repo.approve_outside(PersonAtKeyboard::for_test())).is_err());
    assert_eq!(verified(&repo).as_deref(), Some(before.as_str()));
}

#[test]
fn a_rewritten_master_is_never_adopted_and_landings_refuse() {
    let root = TempDir::new().unwrap();
    let (repo, _) = landed_project(&root, &StateApplyHooks::new());
    let before = verified(&repo).unwrap();
    run_git(
        &repo,
        &["update-ref", "refs/heads/main", &format!("{before}^")],
    );
    assert!(matches!(
        block_on(repo.check_master()).unwrap(),
        MasterStatus::Rewritten { .. }
    ));
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    match land(&repo, &copy, &author) {
        LandingResult::Refused { reasons } => {
            assert!(
                reasons[0].contains("rewritten outside bongos"),
                "{reasons:?}"
            )
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(verified(&repo).as_deref(), Some(before.as_str()));
}

#[test]
fn a_forged_change_trailer_is_outside_and_crash_recovery_ignores_it() {
    let root = TempDir::new().unwrap();
    let (repo, _) = landed_project(&root, &StateApplyHooks::new());
    let before = verified(&repo).unwrap();
    let real = repo
        .changes(&crate::ChangeFilter {
            state: Some(crate::ChangeState::Landed),
            ..Default::default()
        })
        .unwrap()[0]
        .change_id
        .clone();

    // On a quiet master: an outside commit, not a landing, whatever its trailers say.
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let forged = block_on(outside_commit(
        repo.git().clone(),
        repo.location().repository.clone(),
        vec![("definitions/documents/x.md".to_string(), "x\n".to_string())],
        forged_message(&real),
    ));
    assert!(matches!(
        block_on(repo.check_master()).unwrap(),
        MasterStatus::Outside(_)
    ));
    assert_eq!(verified(&repo).as_deref(), Some(before.as_str()));
    assert!(!landing_commits(&repo).contains(&forged));
    assert_eq!(repo.revertible(&forged), Err(NOT_A_LANDING.to_string()));
    run_git(&repo, &["update-ref", "refs/heads/main", &before]);
    assert_eq!(block_on(repo.check_master()).unwrap(), MasterStatus::Clean);

    // During a crashed landing: a commit forged with the landing's own change id is not taken
    // as the landing; recovery finds a split master and keeps bongos' state.
    repo.set_probe(Arc::new(|point, _| {
        async move { point == ProbePoint::AfterStateTransaction }.boxed()
    }));
    assert!(block_on(repo.land_now(&copy.change_id, request("crash", &author), start())).is_err());
    let recorded = verified(&repo).unwrap();
    let forged = block_on(outside_commit(
        repo.git().clone(),
        repo.location().repository.clone(),
        vec![("definitions/documents/y.md".to_string(), "y\n".to_string())],
        forged_message(&copy.change_id),
    ));
    let repo = reopen(repo, &StateApplyHooks::new());
    let Recovery::Split(split) = block_on(repo.recover_landing()).unwrap() else {
        panic!("expected a split master");
    };
    assert_eq!(split.main, forged);
    assert_eq!(split.verified, recorded);
    assert_eq!(verified(&repo).as_deref(), Some(recorded.as_str()));
    assert_eq!(main_commit(&repo), forged);
    assert!(!landing_commits(&repo).contains(&forged));
}

/// Lands a change while an outside commit (with forged trailers) moves master after the
/// re-check and before the ref move. Returns the landed commit and the outside commit.
fn split_by_outside_commit(repo: &ProjectRepo, remote: &PathBuf) -> (String, String, String) {
    let author = person();
    let copy = new_copy(repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let old = main_commit(repo);
    let git = repo.git().clone();
    let repository = repo.location().repository.clone();
    let message = forged_message(&copy.change_id);
    let remote_text = remote.to_string_lossy().into_owned();
    let outside = Arc::new(parking_lot::Mutex::new(None));
    {
        let outside = outside.clone();
        repo.set_probe(Arc::new(move |point, _| {
            let git = git.clone();
            let repository = repository.clone();
            let message = message.clone();
            let remote_text = remote_text.clone();
            let outside = outside.clone();
            async move {
                if point == ProbePoint::BeforeRefMove && outside.lock().is_none() {
                    let commit = outside_commit(
                        git.clone(),
                        repository.clone(),
                        vec![("definitions/documents/x.md".to_string(), "x\n".to_string())],
                        message,
                    )
                    .await;
                    // Whoever made it also pushed it.
                    git.run(&repository, &["push", "-q", &remote_text, "main"])
                        .await
                        .unwrap();
                    *outside.lock() = Some(commit);
                }
                false
            }
            .boxed()
        }));
    }
    let mut events = repo.repo_events();
    let new = landed(land(repo, &copy, &author));
    let outside = outside.lock().clone().unwrap();
    let mut split_event = None;
    while let Ok(event) = events.try_recv() {
        if let RepoEvent::MasterSplit { .. } = event {
            split_event = Some(event);
        }
    }
    assert_eq!(
        split_event,
        Some(RepoEvent::MasterSplit {
            change_id: copy.change_id,
            main: outside.clone(),
            verified: new.clone(),
        })
    );
    (old, new, outside)
}

#[test]
fn a_live_split_keeps_the_lock_refuses_landings_and_keep_bongos_fast_forwards_to_the_merge() {
    let root = TempDir::new().unwrap();
    let (repo, remote) = landed_project(&root, &StateApplyHooks::new());
    let (_old, new, outside) = split_by_outside_commit(&repo, &remote);

    let split = repo.split_master().unwrap().unwrap();
    assert_eq!(
        (split.main.as_str(), split.verified.as_str()),
        (outside.as_str(), new.as_str())
    );
    assert_eq!(repo.landing_holder(), Some(split.change_id));
    assert_eq!(verified(&repo).as_deref(), Some(new.as_str()));
    assert_eq!(main_commit(&repo), outside);
    // Bongos' commit is pinned so gc cannot prune it while nothing else reaches it.
    assert_eq!(run_git(&repo, &["rev-parse", &landed_ref(&new)]), new);
    let status = block_on(repo.check_master()).unwrap();
    assert!(status.warning().unwrap().contains("changed outside bongos"));
    // A person's landing is refused while master is split.
    let author = person();
    let other = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&other, "beta", "b1");
    assert_eq!(
        land(&repo, &other, &author),
        LandingResult::Refused {
            reasons: vec![MASTER_IS_SPLIT.to_string()]
        }
    );

    let resolved = block_on(repo.resolve_split(
        SplitResolution::KeepBongos,
        PersonAtKeyboard::for_test(),
        start(),
    ))
    .unwrap();
    let merge = resolved.merge;
    assert_eq!(resolved.outside, outside);
    assert_eq!(resolved.saved, vec![outside.clone()]);
    assert!(resolved.brought_in.is_none());
    assert_eq!(main_commit(&repo), merge);
    assert_eq!(
        run_git(&repo, &["rev-list", "--parents", "-n", "1", &merge]),
        format!("{merge} {new} {outside}")
    );
    assert_eq!(
        run_git(&repo, &["rev-parse", &format!("{merge}^{{tree}}")]),
        run_git(&repo, &["rev-parse", &format!("{new}^{{tree}}")])
    );
    assert_eq!(
        run_git(&repo, &["rev-parse", &outside_ref(&outside)]),
        outside
    );
    assert_eq!(verified(&repo).as_deref(), Some(merge.as_str()));
    assert!(repo.split_master().unwrap().is_none());
    assert!(repo.landing_holder().is_none());
    // The merge reaches it now, and the pin is gone.
    assert!(!run_git(&repo, &["for-each-ref", "refs/bongos/landed/"]).contains(&new));
    // The remote that already held the outside commit took the merge as a fast-forward.
    assert_eq!(remote_main(&repo, &remote), merge);
    assert_eq!(repo.push_failure().unwrap(), None);
    // Neither the outside commit nor the merge is a landing, and neither can be reverted.
    let landings = landing_commits(&repo);
    assert!(landings.contains(&new));
    assert!(!landings.contains(&outside) && !landings.contains(&merge));
    assert!(repo.history().unwrap().iter().any(|row| row.commit == merge
        && row.kind
            == HistoryKind::SplitResolution {
                outside: outside.clone()
            }));
    assert_eq!(repo.revertible(&merge), Err(NOT_A_LANDING.to_string()));
    assert_eq!(repo.revertible(&outside), Err(NOT_A_LANDING.to_string()));
    assert_eq!(repo.revertible(&new), Ok(()));
    // Landings go on.
    landed(land(&repo, &other, &author));
    assert_eq!(block_on(repo.check_master()).unwrap(), MasterStatus::Clean);
}

#[test]
fn a_second_outside_commit_becomes_the_merges_parent_and_bring_in_carries_both_diffs() {
    let root = TempDir::new().unwrap();
    let (repo, remote) = landed_project(&root, &StateApplyHooks::new());
    let (_old, new, first_outside) = split_by_outside_commit(&repo, &remote);
    let second_outside = outside_document(&repo, "y.md", "a later outside edit");
    run_git(&repo, &["push", "-q", &remote.to_string_lossy(), "main"]);

    let resolved = block_on(repo.resolve_split(
        SplitResolution::KeepBongosAndBringIn,
        PersonAtKeyboard::for_test(),
        start(),
    ))
    .unwrap();
    let merge = resolved.merge.clone();
    assert_eq!(resolved.outside, second_outside);
    assert_eq!(
        resolved.saved,
        vec![first_outside.clone(), second_outside.clone()]
    );
    assert_eq!(
        run_git(&repo, &["rev-list", "--parents", "-n", "1", &merge]),
        format!("{merge} {new} {second_outside}")
    );
    for commit in [&first_outside, &second_outside] {
        assert_eq!(
            run_git(&repo, &["rev-parse", &outside_ref(commit)]),
            *commit
        );
    }
    assert_eq!(main_commit(&repo), merge);
    assert_eq!(remote_main(&repo, &remote), merge);
    assert_eq!(verified(&repo).as_deref(), Some(merge.as_str()));
    let landings = landing_commits(&repo);
    for commit in [&first_outside, &second_outside, &merge] {
        assert!(!landings.contains(commit));
    }

    // The outside change comes back as an unattributed working copy with both diffs.
    let copy = resolved.brought_in.unwrap();
    for name in ["x.md", "y.md"] {
        assert!(copy.path.join("definitions/documents").join(name).is_file());
    }
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.author.kind(), AuthorKind::Unattributed);
    assert_eq!(
        run_git(&repo, &["rev-parse", &format!("{}^", copy.branch)]),
        merge
    );
}

#[test]
fn a_lane_landing_during_the_split_waits_and_resumes_after_the_resolve() {
    let root = TempDir::new().unwrap();
    let (repo, remote) = landed_project(&root, &StateApplyHooks::new());
    let agent = Author::agent("lane 2");
    let lane = new_copy(&repo, "lane-change-0003", &agent);
    add_task(&lane, "gamma", "c1");
    split_by_outside_commit(&repo, &remote);

    let mut events = repo.landing_events();
    let lane_id = lane.change_id.clone();
    let landing = repo.land_now(&lane.change_id, request("lane", &agent), start());
    let resolve = async {
        while let Some((change, event)) = events.next().await {
            if change == lane_id
                && event
                    == (LandingEvent::Waiting {
                        reason: WAITING_MASTER_SPLIT.to_string(),
                    })
            {
                break;
            }
        }
        repo.resolve_split(
            SplitResolution::KeepBongos,
            PersonAtKeyboard::for_test(),
            start(),
        )
        .await
        .unwrap()
    };
    let (result, resolved) = block_on(async { futures::join!(landing, resolve) });
    let commit = landed(result.unwrap());
    assert_eq!(main_commit(&repo), commit);
    assert_eq!(
        run_git(&repo, &["rev-parse", &format!("{commit}^")]),
        resolved.merge
    );
}

#[test]
fn an_outside_commit_before_the_first_landing_is_outside_not_adopted() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project_with_hooks(&root, &StateApplyHooks::new());
    let created = main_commit(&repo);
    assert_eq!(verified(&repo), None);

    let outside = outside_document(&repo, "note.md", "an edit made with git before any landing");
    let MasterStatus::Outside(seen) = block_on(repo.check_master()).unwrap() else {
        panic!("expected an outside change");
    };
    assert_eq!(seen.commit, outside);
    // The Work view and new working copies stay on the commit that created the project.
    assert_eq!(block_on(repo.shown_commit()).unwrap().0, created);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    assert_eq!(
        run_git(&repo, &["rev-parse", &copy.branch]),
        created,
        "a working copy starts from the creation commit"
    );
    add_task(&copy, "alpha", "a1");
    assert_eq!(
        land(&repo, &copy, &author),
        LandingResult::Refused {
            reasons: vec![UNAPPROVED_OUTSIDE_CHANGE.to_string()]
        }
    );
    assert_eq!(main_commit(&repo), outside);

    // Approved, it is adopted like any other outside commit, and landings go on from it.
    let adopted = block_on(repo.approve_outside(PersonAtKeyboard::for_test())).unwrap();
    assert_eq!(adopted, outside);
    assert_eq!(verified(&repo).as_deref(), Some(outside.as_str()));
    let commit = landed(land(&repo, &copy, &author));
    assert_eq!(
        run_git(&repo, &["rev-parse", &format!("{commit}^")]),
        outside
    );
}
