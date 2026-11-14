use std::sync::Arc;

use bongos_project_repo_model::{TaskStateRecord, TodoStateRecord};
use bongos_work_model::{Status, TodoState};
use tempfile::TempDir;

use crate::{Location, StateApplyHooks, StateStore, StateWriter};

/// A project location whose parts are all inside `root`, with an empty state directory.
pub(crate) fn location(root: &TempDir) -> Location {
    let folder = root.path().join("project");
    let state = folder.join("state");
    std::fs::create_dir_all(&state).unwrap();
    Location {
        id: "project-1".to_string(),
        repository: folder.join("repository"),
        working_copies: folder.join("working-copies"),
        files: folder.join("files"),
        state,
        folder,
        remote: None,
        converted_from: None,
    }
}

/// A writer with its own hook registry, so tests never share the global one.
pub(crate) fn writer(location: &Location, hooks: &StateApplyHooks) -> Arc<StateWriter> {
    Arc::new(StateStore::open_writer_with_hooks(location, hooks).unwrap())
}

pub(crate) fn todo(id: &str, state: TodoState) -> TodoStateRecord {
    TodoStateRecord {
        id: Some(id.to_string()),
        state,
        started: None,
    }
}

pub(crate) fn task_with_todos(todos: &[(&str, TodoState)]) -> TaskStateRecord {
    TaskStateRecord {
        status: Status::Open,
        todos: todos.iter().map(|(id, state)| todo(id, *state)).collect(),
        ..TaskStateRecord::default()
    }
}

pub(crate) fn git() -> crate::Git {
    crate::Git::locate().expect("git is installed").isolated()
}

pub(crate) const PROJECT_TOML: &str = "schema = 1\nid = \"project-1\"\ntitle = \"Invoicing\"\n";

/// A definition `task.toml`: a leaf of `kind` with the given todos (id, name).
pub(crate) fn definition_toml(id: &str, goal: &str, kind: &str, todos: &[(&str, &str)]) -> String {
    let mut text = format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\nkind = \"{kind}\"\n\
         created = 2026-01-01T10:00:00Z\n"
    );
    for (todo_id, name) in todos {
        text.push_str(&format!(
            "\n[[todos]]\nid = \"{todo_id}\"\nname = \"{name}\"\n"
        ));
    }
    text
}

/// The definitions of the fixture project: the top group `g1` with the leaves `backfill` (`l1`,
/// three todos) and `report` (`l2`, one todo).
pub(crate) fn fixture_definitions() -> Vec<(&'static str, String)> {
    vec![
        ("project.toml", PROJECT_TOML.to_string()),
        (
            "task.toml",
            definition_toml("g1", "Invoicing", "other", &[]),
        ),
        ("plan.md", String::new()),
        (
            "tasks/backfill/task.toml",
            definition_toml(
                "l1",
                "Backfill",
                "other",
                &[("t1", "implement"), ("t2", "review"), ("t3", "check")],
            ),
        ),
        (
            "tasks/report/task.toml",
            definition_toml("l2", "Report", "other", &[("r1", "write")]),
        ),
    ]
}

/// A new-format project in a real git repository inside `root`: the fixture definitions
/// committed on `main`, the state store open with its writer. Returns the repository with its
/// state open.
pub(crate) fn fixture_project(root: &TempDir) -> crate::ProjectRepo {
    fixture_project_with_hooks(root, &StateApplyHooks::new())
}

/// [`fixture_project`] whose writer runs `hooks`.
pub(crate) fn fixture_project_with_hooks(
    root: &TempDir,
    hooks: &StateApplyHooks,
) -> crate::ProjectRepo {
    let location = location(root);
    let definitions = location.repository.join("definitions");
    for (path, text) in fixture_definitions() {
        let path = definitions.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    std::fs::create_dir_all(&location.files).unwrap();
    let git = git();
    futures::executor::block_on(async {
        git.init_repo(&location.repository, crate::MAIN_BRANCH)
            .await
            .unwrap();
        git.commit_all(&location.repository, "create", &crate::Identity::default())
            .await
            .unwrap();
    });
    let writer = writer(&location, hooks);
    with_test_rules(
        crate::ProjectRepo::new(location, git).with_state(crate::StateHandle::Writer(writer)),
    )
}

/// The fixture project without the test approval rules: an agent's, workflow's or healer's
/// change waits for a person as with no feature's hook registered.
pub(crate) fn bare_fixture_project(root: &TempDir) -> crate::ProjectRepo {
    without_test_rules(fixture_project(root))
}

/// The project of `repo` opened again, as after a restart of the process: `repo` must be the
/// last handle on its writer.
pub(crate) fn reopen(repo: crate::ProjectRepo, hooks: &StateApplyHooks) -> crate::ProjectRepo {
    with_test_rules(reopen_bare(repo, hooks))
}

/// [`reopen`] without the test approval rules.
pub(crate) fn reopen_bare(repo: crate::ProjectRepo, hooks: &StateApplyHooks) -> crate::ProjectRepo {
    let location = repo.location().clone();
    let git = repo.git().clone();
    drop(repo);
    let writer = writer(&location, hooks);
    crate::ProjectRepo::new(location, git).with_state(crate::StateHandle::Writer(writer))
}

/// An approval hook implementing `rule` that approves every change it is asked about: what the
/// features that implement the automatic rules (the task builder, lanes, the healer) answer for
/// the changes of the landing tests, which are not about approval.
pub(crate) struct ApproveRule(pub(crate) crate::AutomaticRule);

impl crate::ApprovalHook for ApproveRule {
    fn name(&self) -> &str {
        "test rule"
    }

    fn rule(&self) -> Option<crate::AutomaticRule> {
        Some(self.0)
    }

    fn judge(&self, _input: &crate::ApprovalInput) -> crate::ApprovalOpinion {
        crate::ApprovalOpinion::Approve(format!("test {} rule", self.0.word()))
    }
}

/// `repo` with an [`ApproveRule`] for each automatic rule.
pub(crate) fn with_test_rules(repo: crate::ProjectRepo) -> crate::ProjectRepo {
    for rule in [
        crate::AutomaticRule::OwnSubtree,
        crate::AutomaticRule::DeclaredScope,
        crate::AutomaticRule::IndependentReview,
    ] {
        repo.add_approval_hook(Arc::new(ApproveRule(rule)));
    }
    repo
}

/// `repo` with the approval hooks added to it removed.
pub(crate) fn without_test_rules(repo: crate::ProjectRepo) -> crate::ProjectRepo {
    repo.engine.local.lock().approval_hooks.clear();
    repo
}

/// Runs git with `args` in the repository of `repo`.
pub(crate) fn run_git(repo: &crate::ProjectRepo, args: &[&str]) -> String {
    futures::executor::block_on(repo.git().run(&repo.location().repository, args)).unwrap()
}

/// The commits of every branch and the branch names of the repository.
pub(crate) fn refs(repo: &crate::ProjectRepo) -> String {
    futures::executor::block_on(repo.git().run(
        &repo.location().repository,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    ))
    .unwrap()
}

/// The fixed time landing tests act at.
pub(crate) fn start() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-03-01T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

/// A person at the keyboard, as a UI action makes one.
pub(crate) fn person() -> bongos_project_repo_model::Author {
    keyboard().author
}

pub(crate) fn new_copy(
    repo: &crate::ProjectRepo,
    name: &str,
    author: &bongos_project_repo_model::Author,
) -> crate::WorkingCopy {
    futures::executor::block_on(repo.create_working_copy(name, "work/", author, start())).unwrap()
}

/// Writes `text` at `relative` under the working copy's `definitions/`.
pub(crate) fn write_definition(copy: &crate::WorkingCopy, relative: &str, text: &str) {
    let path = copy.path.join("definitions").join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Adds the leaf task `slug` with id `id` under the top group of the working copy.
pub(crate) fn add_task(copy: &crate::WorkingCopy, slug: &str, id: &str) {
    write_definition(
        copy,
        &format!("tasks/{slug}/task.toml"),
        &definition_toml(id, slug, "other", &[]),
    );
}

pub(crate) fn request(
    subject: &str,
    author: &bongos_project_repo_model::Author,
) -> crate::LandRequest {
    crate::LandRequest {
        meta: bongos_project_repo_model::ChangeMeta::new(subject, author.clone()),
        mode: crate::LandMode::Land,
    }
}

/// A person at the keyboard, with the marker a UI action holds.
pub(crate) fn keyboard() -> bongos_work_model::commands::Context {
    bongos_work_model::commands::Context::at_keyboard(
        bongos_project_repo_model::PersonAtKeyboard::for_test(),
    )
}

/// Lands as `project: land` would: a person author lands as the person at the keyboard (which
/// approves the person's own change), any other author through [`crate::ProjectRepo::land_now`].
pub(crate) async fn land_as(
    repo: &crate::ProjectRepo,
    change_id: &str,
    request: crate::LandRequest,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<crate::LandingResult> {
    if request.meta.author.kind() == bongos_project_repo_model::AuthorKind::Person {
        repo.land_as_person(change_id, request, keyboard(), now)
            .await
    } else {
        repo.land_now(change_id, request, now).await
    }
}

pub(crate) fn land(
    repo: &crate::ProjectRepo,
    copy: &crate::WorkingCopy,
    author: &bongos_project_repo_model::Author,
) -> crate::LandingResult {
    futures::executor::block_on(land_as(
        repo,
        &copy.change_id,
        request("land it", author),
        start(),
    ))
    .unwrap()
}

/// The commit a landing result landed, or a panic naming the result.
pub(crate) fn landed(result: crate::LandingResult) -> String {
    match result {
        crate::LandingResult::Landed { commit } => commit,
        other => panic!("expected a landing, got {other:?}"),
    }
}

pub(crate) fn main_commit(repo: &crate::ProjectRepo) -> String {
    run_git(repo, &["rev-parse", "main"])
}

pub(crate) fn verified(repo: &crate::ProjectRepo) -> Option<String> {
    repo.verified_commit().unwrap().map(|commit| commit.0)
}

/// Commits `files` (paths from the repository root) on top of `main` the way a person running
/// git in the repository would, outside bongos, and moves `main` to it. Returns the commit.
pub(crate) async fn outside_commit(
    git: crate::Git,
    repository: std::path::PathBuf,
    files: Vec<(String, String)>,
    message: String,
) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let scratch = repository
        .parent()
        .unwrap()
        .join(format!("outside-{number}"));
    let scratch_text = scratch.to_str().unwrap().to_string();
    let main = git.rev_parse(&repository, "main").await.unwrap();
    git.run(
        &repository,
        &["worktree", "add", "-q", "--detach", &scratch_text, &main],
    )
    .await
    .unwrap();
    for (path, text) in files {
        let path = scratch.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    git.run(&scratch, &["add", "-A"]).await.unwrap();
    git.run(
        &scratch,
        &[
            "-c",
            "user.name=someone",
            "-c",
            "user.email=someone@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            &message,
        ],
    )
    .await
    .unwrap();
    let commit = git.rev_parse(&scratch, "HEAD").await.unwrap();
    git.run(
        &repository,
        &["update-ref", "refs/heads/main", &commit, &main],
    )
    .await
    .unwrap();
    git.run(
        &repository,
        &["worktree", "remove", "--force", &scratch_text],
    )
    .await
    .unwrap();
    commit
}

/// [`outside_commit`] of one document, blocking.
pub(crate) fn outside_document(repo: &crate::ProjectRepo, name: &str, message: &str) -> String {
    futures::executor::block_on(outside_commit(
        repo.git().clone(),
        repo.location().repository.clone(),
        vec![(format!("definitions/documents/{name}"), format!("{name}\n"))],
        message.to_string(),
    ))
}

/// A state apply hook that counts its calls, remembers which tasks' definitions it saw change,
/// writes a `kv` entry in the landing's transaction, and fails when told to.
pub(crate) struct CountingHook {
    pub(crate) name: &'static str,
    pub(crate) fail: bool,
    pub(crate) calls: parking_lot::Mutex<Vec<Vec<String>>>,
}

impl CountingHook {
    pub(crate) fn new(name: &'static str, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            name,
            fail,
            calls: parking_lot::Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn call_count(&self) -> usize {
        self.calls.lock().len()
    }

    /// A registry holding only this hook.
    pub(crate) fn registry(hook: &Arc<Self>) -> StateApplyHooks {
        let hooks = StateApplyHooks::new();
        hooks.register(hook.clone());
        hooks
    }
}

impl crate::StateApplyHook for CountingHook {
    fn name(&self) -> &str {
        self.name
    }

    fn wants(&self, change: &crate::StateApplyChange) -> bool {
        change.old_definitions.is_some()
    }

    fn apply(
        &self,
        tx: &mut crate::StateTx,
        change: &crate::StateApplyChange,
    ) -> anyhow::Result<()> {
        let redefined: Vec<String> = change
            .touched
            .iter()
            .filter(|touch| touch.definition_before != touch.definition_after)
            .map(|touch| touch.id.clone())
            .collect();
        self.calls.lock().push(redefined.clone());
        if self.fail {
            anyhow::bail!("refused on purpose");
        }
        tx.kv("test-hook").put(
            "redefined",
            toml::Value::Array(redefined.into_iter().map(toml::Value::String).collect()),
        )
    }
}
