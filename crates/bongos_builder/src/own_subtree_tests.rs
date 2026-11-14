//! The proposal flow on a project repository: real SQLite state and real git in a temporary
//! folder. An agent's proposal is judged by the `own-subtree` hook this crate registers; a
//! person's applies through `project: approve` of its working copy; a reject abandons it.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bongos_builder_model::apply::{Request, file_hash};
use bongos_builder_model::{Choices, LiveSet, applied_summary, diff, fingerprint};
use bongos_project_repo::{
    AmendOutcome, AmendingCheck, ApprovalHook, ApprovalInput, ApprovalOpinion, ChangeFilter,
    ChangeState, CheckInput, Git, Identity, LoadedProject, Location, MAIN_BRANCH, NEEDS_A_WINDOW,
    ProjectRepo, ProjectRepositorySettings, StateApplyHooks, StateHandle, StateStore,
};
use futures::FutureExt as _;
use futures::future::BoxFuture;
use bongos_work::model::commands::Context as WorkContext;
use bongos_work::model::store::WorkTree;
use bongos_work::model::{Author, PersonAtKeyboard, ProposalState};
use bongos_work::{LandingVerdict, WorkDirs, WorkSettings, WorkStore, WriteOutcome};
use fs::RealFs;
use gpui::{Entity, TestAppContext, UpdateGlobal as _};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::apply::{applied_as_is, apply_and_approve, apply_command, reject_command};
use crate::later::{ChatOwner, TestChats, agent_context};
use crate::{BuilderDirs, own_subtree, view};

struct Fixture {
    _temp: TempDir,
    store: Entity<WorkStore>,
    repo: ProjectRepo,
    _registration: gpui::Task<()>,
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A project repository with the top group `g1` and its leaves `r1` and `r2`, its state store
/// open and registered with the work store; `chats` are `(chat id, task it is indexed under)`.
async fn fixture(cx: &mut TestAppContext, chats: &[(&str, &str)]) -> Fixture {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        cx.set_global(db::AppDatabase::test_new());
        editor::init(cx);
        WorkSettings::register(cx);
        ProjectRepositorySettings::register(cx);
        SettingsStore::update_global(cx, |settings, cx| {
            settings.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(true);
            });
        });
    });
    let real = Arc::new(RealFs::new(None, cx.executor()));
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    cx.update(|cx| {
        bongos_work::init(real.clone(), WorkDirs::for_test(base.join("work")), cx);
        crate::init(real.clone(), BuilderDirs::new(&home), cx);
        cx.set_global(TestChats(
            chats
                .iter()
                .map(|(chat, task)| {
                    (
                        chat.to_string(),
                        ChatOwner {
                            task_id: task.to_string(),
                            name: chat.to_string(),
                        },
                    )
                })
                .collect(),
        ));
        own_subtree::refresh(cx);
    });
    let store = cx.update(|cx| WorkStore::global(cx).unwrap());

    let folder = base.join("project");
    let location = Location {
        id: "project-1".to_string(),
        folder: folder.clone(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        state: folder.join("state"),
        files: folder.join("files"),
        remote: None,
        converted_from: None,
    };
    let definitions = location.repository.join("definitions");
    let definition = |id: &str, goal: &str| {
        format!(
            "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\n\
             created = 2026-01-01T10:00:00Z\n\n[[todos]]\nid = \"{id}-t\"\nname = \"do it\"\n"
        )
    };
    write(
        &definitions.join("project.toml"),
        "schema = 1\nid = \"project-1\"\ntitle = \"Repo project\"\n",
    );
    write(
        &definitions.join("task.toml"),
        &definition("g1", "Repo project"),
    );
    write(
        &definitions.join("tasks/leaf/task.toml"),
        &definition("r1", "Repo leaf"),
    );
    write(
        &definitions.join("tasks/leaf2/task.toml"),
        &definition("r2", "Other leaf"),
    );
    std::fs::create_dir_all(&location.state).unwrap();
    std::fs::create_dir_all(&location.files).unwrap();
    let git = Git::locate().expect("git is installed").isolated();
    git.init_repo(&location.repository, MAIN_BRANCH)
        .await
        .unwrap();
    git.commit_all(&location.repository, "create", &Identity::default())
        .await
        .unwrap();
    let writer =
        Arc::new(StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap());
    let repo = ProjectRepo::new(location, git).with_state(StateHandle::Writer(writer));
    let registration =
        cx.update(|cx| bongos_work::register_project_repo(LoadedProject::new(repo.clone()), cx));
    for _ in 0..200 {
        if store.read_with(cx, |store, _| {
            store
                .snapshot()
                .is_some_and(|tree| tree.task_by_id("r2").is_some())
        }) {
            return Fixture {
                _temp: temp,
                store,
                repo,
                _registration: registration,
            };
        }
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    panic!("the repository never loaded");
}

impl Fixture {
    fn tree(&self, cx: &mut TestAppContext) -> Arc<WorkTree> {
        self.store
            .read_with(cx, |store, _| store.snapshot().unwrap())
    }

    fn main(&self) -> String {
        futures::executor::block_on(
            self.repo
                .git()
                .rev_parse(&self.repo.location().repository, "refs/heads/main"),
        )
        .unwrap()
    }

    fn branches(&self) -> String {
        futures::executor::block_on(self.repo.git().run(
            &self.repo.location().repository,
            &["for-each-ref", "--format=%(refname)", "refs/heads"],
        ))
        .unwrap()
    }

    fn rows(&self, state: Option<ChangeState>) -> Vec<bongos_project_repo::ChangeRow> {
        self.repo
            .changes(&ChangeFilter {
                state,
                ..ChangeFilter::default()
            })
            .unwrap()
    }
}

/// A proposal for the top group that adds a child, written `by` (a header line).
fn proposal(tree: &WorkTree, number: u32, by: &str) -> String {
    let slug = &tree.task_by_id("g1").unwrap().slug;
    format!(
        "proposal {number}\nfor g1\n{by}\n\ntask {slug}: Repo project | id g1\n  \
         task helper-{number}: Helper {number}\n    result a helper in code\n"
    )
}

fn request(tree: &WorkTree, text: &str, number: u32) -> (Request, String) {
    let live = LiveSet::default();
    let analysis = view::analyze(text, number, "g1", tree, &live, 0, None);
    let choices = Choices::default();
    let diff = diff(&analysis.parsed, tree, &live, &choices);
    assert!(!diff.is_empty(), "{:?}", diff.refusal);
    let request = Request {
        fingerprint: fingerprint(&diff, &choices, &file_hash(text.as_bytes())),
        parsed: analysis.parsed,
        choices,
    };
    (request, applied_summary(number, &diff))
}

fn person() -> WorkContext {
    WorkContext::at_keyboard(PersonAtKeyboard::for_test())
}

/// What `later` does with an agent's proposal: write it as the agent and follow its landing.
async fn agent_apply(
    cx: &mut TestAppContext,
    fixture: &Fixture,
    number: u32,
    chat: &str,
) -> (LandingVerdict, String) {
    let tree = fixture.tree(cx);
    let text = proposal(&tree, number, &format!("by chat {chat}"));
    let (request, _) = request(&tree, &text, number);
    let bytes = text.into_bytes();
    let (watch, write) = cx.update(|cx| {
        let mut ctx = agent_context(&bytes);
        ctx.author.proposal = Some(("g1".to_string(), number));
        let watch = bongos_work::watch_landings(cx);
        let write = fixture.store.update(cx, |store, cx| {
            store.write_outcome(
                ctx,
                move |tree, ctx| {
                    apply_command(
                        &request,
                        "g1",
                        number,
                        tree,
                        ctx,
                        &LiveSet::default(),
                        &bytes,
                    )
                },
                cx,
            )
        });
        (watch, write)
    });
    let WriteOutcome::Landing { change_id, .. } = write.await.unwrap() else {
        panic!("expected a working copy");
    };
    (watch.verdict(&change_id).await, change_id)
}

#[gpui::test]
async fn an_agent_inside_its_chats_subtree_is_approved_with_a_log_line(cx: &mut TestAppContext) {
    let fixture = fixture(cx, &[("c1", "g1")]).await;
    let before = fixture.main();
    let (verdict, change_id) = agent_apply(cx, &fixture, 1, "c1").await;
    assert_eq!(verdict, LandingVerdict::Landed);
    assert_ne!(fixture.main(), before);
    let landings = fixture
        .repo
        .state()
        .unwrap()
        .read(|read| read.landings())
        .unwrap();
    let (_, record) = landings.iter().find(|(id, _)| *id == change_id).unwrap();
    assert!(
        record.contains("approved automatically: agent change inside its own subtree"),
        "{record}"
    );
    // An agent never writes an approval row; the proposal's working copy is named for it.
    assert!(fixture.repo.approvals(&change_id).unwrap().is_empty());
    let row = fixture.repo.change(&change_id).unwrap().unwrap();
    assert_eq!(row.name, "proposal-repo-project-1");
    assert_eq!(row.author.proposal, Some(("g1".to_string(), 1)));
}

#[gpui::test]
async fn an_agent_outside_its_chats_subtree_waits_for_a_person(cx: &mut TestAppContext) {
    // The chat works on the leaf `r1`; the proposal adds a task under the top group.
    let fixture = fixture(cx, &[("c2", "r1")]).await;
    let before = fixture.main();
    let (verdict, change_id) = agent_apply(cx, &fixture, 1, "c2").await;
    let LandingVerdict::Held(reason) = verdict else {
        panic!("expected a wait");
    };
    assert!(reason.contains("own-subtree"), "{reason}");
    assert!(reason.contains("outside the chat's task r1"), "{reason}");
    assert_eq!(fixture.main(), before);
    assert!(fixture.repo.approvals(&change_id).unwrap().is_empty());
}

#[gpui::test]
async fn an_agent_from_an_unindexed_chat_waits_for_a_person(cx: &mut TestAppContext) {
    let fixture = fixture(cx, &[("c1", "g1")]).await;
    let before = fixture.main();
    // `c9` is not in the chat index, whatever the proposal claims.
    let (verdict, _) = agent_apply(cx, &fixture, 1, "c9").await;
    let LandingVerdict::Held(reason) = verdict else {
        panic!("expected a wait");
    };
    assert!(reason.contains("unknown chat"), "{reason}");
    assert_eq!(fixture.main(), before);
}

#[gpui::test]
async fn a_person_applies_a_proposal_through_approve(cx: &mut TestAppContext) {
    let fixture = fixture(cx, &[]).await;
    let before = fixture.main();
    let tree = fixture.tree(cx);
    let text = proposal(&tree, 1, "by kyle");
    let (request, summary) = request(&tree, &text, 1);
    let bytes = text.into_bytes();
    let hash = file_hash(&bytes);
    let message = cx
        .update(|cx| {
            apply_and_approve(
                &fixture.store,
                (person(), person()),
                (request, "g1".to_string(), 1),
                (LiveSet::default(), bytes),
                summary.clone(),
                cx,
            )
        })
        .await;
    assert_eq!(message, summary);
    assert_ne!(fixture.main(), before);
    // The record is in state, written by the landing, and the joined file reads it unchanged.
    for _ in 0..100 {
        if applied_as_is(&fixture.tree(cx), "g1", 1, &hash) {
            break;
        }
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    let tree = fixture.tree(cx);
    assert!(applied_as_is(&tree, "g1", 1, &hash));
    let state = fixture
        .repo
        .state()
        .unwrap()
        .read(|read| read.task("g1"))
        .unwrap()
        .unwrap();
    assert_eq!(state.proposals[0].state, ProposalState::Applied);
    let landed = fixture.rows(Some(ChangeState::Landed));
    assert_eq!(landed.len(), 1);
    assert_eq!(landed[0].name, "proposal-repo-project-1");
    let approvals = fixture.repo.approvals(&landed[0].change_id).unwrap();
    assert_eq!(approvals.len(), 1);
    assert_eq!(format!("{:?}", approvals[0].basis), "PersonApprove");
}

/// Writes the proposal as the person at the keyboard, as the Apply does, but never approves it.
async fn write_without_approving(
    cx: &mut TestAppContext,
    fixture: &Fixture,
    number: u32,
) -> (String, String) {
    let tree = fixture.tree(cx);
    let text = proposal(&tree, number, "by kyle");
    let (request, _) = request(&tree, &text, number);
    let bytes = text.clone().into_bytes();
    let mut ctx = person();
    ctx.author.proposal = Some(("g1".to_string(), number));
    let outcome = fixture
        .store
        .update(cx, |store, cx| {
            store.write_outcome(
                ctx,
                move |tree, ctx| {
                    apply_command(
                        &request,
                        "g1",
                        number,
                        tree,
                        ctx,
                        &LiveSet::default(),
                        &bytes,
                    )
                },
                cx,
            )
        })
        .await
        .unwrap();
    let WriteOutcome::Landing { change_id, .. } = outcome else {
        panic!("expected a working copy");
    };
    (change_id, text)
}

#[gpui::test]
async fn a_proposal_never_approved_leaves_master_unchanged_with_its_working_copy(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, &[]).await;
    let before = fixture.main();
    let (change_id, _) = write_without_approving(cx, &fixture, 1).await;
    // The person's marker approves nothing by itself: the landing waits.
    cx.executor().timer(Duration::from_millis(500)).await;
    assert_eq!(fixture.main(), before);
    assert!(fixture.repo.approvals(&change_id).unwrap().is_empty());
    let row = fixture.repo.change(&change_id).unwrap().unwrap();
    assert!(matches!(
        row.state,
        ChangeState::Open | ChangeState::Requested
    ));
    assert!(
        fixture.branches().contains(&row.branch),
        "{}",
        fixture.branches()
    );
    assert_eq!(row.name, "proposal-repo-project-1");
}

#[gpui::test]
async fn a_reject_abandons_the_working_copy_and_records_the_rejection(cx: &mut TestAppContext) {
    let fixture = fixture(cx, &[]).await;
    let before = fixture.main();
    let (change_id, text) = write_without_approving(cx, &fixture, 1).await;
    let row = fixture.repo.change(&change_id).unwrap().unwrap();
    let bytes = text.into_bytes();
    fixture
        .store
        .update(cx, |store, cx| {
            store.write(
                person(),
                move |tree, ctx| reject_command("g1", 1, tree, ctx, &bytes),
                cx,
            )
        })
        .await
        .unwrap();
    let abandoned = cx
        .update(|cx| bongos_work::abandon_proposal("g1", 1, cx))
        .await;
    assert_eq!(abandoned, Ok(1));
    assert_eq!(fixture.main(), before);
    assert_eq!(
        fixture.repo.change(&change_id).unwrap().unwrap().state,
        ChangeState::Abandoned
    );
    assert!(!fixture.branches().contains(&row.branch));
    let state = fixture
        .repo
        .state()
        .unwrap()
        .read(|read| read.task("g1"))
        .unwrap()
        .unwrap();
    assert_eq!(state.proposals.len(), 1);
    assert_eq!(state.proposals[0].state, ProposalState::Rejected);
}

#[gpui::test]
async fn an_agent_never_receives_a_person_approval(cx: &mut TestAppContext) {
    let fixture = fixture(cx, &[("c2", "r1")]).await;
    let (_, change_id) = agent_apply(cx, &fixture, 1, "c2").await;
    let before = fixture.main();
    // An agent's context holds no marker; one with an agent author (a person's marker moved
    // under an agent's name) and one with a person author read back from storage are no person.
    let forged: Author = serde_json::from_str(r#"{"kind":"person","name":"kyle"}"#).unwrap();
    let contexts = [
        WorkContext::new("agent", bongos_work::model::now()).with_author(Author::agent("agent")),
        person().with_author(Author::agent("agent").with_chat("c2")),
        WorkContext::new("kyle", bongos_work::model::now()).with_author(forged.clone()),
        person().with_author(forged),
    ];
    for ctx in contexts {
        let error = cx
            .update(|cx| bongos_work::approve_proposal(&change_id, ctx, cx))
            .await
            .unwrap_err();
        assert!(error.contains(NEEDS_A_WINDOW), "{error}");
    }
    assert!(fixture.repo.approvals(&change_id).unwrap().is_empty());
    assert_eq!(fixture.main(), before);
}

/// `future`, failing the test when it takes longer than a landing ever should.
async fn within<T>(cx: &mut TestAppContext, future: impl std::future::Future<Output = T>) -> T {
    let timeout = cx.executor().timer(Duration::from_secs(60));
    futures::select_biased! {
        value = future.fuse() => value,
        _ = timeout.fuse() => panic!("the landing never reached a verdict"),
    }
}

#[gpui::test]
async fn an_apply_whose_landing_waited_before_the_approval_still_lands(cx: &mut TestAppContext) {
    let fixture = fixture(cx, &[]).await;
    let before = fixture.main();
    let tree = fixture.tree(cx);
    let (_, summary) = request(&tree, &proposal(&tree, 1, "by kyle"), 1);
    // The Apply's watch starts before the write, as `apply_and_approve`'s does.
    let watch = cx.update(|cx| bongos_work::watch_landings(cx));
    let probe = cx.update(|cx| bongos_work::watch_landings(cx));
    let (change_id, _) = write_without_approving(cx, &fixture, 1).await;
    // The landing runs first: it says it waits for approval before any approval exists.
    let LandingVerdict::Held(reason) = within(cx, probe.verdict(&change_id)).await else {
        panic!("expected the landing to wait");
    };
    assert!(reason.starts_with("waiting for approval"), "{reason}");
    assert!(fixture.repo.approvals(&change_id).unwrap().is_empty());
    let expected = summary.clone();
    let following = cx.spawn(|mut cx| async move {
        crate::apply::approve_and_follow(
            watch,
            &change_id,
            person(),
            ("g1".to_string(), 1),
            summary,
            &mut cx,
        )
        .await
    });
    let message = within(cx, following).await;
    assert_eq!(message, expected);
    assert_ne!(fixture.main(), before);
    assert_eq!(fixture.rows(Some(ChangeState::Landed)).len(), 1);
}

/// The `task.toml` under `folder` whose text holds `needle`.
fn task_file_holding(folder: &Path, needle: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(folder).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = task_file_holding(&path, needle) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|name| name == "task.toml")
            && std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle))
        {
            return Some(path);
        }
    }
    None
}

/// An amending check that waits until the change has an approval row, then sizes the task the
/// proposal adds: the approval was written before the amendment, so it no longer covers the
/// change.
struct AmendsAfterTheApproval {
    repo: ProjectRepo,
    executor: gpui::BackgroundExecutor,
}

impl AmendingCheck for AmendsAfterTheApproval {
    fn name(&self) -> &str {
        "late-amender"
    }

    fn amend(&self, _input: CheckInput, writable: &Path) -> BoxFuture<'static, AmendOutcome> {
        let (repo, executor) = (self.repo.clone(), self.executor.clone());
        let definitions = writable.join("definitions");
        async move {
            loop {
                let changes = repo.changes(&ChangeFilter::default()).unwrap();
                if changes
                    .iter()
                    .any(|row| !repo.approvals(&row.change_id).unwrap().is_empty())
                {
                    break;
                }
                executor.timer(Duration::from_millis(20)).await;
            }
            let task = task_file_holding(&definitions, "Helper 1").unwrap();
            let text = std::fs::read_to_string(&task).unwrap();
            if text.contains("size = ") {
                return AmendOutcome::Unchanged;
            }
            std::fs::write(&task, text.replacen("schema = 1\n", "schema = 1\nsize = 3\n", 1))
                .unwrap();
            AmendOutcome::Amended {
                message: "size".to_string(),
            }
        }
        .boxed()
    }
}

/// A protected-section rule on every change: only a person's approval of the change as amended
/// covers it.
struct EveryChangeNeedsAPerson;

impl ApprovalHook for EveryChangeNeedsAPerson {
    fn name(&self) -> &str {
        "protected-section"
    }

    fn judge(&self, _input: &ApprovalInput) -> ApprovalOpinion {
        ApprovalOpinion::NeedsPerson("the change touches the protected section".to_string())
    }
}

#[gpui::test]
async fn an_apply_whose_landing_waits_after_the_approval_reports_it_held(
    cx: &mut TestAppContext,
) {
    let fixture = fixture(cx, &[]).await;
    fixture.repo.add_amending_check(Arc::new(AmendsAfterTheApproval {
        repo: fixture.repo.clone(),
        executor: cx.executor(),
    }));
    fixture
        .repo
        .add_approval_hook(Arc::new(EveryChangeNeedsAPerson));
    let before = fixture.main();
    let watch = cx.update(|cx| bongos_work::watch_landings(cx));
    let (change_id, _) = write_without_approving(cx, &fixture, 1).await;
    let following = cx.spawn({
        let change_id = change_id.clone();
        |mut cx| async move {
            crate::apply::approve_and_follow(
                watch,
                &change_id,
                person(),
                ("g1".to_string(), 1),
                "applied".to_string(),
                &mut cx,
            )
            .await
        }
    });
    // The amendment after the approval makes it stale: the landing waits for a person again,
    // and the Apply says so instead of following it forever.
    let message = within(cx, following).await;
    assert!(
        message.starts_with("Proposal 1 pending: ")
            && message.contains("the change touches the protected section"),
        "{message}"
    );
    assert_eq!(fixture.repo.approvals(&change_id).unwrap().len(), 1);
    assert_eq!(fixture.main(), before);
    assert!(fixture.rows(Some(ChangeState::Landed)).is_empty());
}

#[gpui::test]
async fn a_reject_ends_the_watch_on_the_proposals_landing(cx: &mut TestAppContext) {
    let fixture = fixture(cx, &[]).await;
    let watch = cx.update(|cx| bongos_work::watch_landings(cx));
    let probe = cx.update(|cx| bongos_work::watch_landings(cx));
    let (change_id, text) = write_without_approving(cx, &fixture, 1).await;
    assert!(matches!(
        within(cx, probe.verdict(&change_id)).await,
        LandingVerdict::Held(_)
    ));
    let bytes = text.into_bytes();
    let message = cx
        .update(|cx| {
            crate::apply::reject_and_abandon(
                &fixture.store,
                person(),
                ("g1".to_string(), 1),
                move |tree, ctx| reject_command("g1", 1, tree, ctx, &bytes),
                cx,
            )
        })
        .await;
    assert_eq!(message, crate::apply::rejected_message(1));
    // Even a watcher that passed over the waits hears that the landing is over.
    let mut watch = watch;
    watch.pass_waits(&change_id);
    assert_eq!(
        within(cx, watch.verdict(&change_id)).await,
        LandingVerdict::Ended(bongos_project_repo::LANDING_ABANDONED.to_string())
    );
}

#[gpui::test]
async fn a_reject_that_was_not_written_leaves_the_working_copy_waiting(cx: &mut TestAppContext) {
    let fixture = fixture(cx, &[]).await;
    let before = fixture.main();
    let (change_id, _) = write_without_approving(cx, &fixture, 1).await;
    let message = cx
        .update(|cx| {
            crate::apply::reject_and_abandon(
                &fixture.store,
                person(),
                ("g1".to_string(), 1),
                |_, _| Err("the rejection was refused".to_string()),
                cx,
            )
        })
        .await;
    assert!(message.contains("the rejection was refused"), "{message}");
    let row = fixture.repo.change(&change_id).unwrap().unwrap();
    assert!(
        matches!(row.state, ChangeState::Open | ChangeState::Requested),
        "{:?}",
        row.state
    );
    assert!(fixture.branches().contains(&row.branch));
    assert_eq!(fixture.main(), before);
}
