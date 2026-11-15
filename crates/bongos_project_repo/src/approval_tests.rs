//! Check 9 through real landings: real git and a real state store in a temporary folder. Person
//! paths run on synthetic changes with approval rows (the quick edit path comes later).

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use bongos_project_repo_model::checks::ApprovalBasis;
use bongos_project_repo_model::{Author, ChangeMeta};
use bongos_work_model::commands::Context as WorkContext;
use futures::channel::mpsc::UnboundedReceiver;
use futures::executor::block_on;
use futures::future::BoxFuture;
use futures::{FutureExt as _, StreamExt as _};
use parking_lot::Mutex;
use tempfile::TempDir;

use crate::approval::PersonGrant;
use crate::check_fakes::FakeAmender;
use crate::landing::{LandingRecord, ProbePoint};
use crate::test_support::{
    ApproveRule, add_task, bare_fixture_project, fixture_project, keyboard, land_as, landed,
    main_commit, new_copy, person, reopen_bare, request, run_git, start, write_definition,
};
use crate::{
    ApprovalHook, ApprovalInput, ApprovalOpinion, AutomaticRule, ClientKind, HostBoundary,
    LandMode, LandRequest, LandingEvent, LandingResult, NEEDS_A_WINDOW, OwnSubtreeHook,
    PeerIdentity, PersonClaim, ProjectRepo, RepoCommand, StateApplyHooks,
};

type Landing = futures::future::Fuse<BoxFuture<'static, Result<LandingResult>>>;

/// The landing engine asked to land `change_id` with no person's grant (a quick edit, a lane, an
/// agent, or a person's change landed by code).
fn landing(repo: &ProjectRepo, change_id: &str, request: LandRequest) -> Landing {
    let repo = repo.clone();
    let change_id = change_id.to_string();
    async move { repo.land_now(&change_id, request, start()).await }
        .boxed()
        .fuse()
}

/// `project: land` by the person at the keyboard.
fn person_landing(repo: &ProjectRepo, change_id: &str, request: LandRequest) -> Landing {
    let repo = repo.clone();
    let change_id = change_id.to_string();
    async move {
        repo.land_as_person(&change_id, request, keyboard(), start())
            .await
    }
    .boxed()
    .fuse()
}

/// Drives `landing` until `change_id` shows an event `wanted` accepts; a landing that ends first
/// fails the test.
async fn until_event(
    mut landing: &mut Landing,
    events: &mut UnboundedReceiver<(String, LandingEvent)>,
    change_id: &str,
    wanted: impl Fn(&LandingEvent) -> bool,
) -> LandingEvent {
    loop {
        futures::select! {
            result = landing => panic!("{change_id} ended before the event: {result:?}"),
            next = events.next() => {
                let (id, event) = next.expect("the landing events");
                if id == change_id && wanted(&event) {
                    return event;
                }
            }
        }
    }
}

fn waits_for_approval(event: &LandingEvent) -> bool {
    matches!(event, LandingEvent::Waiting { reason, .. } if reason.starts_with("waiting for approval"))
}

/// Lands `change_id` with no person's grant until it waits for approval and returns the reason
/// shown, then abandons it.
fn waits_for_a_person(repo: &ProjectRepo, change_id: &str, request: LandRequest) -> String {
    waits_in(repo, change_id, landing(repo, change_id, request))
}

/// Drives `landing` of `change_id` until it waits for approval and returns the reason shown,
/// then abandons it.
fn waits_in(repo: &ProjectRepo, change_id: &str, mut landing: Landing) -> String {
    let before = main_commit(repo);
    let mut events = repo.landing_events();
    let reason = block_on(async {
        let event = until_event(&mut landing, &mut events, change_id, waits_for_approval).await;
        repo.abandon_now(change_id, start()).await.unwrap();
        assert_eq!(landing.await.unwrap(), LandingResult::Abandoned);
        event
    });
    assert_eq!(
        main_commit(repo),
        before,
        "an unapproved change moved master"
    );
    match reason {
        LandingEvent::Waiting { reason, .. } => reason,
        _ => unreachable!(),
    }
}

/// Commits what the working copy holds.
fn commit(repo: &ProjectRepo, copy: &crate::WorkingCopy, subject: &str) -> String {
    block_on(
        repo.git()
            .commit_if_dirty(&copy.path, subject, &crate::Identity::default()),
    )
    .unwrap();
    block_on(repo.git().rev_parse(&copy.path, "HEAD")).unwrap()
}

/// Writes a person approval row of `basis` for the change as its branch is now, as the person
/// at the keyboard (`PersonEdit` stands for the quick edit path).
fn record(repo: &ProjectRepo, change_id: &str, basis: ApprovalBasis) -> crate::ApprovalRow {
    let writer = repo.writer_or_error().unwrap();
    let row = repo.change(change_id).unwrap().unwrap();
    block_on(repo.record_person_approval(
        &writer,
        &row,
        basis,
        PersonGrant::for_test(&person().name),
        start(),
    ))
    .unwrap()
}

/// The approval line of the landing of `change_id`.
fn approval_line(repo: &ProjectRepo, change_id: &str) -> Option<String> {
    let landings = repo.read_state(|read| read.landings()).unwrap();
    let (_, text) = landings
        .into_iter()
        .find(|(id, _)| id == change_id)
        .expect("the landing record");
    toml::from_str::<LandingRecord>(&text).unwrap().approval
}

fn engine_request(subject: &str, author: &Author) -> LandRequest {
    // As the landing engine is asked by a quick edit, a lane or an agent: no person's grant.
    LandRequest {
        meta: ChangeMeta::new(subject, author.clone()),
        mode: LandMode::Land,
    }
}

fn land_now(repo: &ProjectRepo, change_id: &str, author: &Author) -> LandingResult {
    block_on(repo.land_now(change_id, engine_request("land it", author), start())).unwrap()
}

/// An amending check that adds an edge to the task `alpha` and writes the stamp file.
fn edge_and_stamp(times: usize) -> FakeAmender {
    let mut amender = FakeAmender::new(
        "rebalance",
        times,
        Arc::new(|writable: &Path| {
            let task = writable.join("definitions/tasks/alpha/task.toml");
            let Ok(text) = std::fs::read_to_string(&task) else {
                return false;
            };
            let stamp = writable.join("definitions/stamp.txt");
            if std::fs::read_to_string(&stamp).ok().as_deref() == Some("balanced\n") {
                return false;
            }
            if !text.contains("after =") {
                std::fs::write(
                    &task,
                    text.replacen(
                        "kind = \"other\"\n",
                        "kind = \"other\"\nafter = [{ task = \"backfill\", unblock = \"merged\" }]\n",
                        1,
                    ),
                )
                .unwrap();
            }
            std::fs::write(&stamp, "balanced\n").unwrap();
            true
        }),
    );
    amender.stamp_paths = vec!["stamp.txt"];
    amender
}

/// A hook that implements no rule and records what it saw.
#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<(String, String, usize, usize, bool, bool)>>,
}

impl ApprovalHook for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }

    fn judge(&self, input: &ApprovalInput) -> ApprovalOpinion {
        self.seen.lock().push((
            input.change_id.to_string(),
            input.project_id.to_string(),
            input.master.nodes().len(),
            input.change.nodes().len(),
            input.master_root.join("definitions/project.toml").is_file(),
            input
                .change_root
                .join("definitions/tasks/alpha/task.toml")
                .is_file(),
        ));
        // Its approval counts for nothing: it implements no rule.
        ApprovalOpinion::Approve("looks fine".to_string())
    }
}

/// A protected-section rule: any change holding `stamp.txt` needs a person.
struct Protected;

impl ApprovalHook for Protected {
    fn name(&self) -> &str {
        "protected-section"
    }

    fn judge(&self, input: &ApprovalInput) -> ApprovalOpinion {
        if input.change_root.join("definitions/stamp.txt").is_file() {
            ApprovalOpinion::NeedsPerson("the change touches the protected section".to_string())
        } else {
            ApprovalOpinion::NoOpinion
        }
    }
}

#[test]
fn a_person_edit_amended_by_an_amending_check_lands_with_no_further_action() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    repo.add_amending_check(Arc::new(edge_and_stamp(1)));
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    commit(&repo, &copy, "add alpha");
    record(&repo, &copy.change_id, ApprovalBasis::PersonEdit);

    let commit = landed(land_now(&repo, &copy.change_id, &author));
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    assert_eq!(row.amend_commits.len(), 1);
    assert_eq!(
        run_git(&repo, &["show", &format!("{commit}:definitions/stamp.txt")]),
        "balanced"
    );
    assert_eq!(repo.approvals(&copy.change_id).unwrap().len(), 1);
    assert_eq!(
        approval_line(&repo, &copy.change_id).as_deref(),
        Some(format!("approved: edited by {}", author.name).as_str())
    );
}

#[test]
fn project_approve_survives_master_moving_and_a_re_amend() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    // A stamp of the task folders, which differs once master gains a task.
    let mut stamp = FakeAmender::new(
        "stamp",
        10,
        Arc::new(|writable: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(writable.join("definitions/tasks"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            let text = format!("{}\n", names.join(","));
            let path = writable.join("definitions/stamp.txt");
            if std::fs::read_to_string(&path).ok() == Some(text.clone()) {
                return false;
            }
            std::fs::write(&path, text).unwrap();
            true
        }),
    );
    stamp.stamp_paths = vec!["stamp.txt"];
    repo.add_amending_check(Arc::new(stamp));
    let agent = Author::agent("chat 1");
    let change = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&change, "alpha", "a1");
    let other = new_copy(&repo, "calm-heron-0002", &person());
    add_task(&other, "beta", "b1");

    // `other` holds the queue while `change` waits for approval, is approved, and queues.
    let (paused_tx, paused_rx) = futures::channel::oneshot::channel::<()>();
    let (open_tx, open_rx) = futures::channel::oneshot::channel::<()>();
    let paused_tx = Mutex::new(Some(paused_tx));
    let open_rx = Mutex::new(Some(open_rx));
    let other_id = other.change_id.clone();
    repo.set_probe(Arc::new(move |point, change| -> BoxFuture<'static, bool> {
        if point == ProbePoint::InQueue && change == other_id {
            let paused = paused_tx.lock().take();
            let open = open_rx.lock().take();
            return async move {
                if let Some(paused) = paused {
                    paused.send(()).ok();
                }
                if let Some(open) = open {
                    open.await.ok();
                }
                false
            }
            .boxed();
        }
        async { false }.boxed()
    }));
    let mut events = repo.landing_events();
    let land_other = land_as(
        &repo,
        &other.change_id,
        request("other", &person()),
        start(),
    );
    let land_change = async {
        paused_rx.await.unwrap();
        let mut landing = landing(&repo, &change.change_id, engine_request("change", &agent));
        until_event(
            &mut landing,
            &mut events,
            &change.change_id,
            waits_for_approval,
        )
        .await;
        let first_amend = repo
            .change(&change.change_id)
            .unwrap()
            .unwrap()
            .amend_commits;
        assert_eq!(first_amend.len(), 1);
        repo.approve(&change.change_id, keyboard(), start())
            .await
            .unwrap();
        until_event(&mut landing, &mut events, &change.change_id, |event| {
            *event == LandingEvent::Queued { position: 1 }
        })
        .await;
        open_tx.send(()).unwrap();
        (landing.await.unwrap(), first_amend)
    };
    let (other_result, (result, first_amend)) =
        block_on(futures::future::join(land_other, land_change));
    let other_commit = landed(other_result.unwrap());
    let commit = landed(result);
    // It landed on top of the other change: master moved, it was rebased and amended again.
    assert_eq!(
        run_git(&repo, &["rev-parse", &format!("{commit}^")]),
        other_commit
    );
    let row = repo.change(&change.change_id).unwrap().unwrap();
    assert_eq!(row.amend_commits.len(), 1);
    assert_ne!(row.amend_commits[0].commit, first_amend[0].commit);
    assert_eq!(
        run_git(&repo, &["show", &format!("{commit}:definitions/stamp.txt")]),
        "alpha,backfill,beta,report"
    );
    // One approval, the person's, still counted.
    let approvals = repo.approvals(&change.change_id).unwrap();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].basis, ApprovalBasis::PersonApprove);
    assert_eq!(
        approval_line(&repo, &change.change_id).as_deref(),
        Some(format!("approved: approved by {}", person().name).as_str())
    );
}

#[test]
fn an_amendment_that_trips_a_protected_section_hook_waits_for_a_person() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    repo.add_amending_check(Arc::new(edge_and_stamp(1)));
    repo.add_approval_hook(Arc::new(Protected));
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    commit(&repo, &copy, "add alpha");
    // The person's edit was approved before the amendment existed.
    record(&repo, &copy.change_id, ApprovalBasis::PersonEdit);

    let before = main_commit(&repo);
    let mut events = repo.landing_events();
    let mut landing = landing(&repo, &copy.change_id, engine_request("edit", &author));
    let result = block_on(async {
        let event = until_event(
            &mut landing,
            &mut events,
            &copy.change_id,
            waits_for_approval,
        )
        .await;
        assert!(
            matches!(&event, LandingEvent::Waiting { reason, .. } if reason.contains("protected-section: the change touches the protected section")),
            "{event:?}"
        );
        assert_eq!(
            repo.git()
                .rev_parse(&repo.location().repository, "main")
                .await
                .unwrap(),
            before
        );
        // A person looks at the whole change and approves it.
        repo.approve(&copy.change_id, keyboard(), start())
            .await
            .unwrap();
        landing.await.unwrap()
    });
    landed(result);
}

#[test]
fn a_hook_receives_the_change_and_project_ids_and_both_parsed_trees() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let recorder = Arc::new(Recorder::default());
    repo.add_approval_hook(recorder.clone());
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    landed(
        block_on(land_as(
            &repo,
            &copy.change_id,
            request("alpha", &author),
            start(),
        ))
        .unwrap(),
    );
    let seen = recorder.seen.lock().clone();
    assert!(!seen.is_empty());
    for (change_id, project_id, master_tasks, change_tasks, master_root, change_root) in seen {
        assert_eq!(change_id, copy.change_id);
        assert_eq!(project_id, "project-1");
        assert_eq!(master_tasks, 3);
        assert_eq!(change_tasks, 4);
        assert!(master_root && change_root);
    }
}

#[test]
fn a_hook_without_a_rule_cannot_approve() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    repo.add_approval_hook(Arc::new(Recorder::default()));
    let agent = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    waits_for_a_person(&repo, &copy.change_id, engine_request("agent", &agent));
}

#[test]
fn agents_and_unknown_runs_wait_for_a_person_without_a_hook_that_approves_them() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let agent = Author::agent("chat 3f2a").with_chat("3f2a");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    assert_eq!(
        waits_for_a_person(&repo, &copy.change_id, engine_request("agent", &agent)),
        "waiting for approval: no own-subtree rule approved it; run project: approve"
    );

    let mut workflow = Author::workflow("run 99 step plan");
    workflow.run = Some("99".to_string());
    workflow.step = Some("plan".to_string());
    let copy = new_copy(&repo, "calm-heron-0002", &workflow);
    add_task(&copy, "beta", "b1");
    assert_eq!(
        waits_for_a_person(&repo, &copy.change_id, engine_request("run", &workflow)),
        "waiting for approval: no declared-scope rule approved it; run project: approve"
    );

    let healer = Author::healer("healer run 12");
    let copy = new_copy(&repo, "dry-finch-0003", &healer);
    add_task(&copy, "gamma", "c1");
    assert_eq!(
        waits_for_a_person(&repo, &copy.change_id, engine_request("heal", &healer)),
        "waiting for approval: no independent-review rule approved it; run project: approve"
    );
    // Nothing of theirs ever wrote an approval.
    assert!(
        repo.read_state(|read| read.approvals(&copy.change_id))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_workflow_change_inside_a_lanes_top_level_task_waits_without_a_lane_run_scope_hook() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let mut lane = Author::workflow("run 7 step write");
    lane.run = Some("7".to_string());
    let copy = new_copy(&repo, "amber-otter-0001", &lane);
    // A child task and a document of the top-level task `backfill`.
    add_task(&copy, "backfill/tasks/part", "p1");
    write_definition(&copy, "tasks/backfill/documents/notes.md", "notes\n");
    waits_for_a_person(&repo, &copy.change_id, engine_request("lane", &lane));
}

#[test]
fn the_own_subtree_hook_approves_an_agent_inside_its_chats_task_only() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    // The chat index: chat 3f2a works on the task `l2` (`report`).
    repo.add_approval_hook(Arc::new(OwnSubtreeHook::new(Arc::new(|chat: &str| {
        (chat == "3f2a").then(|| "l2".to_string())
    }))));
    let agent = Author::agent("chat 3f2a").with_chat("3f2a");
    let inside = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&inside, "report/tasks/draft", "d1");
    let commit = landed(land_now(&repo, &inside.change_id, &agent));
    assert!(!commit.is_empty());
    assert_eq!(
        approval_line(&repo, &inside.change_id).as_deref(),
        Some("approved automatically: agent change inside its own subtree")
    );
    // An agent never writes an approval row.
    assert!(repo.approvals(&inside.change_id).unwrap().is_empty());

    let outside = new_copy(&repo, "calm-heron-0002", &agent);
    add_task(&outside, "backfill/tasks/part", "p1");
    assert_eq!(
        waits_for_a_person(&repo, &outside.change_id, engine_request("out", &agent)),
        "waiting for approval: own-subtree: task p1 is outside the chat's task l2; run project: approve"
    );

    let unknown = Author::agent("chat 9").with_chat("9");
    let copy = new_copy(&repo, "dry-finch-0003", &unknown);
    add_task(&copy, "report/tasks/other", "o1");
    assert_eq!(
        waits_for_a_person(&repo, &copy.change_id, engine_request("unknown", &unknown)),
        "waiting for approval: own-subtree: unknown chat; run project: approve"
    );

    // The policy can turn the rule off; it is read from master.
    let policy = new_copy(&repo, "eager-gull-0004", &person());
    write_definition(
        &policy,
        "project.toml",
        &format!(
            "{}\n[approval]\nagent = \"person\"\n",
            crate::test_support::PROJECT_TOML
        ),
    );
    landed(
        block_on(land_as(
            &repo,
            &policy.change_id,
            request("policy", &person()),
            start(),
        ))
        .unwrap(),
    );
    let copy = new_copy(&repo, "fair-lynx-0005", &agent);
    add_task(&copy, "report/tasks/again", "x1");
    assert_eq!(
        waits_for_a_person(&repo, &copy.change_id, engine_request("off", &agent)),
        "waiting for approval: the project's approval policy asks a person; run project: approve"
    );
}

#[test]
fn an_agent_change_editing_the_policy_never_approves_itself() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    repo.add_approval_hook(Arc::new(OwnSubtreeHook::new(Arc::new(|_: &str| {
        Some("g1".to_string())
    }))));
    let agent = Author::agent("chat 1").with_chat("1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    // Even with the top group as its chat's task, project.toml is outside every subtree.
    write_definition(
        &copy,
        "project.toml",
        &format!(
            "{}\n[approval]\nperson = \"approve\"\n",
            crate::test_support::PROJECT_TOML
        ),
    );
    assert_eq!(
        waits_for_a_person(&repo, &copy.change_id, engine_request("policy", &agent)),
        "waiting for approval: own-subtree: project.toml is outside the chat's task g1; run project: approve"
    );
}

#[test]
fn an_author_commit_with_an_amend_subject_is_judged_as_the_authors() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let agent = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    commit(&repo, &copy, "add alpha");
    // A person approved what the change held then.
    block_on(repo.approve(&copy.change_id, keyboard(), start())).unwrap();
    // The author adds a commit labelled like an amendment; only the engine's list makes one.
    add_task(&copy, "beta", "b1");
    commit(&repo, &copy, "amend: x rebalance");
    assert!(
        repo.change(&copy.change_id)
            .unwrap()
            .unwrap()
            .amend_commits
            .is_empty()
    );
    waits_for_a_person(&repo, &copy.change_id, engine_request("agent", &agent));
}

#[test]
fn approval_rows_of_another_author_part_are_ignored() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let agent = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    commit(&repo, &copy, "add alpha");
    let first = block_on(repo.approve(&copy.change_id, keyboard(), start())).unwrap();
    // The author edits the change after it was approved.
    write_definition(
        &copy,
        "tasks/alpha/task.toml",
        &crate::test_support::definition_toml("a1", "alpha changed", "other", &[]),
    );
    commit(&repo, &copy, "change alpha");
    waits_for_a_person(&repo, &copy.change_id, engine_request("agent", &agent));
    let rows = repo.approvals(&copy.change_id).unwrap();
    assert_eq!(rows, vec![first]);
}

#[test]
fn a_forged_person_author_in_the_changes_table_waits_for_project_approve() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let agent = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    // Something with access to the store writes a person author into the change's row.
    let forged: Author = toml::from_str(&format!(
        "kind = \"person\"\nname = \"{}\"\n",
        person().name
    ))
    .unwrap();
    assert_eq!(
        forged.kind(),
        bongos_project_repo_model::AuthorKind::PersonClaimed
    );
    let mut row = repo.change(&copy.change_id).unwrap().unwrap();
    row.author = forged;
    let writer = repo.writer_or_error().unwrap();
    writer.transact(|tx| tx.set_change(&row)).unwrap();
    let json: Author = serde_json::from_str(r#"{"kind":"person","name":"someone"}"#).unwrap();

    let before = main_commit(&repo);
    let mut events = repo.landing_events();
    let mut landing = landing(&repo, &copy.change_id, engine_request("forged", &json));
    let result = block_on(async {
        let event = until_event(
            &mut landing,
            &mut events,
            &copy.change_id,
            waits_for_approval,
        )
        .await;
        assert_eq!(
            event,
            LandingEvent::Waiting {
                reason: "waiting for approval: no person approved this version of the change; run project: approve".to_string(),
                approvals: 0,
            }
        );
        assert_eq!(
            repo.git()
                .rev_parse(&repo.location().repository, "main")
                .await
                .unwrap(),
            before
        );
        repo.approve(&copy.change_id, keyboard(), start())
            .await
            .unwrap();
        landing.await.unwrap()
    });
    landed(result);
}

#[test]
fn a_person_edit_waiting_in_the_queue_lands_after_a_restart_without_asking_again() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    commit(&repo, &copy, "add alpha");
    record(&repo, &copy.change_id, ApprovalBasis::PersonEdit);
    // The process dies while the change waits in the queue.
    repo.set_probe(Arc::new(|point, _| {
        async move { point == ProbePoint::InQueue }.boxed()
    }));
    let error = block_on(repo.land_now(&copy.change_id, engine_request("edit", &author), start()))
        .unwrap_err();
    assert!(format!("{error}").contains(crate::landing::SIMULATED_CRASH));

    let repo = reopen_bare(repo, &StateApplyHooks::new());
    block_on(repo.recover_landing()).unwrap();
    landed(land_now(&repo, &copy.change_id, &author));
    assert_eq!(repo.approvals(&copy.change_id).unwrap().len(), 1);
}

#[test]
fn project_land_approves_only_the_persons_own_change() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let author = person();
    let own = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&own, "alpha", "a1");
    landed(
        block_on(land_as(
            &repo,
            &own.change_id,
            request("own", &author),
            start(),
        ))
        .unwrap(),
    );
    let rows = repo.approvals(&own.change_id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].basis, ApprovalBasis::PersonLand);
    assert_eq!(rows[0].name, author.name);

    // A person landing an agent's change does not approve it.
    let agent = Author::agent("chat 1");
    let theirs = new_copy(&repo, "calm-heron-0002", &agent);
    add_task(&theirs, "beta", "b1");
    waits_in(
        &repo,
        &theirs.change_id,
        person_landing(&repo, &theirs.change_id, request("theirs", &author)),
    );
    assert!(repo.approvals(&theirs.change_id).unwrap().is_empty());
}

#[test]
fn project_land_on_a_change_waiting_for_approval_approves_it() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let mut events = repo.landing_events();
    // The engine was asked first (no person), so it waits.
    let mut landing = landing(&repo, &copy.change_id, engine_request("edit", &author));
    let result = block_on(async {
        until_event(
            &mut landing,
            &mut events,
            &copy.change_id,
            waits_for_approval,
        )
        .await;
        // The person's `project: land` meets the running landing and approves it.
        let again = land_as(&repo, &copy.change_id, request("again", &author), start())
            .await
            .unwrap();
        assert_eq!(
            again,
            LandingResult::NoVerdict {
                reason: crate::ALREADY_LANDING.to_string()
            }
        );
        landing.await.unwrap()
    });
    landed(result);
}

#[test]
fn a_change_waiting_for_approval_holds_no_lock_and_abandon_ends_it() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let agent = Author::agent("chat 1");
    let waiting = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&waiting, "alpha", "a1");
    let author = person();
    let other = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&other, "beta", "b1");
    let mut events = repo.landing_events();
    let mut landing = landing(&repo, &waiting.change_id, engine_request("agent", &agent));
    let result = block_on(async {
        until_event(
            &mut landing,
            &mut events,
            &waiting.change_id,
            waits_for_approval,
        )
        .await;
        assert_eq!(repo.landing_holder(), None);
        // Another change lands meanwhile.
        landed(
            land_as(&repo, &other.change_id, request("other", &author), start())
                .await
                .unwrap(),
        );
        repo.abandon_now(&waiting.change_id, start()).await.unwrap();
        landing.await.unwrap()
    });
    assert_eq!(result, LandingResult::Abandoned);
    assert!(!run_git(&repo, &["ls-tree", "-r", "--name-only", "main"]).contains("alpha"));
}

#[test]
fn a_cloned_context_carries_no_attestation_and_approves_nothing() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let agent = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    commit(&repo, &copy, "add alpha");
    let ctx = keyboard();
    let clone = ctx.clone();
    assert!(ctx.has_attestation());
    assert!(!clone.has_attestation());
    assert_eq!(
        clone.author.kind(),
        bongos_project_repo_model::AuthorKind::Person
    );
    let error = block_on(repo.approve(&copy.change_id, clone.clone(), start())).unwrap_err();
    assert_eq!(error.to_string(), NEEDS_A_WINDOW);
    let error = block_on(repo.land_as_person(
        &copy.change_id,
        request("clone", &clone.author),
        clone,
        start(),
    ))
    .unwrap_err();
    assert_eq!(error.to_string(), NEEDS_A_WINDOW);
    // Taken once, the attestation is gone.
    let mut once = keyboard();
    assert!(once.take_attestation().is_some());
    assert!(block_on(repo.approve(&copy.change_id, once, start())).is_err());
    assert!(repo.approvals(&copy.change_id).unwrap().is_empty());
}

#[test]
fn project_approve_without_a_window_is_refused() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let agent = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    commit(&repo, &copy, "add alpha");
    let at: bongos_work_model::Timestamp = "2026-03-01T10:00:00Z".parse().unwrap();
    for ctx in [
        WorkContext::new("person", at),
        WorkContext::new("x", at).with_author(Author::agent("chat 1")),
        WorkContext::new("x", at).with_author(Author::workflow("run 1")),
    ] {
        let error = block_on(repo.approve(&copy.change_id, ctx, start())).unwrap_err();
        assert_eq!(error.to_string(), NEEDS_A_WINDOW);
    }
    // A command from the command-line client cannot approve either.
    let boundary = HostBoundary::new(repo.clone());
    let cli = PeerIdentity::for_test(4242, ClientKind::Cli, true);
    assert!(
        block_on(boundary.accept_now(
            RepoCommand::Approve {
                change_id: copy.change_id.clone(),
                person: PersonClaim {
                    name: person().name
                },
            },
            &cli,
            start(),
        ))
        .is_err()
    );
    assert!(repo.approvals(&copy.change_id).unwrap().is_empty());
}

#[test]
fn the_host_writes_a_person_approval_only_for_a_verified_window() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let agent = Author::agent("chat 1");
    let copy = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&copy, "alpha", "a1");
    commit(&repo, &copy, "add alpha");
    let boundary = HostBoundary::new(repo.clone());
    let approve = || RepoCommand::Approve {
        change_id: copy.change_id.clone(),
        person: PersonClaim {
            name: person().name,
        },
    };
    for peer in [
        PeerIdentity::for_test(4242, ClientKind::Cli, true),
        PeerIdentity::for_test(4242, ClientKind::Cli, false),
        PeerIdentity::for_test(4242, ClientKind::Window, false),
    ] {
        assert!(!peer.may_attest_person());
        assert!(block_on(boundary.accept_now(approve(), &peer, start())).is_err());
        assert!(repo.approvals(&copy.change_id).unwrap().is_empty());
    }
    let window = PeerIdentity::for_test(4242, ClientKind::Window, true);
    assert!(window.may_attest_person());
    block_on(boundary.accept_now(approve(), &window, start())).unwrap();
    let rows = repo.approvals(&copy.change_id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].basis, ApprovalBasis::PersonApprove);
}

#[test]
fn a_land_command_from_an_unverified_peer_waits_and_from_a_verified_window_lands() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let author = person();
    let boundary = HostBoundary::new(repo.clone());
    let land = |change_id: &str| RepoCommand::Land {
        change_id: change_id.to_string(),
        request: request("over the socket", &author),
        person: Some(PersonClaim {
            name: author.name.clone(),
        }),
    };

    // The command-line client: applied with the author as a claimed person, which waits.
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let cli = PeerIdentity::for_test(4242, ClientKind::Cli, true);
    let before = main_commit(&repo);
    let mut events = repo.landing_events();
    let change_id = copy.change_id.clone();
    let mut accepted = {
        let boundary = HostBoundary::new(repo.clone());
        let command = land(&change_id);
        async move {
            boundary.accept_now(command, &cli, start()).await?;
            Ok(LandingResult::NotRequested)
        }
        .boxed()
        .fuse()
    };
    block_on(async {
        until_event(
            &mut accepted,
            &mut events,
            &copy.change_id,
            waits_for_approval,
        )
        .await;
        repo.abandon_now(&copy.change_id, start()).await.unwrap();
        accepted.await.unwrap();
    });
    assert_eq!(main_commit(&repo), before);
    assert!(repo.approvals(&copy.change_id).unwrap().is_empty());

    // A verified window: its claim is the person's `project: land`.
    let copy = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&copy, "beta", "b1");
    let window = PeerIdentity::for_test(4242, ClientKind::Window, true);
    block_on(boundary.accept_now(land(&copy.change_id), &window, start())).unwrap();
    assert!(matches!(
        repo.landing_result(&copy.change_id).unwrap(),
        Some(LandingResult::Landed { .. })
    ));
    assert_eq!(
        repo.approvals(&copy.change_id).unwrap()[0].basis,
        ApprovalBasis::PersonLand
    );
}

#[test]
fn no_call_path_lands_an_unapproved_change() {
    // Under the default policy with every automatic rule answering yes, an unattributed change
    // and a claimed person's change still wait, and so does a person's change landed without
    // the person.
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let unattributed = Author::unattributed("script");
    let copy = new_copy(&repo, "amber-otter-0001", &unattributed);
    add_task(&copy, "alpha", "a1");
    assert_eq!(
        waits_for_a_person(
            &repo,
            &copy.change_id,
            engine_request("script", &unattributed)
        ),
        "waiting for approval: an unattributed change needs a person; run project: approve"
    );
    let author = person();
    let copy = new_copy(&repo, "calm-heron-0002", &author);
    add_task(&copy, "beta", "b1");
    waits_for_a_person(&repo, &copy.change_id, engine_request("engine", &author));
    // `project: check` reports the approval check and lands nothing.
    let copy = new_copy(&repo, "dry-finch-0003", &unattributed);
    add_task(&copy, "gamma", "c1");
    let before = main_commit(&repo);
    let checked = block_on(repo.land_now(
        &copy.change_id,
        LandRequest {
            meta: ChangeMeta::new("check", unattributed),
            mode: LandMode::CheckOnly,
        },
        start(),
    ))
    .unwrap();
    let LandingResult::Checked { checks, .. } = checked else {
        panic!("expected check results, got {checked:?}");
    };
    let approval = checks
        .iter()
        .find(|check| check.check == "approval")
        .unwrap();
    assert!(matches!(
        &approval.outcome,
        crate::CheckOutcome::NoVerdict { reason, .. } if reason.starts_with("waiting for approval")
    ));
    assert_eq!(main_commit(&repo), before);
    assert!(repo.approvals(&copy.change_id).unwrap().is_empty());
}

#[gpui::test]
fn the_local_sink_takes_no_repository_command(cx: &mut gpui::TestAppContext) {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let copy = new_copy(&repo, "amber-otter-0001", &Author::agent("chat 1"));
    add_task(&copy, "alpha", "a1");
    commit(&repo, &copy, "add alpha");
    let sink = crate::LocalSink::new(repo.writer_or_error().unwrap(), cx.executor());
    for command in [
        RepoCommand::Approve {
            change_id: copy.change_id.clone(),
            person: PersonClaim {
                name: person().name,
            },
        },
        RepoCommand::Land {
            change_id: copy.change_id.clone(),
            request: request("land", &person()),
            person: Some(PersonClaim {
                name: person().name,
            }),
        },
    ] {
        assert!(block_on(crate::StateSink::repo(&sink, command)).is_err());
    }
    assert!(repo.approvals(&copy.change_id).unwrap().is_empty());
    assert_eq!(
        repo.change(&copy.change_id).unwrap().unwrap().state,
        crate::ChangeState::Open
    );
}

#[test]
fn a_healer_change_lands_only_after_an_independent_review_says_ship() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let healer = Author::healer("healer run 12");
    let copy = new_copy(&repo, "amber-otter-0001", &healer);
    add_task(&copy, "alpha", "a1");
    waits_for_a_person(&repo, &copy.change_id, engine_request("heal", &healer));
    repo.add_approval_hook(Arc::new(ApproveRule(AutomaticRule::IndependentReview)));
    let copy = new_copy(&repo, "calm-heron-0002", &healer);
    add_task(&copy, "beta", "b1");
    landed(land_now(&repo, &copy.change_id, &healer));
    assert_eq!(
        approval_line(&repo, &copy.change_id).as_deref(),
        Some("approved automatically: test independent-review rule")
    );
}

fn record_rollback(repo: &ProjectRepo, change_id: &str, target_commit: &str, approved: bool) {
    let writer = repo.writer_or_error().unwrap();
    let mut value = toml::Table::new();
    value.insert("move_id".into(), "m1".into());
    value.insert("target_commit".into(), target_commit.into());
    value.insert("target_approved".into(), approved.into());
    writer
        .transact(|tx| {
            tx.move_rollbacks()
                .put(change_id, toml::Value::Table(value))
        })
        .unwrap();
}

#[test]
fn a_move_rollback_is_approved_only_as_the_exact_revert_of_the_approved_landing() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let author = person();
    let target = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&target, "alpha", "a1");
    let target_commit = landed(
        block_on(land_as(
            &repo,
            &target.change_id,
            request("move in", &author),
            start(),
        ))
        .unwrap(),
    );
    let mover = Author::workflow("move m1");

    // A rollback that does more than the revert waits for a person.
    let wider = new_copy(&repo, "calm-heron-0002", &mover);
    std::fs::remove_dir_all(wider.path.join("definitions/tasks/alpha")).unwrap();
    add_task(&wider, "beta", "b1");
    commit(&repo, &wider, "roll back");
    record_rollback(&repo, &wider.change_id, &target_commit, true);
    assert_eq!(
        waits_for_a_person(&repo, &wider.change_id, engine_request("back", &mover)),
        "waiting for approval: the rollback of move m1 is not the revert of its landing; run project: approve"
    );

    // The exact revert is covered by the move's approval.
    let exact = new_copy(&repo, "dry-finch-0003", &mover);
    std::fs::remove_dir_all(exact.path.join("definitions/tasks/alpha")).unwrap();
    commit(&repo, &exact, "roll back");
    record_rollback(&repo, &exact.change_id, &target_commit, true);
    landed(land_now(&repo, &exact.change_id, &mover));
    assert_eq!(
        approval_line(&repo, &exact.change_id).as_deref(),
        Some("approved: rollback of move m1, covered by the move's approval")
    );
}

#[test]
fn a_rollback_of_an_unapproved_move_waits_for_a_person() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let author = person();
    let target = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&target, "alpha", "a1");
    let target_commit = landed(
        block_on(land_as(
            &repo,
            &target.change_id,
            request("move in", &author),
            start(),
        ))
        .unwrap(),
    );
    let mover = Author::workflow("move m1");
    let exact = new_copy(&repo, "calm-heron-0002", &mover);
    std::fs::remove_dir_all(exact.path.join("definitions/tasks/alpha")).unwrap();
    commit(&repo, &exact, "roll back");
    record_rollback(&repo, &exact.change_id, &target_commit, false);
    waits_for_a_person(&repo, &exact.change_id, engine_request("back", &mover));
}

#[test]
fn a_declared_workflow_check_with_the_amending_phase_editing_the_policy_is_refused() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(
        &copy,
        "project.toml",
        &format!(
            "{}\n[approval]\nagent = \"own-subtree\"\n\n[[landing.checks]]\nname = \"loosen-approval\"\nphase = \"amending\"\nrun = {{ workflow = \"loosen-approval\" }}\n",
            crate::test_support::PROJECT_TOML
        ),
    );
    let before = main_commit(&repo);
    let result = block_on(land_as(
        &repo,
        &copy.change_id,
        request("loosen", &author),
        start(),
    ))
    .unwrap();
    let LandingResult::Refused { reasons } = result else {
        panic!("expected a refusal, got {result:?}");
    };
    assert!(
        reasons.iter().any(|reason| reason.starts_with("schema:")
            && reason.contains("landing check loosen-approval: phase \"amending\" is not allowed")),
        "{reasons:?}"
    );
    assert_eq!(main_commit(&repo), before);
    assert!(
        repo.change(&copy.change_id)
            .unwrap()
            .unwrap()
            .amend_commits
            .is_empty()
    );
    // Even in master such an entry is never an amending check: only Rust code registers one.
    let project = bongos_work_model::parse_project(&format!(
        "{}\n[[landing.checks]]\nname = \"loosen-approval\"\nphase = \"amending\"\nrun = {{ workflow = \"loosen-approval\" }}\n",
        crate::test_support::PROJECT_TOML
    ))
    .unwrap();
    let plan = crate::declared_checks::plan(Some(&project), &repo.check_registry());
    assert!(plan.amending.is_empty());
}

#[test]
fn an_amending_check_writing_outside_definitions_gives_no_verdict() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.add_amending_check(Arc::new(FakeAmender::new(
        "outside",
        1,
        Arc::new(|writable: &Path| {
            std::fs::write(writable.join("notes.txt"), "outside\n").unwrap();
            true
        }),
    )));
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let before = main_commit(&repo);
    let result = block_on(land_as(
        &repo,
        &copy.change_id,
        request("alpha", &author),
        start(),
    ))
    .unwrap();
    assert!(
        matches!(&result, LandingResult::NoVerdict { reason } if reason.contains("outside") && reason.contains("notes.txt")),
        "{result:?}"
    );
    assert_eq!(main_commit(&repo), before);
    assert!(
        repo.change(&copy.change_id)
            .unwrap()
            .unwrap()
            .amend_commits
            .is_empty()
    );
}

#[test]
fn a_commit_on_top_of_an_amendment_while_queued_does_not_land_on_the_old_approval() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    // Its stamp does not move with the tree, so nothing but the approval sees the commit below.
    let mut amender = edge_and_stamp(10);
    amender.fixed_stamp = Some("balanced");
    repo.add_amending_check(Arc::new(amender));
    let agent = Author::agent("chat 1");
    let change = new_copy(&repo, "amber-otter-0001", &agent);
    add_task(&change, "alpha", "a1");
    // Once the approved change is in the queue, the author commits on top of its amendment.
    let id = change.change_id.clone();
    let path = change.path.clone();
    let git = repo.git().clone();
    let sneaked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sneaked_in_probe = sneaked.clone();
    repo.set_probe(Arc::new(move |point, at| -> BoxFuture<'static, bool> {
        if point == ProbePoint::InQueue
            && at == id
            && !sneaked_in_probe.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let (git, path) = (git.clone(), path.clone());
            return async move {
                let document = path.join("definitions/documents/sneaked.md");
                std::fs::create_dir_all(document.parent().unwrap()).unwrap();
                std::fs::write(&document, "never approved\n").unwrap();
                git.commit_if_dirty(&path, "sneak", &crate::Identity::default())
                    .await
                    .unwrap();
                false
            }
            .boxed();
        }
        async { false }.boxed()
    }));

    let before = main_commit(&repo);
    let mut events = repo.landing_events();
    let mut landing = landing(&repo, &change.change_id, engine_request("change", &agent));
    block_on(async {
        until_event(
            &mut landing,
            &mut events,
            &change.change_id,
            waits_for_approval,
        )
        .await;
        let row = repo.change(&change.change_id).unwrap().unwrap();
        assert_eq!(row.amend_commits.len(), 1);
        repo.approve(&change.change_id, keyboard(), start())
            .await
            .unwrap();
        // The sneaked commit, made once it is in the queue, makes a new author part: the
        // change waits for a person again instead of landing.
        until_event(
            &mut landing,
            &mut events,
            &change.change_id,
            waits_for_approval,
        )
        .await;
        assert!(sneaked.load(std::sync::atomic::Ordering::SeqCst));
        repo.abandon_now(&change.change_id, start()).await.unwrap();
        assert_eq!(landing.await.unwrap(), LandingResult::Abandoned);
    });
    assert_eq!(main_commit(&repo), before, "the sneaked commit landed");
}

#[test]
fn only_listed_amend_commits_at_the_tip_are_left_out_of_the_author_part() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    add_task(&copy, "alpha", "a1");
    let authored = commit(&repo, &copy, "add alpha");
    write_definition(&copy, "stamp.txt", "balanced\n");
    let amend = commit(&repo, &copy, "amend: rebalance stamp");
    let writer = repo.writer_or_error().unwrap();
    let mut row = repo.change(&copy.change_id).unwrap().unwrap();
    row.amend_commits.push(crate::changes::AmendCommit {
        commit: amend.clone(),
        check: "rebalance".to_string(),
    });
    writer.transact(|tx| tx.set_change(&row)).unwrap();
    let master = main_commit(&repo);

    let part = block_on(repo.author_part(&row, &amend, &master)).unwrap();
    assert_eq!(part.last_author, authored);
    assert_ne!(part.amend_patch_id, "");

    // An author commit on top: the whole tip is the author's part, with no amendment.
    write_definition(&copy, "documents/sneaked.md", "never approved\n");
    let sneaked = commit(&repo, &copy, "sneak");
    let part = block_on(repo.author_part(&row, &sneaked, &master)).unwrap();
    assert_eq!(part.last_author, sneaked);
    assert_eq!(part.amend_patch_id, "");
    let approved = block_on(repo.author_part(&row, &amend, &master)).unwrap();
    assert_ne!(part.patch_id, approved.patch_id);
}

#[test]
fn whitespace_changed_inside_a_line_after_approval_needs_a_person_again() {
    let root = TempDir::new().unwrap();
    let repo = bare_fixture_project(&root);
    let author = person();
    let copy = new_copy(&repo, "amber-otter-0001", &author);
    write_definition(&copy, "documents/cleanup.md", "rm -rf /tmp/x\n");
    commit(&repo, &copy, "add the cleanup");
    let approved = record(&repo, &copy.change_id, ApprovalBasis::PersonApprove);

    write_definition(&copy, "documents/cleanup.md", "rm -rf / tmp/x\n");
    commit(&repo, &copy, "tidy the cleanup");
    let row = repo.change(&copy.change_id).unwrap().unwrap();
    let tip = block_on(repo.branch_tip(&row)).unwrap();
    let part = block_on(repo.author_part(&row, &tip, &main_commit(&repo))).unwrap();
    assert_ne!(part.patch_id, approved.author_patch_id);

    let reason = waits_for_a_person(&repo, &copy.change_id, engine_request("cleanup", &author));
    assert!(
        reason.contains("no person approved this version"),
        "{reason}"
    );
}
