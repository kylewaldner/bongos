//! Tests of feature 20 in a project repository: the `workflow` landing check kind (its run starts
//! with `StartedBy::Landing`), check 5 (`workflow-valid`, naming node and line), the commit a run
//! reads its definition from (a run started before a landing keeps its prompt, a restart reads
//! the new one), `workflow: edit definition` opening a working copy, and what runs tell the
//! project (live source, activity, `runs.toml`). Real git and SQLite in a `TempDir`; the runs are
//! the scripted ones of the lifecycle tests, a child of which this is.

use std::sync::Mutex as StdMutex;

use bongos_project_repo::{
    ActivitySource as _, ChangeFilter, CheckOutcome, Git, Identity, LandMode, LandRequest,
    LandingResult, LiveSource as _, LoadedProject, Location, MAIN_BRANCH, ProjectRepo,
    ProjectRepositorySettings, RUNS_NAMESPACE, StateApplyHooks, StateHandle, StateStore,
};
use bongos_project_repo_model::ChangeMeta;
use bongos_work_model::PersonAtKeyboard;
use bongos_work_model::commands::Context;
use bongos_workflow_model::record::{RunToml, StartedBy};
use chrono::{DateTime, Utc};
use futures::channel::oneshot;
use futures::executor::block_on;
use futures::future::BoxFuture;

use super::task_link_tests::{Linked, linked};
use super::*;
use crate::engine::EditTarget;
use crate::landing::{
    self, LandingRunEnd, LandingRuns, WORKFLOW_KIND, WorkflowValid, parse_workflow,
    set_landing_runs, set_resolver_dirs, workflow_resolver,
};

const PROJECT_TOML: &str = "schema = 1\nid = \"project-1\"\ntitle = \"Repo project\"\n";

/// A project that declares a `workflow` check named `judge`.
const JUDGING_PROJECT_TOML: &str = "schema = 1\nid = \"project-1\"\ntitle = \"Repo project\"\n\n\
    [[landing.checks]]\nname = \"judge\"\nrun = { workflow = \"single\" }\n";

fn review(name: &str) -> String {
    SINGLE.replace("\"single\"", &format!("\"{name}\""))
}

fn at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn definition(id: &str, goal: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"other\"\n\
         created = 2026-01-01T10:00:00Z\n\n[[todos]]\nid = \"{id}-t\"\nname = \"do it\"\n"
    )
}

/// A new-format project in a real git repository inside `temp`: the top group, `project_toml`
/// and `files` (paths under `definitions/`) committed on `main`, its state store open.
fn project(temp: &TempDir, project_toml: &str, files: &[(&str, String)]) -> ProjectRepo {
    let folder = temp.path().canonicalize().unwrap().join("project");
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
    let mut all = vec![
        ("project.toml", project_toml.to_string()),
        ("task.toml", definition("g1", "Repo project")),
    ];
    all.extend(files.iter().map(|(path, text)| (*path, text.clone())));
    for (path, text) in all {
        let path = definitions.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fs::create_dir_all(&location.state).unwrap();
    fs::create_dir_all(&location.files).unwrap();
    let git = Git::locate().expect("git is installed").isolated();
    block_on(async {
        git.init_repo(&location.repository, MAIN_BRANCH)
            .await
            .unwrap();
        git.commit_all(&location.repository, "create", &Identity::default())
            .await
            .unwrap();
    });
    let writer =
        Arc::new(StateStore::open_writer_with_hooks(&location, &StateApplyHooks::new()).unwrap());
    ProjectRepo::new(location, git).with_state(StateHandle::Writer(writer))
}

fn person() -> Context {
    Context::at_keyboard(PersonAtKeyboard::for_test())
}

fn main_commit(repo: &ProjectRepo) -> String {
    block_on(
        repo.git()
            .run(&repo.location().repository, &["rev-parse", "main"]),
    )
    .unwrap()
}

/// A working copy of `repo` by a person, with `files` (under `definitions/`) written in it.
async fn copy_with(repo: &ProjectRepo, name: &str, files: &[(&str, &str)]) -> String {
    let copy = repo
        .create_working_copy(name, "work/", &person().author, at())
        .await
        .unwrap();
    for (path, text) in files {
        let path = copy.path.join("definitions").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    copy.change_id
}

fn request(mode: LandMode) -> LandRequest {
    LandRequest {
        meta: ChangeMeta::new("the change", person().author),
        mode,
    }
}

/// The checks a check-only landing ran, by name.
async fn checked(repo: &ProjectRepo, change: &str) -> Vec<(String, CheckOutcome)> {
    match repo
        .land_now(change, request(LandMode::CheckOnly), at())
        .await
        .unwrap()
    {
        LandingResult::Checked { checks, .. } => checks
            .into_iter()
            .map(|result| (result.check, result.outcome))
            .collect(),
        other => panic!("expected the results of the checks, got {other:?}"),
    }
}

fn outcome_of<'a>(checks: &'a [(String, CheckOutcome)], name: &str) -> &'a CheckOutcome {
    &checks
        .iter()
        .find(|(check, _)| check == name)
        .unwrap_or_else(|| panic!("no check {name} in {checks:?}"))
        .1
}

/// The tests that set the process-wide starter of landing runs take turns.
static LANDING_RUNS_TURN: StdMutex<()> = StdMutex::new(());

fn take_turn() -> std::sync::MutexGuard<'static, ()> {
    LANDING_RUNS_TURN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Starts nothing: every run ends as `end`.
struct EndsAs(LandingRunEnd);

impl LandingRuns for EndsAs {
    fn run(&self, _: &str, _: &str) -> BoxFuture<'static, LandingRunEnd> {
        let end = self.0.clone();
        Box::pin(async move { end })
    }
}

/// Forwards to the engine's own bridge and says when the first request went by.
struct Signalling {
    inner: Arc<dyn LandingRuns>,
    first: StdMutex<Option<oneshot::Sender<()>>>,
}

impl LandingRuns for Signalling {
    fn run(&self, workflow: &str, task_id: &str) -> BoxFuture<'static, LandingRunEnd> {
        let end = self.inner.run(workflow, task_id);
        if let Some(first) = self.first.lock().unwrap().take() {
            first.send(()).ok();
        }
        end
    }
}

// The turn is held for the whole test on purpose: the starter of runs is process-wide.
#[allow(clippy::await_holding_lock)]
#[gpui::test]
async fn a_workflow_landing_check_starts_its_run_as_a_landing_run(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let _turn = take_turn();
    let world = World::new();
    let temp = TempDir::new().unwrap();
    let repo = project(&temp, JUDGING_PROJECT_TOML, &[]);
    repo.add_check_kind(WORKFLOW_KIND, parse_workflow);
    let linked = linked(&world, FakeRunner::new(), cx);
    cx.update(|cx| linked.engine.install_landing_runs(cx));
    let (first, started) = oneshot::channel();
    set_landing_runs(Some(Arc::new(Signalling {
        inner: landing::landing_runs().expect("the engine installed its runs"),
        first: StdMutex::new(Some(first)),
    })));

    // The change adds the task the world's engine knows, so that is the task the check runs on.
    let task = definition(LEAF, "Add login");
    let change = copy_with(&repo, "judged", &[("tasks/login/task.toml", &task)]).await;
    let checking = {
        let repo = repo.clone();
        cx.executor()
            .spawn(async move { checked(&repo, &change).await })
    };
    started.await.unwrap();
    cx.run_until_parked();

    let run = linked
        .engine
        .active_run_id(LEAF)
        .expect("the landing check started a run");
    let run_dir = linked
        .engine
        .lifecycle()
        .run_dir(&run)
        .unwrap()
        .to_path_buf();
    let text = fs::read_to_string(run_dir.join("run.toml")).unwrap();
    assert!(text.contains("started_by = \"landing\""), "{text}");
    let record = RunToml::parse(&text).unwrap();
    assert_eq!(record.started_by, StartedBy::Landing);
    assert_eq!(record.workflow, "single");

    // The check waits on the run: cancelling it is the check's end, with no verdict.
    cx.update(|cx| linked.engine.cancel(&run, cx)).unwrap();
    let checks = checking.await;
    assert_eq!(
        outcome_of(&checks, "judge"),
        &CheckOutcome::no_verdict("workflow single was cancelled")
    );
    set_landing_runs(None);
}

#[test]
fn the_landing_resolver_finds_a_user_workflow_and_refuses_a_missing_one() {
    let _turn = take_turn();
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("profile");
    fs::create_dir_all(profile.join("review")).unwrap();
    fs::write(profile.join("review/workflow.toml"), review("review")).unwrap();
    let resolver = workflow_resolver();
    assert!(resolver("ghost"), "with no folders set every name resolves");
    set_resolver_dirs(Some(LayerDirs {
        shipped: None,
        profile: Some(profile),
        project: None,
    }));
    let found = resolver("review");
    let missing = resolver("ghost");
    set_resolver_dirs(None);
    assert!(found);
    assert!(!missing);
}

#[test]
fn a_workflow_check_turns_the_end_of_its_run_into_its_verdict() {
    let _turn = take_turn();
    let temp = TempDir::new().unwrap();
    let repo = project(&temp, JUDGING_PROJECT_TOML, &[]);
    repo.add_check_kind(WORKFLOW_KIND, parse_workflow);
    let cases = [
        (LandingRunEnd::Succeeded, CheckOutcome::Pass),
        (
            LandingRunEnd::Failed("it said no".to_string()),
            CheckOutcome::fail("workflow single failed: it said no"),
        ),
        (
            LandingRunEnd::Cancelled,
            CheckOutcome::no_verdict("workflow single was cancelled"),
        ),
        (
            LandingRunEnd::NotStarted("This task has no worktree.".to_string()),
            CheckOutcome::no_verdict("workflow single did not start: This task has no worktree."),
        ),
    ];
    let task = definition("t1", "Add one");
    for (index, (end, expected)) in cases.into_iter().enumerate() {
        set_landing_runs(Some(Arc::new(EndsAs(end))));
        let path = format!("tasks/one-{index}/task.toml");
        let change = block_on(copy_with(
            &repo,
            &format!("case-{index}"),
            &[(&path, &task)],
        ));
        let checks = block_on(checked(&repo, &change));
        assert_eq!(outcome_of(&checks, "judge"), &expected, "case {index}");
    }

    // No engine, no task to run on: no verdict either way.
    set_landing_runs(None);
    let change = block_on(copy_with(
        &repo,
        "no-engine",
        &[("tasks/two/task.toml", &definition("t2", "Add two"))],
    ));
    let checks = block_on(checked(&repo, &change));
    assert_eq!(
        outcome_of(&checks, "judge"),
        &CheckOutcome::no_verdict("the workflow engine is not running")
    );
    set_landing_runs(Some(Arc::new(EndsAs(LandingRunEnd::Succeeded))));
    let change = block_on(copy_with(
        &repo,
        "no-task",
        &[("documents/note.md", "a note\n")],
    ));
    let checks = block_on(checked(&repo, &change));
    assert_eq!(
        outcome_of(&checks, "judge"),
        &CheckOutcome::no_verdict(
            "the change adds or redefines no task for workflow single to run on"
        )
    );
    set_landing_runs(None);
}

#[test]
fn a_workflow_check_entry_must_name_a_workflow() {
    for bad in [
        toml::Value::String(String::new()),
        toml::Value::String("  ".to_string()),
        toml::Value::Integer(3),
        toml::Value::String("a\nb".to_string()),
        toml::Value::String("x".repeat(81)),
    ] {
        assert!(parse_workflow(&bad).is_err(), "{bad:?}");
    }
    let check = parse_workflow(&toml::Value::String(" review ".to_string())).unwrap();
    assert_eq!(check.name(), "workflow review");
}

#[test]
fn workflow_valid_names_the_workflow_the_node_and_the_line() {
    let temp = TempDir::new().unwrap();
    let repo = project(&temp, PROJECT_TOML, &[]);
    repo.add_landing_check(Arc::new(WorkflowValid));
    let broken = "schema = 1\nname = \"broken\"\nstart = \"only\"\n\n[nodes.only]\n\
                  kind = \"program\"\nuse = \"wait\"\nroutes = [{ to = \"nowhere\" }]\n\n\
                  [ends.done]\nkind = \"success\"\n";
    let change = block_on(copy_with(
        &repo,
        "broken",
        &[
            ("workflows/broken/workflow.toml", broken),
            ("workflows/fine/workflow.toml", &review("fine")),
            ("workflows/fine/work.md", "Do the work\n"),
        ],
    ));
    let checks = block_on(checked(&repo, &change));
    let CheckOutcome::Fail { message, .. } = outcome_of(&checks, "workflow-valid") else {
        panic!("expected a failure, got {checks:?}");
    };
    // One line per error; the valid workflow is not named.
    assert!(!message.contains("workflow fine"), "{message}");
    let lines: Vec<&str> = message.lines().collect();
    assert!(!lines.is_empty(), "{message}");
    for line in &lines {
        assert!(line.starts_with("workflow broken: "), "{line}");
    }
    // The error of a node names the node and its line; one of the whole definition has no node.
    let node_line = lines
        .iter()
        .find(|line| line.starts_with("workflow broken: node only, line "))
        .unwrap_or_else(|| panic!("an error names the node: {message}"));
    assert_eq!(
        *node_line,
        "workflow broken: node only, line 5: node 'only' cannot reach any end"
    );
    let number = node_line
        .strip_prefix("workflow broken: node only, line ")
        .and_then(|rest| rest.split_once(": "))
        .map(|(number, _)| number.parse::<usize>().unwrap())
        .unwrap();
    // `[nodes.only]` is the fifth line of the file.
    assert_eq!(number, 5, "{message}");
}

#[test]
fn workflow_valid_checks_a_workflow_whose_prompt_changed() {
    let temp = TempDir::new().unwrap();
    // `other` was already missing its prompt in master; the change does not touch it.
    let repo = project(
        &temp,
        PROJECT_TOML,
        &[
            ("workflows/fine/workflow.toml", review("fine")),
            ("workflows/fine/work.md", "Do the work\n".to_string()),
            ("workflows/other/workflow.toml", review("other")),
        ],
    );
    repo.add_landing_check(Arc::new(WorkflowValid));
    // The change touches only the prompt of `fine`, and removes it: the node names a file that
    // is gone.
    let change = block_on(async {
        let copy = repo
            .create_working_copy("prompt", "work/", &person().author, at())
            .await
            .unwrap();
        fs::remove_file(copy.path.join("definitions/workflows/fine/work.md")).unwrap();
        copy.change_id
    });
    let checks = block_on(checked(&repo, &change));
    let CheckOutcome::Fail { message, .. } = outcome_of(&checks, "workflow-valid") else {
        panic!("expected a failure, got {checks:?}");
    };
    assert!(
        message.contains("workflow fine: node work, line "),
        "{message}"
    );
    assert!(message.contains("work.md"), "{message}");
    assert!(!message.contains("workflow other"), "{message}");
}

#[test]
fn workflow_valid_applies_only_to_a_change_that_touches_a_workflow() {
    let temp = TempDir::new().unwrap();
    let repo = project(&temp, PROJECT_TOML, &[]);
    repo.add_landing_check(Arc::new(WorkflowValid));
    let change = block_on(copy_with(
        &repo,
        "doc",
        &[("documents/note.md", "a note\n")],
    ));
    let checks = block_on(checked(&repo, &change));
    assert!(
        checks.iter().all(|(name, _)| name != "workflow-valid"),
        "{checks:?}"
    );
}

#[test]
fn problems_skip_a_removed_workflow_and_a_folder_name_that_is_not_plain() {
    let temp = TempDir::new().unwrap();
    let workflows = temp.path().join("workflows");
    fs::create_dir_all(workflows.join("leftover")).unwrap();
    fs::write(workflows.join("leftover/work.md"), "no workflow.toml\n").unwrap();
    assert!(
        landing::problems(&workflows, &["leftover".to_string(), "gone".to_string()]).is_empty()
    );
    let lines = landing::problems(
        &workflows,
        &["../escape".to_string(), "a/b".to_string(), "..".to_string()],
    );
    assert_eq!(lines.len(), 3, "{lines:?}");
    // A definition that does not parse is a line of its own, with no node.
    fs::create_dir_all(workflows.join("torn")).unwrap();
    fs::write(
        workflows.join("torn/workflow.toml"),
        "schema = 1\nname = \n",
    )
    .unwrap();
    let lines = landing::problems(&workflows, &["torn".to_string()]);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with("workflow torn: "), "{lines:?}");
}

/// The world's leaf as a task of `repo`, and an engine over the world with a scripted runner;
/// the repository is open in the app, with check 5 registered before it opened.
struct Setup {
    repo: ProjectRepo,
    linked: Linked,
    _world: World,
    _temp: TempDir,
    _open: gpui::Task<()>,
}

async fn setup(cx: &mut TestAppContext, prompt: &str) -> Setup {
    cx.executor().allow_parking();
    let mut world = World::new();
    let temp = TempDir::new().unwrap();
    let repo = project(
        &temp,
        PROJECT_TOML,
        &[
            ("workflows/review/workflow.toml", review("review")),
            ("workflows/review/work.md", prompt.to_string()),
        ],
    );
    let leaf = &mut world.tree.projects[0].task.children[0];
    leaf.dir = repo.location().files.join(LEAF);
    leaf.definition = Some(PathBuf::from("tasks/login"));
    let linked = linked(&world, FakeRunner::new(), cx);
    let open = cx.update(|cx| {
        ProjectRepositorySettings::register(cx);
        landing::register(cx);
        bongos_work::register_project_repo(LoadedProject::new(repo.clone()), cx)
    });
    Setup {
        repo,
        linked,
        _world: world,
        _temp: temp,
        _open: open,
    }
}

fn review_request() -> StartRequest {
    StartRequest {
        workflow: Some("review".to_string()),
        ..StartRequest::new(LEAF)
    }
}

/// The prompt copied into attempt `n` of `run`.
fn attempt_prompt(linked: &Linked, run: &str, n: u32) -> String {
    let run_dir = linked
        .engine
        .lifecycle()
        .run_dir(run)
        .unwrap()
        .to_path_buf();
    fs::read_to_string(definition_copy_dir(&attempt_dir(&run_dir, n), 0).join("work.md")).unwrap()
}

fn attempt_commits(linked: &Linked, run: &str) -> Vec<Option<String>> {
    let run_dir = linked
        .engine
        .lifecycle()
        .run_dir(run)
        .unwrap()
        .to_path_buf();
    let record = RunToml::parse(&fs::read_to_string(run_dir.join("run.toml")).unwrap()).unwrap();
    record
        .attempts
        .iter()
        .map(|attempt| attempt.definition_commit.clone())
        .collect()
}

#[gpui::test]
async fn a_run_started_before_a_landing_reads_the_old_prompt_and_restart_reads_the_new(
    cx: &mut TestAppContext,
) {
    let setup = setup(cx, "the old prompt\n").await;
    let before = main_commit(&setup.repo);
    let run = setup.linked.start(review_request(), cx);
    assert_eq!(attempt_prompt(&setup.linked, &run, 1), "the old prompt\n");
    assert_eq!(attempt_commits(&setup.linked, &run), [Some(before.clone())]);

    // A landing edits the prompt; check 5 validates the workflow on the way in.
    let change = copy_with(
        &setup.repo,
        "better-prompt",
        &[("workflows/review/work.md", "the new prompt\n")],
    )
    .await;
    let landed = setup
        .repo
        .land_as_person(&change, request(LandMode::Land), person(), at())
        .await
        .unwrap();
    let LandingResult::Landed { commit } = landed else {
        panic!("expected a landing, got {landed:?}");
    };
    assert_ne!(commit, before);
    assert_eq!(
        setup.repo.verified_commit().unwrap().map(|commit| commit.0),
        Some(commit.clone())
    );

    // The run in flight keeps what it started with.
    assert_eq!(attempt_prompt(&setup.linked, &run, 1), "the old prompt\n");
    assert_eq!(attempt_commits(&setup.linked, &run), [Some(before.clone())]);

    // `workflow: restart` reads the commit that is verified now.
    cx.update(|cx| setup.linked.engine.restart(&run, cx))
        .unwrap();
    assert_eq!(attempt_prompt(&setup.linked, &run, 2), "the new prompt\n");
    assert_eq!(attempt_prompt(&setup.linked, &run, 1), "the old prompt\n");
    assert_eq!(
        attempt_commits(&setup.linked, &run),
        [Some(before), Some(commit)]
    );
}

#[gpui::test]
async fn a_landing_that_breaks_a_workflow_is_refused_and_a_started_run_is_not_touched(
    cx: &mut TestAppContext,
) {
    let setup = setup(cx, "the prompt\n").await;
    let run = setup.linked.start(review_request(), cx);
    let before = main_commit(&setup.repo);
    // The change deletes the prompt the workflow names.
    let change = {
        let copy = setup
            .repo
            .create_working_copy("no-prompt", "work/", &person().author, at())
            .await
            .unwrap();
        fs::remove_file(copy.path.join("definitions/workflows/review/work.md")).unwrap();
        copy.change_id
    };
    let result = setup
        .repo
        .land_as_person(&change, request(LandMode::Land), person(), at())
        .await
        .unwrap();
    let LandingResult::Refused { reasons } = result else {
        panic!("expected a refusal, got {result:?}");
    };
    assert!(
        reasons
            .iter()
            .any(|reason| reason.contains("workflow review: node work, line ")),
        "{reasons:?}"
    );
    assert_eq!(main_commit(&setup.repo), before);
    assert_eq!(attempt_prompt(&setup.linked, &run, 1), "the prompt\n");
}

#[gpui::test]
async fn edit_definition_opens_a_project_definition_in_a_working_copy(cx: &mut TestAppContext) {
    let setup = setup(cx, "the prompt\n").await;
    let task = setup.linked.writes.leaf();
    let master = main_commit(&setup.repo);
    let target = cx
        .update(|cx| {
            setup
                .linked
                .engine
                .edit_target(Some("review"), Some(&task), cx)
        })
        .unwrap();
    let EditTarget::WorkingCopy { repo, folder } = target else {
        panic!("a definition of a project repository is edited in a working copy");
    };
    assert_eq!(folder, PathBuf::from("workflows/review"));

    let opened = cx
        .update(|cx| {
            bongos_work::working_copy_to_edit(
                repo,
                "edit-review".to_string(),
                Some(folder),
                bongos_work_model::Author::agent("tester"),
                "work/".to_string(),
                cx,
            )
        })
        .await
        .unwrap();
    let copy = setup.repo.working_copy_path("edit-review");
    assert_eq!(opened, copy.join("definitions/workflows/review"));
    assert_eq!(
        fs::read_to_string(opened.join("work.md")).unwrap(),
        "the prompt\n"
    );
    let changes = setup.repo.changes(&ChangeFilter::default()).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].branch, "work/edit-review");
    assert!(!opened.starts_with(&setup.repo.location().repository));
    assert_eq!(
        main_commit(&setup.repo),
        master,
        "master is not edited in place"
    );

    // A definition of another layer, or no task at point, is a file to open as before.
    for (name, task) in [("single", Some(&task)), ("single", None)] {
        let target = cx
            .update(|cx| setup.linked.engine.edit_target(Some(name), task, cx))
            .unwrap();
        let EditTarget::File(file) = target else {
            panic!("{name} is a profile definition");
        };
        assert!(file.ends_with("single/workflow.toml"), "{file:?}");
    }
    // Without a task the project's workflows are not read.
    let error = cx
        .update(|cx| setup.linked.engine.edit_target(Some("review"), None, cx))
        .err()
        .expect("review is only in the project layer");
    assert_eq!(error, "There is no workflow named 'review'");
}

#[gpui::test]
async fn a_run_tells_the_project_its_state_and_holds_back_work_during_a_restore(
    cx: &mut TestAppContext,
) {
    let setup = setup(cx, "the prompt\n").await;
    let engine = &setup.linked.engine;
    let live = engine.live_source();
    let activity = engine.activity_source();
    assert!(!live.started(LEAF));
    assert!(activity.activity("project-1").is_empty());

    let run = setup.linked.start(review_request(), cx);
    // The task counts as started and the run counts as work in flight.
    assert!(live.started(LEAF));
    assert_eq!(activity.activity("project-1").len(), 1);
    assert!(activity.activity("another-project").is_empty());
    let row = |state: &bongos_project_repo::StateRead| {
        state.kv(RUNS_NAMESPACE).get(&run).unwrap().unwrap()
    };
    let rows = setup
        .repo
        .state()
        .unwrap()
        .read(|state| Ok(row(state)))
        .unwrap();
    assert_eq!(rows.get("task").and_then(|v| v.as_str()), Some(LEAF));
    assert_eq!(
        rows.get("workflow").and_then(|v| v.as_str()),
        Some("review")
    );
    // The start ticked once, so the run is already running.
    assert_eq!(rows.get("state").and_then(|v| v.as_str()), Some("running"));
    assert!(rows.get("outcome").is_none());

    // A restore holds new work back: nothing restarts or resumes while it does.
    activity.hold_new_work("project-1");
    let refusal = cx.update(|cx| engine.restart(&run, cx)).unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::Held);
    activity.release_new_work("project-1");

    // Stopping at the job boundary pauses the run, which then is no activity.
    // The started node is still work in flight until it finishes; then nothing more starts.
    activity.stop_at_job_boundary("project-1").unwrap();
    assert_eq!(setup.linked.status(&run), RunStatus::Pausing);
    assert_eq!(activity.activity("project-1").len(), 1);
    setup.linked.release(cx);
    assert_eq!(setup.linked.status(&run), RunStatus::WaitingOnYou);
    assert!(activity.activity("project-1").is_empty());
    assert!(live.started(LEAF), "a stopped run still holds its task");

    // The end of the run is written as its outcome.
    cx.update(|cx| engine.cancel(&run, cx)).unwrap();
    let rows = setup
        .repo
        .state()
        .unwrap()
        .read(|state| Ok(row(state)))
        .unwrap();
    assert_eq!(
        rows.get("state").and_then(|v| v.as_str()),
        Some("cancelled")
    );
    assert_eq!(
        rows.get("outcome").and_then(|v| v.as_str()),
        Some("cancelled")
    );
    assert!(!live.started(LEAF));
}

/// A world whose project layer is the plain directory `layer`, holding `two` with `prompt`.
fn world_with_layer(layer: &str, prompt: &str) -> World {
    let mut world = World::new();
    let dir = world.root.join(layer);
    write_definition(&dir, "two", TWO);
    fs::write(dir.join("two").join("work.md"), prompt).unwrap();
    world.dirs.project = Some(dir);
    world
}

fn commit(digit: char) -> String {
    digit.to_string().repeat(40)
}

fn recorded(lifecycle: &Lifecycle<FakeRunner>, run: &str) -> Vec<Option<String>> {
    lifecycle
        .record(run)
        .unwrap()
        .attempts
        .iter()
        .map(|attempt| attempt.definition_commit.clone())
        .collect()
}

#[test]
fn an_attempt_records_the_commit_of_the_project_layer_it_read() {
    let world = world_with_layer("layer-1", "version 1\n");
    let mut lifecycle = world.lifecycle();
    lifecycle.runtime_mut().runner_mut().fail("work", 2);
    let context = StartContext {
        tree: &world.tree,
        active_run: None,
        checks: &world.checks,
        definition_commit: Some(commit('1')),
        definitions: None,
    };
    let run = lifecycle
        .start(with_workflow("two"), &world.dirs, &context)
        .unwrap();
    assert_eq!(recorded(&lifecycle, &run), [Some(commit('1'))]);
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));

    // A retry that reads nothing again keeps the attempt's commit, whatever the caller offers.
    let newer = world_with_layer("layer-2", "version 2\n");
    lifecycle
        .retry(&run, None, false, &newer.dirs, Some(commit('2')))
        .unwrap();
    assert_eq!(recorded(&lifecycle, &run), [Some(commit('1'))]);
    settle(&mut lifecycle);
    finish_all(&mut lifecycle);
    assert_eq!(lifecycle.status(&run), Some(RunStatus::Failed));

    // A fresh retry reads the layer again and records the commit it came from, in run.toml too.
    lifecycle
        .retry(&run, None, true, &newer.dirs, Some(commit('2')))
        .unwrap();
    assert_eq!(recorded(&lifecycle, &run), [Some(commit('2'))]);
    let on_disk = RunToml::parse(
        &fs::read_to_string(lifecycle.run_dir(&run).unwrap().join("run.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk.attempts[0].definition_commit, Some(commit('2')));
    let attempt = attempt_dir(lifecycle.run_dir(&run).unwrap(), 1);
    assert_eq!(
        fs::read_to_string(definition_copy_dir(&attempt, 1).join("work.md")).unwrap(),
        "version 2\n"
    );
    assert_eq!(
        fs::read_to_string(definition_copy_dir(&attempt, 0).join("work.md")).unwrap(),
        "version 1\n"
    );

    // A restart is a new attempt with the commit then verified; the first keeps its own.
    let newest = world_with_layer("layer-3", "version 3\n");
    lifecycle
        .restart(&run, &newest.dirs, Some(commit('3')))
        .unwrap();
    assert_eq!(
        recorded(&lifecycle, &run),
        [Some(commit('2')), Some(commit('3'))]
    );
}

#[test]
fn a_definition_of_another_layer_records_no_commit() {
    let world = World::new();
    let mut lifecycle = world.lifecycle();
    let context = StartContext {
        tree: &world.tree,
        active_run: None,
        checks: &world.checks,
        // The caller read a project layer, but `single` comes from the profile layer.
        definition_commit: Some(commit('1')),
        definitions: None,
    };
    let run = lifecycle
        .start(with_workflow("single"), &world.dirs, &context)
        .unwrap();
    assert_eq!(recorded(&lifecycle, &run), [None]);
    let text = fs::read_to_string(lifecycle.run_dir(&run).unwrap().join("run.toml")).unwrap();
    assert!(!text.contains("definition_commit"), "{text}");
    lifecycle
        .restart(&run, &world.dirs, Some(commit('2')))
        .unwrap();
    assert_eq!(recorded(&lifecycle, &run), [None, None]);
}

/// Cancels every run at its first event, the way a run that ends the moment it starts reads.
#[gpui::test]
fn a_landing_run_that_ends_while_it_starts_still_gives_the_check_its_verdict(
    cx: &mut TestAppContext,
) {
    let world = World::new();
    let linked = linked(&world, FakeRunner::new(), cx);
    let engine = &linked.engine;
    cx.update(|cx| {
        let engine = engine.clone();
        WorkflowEngine::observe(cx, move |event, cx| {
            let WorkflowEvent::Changed { run, state, .. } = event;
            if *state == RunStatus::Queued {
                engine.cancel(run, cx).unwrap();
            }
        });
    });
    let (reply, end) = oneshot::channel();
    cx.update(|cx| {
        engine.start_landing_run(
            crate::engine::LandingRequest {
                workflow: "single".to_string(),
                task_id: LEAF.to_string(),
                reply,
            },
            cx,
        );
    });
    cx.run_until_parked();
    assert!(matches!(block_on(end).unwrap(), LandingRunEnd::Cancelled));
}

/// Takes the restore's hold on the project while the run's worktree is made, which is after
/// the engine checked the hold and before the run is known to the project.
struct HoldingWorktrees {
    inner: DirWorktrees,
    activity: Arc<StdMutex<Option<crate::project_sources::WorkflowActivity>>>,
}

impl RunWorktrees for HoldingWorktrees {
    fn create(
        &mut self,
        task: &TaskCheckout,
        run_id: &str,
        attempt: u32,
        settings: &WorkflowsSettings,
    ) -> Result<RunWorktree, WorktreeError> {
        if let Some(activity) = self.activity.lock().unwrap().as_ref() {
            activity.hold_new_work("project-1");
        }
        self.inner.create(task, run_id, attempt, settings)
    }
}

#[gpui::test]
async fn a_restore_that_starts_while_a_run_starts_cancels_the_run(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let mut world = World::new();
    let temp = TempDir::new().unwrap();
    let repo = project(
        &temp,
        PROJECT_TOML,
        &[
            ("workflows/review/workflow.toml", review("review")),
            ("workflows/review/work.md", "the prompt\n".to_string()),
        ],
    );
    let leaf = &mut world.tree.projects[0].task.children[0];
    leaf.dir = repo.location().files.join(LEAF);
    leaf.definition = Some(PathBuf::from("tasks/login"));
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        WorkflowsSettings::register(cx);
    });
    let activity_slot = Arc::new(StdMutex::new(None));
    let worktrees = HoldingWorktrees {
        inner: DirWorktrees {
            root: world.root.join("runs"),
            error: world.worktree_error.clone(),
        },
        activity: activity_slot.clone(),
    };
    let lifecycle = Lifecycle::new(
        Box::new(FakeRunner::new()) as EngineRunner,
        Box::new(worktrees),
        world.settings(),
    );
    let writes = super::task_link_tests::MemoryWrites::new(world.tree.clone());
    let engine = WorkflowEngine::with_lifecycle(world.dirs.clone(), lifecycle)
        .with_task_writes(writes.clone());
    *activity_slot.lock().unwrap() = Some(engine.activity_source());
    let _open = cx.update(|cx| {
        ProjectRepositorySettings::register(cx);
        landing::register(cx);
        bongos_work::register_project_repo(LoadedProject::new(repo.clone()), cx)
    });
    cx.run_until_parked();

    let tree = writes.tree.lock().unwrap().clone();
    let refusal = cx
        .update(|cx| engine.start_in(review_request(), &tree, cx))
        .unwrap_err();
    assert_eq!(refusal.kind, RefusalKind::Held);
    assert!(
        engine.active_runs().is_empty(),
        "the refused run is not active"
    );
    assert!(engine.activity_source().activity("project-1").is_empty());
}

#[gpui::test]
async fn a_start_with_a_definition_commit_reads_that_commits_prompt(cx: &mut TestAppContext) {
    let setup = setup(cx, "the old prompt\n").await;
    let before = main_commit(&setup.repo);
    let change = copy_with(
        &setup.repo,
        "newer-prompt",
        &[("workflows/review/work.md", "the new prompt\n")],
    )
    .await;
    let landed = setup
        .repo
        .land_as_person(&change, request(LandMode::Land), person(), at())
        .await
        .unwrap();
    let LandingResult::Landed { commit } = landed else {
        panic!("expected a landing, got {landed:?}");
    };
    assert_ne!(commit, before);

    // The verified commit has the new prompt; the request names the commit before the landing.
    let request = StartRequest {
        definition_commit: Some(bongos_project_repo::CommitId(before.clone())),
        ..review_request()
    };
    let run = setup.linked.start(request, cx);
    assert_eq!(attempt_prompt(&setup.linked, &run, 1), "the old prompt\n");
    assert_eq!(attempt_commits(&setup.linked, &run), [Some(before)]);
}
