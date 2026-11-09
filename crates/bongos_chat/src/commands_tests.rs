//! The chat commands of part 1: where a new chat lives and runs (scope and `cwd`, the implement
//! refusal), the agents offered, new, open and resume, next and previous, close with its
//! mid-turn question, the never-prompted cleanup with each of its keep cases, and "Resume and
//! send". Integration tests run the fake CLIs in a real PTY (the harness of `runtime_tests`).

#![cfg(unix)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bongos_chat_model::{
    Agent, ChatRecord, LaunchKind, Layout, NamedBy, project_slug, read_record, record_path,
    write_record,
};
use bongos_work::Repository;
use bongos_work_model::store::{WorkTree, load};
use fs::RealFs;
use gpui::{Entity, Focusable as _, TestAppContext, UpdateGlobal as _, VisualTestContext};
use project::Project;
use settings::{SettingsStore, WorkRepositoryContent};
use tempfile::TempDir;
use workspace::notifications::NotificationId;
use workspace::{MultiWorkspace, Workspace};

use crate::commands::{ChatPlan, open_chat, resume_chat, runtime_dir_of, start_chat};
use crate::launch::prepare;
use crate::runtime_tests::{
    Harness, STARTUP, WITHIN, claude_caps, codex_caps, harness, wait_for, write,
};
use crate::{
    AgentCommand, ChatPanel, ChatRuntime, ChatState, ChatToast, Cleanup, CwdSource, HealthState,
    KeepReason, START_FIRST, agent_choices, cleanup_verdict, init_panel, list_types, resolve_scope,
};

// ---------------------------------------------------------------------------------------------
// Scope and working directory (pure, over a tree loaded from a temporary directory).

const PR_LEAF: &str = "kind = \"pr-ship\"";

fn task_toml(id: &str, goal: &str, kind: &str, extra: &str) -> String {
    format!(
        "schema = 1\nid = \"{id}\"\ngoal = \"{goal}\"\n{kind}\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n{extra}"
    )
}

/// A project `invoicing` (top task `top`, repositories `app` then `docs`) holding the leaf
/// `backfill` (`t1`, pr-ship, no worktree), the leaf `spec` (`t2`, pr-ship, with a worktree that
/// exists), the leaf `notes` (`t3`, a document), and the group `phase` (`g1`) with the leaf
/// `alpha` (`t4`). Returns the work root.
fn seed_tree(base: &Path) -> PathBuf {
    let root = base.join("work");
    let project = root.join("projects/invoicing");
    write(
        &project.join("project.toml"),
        "schema = 1\ntitle = \"Invoicing\"\nrepositories = [\"app\", \"docs\"]\n",
    );
    write(
        &project.join("task.toml"),
        &task_toml("top", "Invoicing", "kind = \"other\"", ""),
    );
    let tasks = project.join("tasks");
    write(
        &tasks.join("backfill/task.toml"),
        &task_toml("t1", "Backfill usage events", PR_LEAF, ""),
    );
    let worktree = base.join("worktrees/spec");
    std::fs::create_dir_all(&worktree).unwrap();
    write(
        &tasks.join("spec/task.toml"),
        &task_toml(
            "t2",
            "Proration spec",
            PR_LEAF,
            &format!("worktree = \"{}\"\n", worktree.display()),
        ),
    );
    write(
        &tasks.join("notes/task.toml"),
        &task_toml("t3", "Release notes", "kind = \"document\"", ""),
    );
    write(
        &tasks.join("phase/task.toml"),
        &task_toml("g1", "Phase two", PR_LEAF, ""),
    );
    write(
        &tasks.join("phase/tasks/alpha/task.toml"),
        &task_toml("t4", "Alpha", PR_LEAF, ""),
    );
    std::fs::create_dir_all(base.join("repos/app")).unwrap();
    std::fs::create_dir_all(base.join("repos/docs")).unwrap();
    root
}

fn repository_content(base: &Path, name: &str) -> WorkRepositoryContent {
    WorkRepositoryContent {
        name: name.to_string(),
        path: base.join("repos").join(name).to_string_lossy().into_owned(),
        setup_command: None,
        merge_is_ship: None,
        deployments: None,
        branch_template: None,
    }
}

fn repositories(base: &Path) -> Vec<Repository> {
    ["app", "docs"]
        .into_iter()
        .map(|name| Repository::from(&repository_content(base, name)))
        .collect()
}

fn tree_in(temp: &TempDir) -> (PathBuf, WorkTree) {
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let root = seed_tree(&base);
    let tree = load(&root);
    (base, tree)
}

#[test]
fn test_a_task_with_a_worktree_runs_there() {
    let temp = TempDir::new().unwrap();
    let (base, tree) = tree_in(&temp);
    let scope = resolve_scope(&tree, "t2", "implement", &repositories(&base), None).unwrap();
    assert_eq!(scope.cwd, base.join("worktrees/spec"));
    assert_eq!(scope.cwd_source, CwdSource::Worktree);
    assert_eq!(
        scope.scope_dir,
        base.join("work/projects/invoicing/tasks/spec")
    );
    assert!(!scope.project_chat);
}

#[test]
fn test_a_project_chat_runs_in_the_repository() {
    let temp = TempDir::new().unwrap();
    let (base, tree) = tree_in(&temp);
    let repositories = repositories(&base);
    let scope = resolve_scope(&tree, "top", "plan", &repositories, None).unwrap();
    assert!(scope.project_chat);
    assert_eq!(scope.scope_dir, base.join("work/projects/invoicing"));
    // The project lists two: the first.
    assert_eq!(scope.cwd, base.join("repos/app"));
    assert_eq!(scope.cwd_source, CwdSource::Repository("app".to_string()));
    // `chat: new with options` can name the other.
    let docs = resolve_scope(&tree, "top", "plan", &repositories, Some("docs")).unwrap();
    assert_eq!(docs.cwd, base.join("repos/docs"));
    // A repository whose checkout is not there leaves the project directory.
    std::fs::remove_dir_all(base.join("repos/app")).unwrap();
    let gone = resolve_scope(&tree, "top", "plan", &repositories, None).unwrap();
    assert_eq!(gone.cwd, base.join("work/projects/invoicing"));
    assert_eq!(gone.cwd_source, CwdSource::ScopeDir);
    // Without any repository setting, the same.
    let none = resolve_scope(&tree, "top", "implement", &[], None).unwrap();
    assert_eq!(none.cwd_source, CwdSource::ScopeDir);
}

#[test]
fn test_a_group_runs_in_its_directory() {
    let temp = TempDir::new().unwrap();
    let (base, tree) = tree_in(&temp);
    let group = base.join("work/projects/invoicing/tasks/phase");
    for chat_type in ["plan", "implement"] {
        let scope = resolve_scope(&tree, "g1", chat_type, &repositories(&base), None).unwrap();
        assert_eq!(scope.cwd, group, "{chat_type}");
        assert_eq!(scope.scope_dir, group);
        assert_eq!(scope.cwd_source, CwdSource::ScopeDir);
        assert!(!scope.project_chat);
    }
}

#[test]
fn test_implement_without_a_worktree_is_refused() {
    let temp = TempDir::new().unwrap();
    let (base, tree) = tree_in(&temp);
    let repositories = repositories(&base);
    assert_eq!(
        resolve_scope(&tree, "t1", "implement", &repositories, None),
        Err(START_FIRST.to_string())
    );
    assert_eq!(
        START_FIRST,
        "Start the task first (task: start, C-c w s); implement chats run in its worktree"
    );
    // Other types run in the task directory.
    let plan = resolve_scope(&tree, "t1", "plan", &repositories, None).unwrap();
    assert_eq!(
        plan.cwd,
        base.join("work/projects/invoicing/tasks/backfill")
    );
    // A document leaf is not a pull request: implement runs in its directory.
    let notes = resolve_scope(&tree, "t3", "implement", &repositories, None).unwrap();
    assert_eq!(notes.cwd_source, CwdSource::ScopeDir);
    // A recorded worktree that is gone is no worktree.
    std::fs::remove_dir_all(base.join("worktrees/spec")).unwrap();
    assert_eq!(
        resolve_scope(&tree, "t2", "implement", &repositories, None),
        Err(START_FIRST.to_string())
    );
    assert!(resolve_scope(&tree, "nope", "plan", &repositories, None).is_err());
}

#[test]
fn test_session_types_list_shipped_first() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("chat-types");
    bongos_chat_model::seed(&root).unwrap();
    write(
        &root.join("audit/type.toml"),
        "schema = 1\ndescription = \"Audit the code\"\n",
    );
    write(&root.join("skills/model-routing/SKILL.md"), "skill\n");
    write(&root.join("broken/type.toml"), "schema = \n");
    std::fs::create_dir_all(root.join("empty")).unwrap();
    let names: Vec<String> = list_types(&root)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        [
            "plan",
            "implement",
            "review",
            "thinking",
            "free",
            "audit",
            "broken"
        ]
    );
    let labels: Vec<String> = list_types(&root)
        .into_iter()
        .map(|(_, label)| label)
        .collect();
    assert!(labels[5].starts_with("audit: Audit the code"));
    assert_eq!(labels[6], "broken: type.toml has an error");
}

// ---------------------------------------------------------------------------------------------
// The never-prompted cleanup, verdict by verdict.

fn ended(agent: Agent, cwd: &Path) -> ChatRecord {
    ChatRecord::new(agent, "plan", cwd.to_path_buf())
}

#[test]
fn test_cleanup_deletes_only_when_sure() {
    let temp = TempDir::new().unwrap();
    let home = std::fs::canonicalize(temp.path()).unwrap();
    let cwd = home.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let never = || -> Option<String> { panic!("Claude chats never look for a Codex session") };
    let none = || None;
    let env = BTreeMap::new();

    // Claude, its projects directory there, no transcript: the sure case.
    let mut claude = ended(Agent::Claude, &cwd);
    claude.session_id = Some("8c1e4f8a-0000-4000-8000-000000000001".to_string());
    let projects = home.join(".claude/projects");
    // Unsure: Claude's storage is not where it is looked for.
    assert_eq!(
        cleanup_verdict(&claude, false, None, &home, &env, never),
        Cleanup::Keep(KeepReason::CannotCheck)
    );
    std::fs::create_dir_all(&projects).unwrap();
    assert_eq!(
        cleanup_verdict(&claude, false, None, &home, &env, never),
        Cleanup::Delete
    );
    // Unsure: the record says prompted.
    let mut prompted = claude.clone();
    prompted.prompted = true;
    assert_eq!(
        cleanup_verdict(&prompted, false, None, &home, &env, never),
        Cleanup::Keep(KeepReason::Prompted)
    );
    // Unsure: the runtime saw input.
    assert_eq!(
        cleanup_verdict(&claude, true, None, &home, &env, never),
        Cleanup::Keep(KeepReason::InputSeen)
    );
    // Unsure: no session id to look up.
    let mut no_id = claude.clone();
    no_id.session_id = None;
    assert_eq!(
        cleanup_verdict(&no_id, false, None, &home, &env, never),
        Cleanup::Keep(KeepReason::SessionUnknown)
    );
    // The live session id counts when the record has none yet.
    assert_eq!(
        cleanup_verdict(&no_id, false, Some("abc"), &home, &env, never),
        Cleanup::Delete
    );
    // Unsure: a transcript exists.
    let slug_dir = projects.join(project_slug(&cwd));
    write(
        &slug_dir.join("8c1e4f8a-0000-4000-8000-000000000001.jsonl"),
        "{}\n",
    );
    assert_eq!(
        cleanup_verdict(&claude, false, None, &home, &env, never),
        Cleanup::Keep(KeepReason::ConversationStored)
    );
    // Unsure: an agent this version does not know.
    let mut unknown = claude;
    unknown.agent = "gemini".to_string();
    assert_eq!(
        cleanup_verdict(&unknown, false, None, &home, &env, never),
        Cleanup::Keep(KeepReason::UnknownAgent)
    );

    // Codex: an id captured means a session exists.
    let mut codex = ended(Agent::Codex, &cwd);
    codex.session_id = Some("019a".to_string());
    assert_eq!(
        cleanup_verdict(&codex, false, None, &home, &env, none),
        Cleanup::Keep(KeepReason::ConversationStored)
    );
    codex.session_id = None;
    assert_eq!(
        cleanup_verdict(&codex, false, Some("019a"), &home, &env, none),
        Cleanup::Keep(KeepReason::ConversationStored)
    );
    // Unsure: Codex's sessions directory is not there.
    assert_eq!(
        cleanup_verdict(&codex, false, None, &home, &env, none),
        Cleanup::Keep(KeepReason::CannotCheck)
    );
    std::fs::create_dir_all(home.join(".codex/sessions")).unwrap();
    // Unsure: a last look finds a session.
    assert_eq!(
        cleanup_verdict(&codex, false, None, &home, &env, || Some(
            "019b".to_string()
        )),
        Cleanup::Keep(KeepReason::ConversationStored)
    );
    // Sure: no id captured and none found.
    assert_eq!(
        cleanup_verdict(&codex, false, None, &home, &env, none),
        Cleanup::Delete
    );

    // Unsure: the CLI's storage was moved by its environment, so `~/.claude` and `~/.codex` say
    // nothing about it.
    let moved = |name: &str| BTreeMap::from([(name.to_string(), "/elsewhere".to_string())]);
    assert_eq!(
        cleanup_verdict(&no_id, false, Some("abc"), &home, &env, never),
        Cleanup::Delete
    );
    assert_eq!(
        cleanup_verdict(
            &no_id,
            false,
            Some("abc"),
            &home,
            &moved("CLAUDE_CONFIG_DIR"),
            never
        ),
        Cleanup::Keep(KeepReason::CannotCheck)
    );
    assert_eq!(
        cleanup_verdict(&codex, false, None, &home, &moved("CODEX_HOME"), none),
        Cleanup::Keep(KeepReason::CannotCheck)
    );
    // The other CLI's variable changes nothing.
    assert_eq!(
        cleanup_verdict(
            &codex,
            false,
            None,
            &home,
            &moved("CLAUDE_CONFIG_DIR"),
            none
        ),
        Cleanup::Delete
    );
}

// ---------------------------------------------------------------------------------------------
// The agents offered.

#[gpui::test]
async fn test_a_missing_cli_is_not_offered(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let health = cx.update(|cx| crate::ChatHealth::global(cx).unwrap());
    wait_for(cx, "both CLIs probed", STARTUP, |cx| {
        health.read_with(cx, |health, _| {
            !health.is_probing()
                && health
                    .all()
                    .iter()
                    .all(|health| matches!(health.state, HealthState::Ready { .. }))
        })
    })
    .await;
    let offered = |last: Option<Agent>, cx: &mut TestAppContext| -> Vec<Agent> {
        health.read_with(cx, |health, _| {
            agent_choices(health.all(), last)
                .into_iter()
                .map(|(agent, _)| agent)
                .collect()
        })
    };
    assert_eq!(offered(None, cx), [Agent::Claude, Agent::Codex]);
    // The last agent comes first.
    assert_eq!(
        offered(Some(Agent::Codex), cx),
        [Agent::Codex, Agent::Claude]
    );
    let labels: Vec<String> = health.read_with(cx, |health, _| {
        agent_choices(health.all(), None)
            .into_iter()
            .map(|(_, label)| label)
            .collect()
    });
    assert!(labels[0].starts_with("claude: Claude Code"), "{labels:?}");

    // Codex's command is not there: it is not offered, even as the last agent.
    let missing = h.base.join("no-such-codex");
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |content| {
                let agents = content
                    .chats
                    .get_or_insert_default()
                    .agents
                    .get_or_insert_default();
                agents.codex.get_or_insert_default().command =
                    Some(missing.to_string_lossy().into_owned());
            });
        });
    });
    wait_for(cx, "codex not found", STARTUP, |cx| {
        health.read_with(cx, |health, _| {
            health.health(Agent::Codex).state == HealthState::NotFound
        })
    })
    .await;
    assert_eq!(offered(Some(Agent::Codex), cx), [Agent::Claude]);
}

// ---------------------------------------------------------------------------------------------
// The commands in a workspace.

/// A workspace with the `chat` commands and the panel, the harness's runtime as the global one,
/// and the extra tasks of [`seed_tree`] (beside the harness's own) in the work tree.
pub(super) async fn command_window<'a>(
    h: &Harness,
    cx: &'a mut TestAppContext,
) -> (Entity<Workspace>, &'a mut VisualTestContext) {
    cx.update(|cx| {
        editor::init(cx);
        ChatRuntime::set_global(h.runtime.clone(), cx);
    });
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let project = Project::test(fs, [h.scope.as_path()], cx).await;
    let (multi_workspace, cx) = cx.add_window_view(move |window, cx| {
        let multi_workspace = MultiWorkspace::test_new(project, window, cx);
        multi_workspace.workspace().update(cx, |workspace, cx| {
            crate::register(workspace);
            init_panel(workspace, window, cx);
        });
        multi_workspace
    });
    let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
    workspace.update_in(cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx)
    });
    cx.run_until_parked();
    (workspace, cx)
}

pub(super) fn dispatch(cx: &mut VisualTestContext, name: &str) {
    let action = cx.update(|_, cx| {
        cx.build_action(name, None)
            .unwrap_or_else(|_| panic!("{name} is registered"))
    });
    cx.update(|window, cx| window.dispatch_action(action, cx));
    cx.run_until_parked();
}

pub(super) fn has_modal(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.update_in(cx, |workspace, window, cx| {
        workspace.has_active_modal(window, cx)
    })
}

/// Waits for a modal (a picker or a prompt), types `text` into it and confirms.
pub(super) async fn answer(workspace: &Entity<Workspace>, text: &str, cx: &mut VisualTestContext) {
    for _ in 0..400 {
        cx.run_until_parked();
        if has_modal(workspace, cx) {
            break;
        }
        cx.executor().timer(Duration::from_millis(25)).await;
    }
    assert!(
        has_modal(workspace, cx),
        "no picker came for `{text}` (a toast is shown: {})",
        toast_shown(workspace, cx)
    );
    if !text.is_empty() {
        cx.simulate_input(text);
    }
    cx.run_until_parked();
    dispatch(cx, "menu::Confirm");
}

pub(super) fn toast_shown(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
    workspace.read_with(cx, |workspace, _| {
        workspace
            .notification_ids()
            .contains(&NotificationId::unique::<ChatToast>())
    })
}

pub(super) fn panel_of(
    workspace: &Entity<Workspace>,
    cx: &mut TestAppContext,
) -> Entity<ChatPanel> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.panel::<ChatPanel>(cx).expect("the chats panel")
    })
}

pub(super) fn shown(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Option<String> {
    let panel = panel_of(workspace, cx);
    panel.read_with(cx, |panel, _| panel.shown_chat().map(str::to_string))
}

/// The ids of the records in `scope`'s `chats/`.
pub(super) fn records_in(scope: &Path) -> Vec<ChatRecord> {
    bongos_chat_model::read_scope_chats(scope)
        .unwrap()
        .into_iter()
        .filter_map(|entry| entry.chat.ok())
        .collect()
}

/// Adds the tasks of [`seed_tree`] to the harness's tree as the project `billing`, sets
/// `work.repositories`, and waits for the work store to see them.
pub(super) async fn add_tasks(h: &Harness, cx: &mut TestAppContext) {
    let extra = h.base.join("extra");
    let root = seed_tree(&extra);
    // Move the extra project in beside the harness's own one.
    std::fs::rename(
        root.join("projects/invoicing"),
        h.root.join("projects/billing"),
    )
    .unwrap();
    // The harness's project already uses the id `top`, and `t1`: rename the moved ones.
    let billing = h.root.join("projects/billing");
    for (path, from, to) in [
        ("task.toml", "\"top\"", "\"b-top\""),
        ("tasks/backfill/task.toml", "\"t1\"", "\"b-t1\""),
    ] {
        let file = billing.join(path);
        let text = std::fs::read_to_string(&file).unwrap().replace(from, to);
        std::fs::write(&file, text).unwrap();
    }
    let repositories = ["app", "docs"].map(|name| repository_content(&extra, name));
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().repositories = Some(repositories.to_vec());
            });
        });
    });
    wait_for(cx, "the extra tasks in the tree", STARTUP, |cx| {
        h.work.read_with(cx, |work, _| {
            work.snapshot().is_some_and(|tree| {
                ["b-top", "t2", "g1", "t4"]
                    .iter()
                    .all(|id| tree.task_by_id(id).is_some())
            })
        })
    })
    .await;
}

pub(super) fn plan_for(
    task_id: &str,
    chat_type: &str,
    agent: Agent,
    cx: &mut TestAppContext,
) -> ChatPlan {
    let scope = cx.update(|cx| {
        let tree = bongos_work::WorkStore::global(cx)
            .unwrap()
            .read(cx)
            .snapshot()
            .unwrap();
        let repositories = <bongos_work::WorkSettings as settings::Settings>::get_global(cx)
            .repositories
            .clone();
        resolve_scope(&tree, task_id, chat_type, &repositories, None).unwrap()
    });
    ChatPlan {
        scope,
        agent,
        chat_type: chat_type.to_string(),
        name: None,
        model: None,
        layout: None,
    }
}

/// The directories the fake logged its starts in, in order.
fn logged_cwds(h: &Harness) -> Vec<String> {
    h.log_lines("claude")
        .into_iter()
        .filter_map(|line| line.strip_prefix("cwd ").map(str::to_string))
        .collect()
}

#[gpui::test]
async fn test_new_chats_run_in_the_worktree_the_repository_and_the_group(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
    add_tasks(&h, cx).await;
    let extra = h.base.join("extra");
    let projects = vec![h.project.clone()];
    let cases = [
        (
            "t2",
            "implement",
            extra.join("worktrees/spec"),
            "billing/tasks/spec",
        ),
        ("b-top", "plan", extra.join("repos/app"), "billing"),
        (
            "g1",
            "plan",
            h.root.join("projects/billing/tasks/phase"),
            "billing/tasks/phase",
        ),
    ];
    for (index, (task_id, chat_type, cwd, scope)) in cases.into_iter().enumerate() {
        let plan = plan_for(task_id, chat_type, Agent::Claude, cx);
        let start = cx.update(|cx| start_chat(plan, projects.clone(), cx));
        let id = start.await.unwrap();
        wait_for(cx, "the fake to log its directory", STARTUP, |_| {
            logged_cwds(&h).len() > index
        })
        .await;
        assert_eq!(logged_cwds(&h)[index], cwd.to_string_lossy(), "{task_id}");
        let scope_dir = h.root.join("projects").join(scope);
        let record = read_record(&record_path(&scope_dir, &id)).unwrap();
        assert_eq!(record.cwd, cwd);
        assert_eq!(record.chat_type, chat_type);
        assert_eq!(record.named_by, NamedBy::None);
        assert_eq!(h.state(&id, cx), ChatState::Running);
    }
    // The implement type's layout is dual, plan's chat-only.
    let spec = records_in(&h.root.join("projects/billing/tasks/spec"));
    assert_eq!(spec[0].layout, Layout::Dual);
    let group = records_in(&h.root.join("projects/billing/tasks/phase"));
    assert_eq!(group[0].layout, Layout::ChatOnly);
}

#[gpui::test]
async fn test_new_chat_through_its_pickers(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    // No task is focused: the task picker comes first.
    dispatch(cx, "chat::New");
    answer(&workspace, "Backfill", cx).await;
    answer(&workspace, "claude:", cx).await;
    answer(&workspace, "plan:", cx).await;
    wait_for(cx, "the chat in the panel", STARTUP, |cx| {
        shown(&workspace, cx).is_some()
    })
    .await;
    let id = shown(&workspace, cx).unwrap();
    let record = read_record(&record_path(&h.scope, &id)).unwrap();
    assert_eq!(record.agent, "claude");
    assert_eq!(record.chat_type, "plan");
    assert_eq!(
        record.cwd, h.scope,
        "a PR leaf without a worktree: plan runs in its directory"
    );
    // The dock opened; the terminal did not take focus.
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.right_dock().read(cx).is_open()
    }));
    let session = panel_of(&workspace, cx).read_with(cx, |panel, _| panel.session().unwrap());
    wait_for(cx, "the terminal view", STARTUP, |cx| {
        session.read_with(cx, |session, _| session.terminal_view().is_some())
    })
    .await;
    let view = session.read_with(cx, |session, _| session.terminal_view().cloned().unwrap());
    assert!(!view.update_in(cx, |view, window, cx| {
        view.focus_handle(cx).is_focused(window)
    }));
    assert!(!toast_shown(&workspace, cx));
    // The next `chat: new` offers this agent first.
    assert_eq!(
        h.runtime.read_with(cx, |runtime, _| runtime.last_agent()),
        Some(Agent::Claude)
    );
}

#[gpui::test]
async fn test_new_with_options_records_the_answers(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    dispatch(cx, "chat::NewWithOptions");
    answer(&workspace, "Backfill", cx).await;
    answer(&workspace, "codex:", cx).await;
    answer(&workspace, "review:", cx).await;
    answer(&workspace, "Usage audit", cx).await;
    answer(&workspace, "gpt-test", cx).await;
    answer(&workspace, "chat-only:", cx).await;
    wait_for(cx, "the chat in the panel", STARTUP, |cx| {
        shown(&workspace, cx).is_some()
    })
    .await;
    let id = shown(&workspace, cx).unwrap();
    let record = read_record(&record_path(&h.scope, &id)).unwrap();
    assert_eq!(record.name, "Usage audit");
    assert_eq!(record.named_by, NamedBy::Kyle);
    assert_eq!(record.model.as_deref(), Some("gpt-test"));
    assert_eq!(record.layout, Layout::ChatOnly);
    assert_eq!(record.agent, "codex");
}

#[gpui::test]
async fn test_implement_shortcut_without_a_worktree_is_refused(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    dispatch(cx, "chat::NewImplement");
    answer(&workspace, "Backfill", cx).await;
    assert!(toast_shown(&workspace, cx));
    assert!(!has_modal(&workspace, cx), "nothing more is asked");
    cx.executor().timer(Duration::from_millis(300)).await;
    cx.run_until_parked();
    assert!(records_in(&h.scope).is_empty());
    assert!(h.log_lines("claude").iter().all(|line| line != "start"));
}

#[gpui::test]
async fn test_resume_uses_the_stored_id_and_cwd(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let id = h.start(Agent::Claude, cx).await;
    // Something was said, so the close keeps it.
    h.type_line(&id, "hello", cx);
    h.wait_content(&id, "got hello", cx).await;
    let session = read_record(&record_path(&h.scope, &id))
        .unwrap()
        .session_id
        .unwrap();
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    // The chat is closed; the resume below runs in the recorded directory with the stored id.
    let starts = |h: &Harness| {
        h.log_lines("claude")
            .iter()
            .filter(|line| *line == "start")
            .count()
    };
    assert_eq!(starts(&h), 1);
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;

    dispatch(cx, "chat::Open");
    answer(&workspace, "implement", cx).await;
    wait_for(cx, "the second start", STARTUP, |_| starts(&h) == 2).await;
    let lines = h.log_lines("claude");
    let second = lines.iter().rposition(|line| line == "start").unwrap();
    let args: Vec<&str> = lines[second..]
        .iter()
        .filter_map(|line| line.strip_prefix("arg "))
        .collect();
    let resume = args.iter().position(|arg| *arg == "--resume").unwrap();
    assert_eq!(args[resume + 1], session);
    assert!(!args.contains(&"--session-id"), "{args:?}");
    assert_eq!(logged_cwds(&h).last().unwrap(), &h.cwd.to_string_lossy());
    wait_for(cx, "the resumed chat in the panel", STARTUP, |cx| {
        shown(&workspace, cx).as_deref() == Some(id.as_str())
    })
    .await;
    assert_eq!(h.state(&id, cx), ChatState::Running);

    // Opening the running chat shows it, with no new launch.
    let launches = h
        .runtime
        .read_with(cx, |runtime, _| runtime.stats().launches);
    let opened = workspace.update_in(cx, |workspace, window, cx| {
        open_chat(&id, workspace, window, cx)
    });
    opened.await.unwrap();
    assert_eq!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.stats().launches),
        launches
    );
}

#[gpui::test]
async fn test_resume_is_refused_when_the_directory_is_gone(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let request = h.request(Agent::Claude, "plan");
    let id = request.record.id.clone();
    let mut record = request.record.clone();
    record.cwd = h.base.join("gone");
    record.prompted = true;
    write_record(&request.record_path, &record).unwrap();
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    let projects = vec![h.project.clone()];
    let error = cx
        .update(|cx| resume_chat(&id, projects, cx))
        .await
        .unwrap_err();
    assert!(error.contains("is gone"), "{error}");
    assert_eq!(h.state(&id, cx), ChatState::Closed);
}

#[gpui::test]
async fn test_close_keeps_the_record_and_asks_mid_turn(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    let panel = panel_of(&workspace, cx);
    assert!(panel.update_in(cx, |panel, window, cx| panel.show_chat(&id, window, cx)));

    // Output just now: the close asks, and keeping it changes nothing.
    h.type_line(&id, "hello", cx);
    h.wait_content(&id, "got hello", cx).await;
    dispatch(cx, "chat::Close");
    assert!(has_modal(&workspace, cx), "a mid-turn close asks");
    answer(&workspace, "Keep", cx).await;
    assert!(crate::live_modes::is_open(h.state(&id, cx)));

    // Quiet for longer than a turn: it closes without asking.
    cx.executor()
        .timer(crate::MID_TURN + Duration::from_millis(300))
        .await;
    dispatch(cx, "chat::Close");
    assert!(!has_modal(&workspace, cx));
    assert_eq!(h.state(&id, cx), ChatState::Closed);
    cx.executor().timer(Duration::from_millis(300)).await;
    cx.run_until_parked();
    let record = read_record(&record_path(&h.scope, &id)).unwrap();
    assert!(record.prompted);
    assert!(!record.archived);
    assert_eq!(shown(&workspace, cx).as_deref(), Some(id.as_str()));
    assert!(!toast_shown(&workspace, cx));

    // Closing a closed chat says so.
    dispatch(cx, "chat::Close");
    assert!(toast_shown(&workspace, cx));
}

#[gpui::test]
async fn test_close_mid_turn_closes_on_yes(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    let panel = panel_of(&workspace, cx);
    assert!(panel.update_in(cx, |panel, window, cx| panel.show_chat(&id, window, cx)));
    h.type_line(&id, "hello", cx);
    h.wait_content(&id, "got hello", cx).await;
    dispatch(cx, "chat::Close");
    answer(&workspace, "Close", cx).await;
    assert_eq!(h.state(&id, cx), ChatState::Closed);
    assert!(record_path(&h.scope, &id).exists());
}

/// Starts an unprompted Claude chat, with Claude's projects directory there when `projects`.
async fn unprompted_claude(h: &Harness, projects: bool, cx: &mut TestAppContext) -> String {
    if projects {
        std::fs::create_dir_all(h.home.join(".claude/projects")).unwrap();
    }
    h.start(Agent::Claude, cx).await
}

async fn settle(cx: &mut TestAppContext) {
    for _ in 0..10 {
        cx.run_until_parked();
        cx.executor().timer(Duration::from_millis(50)).await;
    }
}

#[gpui::test]
async fn test_cleanup_deletes_a_never_prompted_chat_on_close(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = unprompted_claude(&h, true, cx).await;
    assert!(h.runtime_root.join(&id).is_dir());
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    wait_for(cx, "the record to go", STARTUP, |_| {
        !record_path(&h.scope, &id).exists()
    })
    .await;
    wait_for(cx, "the launch files to go", STARTUP, |_| {
        !h.runtime_root.join(&id).exists()
    })
    .await;
    assert_eq!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.stats().cleanups),
        1
    );
}

#[gpui::test]
async fn test_cleanup_deletes_a_never_prompted_chat_on_exit(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = unprompted_claude(&h, true, cx).await;
    assert!(h.runtime.update(cx, |runtime, cx| runtime.kill(&id, cx)));
    wait_for(cx, "the record to go", STARTUP, |_| {
        !record_path(&h.scope, &id).exists()
    })
    .await;
    // The later close finds nothing more to do.
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    settle(cx).await;
    assert_eq!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.stats().cleanups),
        1
    );
}

#[gpui::test]
async fn test_cleanup_keeps_a_chat_with_input(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = unprompted_claude(&h, true, cx).await;
    h.type_line(&id, "hello", cx);
    h.wait_content(&id, "got hello", cx).await;
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    settle(cx).await;
    assert!(read_record(&record_path(&h.scope, &id)).unwrap().prompted);
    assert_eq!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.stats().cleanups),
        0
    );
}

#[gpui::test]
async fn test_cleanup_keeps_a_chat_with_a_transcript(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = unprompted_claude(&h, true, cx).await;
    let session = read_record(&record_path(&h.scope, &id))
        .unwrap()
        .session_id
        .unwrap();
    write(
        &h.home
            .join(".claude/projects")
            .join(project_slug(&h.cwd))
            .join(format!("{session}.jsonl")),
        "{}\n",
    );
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    settle(cx).await;
    assert!(record_path(&h.scope, &id).exists());
    assert!(h.runtime_root.join(&id).exists());
}

#[gpui::test]
async fn test_cleanup_keeps_a_chat_it_cannot_check(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    // No `~/.claude/projects`: unsure.
    let id = unprompted_claude(&h, false, cx).await;
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    settle(cx).await;
    assert!(record_path(&h.scope, &id).exists());
}

#[gpui::test]
async fn test_cleanup_keeps_a_chat_prompted_before(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    std::fs::create_dir_all(h.home.join(".claude/projects")).unwrap();
    // A chat prompted in an earlier run, resumed and closed again without input.
    let mut request = h.request(Agent::Claude, "plan");
    request.record.prompted = true;
    request.record.session_id = Some("8c1e4f8a-0000-4000-8000-000000000002".to_string());
    write_record(&request.record_path, &request.record).unwrap();
    request.kind = bongos_chat_model::LaunchKind::Resume;
    let id = request.record.id.clone();
    h.launch(request, &h.project, cx).await.unwrap();
    h.wait_content(&id, "ready", cx).await;
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    settle(cx).await;
    assert!(record_path(&h.scope, &id).exists());
}

#[gpui::test]
async fn test_cleanup_keeps_a_codex_chat_with_a_session(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    std::fs::create_dir_all(h.home.join(".codex/sessions")).unwrap();
    let id = h.start(Agent::Codex, cx).await;
    wait_for(cx, "the captured id", STARTUP, |_| {
        read_record(&record_path(&h.scope, &id))
            .unwrap()
            .session_id
            .is_some()
    })
    .await;
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    settle(cx).await;
    assert!(record_path(&h.scope, &id).exists());
}

#[gpui::test]
async fn test_next_and_previous_cycle_running_chats(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let first = h.start(Agent::Claude, cx).await;
    let second = h.start(Agent::Codex, cx).await;
    // A closed chat is not cycled through.
    let closed = h.request(Agent::Claude, "plan").record.id;
    for id in [&first, &second, &closed] {
        wait_for(cx, "the chat in the index", STARTUP, |cx| {
            h.store.read_with(cx, |store, _| store.find(id).is_some())
        })
        .await;
    }
    // The list order: most recently active first, ties by id.
    let order = cx.update(|_, cx| {
        let runtime = ChatRuntime::global(cx).unwrap();
        let store = runtime.read(cx).store().read(cx);
        let mut chats: Vec<ChatRecord> = [&first, &second]
            .iter()
            .map(|id| store.find(id).unwrap().entry.record().unwrap().clone())
            .collect();
        chats.sort_by(|a, b| {
            bongos_chat_model::timestamp_utc(&b.last_active)
                .cmp(&bongos_chat_model::timestamp_utc(&a.last_active))
                .then_with(|| a.id.cmp(&b.id))
        });
        chats.into_iter().map(|chat| chat.id).collect::<Vec<_>>()
    });

    dispatch(cx, "chat::Next");
    assert_eq!(shown(&workspace, cx).as_ref(), Some(&order[0]));
    dispatch(cx, "chat::Next");
    assert_eq!(shown(&workspace, cx).as_ref(), Some(&order[1]));
    dispatch(cx, "chat::Next");
    assert_eq!(shown(&workspace, cx).as_ref(), Some(&order[0]));
    dispatch(cx, "chat::Previous");
    assert_eq!(shown(&workspace, cx).as_ref(), Some(&order[1]));
    assert!(!toast_shown(&workspace, cx));
    assert_ne!(shown(&workspace, cx).as_ref(), Some(&closed));
}

#[gpui::test]
async fn test_resume_and_send(cx: &mut TestAppContext) {
    let input_dir = TempDir::new().unwrap();
    let input = input_dir.path().join("input");
    let input_text = input.to_string_lossy().into_owned();
    let h = harness(cx, true, 60, &[("FAKE_CLI_INPUT", &input_text)]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let id = h.start(Agent::Claude, cx).await;
    h.type_line(&id, "hello", cx);
    h.wait_content(&id, "got hello", cx).await;
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    std::fs::write(&input, "").unwrap();

    let name = read_record(&record_path(&h.scope, &id)).unwrap().name;
    workspace.update_in(cx, |workspace, window, cx| {
        crate::commands::offer_resume_and_send(
            id.clone(),
            name,
            "Next step".to_string(),
            crate::SendOptions { submit: true },
            Box::new(|_, _, _| {}),
            workspace,
            window,
            cx,
        )
    });
    answer(&workspace, "Resume", cx).await;
    let expected: String = b"\x1b[200~Next step\x1b[201~\r"
        .iter()
        .map(|byte| format!("{byte:03o} "))
        .collect();
    wait_for(cx, "the paste and Enter after the resume", STARTUP, |_| {
        std::fs::read_to_string(&input).unwrap_or_default() == expected
    })
    .await;
    assert!(crate::live_modes::is_open(h.state(&id, cx)));
    assert!(!toast_shown(&workspace, cx));
}

#[gpui::test]
async fn test_resume_and_send_passes_the_options_through(cx: &mut TestAppContext) {
    let input_dir = TempDir::new().unwrap();
    let input = input_dir.path().join("input");
    let input_text = input.to_string_lossy().into_owned();
    let h = harness(cx, true, 60, &[("FAKE_CLI_INPUT", &input_text)]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let id = h.start(Agent::Claude, cx).await;
    h.type_line(&id, "hello", cx);
    h.wait_content(&id, "got hello", cx).await;
    assert!(h.runtime.update(cx, |runtime, cx| runtime.close(&id, cx)));
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    std::fs::write(&input, "").unwrap();

    let outcome = Rc::new(RefCell::new(None));
    let name = read_record(&record_path(&h.scope, &id)).unwrap().name;
    workspace.update_in(cx, |workspace, window, cx| {
        let outcome = outcome.clone();
        crate::offer_resume_and_send(
            id.clone(),
            name,
            "Next step".to_string(),
            crate::SendOptions { submit: false },
            Box::new(move |result, _, _| *outcome.borrow_mut() = Some(result)),
            workspace,
            window,
            cx,
        )
    });
    answer(&workspace, "Resume", cx).await;
    wait_for(cx, "the outcome of the send", STARTUP, |_| {
        outcome.borrow().is_some()
    })
    .await;
    let sent = outcome.borrow_mut().take().unwrap().unwrap();
    assert!(!sent.submitted);
    let expected: String = b"\x1b[200~Next step\x1b[201~"
        .iter()
        .map(|byte| format!("{byte:03o} "))
        .collect();
    wait_for(cx, "the paste after the resume", STARTUP, |_| {
        std::fs::read_to_string(&input).unwrap_or_default() == expected
    })
    .await;
    // No Enter follows.
    cx.executor().timer(Duration::from_millis(300)).await;
    assert_eq!(std::fs::read_to_string(&input).unwrap(), expected);
    assert!(crate::live_modes::is_open(h.state(&id, cx)));
    assert!(!toast_shown(&workspace, cx));
}

#[gpui::test]
async fn test_start_chat_on_a_task_that_is_not_focused(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    // No task is focused, and no task picker is asked: the agent and the type are.
    workspace.update_in(cx, |workspace, window, cx| {
        crate::start_chat_on_task("t1".to_string(), workspace, window, cx)
    });
    answer(&workspace, "claude:", cx).await;
    answer(&workspace, "plan:", cx).await;
    wait_for(cx, "the chat in the panel", STARTUP, |cx| {
        shown(&workspace, cx).is_some()
    })
    .await;
    let id = shown(&workspace, cx).unwrap();
    let record = read_record(&record_path(&h.scope, &id)).unwrap();
    assert_eq!(record.chat_type, "plan");
    assert_eq!(h.state(&id, cx), ChatState::Running);
    assert!(!toast_shown(&workspace, cx));
}

#[gpui::test]
async fn test_start_chat_on_task_of_type_does_not_ask_the_type(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    // Nothing is focused, and no task or type is asked; only the agent is.
    workspace.update_in(cx, |workspace, window, cx| {
        crate::start_chat_on_task_of_type("t1".to_string(), "plan", workspace, window, cx)
    });
    answer(&workspace, "claude:", cx).await;
    wait_for(cx, "the chat in the panel", STARTUP, |cx| {
        shown(&workspace, cx).is_some()
    })
    .await;
    assert!(!has_modal(&workspace, cx), "the type was not asked");
    let id = shown(&workspace, cx).unwrap();
    let record = read_record(&record_path(&h.scope, &id)).unwrap();
    assert_eq!(record.chat_type, "plan");
    assert_eq!(record.agent, "claude");
    assert!(!toast_shown(&workspace, cx));
}

/// The context extension the tests install: records what it was asked and answers with `section`.
struct TestExtension {
    asked: Rc<RefCell<Vec<(String, Option<String>)>>>,
    section: Option<&'static str>,
}

impl crate::ContextExtension for TestExtension {
    fn section(&self, chat_type: &str, task_id: Option<&str>, _: &gpui::App) -> Option<String> {
        self.asked
            .borrow_mut()
            .push((chat_type.to_string(), task_id.map(str::to_string)));
        self.section.map(str::to_string)
    }
}

#[gpui::test]
async fn test_a_new_chat_gets_the_context_extension_section(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
    add_tasks(&h, cx).await;
    let projects = vec![h.project.clone()];
    let context_of = |id: &str| {
        std::fs::read_to_string(h.runtime_root.join(id).join(crate::CONTEXT_FILE)).unwrap()
    };

    // No extension: the context is the plain one.
    let plan = plan_for("t1", "plan", Agent::Claude, cx);
    let id = cx
        .update(|cx| start_chat(plan, projects.clone(), cx))
        .await
        .unwrap();
    let plain = context_of(&id);

    // An extension with nothing to say changes nothing.
    let asked = Rc::new(RefCell::new(Vec::new()));
    cx.update(|cx| {
        crate::set_context_extension(
            TestExtension {
                asked: asked.clone(),
                section: None,
            },
            cx,
        )
    });
    let plan = plan_for("t1", "plan", Agent::Claude, cx);
    let id = cx
        .update(|cx| start_chat(plan, projects.clone(), cx))
        .await
        .unwrap();
    assert_eq!(context_of(&id), plain);
    assert_eq!(
        *asked.borrow(),
        [("plan".to_string(), Some("t1".to_string()))]
    );

    // A section ends context.md, and codex-instructions.md for Codex.
    let section = "## Task builder\nPlan with proposals.";
    cx.update(|cx| {
        crate::set_context_extension(
            TestExtension {
                asked: asked.clone(),
                section: Some(section),
            },
            cx,
        )
    });
    for agent in [Agent::Claude, Agent::Codex] {
        let plan = plan_for("t1", "plan", agent, cx);
        let id = cx
            .update(|cx| start_chat(plan, projects.clone(), cx))
            .await
            .unwrap();
        assert_eq!(context_of(&id), format!("{plain}\n{section}\n"));
        if agent == Agent::Codex {
            let codex = std::fs::read_to_string(
                h.runtime_root
                    .join(&id)
                    .join(crate::CODEX_INSTRUCTIONS_FILE),
            )
            .unwrap();
            assert!(codex.ends_with(&format!("\n{section}\n")), "{codex}");
        }
    }
}

/// Every command of this step, dispatched by name while chats are off, only says so.
#[gpui::test]
async fn test_the_commands_dispatch_by_name_and_say_when_off(cx: &mut TestAppContext) {
    let h = harness(cx, false, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    for name in [
        "chat::New",
        "chat::NewWithOptions",
        "chat::NewPlan",
        "chat::NewImplement",
        "chat::NewReview",
        "chat::NewThinking",
        "chat::NewFree",
        "chat::Open",
        "chat::OpenAny",
        "chat::Next",
        "chat::Previous",
        "chat::Close",
        "chat::Fork",
        "chat::ForkToTask",
        "chat::ForkToTaskAs",
    ] {
        workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));
        dispatch(cx, name);
        assert!(toast_shown(&workspace, cx), "{name}");
        assert!(!has_modal(&workspace, cx), "{name}");
    }
    cx.executor().timer(WITHIN).await;
    assert!(h.log_lines("claude").is_empty());
    assert!(h.log_lines("codex").is_empty());
}

#[gpui::test]
async fn test_open_any_lists_every_task_and_open_says_when_none(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    dispatch(cx, "chat::Open");
    assert!(toast_shown(&workspace, cx), "no chats yet");
    assert!(!has_modal(&workspace, cx));
    workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));

    let id = h.start(Agent::Claude, cx).await;
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    dispatch(cx, "chat::OpenAny");
    answer(&workspace, "Backfill", cx).await;
    wait_for(cx, "the chat in the panel", STARTUP, |cx| {
        shown(&workspace, cx).as_deref() == Some(id.as_str())
    })
    .await;
}

#[test]
fn test_a_launch_directory_is_named_only_by_one_plain_component() {
    let root = Path::new("/chats/runtime");
    assert_eq!(runtime_dir_of(root, "abc-123"), Some(root.join("abc-123")));
    for bad in ["..", "a/b", ".", "", "foo/", "/abs", "../x", "x/.."] {
        assert_eq!(runtime_dir_of(root, bad), None, "{bad:?}");
    }
}

/// The session id a launch of `kind` gives a record that has none.
fn given_session(
    h: &Harness,
    agent: Agent,
    kind: LaunchKind,
    session: Option<&str>,
) -> Option<String> {
    let mut request = h.request(agent, "plan");
    request.kind = kind;
    request.record.session_id = session.map(str::to_string);
    let (command, caps) = match agent {
        Agent::Claude => ("claude", claude_caps()),
        Agent::Codex => ("codex", codex_caps()),
    };
    let command = AgentCommand {
        command: command.to_string(),
        extra_args: Vec::new(),
    };
    prepare(&request, &h.runtime_root, &h.home, &command, &caps)
        .unwrap()
        .record
        .session_id
}

#[gpui::test]
async fn test_only_a_new_or_forked_claude_chat_is_given_a_session_id(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let source = "8c1e4f8a-0000-4000-8000-000000000010";
    let fork = |native| LaunchKind::Fork {
        source_session: source.to_string(),
        native,
    };
    let is_uuid = |id: &Option<String>| {
        id.as_deref()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
    };
    // A new chat and a fork (native or handoff) choose their own id.
    assert!(is_uuid(&given_session(
        &h,
        Agent::Claude,
        LaunchKind::New,
        None
    )));
    for native in [true, false] {
        let own = given_session(&h, Agent::Claude, fork(native), None);
        assert!(is_uuid(&own));
        assert_ne!(
            own.as_deref(),
            Some(source),
            "a fork never takes the source's id"
        );
    }
    // An id already there stays, whatever the kind.
    let kept = Some("8c1e4f8a-0000-4000-8000-000000000011");
    for kind in [LaunchKind::New, LaunchKind::Resume, fork(true)] {
        assert_eq!(
            given_session(&h, Agent::Claude, kind, kept).as_deref(),
            kept
        );
    }
    // A resume with none stored never makes one up.
    assert_eq!(
        given_session(&h, Agent::Claude, LaunchKind::Resume, None),
        None
    );
    // Codex reports its id in its own files: none is chosen.
    for kind in [LaunchKind::New, LaunchKind::Resume, fork(true), fork(false)] {
        assert_eq!(given_session(&h, Agent::Codex, kind, None), None);
    }
}

#[gpui::test]
async fn test_a_refused_launch_removes_the_record_only_when_sure_nothing_was_said(
    cx: &mut TestAppContext,
) {
    let h = harness(cx, true, 60, &[]).await;
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
    std::fs::create_dir_all(h.home.join(".codex/sessions")).unwrap();
    std::fs::create_dir_all(h.home.join(".claude/projects")).unwrap();
    wait_for(cx, "the task in the tree", STARTUP, |cx| {
        h.work.read_with(cx, |work, _| {
            work.snapshot()
                .is_some_and(|tree| tree.task_by_id("t1").is_some())
        })
    })
    .await;
    // Both CLIs are gone, so every launch is refused.
    let missing = h.base.join("no-such-cli").to_string_lossy().into_owned();
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |content| {
                let agents = content
                    .chats
                    .get_or_insert_default()
                    .agents
                    .get_or_insert_default();
                agents.claude.get_or_insert_default().command = Some(missing.clone());
                agents.codex.get_or_insert_default().command = Some(missing.clone());
            });
        });
    });
    let health = cx.update(|cx| crate::ChatHealth::global(cx).unwrap());
    wait_for(cx, "both CLIs not found", STARTUP, |cx| {
        health.read_with(cx, |health, _| {
            health
                .all()
                .iter()
                .all(|health| health.state == HealthState::NotFound)
        })
    })
    .await;
    let projects = vec![h.project.clone()];

    // Codex: no session id was captured and none is found; the record that never ran is removed.
    let plan = plan_for("t1", "plan", Agent::Codex, cx);
    let started = cx.update(|cx| start_chat(plan, projects.clone(), cx)).await;
    assert!(started.is_err());
    assert!(records_in(&h.scope).is_empty());
    assert!(
        !h.runtime_root.exists() || std::fs::read_dir(&h.runtime_root).unwrap().next().is_none()
    );

    // Claude: the record has no session id to look a transcript up by, so it is unsure and kept.
    let plan = plan_for("t1", "plan", Agent::Claude, cx);
    let started = cx.update(|cx| start_chat(plan, projects, cx)).await;
    assert!(started.is_err());
    let kept = records_in(&h.scope);
    assert_eq!(kept.len(), 1);
    assert!(!kept[0].prompted);
}
