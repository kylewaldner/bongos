//! Tests of `workflow: validate`, `workflow: new` and `workflow: edit definition` through
//! dispatching the actions: on by default, and nothing read or written while `workflows.enabled` is
//! set to false.

use std::path::{Path, PathBuf};

use bongos_workflow_model::LayerDirs;
use bongos_workflow_model::layers::DEFINITION_FILE;
use editor::Editor;
use gpui::{BorrowAppContext as _, Entity, TestAppContext, VisualTestContext};
use settings::SettingsStore;
use tempfile::TempDir;
use workspace::Workspace;

use crate::actions::{LastToast, OFF_MESSAGE};
use crate::commands::VALIDATE_TITLE;
use crate::engine::{EngineRoots, WorkflowEngine};

const BROKEN: &str = "schema = 1\n\
name = \"broken\"\n\
start = \"only\"\n\
\n\
[nodes.only]\n\
kind = \"program\"\n\
use = \"wait\"\n\
routes = [{ to = \"nowhere\" }]\n\
\n\
[ends.done]\n\
kind = \"success\"\n";

/// The workspace of `workspace_with_actions` over a temporary profile layer that holds `broken`.
struct Env {
    workspace: Entity<Workspace>,
    app_state: std::sync::Arc<workspace::AppState>,
    _temp: TempDir,
    profile: PathBuf,
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.workflows.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

async fn env(cx: &mut TestAppContext, enabled: bool) -> (Env, &mut VisualTestContext) {
    cx.update(editor::init);
    let (workspace, app_state, cx) = crate::actions_tests::workspace_with_actions(cx).await;
    if enabled {
        set_enabled(cx, true);
    }
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("workflows");
    std::fs::create_dir_all(profile.join("broken")).unwrap();
    std::fs::write(profile.join("broken").join(DEFINITION_FILE), BROKEN).unwrap();
    cx.update(|_, cx| {
        cx.set_global(WorkflowEngine::new(LayerDirs {
            profile: Some(profile.clone()),
            ..LayerDirs::default()
        }))
    });
    (
        Env {
            workspace,
            app_state,
            _temp: temp,
            profile,
        },
        cx,
    )
}

async fn seed(env: &Env, file: &Path, contents: &[u8]) {
    let fs = env.app_state.fs.clone();
    fs.create_dir(file.parent().unwrap()).await.unwrap();
    fs.as_fake().insert_file(file, contents.to_vec()).await;
}

fn dispatch(name: &str, cx: &mut VisualTestContext) {
    let action = cx.update(|_, cx| cx.build_action(name, None).unwrap());
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

fn active_editor(env: &Env, cx: &mut VisualTestContext) -> Option<Entity<Editor>> {
    env.workspace
        .read_with(cx, |workspace, cx| workspace.active_item_as::<Editor>(cx))
}

/// Every path under `dir`, sorted.
fn tree(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for item in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            out.push(item.path());
            if item.path().is_dir() {
                walk(&item.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

#[gpui::test]
async fn validate_opens_the_diagnostics_in_a_read_only_buffer(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    dispatch("workflow::Validate", cx);

    let editor = active_editor(&env, cx).expect("the diagnostics buffer is open");
    editor.read_with(cx, |editor, cx| {
        assert!(editor.read_only(cx));
        assert_eq!(editor.buffer().read(cx).title(cx), VALIDATE_TITLE);
        let text = editor.text(cx);
        assert!(text.contains("broken (profile)"), "{text}");
        assert!(text.contains("error: [only] line 8: "), "{text}");
        assert!(text.contains("nowhere"), "{text}");
    });
}

#[gpui::test]
async fn new_writes_the_template_and_opens_it(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    // The editor reads through the workspace's file system, which is a fake one: put the file the
    // command is about to write there too.
    let file = env.profile.join("new-workflow").join(DEFINITION_FILE);
    seed(&env, &file, b"schema = 1\n").await;
    dispatch("workflow::New", cx);
    // The list of templates is open with "Minimal" first; confirming it writes the minimal one.
    dispatch("menu::Confirm", cx);

    let written = std::fs::read_to_string(&file).expect("the template is written");
    assert!(written.contains("name = \"new-workflow\""), "{written}");
    assert!(env.profile.join("new-workflow").join("work.md").is_file());
    let editor = active_editor(&env, cx).expect("the new definition is open");
    editor.read_with(cx, |editor, cx| {
        let path = editor
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file().map(|file| file.full_path(cx)));
        assert!(
            path.is_some_and(|path| path.ends_with(DEFINITION_FILE)),
            "opened workflow.toml"
        );
    });

    // The written file validates clean through the same command.
    dispatch("workflow::Validate", cx);
    let editor = active_editor(&env, cx).unwrap();
    let text = editor.read_with(cx, |editor, cx| editor.text(cx));
    assert!(text.contains("new-workflow (profile)"), "{text}");
    assert!(
        text.contains("new-workflow (profile)") && text.contains(": ok"),
        "{text}"
    );
}

#[gpui::test]
async fn shipped_templates_are_offered_by_workflow_new(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    dispatch("workflow::New", cx);
    // The list opens on "Minimal"; the next row is the first shipped definition.
    dispatch("menu::SelectNext", cx);
    dispatch("menu::Confirm", cx);

    let dir = env.profile.join("implement-pr");
    let written = std::fs::read_to_string(dir.join(DEFINITION_FILE)).expect("a copy is written");
    assert!(written.contains("name = \"implement PR\""), "{written}");
    assert!(dir.join("triage.md").is_file());
    assert!(dir.join("run_tests.py").is_file());
    assert!(
        !env.profile.join("new-workflow").exists(),
        "not the minimal one"
    );
}

#[gpui::test]
async fn edit_definition_opens_the_resolved_file(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    let file = env.profile.join("broken").join(DEFINITION_FILE);
    seed(&env, &file, BROKEN.as_bytes()).await;
    let engine = cx.update(|_, cx| cx.global::<WorkflowEngine>().clone());
    assert_eq!(engine.definition_path(None).unwrap(), file);

    dispatch("workflow::EditDefinition", cx);
    let editor = active_editor(&env, cx).expect("the definition is open");
    editor.read_with(cx, |editor, cx| {
        assert_eq!(editor.text(cx), BROKEN);
        assert!(!editor.read_only(cx));
    });
}

#[gpui::test]
async fn edit_definition_with_no_definition_says_so(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    std::fs::remove_dir_all(env.profile.join("broken")).unwrap();
    dispatch("workflow::EditDefinition", cx);
    assert!(active_editor(&env, cx).is_none());
    cx.update(|_, cx| {
        assert_eq!(
            cx.global::<LastToast>().0,
            "There are no workflow definitions"
        )
    });
}

#[gpui::test]
async fn nothing_is_read_or_written_while_off(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, false).await;
    let before = tree(&env.profile);
    let fake_before = env.app_state.fs.as_fake().paths(true);
    for name in [
        "workflow::Validate",
        "workflow::New",
        "workflow::EditDefinition",
        "workflow::Run",
        "workflow::Pause",
        "workflow::Halt",
        "workflow::Resume",
        "workflow::RetryStep",
        "workflow::RetryStepFresh",
        "workflow::Restart",
        "workflow::Cancel",
        "workflow::Approve",
        "workflow::Revise",
        "workflow::CopyResumeCommand",
    ] {
        dispatch(name, cx);
        cx.update(|_, cx| assert_eq!(cx.global::<LastToast>().0, OFF_MESSAGE, "{name}"));
        assert!(active_editor(&env, cx).is_none(), "{name} opens nothing");
    }
    assert_eq!(tree(&env.profile), before, "nothing was written");
    assert_eq!(env.app_state.fs.as_fake().paths(true), fake_before);
}

#[gpui::test]
async fn no_engine_exists_while_off_and_one_is_made_on_demand_when_on(cx: &mut TestAppContext) {
    let (workspace, _, cx) = crate::actions_tests::workspace_with_actions(cx).await;
    let _ = workspace;
    cx.update(|_, cx| {
        assert!(cx.try_global::<WorkflowEngine>().is_none(), "none at init");
        assert!(WorkflowEngine::ensure(cx).is_none(), "none while off");
        assert!(cx.try_global::<WorkflowEngine>().is_none());
    });
    for name in [
        "workflow::Validate",
        "workflow::New",
        "workflow::EditDefinition",
    ] {
        dispatch(name, cx);
    }
    cx.update(|_, cx| {
        assert!(
            cx.try_global::<WorkflowEngine>().is_none(),
            "no command made one"
        )
    });

    set_enabled(cx, true);
    let roots = TempDir::new().unwrap();
    let data_dir = roots.path().join("data");
    let home = roots.path().join("home");
    cx.update(|_, cx| {
        cx.set_global(EngineRoots {
            data_dir: data_dir.clone(),
            home: home.clone(),
        });
        let engine = WorkflowEngine::ensure(cx).expect("an engine while on");
        let dirs = engine.dirs();
        let profile = dirs.profile.as_ref().unwrap();
        assert!(profile.ends_with(".bongos/workflows"));
        assert!(profile.starts_with(&home));
        let shipped = dirs.shipped.as_ref().unwrap();
        assert!(shipped.ends_with("workflows/shipped/0.0.0"));
        assert!(shipped.starts_with(&data_dir), "{shipped:?}");
        assert!(
            shipped.is_dir(),
            "the shipped files land in the temp data dir"
        );
        assert!(
            std::fs::read_dir(shipped).unwrap().next().is_some(),
            "shipped definitions were written"
        );
        assert!(dirs.project.is_none());
        assert!(cx.try_global::<WorkflowEngine>().is_some());
    });
}

// Clear finished, dump engine and approve with an edit.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bongos_workflow_model::definition::{FieldType, ItemType};
use serde_json::json;

use crate::WorkflowsSettings;
use crate::commands::{
    ENGINE_DUMP_FILE, LogsDir, apply_plan, approve_choices, finished_run, offer_removal,
    parse_field, plan_counts, plan_removal,
};
use crate::lifecycle::{ClearedRun, GateField, RunStatus};
use crate::worktrees::{Git, RunWorktree, TaskCheckout, create_run};

const TASK_BRANCH: &str = "task/add-login";

/// A real repository with a task worktree to branch the run worktrees from, and a git that reads
/// no configuration of the machine.
struct Repo {
    _temp: TempDir,
    git: Git,
    task: TaskCheckout,
}

fn plenty(_: &Path) -> Option<u64> {
    Some(u64::MAX)
}

async fn git_out(git: &Git, directory: &Path, args: &[&str]) -> String {
    git.run(directory, args).await.unwrap()
}

impl Repo {
    async fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let git = Git::new("git")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        let repository = root.join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        git_out(&git, &repository, &["init", "-q", "-b", "main"]).await;
        std::fs::write(repository.join("README.md"), "readme\n").unwrap();
        git_out(&git, &repository, &["add", "README.md"]).await;
        git_out(&git, &repository, &["commit", "-q", "-m", "initial"]).await;
        let task_worktree = repository.join(".bongos-worktrees").join("add-login");
        git_out(
            &git,
            &repository,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                TASK_BRANCH,
                &task_worktree.to_string_lossy(),
            ],
        )
        .await;
        Self {
            _temp: temp,
            git,
            task: TaskCheckout {
                slug: "add-login".to_string(),
                worktree: Some(task_worktree),
                branch: Some(TASK_BRANCH.to_string()),
            },
        }
    }

    async fn run(&self, run_id: &str) -> RunWorktree {
        let settings = WorkflowsSettings {
            enabled: true,
            min_free_disk_mb: 0,
            ..WorkflowsSettings::default()
        };
        create_run(&self.git, &self.task, run_id, 1, &settings, &plenty)
            .await
            .unwrap()
    }

    async fn branch_exists(&self, branch: &str) -> bool {
        self.git
            .run(
                self.task.worktree.as_deref().unwrap(),
                &[
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .await
            .is_ok()
    }
}

/// Days as an offset from the epoch: no clock and no calendar date.
fn day(days: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(days * 24 * 60 * 60)
}

fn cleared(id: &str, worktree: &RunWorktree, finished: SystemTime) -> ClearedRun {
    ClearedRun {
        id: id.to_string(),
        dir: PathBuf::new(),
        worktree: worktree.clone(),
        status: RunStatus::Succeeded,
        finished,
    }
}

const CLEAN: &str = "01890a5d-ac96-774b-bcce-b302aaaaaaaa";
const DIRTY: &str = "01890a5d-ac96-774b-bcce-b302bbbbbbbb";
const RECENT: &str = "01890a5d-ac96-774b-bcce-b302cccccccc";

#[gpui::test]
async fn the_removal_is_one_plan_that_skips_a_dirty_worktree_and_keeps_a_recent_run(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let repo = Repo::new().await;
    let clean = repo.run(CLEAN).await;
    let dirty = repo.run(DIRTY).await;
    std::fs::write(dirty.path.join("README.md"), "uncommitted\n").unwrap();
    let recent = repo.run(RECENT).await;
    let finished: Vec<_> = [
        cleared(CLEAN, &clean, day(1)),
        cleared(DIRTY, &dirty, day(1)),
        cleared(RECENT, &recent, day(99)),
    ]
    .into_iter()
    .map(finished_run)
    .collect();

    let plan = plan_removal(&repo.git, &finished, 14, day(100))
        .await
        .unwrap();
    let listed: Vec<&str> = plan.runs.iter().map(|run| run.run_id.as_str()).collect();
    assert_eq!(listed, vec![CLEAN, DIRTY], "the recent run is not offered");
    assert_eq!(plan_counts(&plan), (1, 1));
    assert!(plan.summary().contains("uncommitted changes"));
    // Planning removes nothing.
    assert!(clean.path.exists() && dirty.path.exists() && recent.path.exists());

    let report = apply_plan(&repo.git, &plan).await;
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0.path, dirty.path);
    assert!(!clean.run_dir.exists());
    assert!(!repo.branch_exists(&clean.branch).await);
    assert_eq!(
        std::fs::read_to_string(dirty.path.join("README.md")).unwrap(),
        "uncommitted\n"
    );
    assert!(repo.branch_exists(&dirty.branch).await);
    assert!(recent.path.exists());
    assert!(repo.branch_exists(&recent.branch).await);
}

#[gpui::test]
async fn a_removal_that_fails_is_reported_and_the_rest_go_on(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = Repo::new().await;
    let locked = repo.run(CLEAN).await;
    let other = repo.run(DIRTY).await;
    // Git refuses to remove a locked worktree.
    git_out(
        &repo.git,
        &locked.path,
        &["worktree", "lock", &locked.path.to_string_lossy()],
    )
    .await;
    let finished: Vec<_> = [
        cleared(CLEAN, &locked, day(1)),
        cleared(DIRTY, &other, day(1)),
    ]
    .into_iter()
    .map(finished_run)
    .collect();
    let plan = plan_removal(&repo.git, &finished, 14, day(100))
        .await
        .unwrap();
    assert_eq!(plan_counts(&plan), (2, 0));

    let report = apply_plan(&repo.git, &plan).await;
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0.path, locked.path);
    assert!(locked.path.exists());
    assert!(repo.branch_exists(&locked.branch).await);
    assert!(!other.run_dir.exists());
    assert!(!repo.branch_exists(&other.branch).await);
}

#[gpui::test]
async fn declining_the_question_removes_nothing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = Repo::new().await;
    let clean = repo.run(CLEAN).await;
    let other = repo.run(DIRTY).await;
    let finished: Vec<_> = [
        cleared(CLEAN, &clean, day(1)),
        cleared(DIRTY, &other, day(1)),
    ]
    .into_iter()
    .map(finished_run)
    .collect();
    let plan = plan_removal(&repo.git, &finished, 14, day(100))
        .await
        .unwrap();

    let (env, cx) = env(cx, true).await;
    env.workspace.update_in(cx, |workspace, window, cx| {
        offer_removal(vec![], Ok(plan), workspace, window, cx)
    });
    cx.run_until_parked();
    // One question for both runs.
    let asked = env.workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_modal::<bongos_minibuffer::ReadKeyView>(cx)
            .is_some()
    });
    assert!(asked, "the question is open");
    cx.simulate_keystrokes("n");
    cx.run_until_parked();
    let still_asked = env.workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_modal::<bongos_minibuffer::ReadKeyView>(cx)
            .is_some()
    });
    assert!(!still_asked, "one answer closes it");
    assert!(clean.path.exists() && other.path.exists());
    assert!(repo.branch_exists(&clean.branch).await);
    assert!(repo.branch_exists(&other.branch).await);
}

#[gpui::test]
async fn a_plan_with_nothing_to_remove_asks_nothing(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    env.workspace.update_in(cx, |workspace, window, cx| {
        offer_removal(vec![], Ok(Default::default()), workspace, window, cx)
    });
    cx.run_until_parked();
    let asked = env.workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_modal::<bongos_minibuffer::ReadKeyView>(cx)
            .is_some()
    });
    assert!(!asked);
    cx.update(|_, cx| assert_eq!(cx.global::<LastToast>().0, "No finished runs to clear"));
}

#[gpui::test]
async fn clear_finished_with_no_runs_says_so_and_writes_nothing(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, true).await;
    let before = tree(&env.profile);
    dispatch("workflow::ClearFinished", cx);
    cx.update(|_, cx| assert_eq!(cx.global::<LastToast>().0, "No finished runs to clear"));
    assert_eq!(tree(&env.profile), before);
}

#[gpui::test]
async fn dump_engine_writes_the_engine_file_in_the_logs_directory(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_env, cx) = env(cx, true).await;
    let logs = TempDir::new().unwrap();
    let dir = logs.path().join("logs");
    cx.update(|_, cx| cx.set_global(LogsDir(dir.clone())));
    dispatch("workflow::DumpEngine", cx);
    cx.executor().run_until_parked();

    let text = std::fs::read_to_string(dir.join(ENGINE_DUMP_FILE)).expect("the dump is written");
    assert!(text.contains("runs: 0"), "{text}");
    cx.update(|_, cx| {
        assert_eq!(
            cx.global::<LastToast>().0,
            format!("Wrote {}", dir.join(ENGINE_DUMP_FILE).display())
        )
    });
}

#[gpui::test]
async fn clear_finished_and_dump_engine_only_toast_while_off(cx: &mut TestAppContext) {
    let (env, cx) = env(cx, false).await;
    let logs = TempDir::new().unwrap();
    cx.update(|_, cx| cx.set_global(LogsDir(logs.path().join("logs"))));
    let before = tree(&env.profile);
    for name in ["workflow::ClearFinished", "workflow::DumpEngine"] {
        dispatch(name, cx);
        cx.update(|_, cx| assert_eq!(cx.global::<LastToast>().0, OFF_MESSAGE, "{name}"));
    }
    assert_eq!(tree(&env.profile), before);
    assert!(!logs.path().join("logs").exists(), "nothing is written");
}

#[test]
fn approve_offers_as_is_first_then_each_field_of_the_held_result() {
    let fields = vec![
        GateField {
            name: "summary".to_string(),
            field_type: FieldType::Text,
            value: json!("done"),
        },
        GateField {
            name: "files".to_string(),
            field_type: FieldType::List(ItemType::Path),
            value: json!(["a.rs", "b.rs"]),
        },
    ];
    assert_eq!(
        approve_choices(&fields),
        vec!["Approve as is", "summary: done", "files: a.rs, b.rs"]
    );
}

#[test]
fn an_edited_value_is_parsed_by_the_type_of_its_field() {
    let text = |type_, line| parse_field(&type_, line);
    assert_eq!(text(FieldType::Text, "hello"), Ok(json!("hello")));
    assert_eq!(text(FieldType::Path, "src/a.rs"), Ok(json!("src/a.rs")));
    assert_eq!(
        text(FieldType::Choice(vec!["ship".into()]), "ship"),
        Ok(json!("ship"))
    );
    assert_eq!(text(FieldType::Number, " 42 "), Ok(json!(42)));
    assert_eq!(text(FieldType::Number, "2.5"), Ok(json!(2.5)));
    assert_eq!(
        text(FieldType::Number, "many"),
        Err("'many' is not a number".to_string())
    );
    assert_eq!(
        text(FieldType::List(ItemType::Text), "a, b ,,c"),
        Ok(json!(["a", "b", "c"]))
    );
    assert_eq!(
        text(FieldType::List(ItemType::Number), "1, 2"),
        Ok(json!([1, 2]))
    );
    assert_eq!(
        text(FieldType::List(ItemType::Number), "1, x"),
        Err("'x' is not a number".to_string())
    );
}
