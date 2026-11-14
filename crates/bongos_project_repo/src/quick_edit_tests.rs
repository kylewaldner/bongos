use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bongos_project_repo_model::checks::{ApprovalBasis, CheckPhase};
use bongos_project_repo_model::{Author, ChangeMeta};
use bongos_work_model::commands::{self, Change, Context, NewTask, Outcome, TaskEdit, Write};
use bongos_work_model::store::WorkTree;
use bongos_work_model::{Kind, PersonAtKeyboard, new_id};
use futures::channel::mpsc::UnboundedReceiver;
use futures::executor::block_on;
use futures::future::{BoxFuture, Fuse};
use futures::{FutureExt as _, StreamExt as _};
use tempfile::TempDir;

use crate::changes::ChangeState;
use crate::check_fakes::{FakeAmender, FakeCheck, write_in};
use crate::landing::{InFlight, LANDING_IN_FLIGHT_KEY, LandingRecord};
use crate::router::{DefinitionOp, Routed, apply_state_only, project_into, route};
use crate::test_support::{
    CountingHook, fixture_project, fixture_project_with_hooks, keyboard, main_commit,
    outside_document, run_git, start,
};
use crate::{
    CheckOutcome, LANDING_MARK, LandingEvent, LandingResult, LoadedProject, MasterStatus,
    NOT_LANDED_MARK, ProjectRepo, QuickEdit, QuickEditStarted, Recovery, STALE_SNAPSHOT, StateOp,
    check_fresh, written_tasks,
};

pub(crate) type Landing = Fuse<BoxFuture<'static, anyhow::Result<LandingResult>>>;

pub(crate) fn unattributed() -> Context {
    Context::new("tester", "2026-11-13T10:00:00Z".parse().unwrap())
}

pub(crate) fn loaded(repo: &ProjectRepo) -> Arc<LoadedProject> {
    let loaded = LoadedProject::new(repo.clone());
    block_on(loaded.load()).unwrap();
    loaded
}

fn rename(id: &'static str, goal: &'static str) -> impl Fn(&WorkTree, &Context) -> Outcome {
    move |tree, ctx| {
        commands::edit(
            tree,
            ctx,
            id,
            TaskEdit {
                goal: Some(goal.to_string()),
                kind: None,
                repository: None,
            },
        )
    }
}

/// What the store's definition writer makes of `change`, computed on the loaded tree `tree`.
pub(crate) fn quick_edit_of(
    loaded: &LoadedProject,
    tree: &WorkTree,
    change: &Change,
    author: &Author,
) -> QuickEdit {
    let (commit, definitions) = loaded.shown();
    check_fresh(tree, change, &definitions).unwrap();
    let Routed::Definition {
        definition,
        state,
        files,
    } = route(tree, change).unwrap()
    else {
        panic!("expected a definition change for {}", change.entry.event);
    };
    QuickEdit {
        base: commit.unwrap().0,
        definition,
        state,
        files,
        meta: ChangeMeta::new(String::new(), author.clone()),
        tasks: written_tasks(change),
    }
}

/// Runs `command` as the store would with `ctx` (its marker taken out first) and starts the
/// quick edit.
pub(crate) fn start_edit(
    repo: &ProjectRepo,
    loaded: &LoadedProject,
    mut ctx: Context,
    command: impl Fn(&WorkTree, &Context) -> Outcome,
) -> QuickEditStarted {
    let marker: Option<PersonAtKeyboard> = ctx.take_attestation();
    let tree = loaded.work_tree().unwrap();
    let change = command(&tree, &ctx).unwrap();
    let edit = quick_edit_of(loaded, &tree, &change, &ctx.author);
    block_on(repo.quick_edit(edit, marker, start())).unwrap()
}

pub(crate) fn landing(repo: &ProjectRepo, change_id: &str) -> Landing {
    let repo = repo.clone();
    let change_id = change_id.to_string();
    async move { repo.land_quick_edit(&change_id, start()).await }
        .boxed()
        .fuse()
}

/// Drives `landing` until it waits for approval; a landing that ends first fails the test.
pub(crate) async fn until_waiting(
    mut landing: &mut Landing,
    events: &mut UnboundedReceiver<(String, LandingEvent)>,
    change_id: &str,
) {
    loop {
        futures::select! {
            result = landing => panic!("{change_id} ended before it waited: {result:?}"),
            next = events.next() => {
                let (id, event) = next.expect("the landing events");
                if id == change_id
                    && matches!(&event, LandingEvent::Waiting { reason } if reason.starts_with("waiting for approval"))
                {
                    return;
                }
            }
        }
    }
}

pub(crate) fn goal(loaded: &LoadedProject, id: &str) -> String {
    block_on(loaded.load()).unwrap();
    loaded
        .work_tree()
        .unwrap()
        .task_by_id(id)
        .unwrap()
        .goal()
        .to_string()
}

fn history_events(repo: &ProjectRepo, id: &str) -> Vec<String> {
    repo.read_state(|read| read.task(id))
        .unwrap()
        .unwrap_or_default()
        .history
        .iter()
        .map(|entry| entry.event.clone())
        .collect()
}

pub(crate) fn staged(repo: &ProjectRepo, change_id: &str) -> Option<Vec<StateOp>> {
    repo.read_state(|read| read.staged(change_id)).unwrap()
}

fn landing_record(repo: &ProjectRepo, change_id: &str) -> LandingRecord {
    let landings = repo.read_state(|read| read.landings()).unwrap();
    let (_, text) = landings
        .into_iter()
        .find(|(id, _)| id == change_id)
        .expect("the landing record");
    toml::from_str(&text).unwrap()
}

pub(crate) fn copy_path(repo: &ProjectRepo, change_id: &str) -> PathBuf {
    let row = repo.change(change_id).unwrap().unwrap();
    repo.working_copy_path(&row.name)
}

#[test]
fn a_person_quick_edit_lands_with_no_further_action() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    let before = main_commit(&repo);

    let started = start_edit(&repo, &loaded, keyboard(), rename("l1", "Backfill events"));
    // No working copy is checked out, the branch is one commit on the commit read, master and
    // the state are untouched, and the task shows the change pending.
    assert!(!copy_path(&repo, &started.change_id).exists());
    assert_eq!(
        run_git(
            &repo,
            &[
                "rev-list",
                "--count",
                &format!("{before}..{}", started.branch)
            ]
        ),
        "1"
    );
    assert_eq!(main_commit(&repo), before);
    assert!(!history_events(&repo, "l1").contains(&"edited".to_string()));
    assert!(staged(&repo, &started.change_id).is_some());
    let pending = loaded
        .work_tree()
        .unwrap()
        .task_by_id("l1")
        .unwrap()
        .pending
        .clone();
    assert_eq!(pending.unwrap().label, LANDING_MARK);
    let approvals = repo.approvals(&started.change_id).unwrap();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].basis, ApprovalBasis::PersonEdit);

    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    let LandingResult::Landed { commit } = result else {
        panic!("expected a landing, got {result:?}");
    };
    assert_eq!(main_commit(&repo), commit);
    assert_eq!(goal(&loaded, "l1"), "Backfill events");
    // The staged history entry was applied with the landing and the staged row is gone.
    assert!(history_events(&repo, "l1").contains(&"edited".to_string()));
    assert_eq!(staged(&repo, &started.change_id), None);
    assert_eq!(
        landing_record(&repo, &started.change_id)
            .approval
            .as_deref(),
        Some(format!("approved: edited by {}", keyboard().author.name).as_str())
    );
    // The landed change leaves neither working copy nor branch, and nothing is pending.
    assert!(!copy_path(&repo, &started.change_id).exists());
    assert!(!run_git(&repo, &["branch", "--list"]).contains(&started.branch));
    assert_eq!(
        loaded
            .work_tree()
            .unwrap()
            .task_by_id("l1")
            .unwrap()
            .pending,
        None
    );
}

#[test]
fn a_person_quick_edit_lands_when_an_amending_check_amends_it() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let mut amender = FakeAmender::new("stamp", 1, write_in("stamp.txt", "stamped\n"));
    amender.stamp_paths = vec!["stamp.txt"];
    repo.add_amending_check(Arc::new(amender));
    let loaded = loaded(&repo);

    let started = start_edit(&repo, &loaded, keyboard(), rename("l1", "Backfill events"));
    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    let LandingResult::Landed { commit } = result else {
        panic!("expected a landing, got {result:?}");
    };
    assert_eq!(
        run_git(&repo, &["show", &format!("{commit}:definitions/stamp.txt")]),
        "stamped"
    );
    assert_eq!(goal(&loaded, "l1"), "Backfill events");
}

#[test]
fn an_unattributed_quick_edit_waits_for_project_approve_then_lands() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    let before = main_commit(&repo);

    let started = start_edit(
        &repo,
        &loaded,
        unattributed(),
        rename("l1", "Backfill events"),
    );
    assert!(repo.approvals(&started.change_id).unwrap().is_empty());
    let mut events = repo.landing_events();
    let mut running = landing(&repo, &started.change_id);
    let result = block_on(async {
        until_waiting(&mut running, &mut events, &started.change_id).await;
        let waiting_main = repo
            .git()
            .rev_parse(&repo.location().repository, "main")
            .await
            .unwrap();
        assert_eq!(waiting_main, before);
        repo.approve(&started.change_id, keyboard(), start())
            .await
            .unwrap();
        running.await.unwrap()
    });
    assert!(matches!(result, LandingResult::Landed { .. }), "{result:?}");
    assert_eq!(goal(&loaded, "l1"), "Backfill events");
}

#[test]
fn a_forged_person_author_of_a_quick_edit_waits_for_project_approve() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    let before = main_commit(&repo);

    let started = start_edit(
        &repo,
        &loaded,
        unattributed(),
        rename("l1", "Backfill events"),
    );
    // Someone rewrites the stored author as the person at the keyboard: still no approval row.
    let writer = repo.writer_or_error().unwrap();
    let mut row = repo.change(&started.change_id).unwrap().unwrap();
    row.author = keyboard().author;
    writer.transact(|tx| tx.set_change(&row)).unwrap();

    let mut events = repo.landing_events();
    let mut running = landing(&repo, &started.change_id);
    let result = block_on(async {
        until_waiting(&mut running, &mut events, &started.change_id).await;
        repo.abandon_now(&started.change_id, start()).await.unwrap();
        running.await.unwrap()
    });
    assert_eq!(result, LandingResult::Abandoned);
    assert_eq!(main_commit(&repo), before);
    assert_eq!(staged(&repo, &started.change_id), None);
    assert!(!history_events(&repo, "l1").contains(&"edited".to_string()));
}

#[test]
fn a_refused_quick_edit_drops_its_staged_state_and_is_checked_out() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    repo.add_landing_check(Arc::new(
        FakeCheck::new("refuser", CheckPhase::BeforeQueue)
            .answering(vec![CheckOutcome::fail("not like this")]),
    ));
    let loaded = loaded(&repo);
    let before = main_commit(&repo);
    let state_before = repo.read_state(|read| read.task("l1")).unwrap();

    let started = start_edit(&repo, &loaded, keyboard(), rename("l1", "Backfill events"));
    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    assert!(
        matches!(result, LandingResult::Refused { .. }),
        "{result:?}"
    );
    assert_eq!(main_commit(&repo), before);
    assert_eq!(staged(&repo, &started.change_id), None);
    assert_eq!(
        repo.read_state(|read| read.task("l1")).unwrap(),
        state_before
    );
    // The person repairs it in the working copy now checked out; the row says it did not land.
    let path = copy_path(&repo, &started.change_id);
    assert!(path.join("definitions/tasks/backfill/task.toml").is_file());
    let text = std::fs::read_to_string(path.join("definitions/tasks/backfill/task.toml")).unwrap();
    assert!(text.contains("Backfill events"));
    let pending = loaded
        .work_tree()
        .unwrap()
        .task_by_id("l1")
        .unwrap()
        .pending
        .clone();
    assert_eq!(pending.unwrap().label, NOT_LANDED_MARK);
    assert_eq!(goal(&loaded, "l1"), "Backfill");
}

#[test]
fn a_staged_set_keeps_a_newer_value_and_the_landing_logs_it() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    let tree = loaded.work_tree().unwrap();
    let mut ctx = keyboard();
    let marker = ctx.take_attestation();
    let change = rename("l1", "Backfill events")(&tree, &ctx).unwrap();
    let mut edit = quick_edit_of(&loaded, &tree, &change, &ctx.author);
    let set = |expected: Option<&str>, value: &str| StateOp::Set {
        task: "l1".to_string(),
        field: "linear".to_string(),
        expected: expected.map(|text| toml::Value::String(text.to_string())),
        value: Some(toml::Value::String(value.to_string())),
    };
    edit.state.push(set(None, "SEQ-1"));
    let started = block_on(repo.quick_edit(edit, marker, start())).unwrap();
    // The engine sets the field while the change waits to land.
    apply_state_only(
        &repo.writer_or_error().unwrap(),
        &repo.location().files,
        &[set(None, "SEQ-2")],
        &[],
        &Author::unattributed("engine"),
    )
    .unwrap();

    let result = block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    assert!(matches!(result, LandingResult::Landed { .. }), "{result:?}");
    let state = repo.read_state(|read| read.task("l1")).unwrap().unwrap();
    assert_eq!(state.linear.as_deref(), Some("SEQ-2"));
    assert!(
        landing_record(&repo, &started.change_id)
            .notes
            .contains(&"kept newer linear of l1".to_string())
    );
}

#[test]
fn a_write_through_a_stale_snapshot_is_refused() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    let stale = loaded.work_tree().unwrap();

    let started = start_edit(&repo, &loaded, keyboard(), rename("l1", "Backfill events"));
    block_on(repo.land_quick_edit(&started.change_id, start())).unwrap();
    block_on(loaded.load()).unwrap();

    let change = rename("l1", "Something else")(&stale, &keyboard()).unwrap();
    let error = check_fresh(&stale, &change, &loaded.definitions()).unwrap_err();
    assert!(error.ends_with(STALE_SNAPSHOT), "{error}");
    // A task the newer commit did not change is still writable from that snapshot.
    let change = rename("l2", "Monthly")(&stale, &keyboard()).unwrap();
    check_fresh(&stale, &change, &loaded.definitions()).unwrap();
}

#[test]
fn paths_that_leave_definitions_or_files_are_refused() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    let tree = loaded.work_tree().unwrap();
    let template = commands::new_child(
        &tree,
        &unattributed(),
        "g1",
        NewTask {
            goal: "Escape".to_string(),
            kind: Some(Kind::Other),
            repository: None,
            todos: None,
        },
    )
    .unwrap();
    let group = tree.task_by_id("g1").unwrap().dir.clone();
    let mut escaping = template.clone();
    for write in &mut escaping.writes {
        if let Write::Task { dir, file } = write
            && tree.task_by_id(&file.id).is_none()
        {
            *dir = group.join("tasks").join("..").join("..").join("evil");
        }
    }
    let error = route(&tree, &escaping).unwrap_err();
    assert!(error.contains("not a definitions path"), "{error}");

    let mut absolute = template;
    for write in &mut absolute.writes {
        if let Write::Task { dir, file } = write
            && tree.task_by_id(&file.id).is_none()
        {
            *dir = PathBuf::from("/etc/evil");
        }
    }
    assert!(route(&tree, &absolute).is_err());

    // A quick edit whose ops leave definitions/ makes no branch.
    let refs_before = crate::test_support::refs(&repo);
    let base = loaded.shown().0.unwrap().0;
    for (from, to) in [("tasks/backfill", "../evil"), ("tasks/backfill", "/evil")] {
        let edit = QuickEdit {
            base: base.clone(),
            definition: vec![DefinitionOp::Move {
                from: from.to_string(),
                to: to.to_string(),
            }],
            state: Vec::new(),
            files: Vec::new(),
            meta: ChangeMeta::new(String::new(), keyboard().author),
            tasks: Vec::new(),
        };
        assert!(block_on(repo.quick_edit(edit, None, start())).is_err());
    }
    assert_eq!(crate::test_support::refs(&repo), refs_before);
    // A sent op cannot name such a path either.
    assert!(
        serde_json::from_str::<DefinitionOp>(r#"{"write":{"path":"../x","bytes":[]}}"#).is_err()
    );
    assert!(serde_json::from_str::<DefinitionOp>(r#"{"delete":{"path":"/etc/passwd"}}"#).is_err());
}

#[test]
fn two_hundred_new_tasks_are_one_quick_edit_and_one_commit() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    let tree = loaded.work_tree().unwrap();
    let before = main_commit(&repo);
    let group = tree.task_by_id("g1").unwrap().dir.clone();
    let template = commands::new_child(
        &tree,
        &unattributed(),
        "g1",
        NewTask {
            goal: "Template".to_string(),
            kind: Some(Kind::Other),
            repository: None,
            todos: None,
        },
    )
    .unwrap();
    let Some(Write::Task { file, .. }) = template
        .writes
        .iter()
        .find(|write| matches!(write, Write::Task { file, .. } if tree.task_by_id(&file.id).is_none()))
        .cloned()
    else {
        panic!("the new task's write");
    };
    let mut change = template;
    change.writes = (0..200)
        .map(|number| {
            let mut file = file.clone();
            file.id = new_id();
            file.goal = format!("Task {number}");
            Write::Task {
                dir: group.join("tasks").join(format!("task-{number}")),
                file,
            }
        })
        .collect();

    let timer = Instant::now();
    let projected = project_into(&tree, &repo.location().working_copies.join("projected"));
    assert_eq!(projected.tasks().len(), tree.tasks().len());
    let edit = quick_edit_of(&loaded, &tree, &change, &keyboard().author);
    assert_eq!(edit.definition.len(), 200);
    let started = block_on(repo.quick_edit(edit, None, start())).unwrap();
    let elapsed = timer.elapsed();
    // Generous for a slow machine, far below one git commit per task.
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
    assert_eq!(
        run_git(
            &repo,
            &[
                "rev-list",
                "--count",
                &format!("{before}..{}", started.branch)
            ]
        ),
        "1"
    );
    let listed = run_git(&repo, &["ls-tree", "-r", "--name-only", &started.branch]);
    assert_eq!(
        listed
            .lines()
            .filter(|path| path.starts_with("definitions/tasks/task-"))
            .count(),
        200
    );
    // The files folders of the new tasks wait for the landing, with the state.
    let ids = written_tasks(&change);
    assert_eq!(ids.len(), 200);
    assert!(
        ids.iter()
            .all(|id| !repo.location().files.join(id).exists())
    );
    assert!(!copy_path(&repo, &started.change_id).exists());
}

/// Records a landing in flight from `old` to a new commit of `old`'s tree for the change
/// `change_id`, with staged ops for it that a rerun would apply. Returns the new commit.
pub(crate) fn in_flight(repo: &ProjectRepo, change_id: &str, old: &str) -> String {
    let tree = run_git(repo, &["rev-parse", &format!("{old}^{{tree}}")]);
    let commit = run_git(
        repo,
        &[
            "-c",
            "user.name=bongos",
            "-c",
            "user.email=bongos@example.com",
            "commit-tree",
            &tree,
            "-p",
            old,
            "-m",
            "Landed change\n\nBongos-Change: other",
        ],
    );
    let marker = toml::to_string(&InFlight {
        change_id: change_id.to_string(),
        commit: commit.clone(),
        old: old.to_string(),
    })
    .unwrap();
    let ops = vec![StateOp::Set {
        task: "l1".to_string(),
        field: "linear".to_string(),
        expected: None,
        value: Some(toml::Value::String("RERUN-1".to_string())),
    }];
    repo.writer_or_error()
        .unwrap()
        .transact(|tx| {
            tx.stage(change_id, &ops)?;
            tx.set_meta(LANDING_IN_FLIGHT_KEY, &marker)
        })
        .unwrap();
    commit
}

pub(crate) fn marker(repo: &ProjectRepo) -> Option<String> {
    repo.read_state(|read| read.meta(LANDING_IN_FLIGHT_KEY))
        .unwrap()
}

fn assert_not_rerun(repo: &ProjectRepo, hook: &CountingHook, change_id: &str) {
    assert_eq!(hook.call_count(), 0);
    let state = repo
        .read_state(|read| read.task("l1"))
        .unwrap()
        .unwrap_or_default();
    assert_eq!(state.linear, None);
    assert!(staged(repo, change_id).is_some());
}

#[test]
fn recovery_completes_a_landing_whose_ref_never_moved() {
    let root = TempDir::new().unwrap();
    let hook = CountingHook::new("counting", false);
    let repo = fixture_project_with_hooks(&root, &CountingHook::registry(&hook));
    let old = main_commit(&repo);
    let commit = in_flight(&repo, "c1", &old);

    let recovery = block_on(repo.recover_landing()).unwrap();
    assert_eq!(
        recovery,
        Recovery::Completed {
            commit: commit.clone()
        }
    );
    assert_eq!(main_commit(&repo), commit);
    assert_eq!(marker(&repo), None);
    assert_not_rerun(&repo, &hook, "c1");
}

#[test]
fn recovery_finishes_a_landing_whose_ref_already_moved() {
    let root = TempDir::new().unwrap();
    let hook = CountingHook::new("counting", false);
    let repo = fixture_project_with_hooks(&root, &CountingHook::registry(&hook));
    let old = main_commit(&repo);
    let commit = in_flight(&repo, "c1", &old);
    run_git(&repo, &["update-ref", "refs/heads/main", &commit, &old]);

    let recovery = block_on(repo.recover_landing()).unwrap();
    assert_eq!(
        recovery,
        Recovery::Completed {
            commit: commit.clone()
        }
    );
    assert_eq!(main_commit(&repo), commit);
    assert_eq!(marker(&repo), None);
    assert_not_rerun(&repo, &hook, "c1");
}

#[test]
fn recovery_treats_another_tip_as_a_split_master() {
    let root = TempDir::new().unwrap();
    let hook = CountingHook::new("counting", false);
    let repo = fixture_project_with_hooks(&root, &CountingHook::registry(&hook));
    let old = main_commit(&repo);
    let commit = in_flight(&repo, "c1", &old);
    let outside = outside_document(&repo, "notes.md", "an outside commit");

    let recovery = block_on(repo.recover_landing()).unwrap();
    let Recovery::Split(split) = recovery else {
        panic!("expected a split master, got {recovery:?}");
    };
    assert_eq!(split.main, outside);
    assert_eq!(split.verified, commit);
    assert_eq!(main_commit(&repo), outside);
    assert!(repo.split_master().unwrap().is_some());
    assert_not_rerun(&repo, &hook, "c1");
}

#[test]
fn trailer_text_never_makes_a_landing() {
    let root = TempDir::new().unwrap();
    let repo = fixture_project(&root);
    let loaded = loaded(&repo);
    let started = start_edit(
        &repo,
        &loaded,
        unattributed(),
        rename("l1", "Backfill events"),
    );
    let message = format!(
        "Looks landed\n\nBongos-Change: {}\nBongos-Landed: yes\nBongos-Approved: edited by Sam",
        started.change_id
    );
    let outside = outside_document(&repo, "notes.md", &message);

    assert_eq!(block_on(repo.recover_landing()).unwrap(), Recovery::Nothing);
    let status = block_on(repo.check_master()).unwrap();
    assert!(
        matches!(&status, MasterStatus::Outside(change) if change.commit == outside),
        "{status:?}"
    );
    let row = repo.change(&started.change_id).unwrap().unwrap();
    assert_eq!(row.state, ChangeState::Open);
    assert!(repo.approvals(&started.change_id).unwrap().is_empty());
    assert!(staged(&repo, &started.change_id).is_some());
    assert_ne!(
        crate::test_support::verified(&repo).as_deref(),
        Some(outside.as_str())
    );
}
