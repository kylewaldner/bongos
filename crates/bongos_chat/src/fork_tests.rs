//! Fork and fork to task: native and handoff forks against the fake CLIs, the record a fork writes
//! (parent, label, prompts file, directory), the task's own artifact list, the source left
//! byte for byte as it was, the commands in a workspace and the offer when a resume directory is
//! gone.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use bongos_chat_model::{
    Agent, AgentAdapter as _, ChatRecord, CodexAdapter, Fork, Layout, read_record, record_path,
    write_record,
};
use gpui::{Focusable as _, TestAppContext};

use crate::commands_tests::{
    add_tasks, answer, command_window, dispatch, has_modal, panel_of, records_in, shown,
    toast_shown,
};
use crate::fork::{ForkRequest, ForkTarget, Forked, fork_chat, fork_label};
use crate::runtime_tests::{Harness, STARTUP, claude_caps, codex_caps, harness, wait_for, write};
use crate::{ChatRuntime, ChatState, HealthState};

const SESSION: &str = "8c1e4f8a-0000-4000-8000-0000000000aa";
const CODEX_SESSION: &str = "019a1b2c-0000-7000-8000-0000000000bb";

/// What the test task keeps from the agent; none of it may reach a handoff.
const PRIVATE: [&str; 4] = [
    "secret-root.md",
    "salary-review.md",
    "Root-level compensation numbers",
    "Salary review notes",
];

/// A harness whose runtime is the global one, with the test task in the Work tree.
async fn ready(cx: &mut TestAppContext) -> Harness {
    let h = harness(cx, true, 60, &[]).await;
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
    wait_for(cx, "the task in the tree", STARTUP, |cx| {
        h.work.read_with(cx, |work, _| {
            work.snapshot()
                .is_some_and(|tree| tree.task_by_id("t1").is_some())
        })
    })
    .await;
    h
}

/// A closed chat on the test task, written to disk and waited for in the index. It was prompted.
async fn source(
    h: &Harness,
    agent: Agent,
    cx: &mut TestAppContext,
    change: impl FnOnce(&mut ChatRecord),
) -> ChatRecord {
    let mut record = ChatRecord::new(agent, "plan", h.cwd.clone());
    record.name = "Usage audit".to_string();
    record.prompted = true;
    record.session_id = Some(
        match agent {
            Agent::Claude => SESSION,
            Agent::Codex => CODEX_SESSION,
        }
        .to_string(),
    );
    change(&mut record);
    let path = record_path(&h.scope, &record.id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_record(&path, &record).unwrap();
    wait_for(cx, "the source in the index", STARTUP, |cx| {
        h.store
            .read_with(cx, |store, _| store.find(&record.id).is_some())
    })
    .await;
    record
}

fn request(source: &ChatRecord, target: ForkTarget) -> ForkRequest {
    ForkRequest {
        source_id: source.id.clone(),
        target,
        force_handoff: false,
        caps: Some(match source.known_agent().unwrap() {
            Agent::Claude => claude_caps(),
            Agent::Codex => codex_caps(),
        }),
    }
}

async fn fork(
    h: &Harness,
    request: ForkRequest,
    cx: &mut TestAppContext,
) -> Result<Forked, String> {
    let projects = vec![h.project.clone()];
    cx.update(|cx| fork_chat(request, projects, cx)).await
}

/// The arguments of the fake's most recent start.
fn last_args(h: &Harness, name: &str) -> Vec<String> {
    let lines = h.log_lines(name);
    let start = lines.iter().rposition(|line| line == "start").unwrap();
    lines[start..]
        .iter()
        .filter_map(|line| line.strip_prefix("arg "))
        .map(str::to_string)
        .collect()
}

fn starts(h: &Harness, name: &str) -> usize {
    h.log_lines(name)
        .iter()
        .filter(|line| *line == "start")
        .count()
}

fn record_of(scope: &Path, id: &str) -> ChatRecord {
    read_record(&record_path(scope, id)).unwrap()
}

fn runtime_file(h: &Harness, id: &str, name: &str) -> String {
    std::fs::read_to_string(h.runtime_root.join(id).join(name)).unwrap()
}

fn spec_scope(h: &Harness) -> PathBuf {
    h.root.join("projects/billing/tasks/spec")
}

#[gpui::test]
async fn test_a_fork_leaves_the_source_record_byte_identical(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let source = source(&h, Agent::Claude, cx, |_| {}).await;
    let path = record_path(&h.scope, &source.id);
    let before = std::fs::read(&path).unwrap();
    let siblings = records_in(&h.scope).len();

    let forked = fork(&h, request(&source, ForkTarget::SameScope), cx)
        .await
        .unwrap();
    wait_for(cx, "the fork to start", STARTUP, |_| {
        starts(&h, "claude") == 1
    })
    .await;
    // Everything a fork writes is its own: let any late write land, then compare.
    for _ in 0..10 {
        cx.run_until_parked();
        cx.executor()
            .timer(std::time::Duration::from_millis(50))
            .await;
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(records_in(&h.scope).len(), siblings + 1);
    assert_ne!(forked.chat_id, source.id);
    assert!(h.runtime_root.join(&forked.chat_id).is_dir());
    assert!(!h.runtime_root.join(&source.id).exists());
}

#[gpui::test]
async fn test_claude_forks_natively_with_a_session_of_its_own(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let source = source(&h, Agent::Claude, cx, |_| {}).await;
    let forked = fork(&h, request(&source, ForkTarget::SameScope), cx)
        .await
        .unwrap();
    assert_eq!(forked.fork, Fork::Native);
    wait_for(cx, "the fork to start", STARTUP, |_| {
        starts(&h, "claude") == 1
    })
    .await;
    let record = record_of(&h.scope, &forked.chat_id);
    assert_eq!(record.fork, Some(Fork::Native));
    assert_eq!(record.parent.as_deref(), Some(source.id.as_str()));
    let own = record.session_id.unwrap();
    assert_ne!(own, SESSION);

    let args = last_args(&h, "claude");
    let resume = args.iter().position(|arg| arg == "--resume").unwrap();
    assert_eq!(args[resume + 1], SESSION);
    assert_eq!(args[resume + 2], "--fork-session");
    let id = args.iter().position(|arg| arg == "--session-id").unwrap();
    assert_eq!(args[id + 1], own);
    assert_eq!(h.state(&forked.chat_id, cx), ChatState::Running);
    assert!(forked.toast().contains("forked from Usage audit"));
}

#[gpui::test]
async fn test_a_same_scope_fork_copies_the_prompts_file_and_the_directory(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let prompts = h.base.join("notes/plan-prompts.txt");
    let source = source(&h, Agent::Claude, cx, |record| {
        record.prompts_file = Some(prompts.clone());
        record.model = Some("model-x".to_string());
        record.layout = Layout::ChatOnly;
    })
    .await;
    let forked = fork(&h, request(&source, ForkTarget::SameScope), cx)
        .await
        .unwrap();
    let record = record_of(&h.scope, &forked.chat_id);
    assert_eq!(record.prompts_file, Some(prompts));
    assert_eq!(record.cwd, h.cwd);
    assert_eq!(record.chat_type, "plan");
    assert_eq!(record.model.as_deref(), Some("model-x"));
    // The prompts file is only referenced: nothing was created or copied there.
    assert!(!h.base.join("notes").exists());
    // The source never had a record of its own written to.
    assert_eq!(record_of(&h.scope, &source.id), source);
}

#[gpui::test]
async fn test_a_fork_into_a_task_lands_in_its_chats_with_its_worktree(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    add_tasks(&h, cx).await;
    let prompts = h.base.join("elsewhere/prompts.txt");
    let source = source(&h, Agent::Claude, cx, |record| {
        record.prompts_file = Some(prompts);
        record.model = Some("model-x".to_string());
    })
    .await;
    let target = ForkTarget::Task {
        task_id: "t2".to_string(),
        chat_type: None,
    };
    let forked = fork(&h, request(&source, target), cx).await.unwrap();

    let records = records_in(&spec_scope(&h));
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.id, forked.chat_id);
    assert_eq!(record.cwd, h.base.join("extra/worktrees/spec"));
    assert_eq!(
        record.prompts_file, None,
        "it points at the task's prompts.txt"
    );
    assert_eq!(record.chat_type, "implement");
    assert_eq!(record.parent.as_deref(), Some(source.id.as_str()));
    assert_eq!(record.fork, Some(Fork::Native));
    assert_eq!(record.model, None, "another session type, so its own model");
    wait_for(cx, "the fork to start", STARTUP, |_| {
        starts(&h, "claude") == 1
    })
    .await;
    let cwd = h
        .log_lines("claude")
        .into_iter()
        .rev()
        .find_map(|line| line.strip_prefix("cwd ").map(str::to_string))
        .unwrap();
    assert_eq!(cwd, h.base.join("extra/worktrees/spec").to_string_lossy());
    // The source's task gained nothing.
    assert_eq!(records_in(&h.scope).len(), 1);
}

#[gpui::test]
async fn test_a_fork_into_a_task_is_given_the_tasks_list_not_the_sources(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    add_tasks(&h, cx).await;
    write(&spec_scope(&h).join("docs/proration.md"), "# proration\n");
    write(
        &spec_scope(&h).join("artifacts.toml"),
        "schema = 1\n\n[[artifact]]\npath = \"docs/proration.md\"\n\
         description = \"The proration rules\"\nvisibility = \"shared\"\n",
    );
    let source = source(&h, Agent::Claude, cx, |_| {}).await;
    let target = ForkTarget::Task {
        task_id: "t2".to_string(),
        chat_type: Some("review".to_string()),
    };
    let forked = fork(&h, request(&source, target), cx).await.unwrap();

    let context = runtime_file(&h, &forked.chat_id, "context.md");
    assert!(context.contains("docs/proration.md"), "{context}");
    assert!(context.contains("The proration rules"), "{context}");
    assert!(!context.contains("design.md"), "{context}");
    assert!(!context.contains("The backfill design"), "{context}");
    for private in PRIVATE {
        assert!(!context.contains(private), "{private}");
    }
    assert_eq!(
        record_of(&spec_scope(&h), &forked.chat_id).chat_type,
        "review"
    );
    // A fork in the source's own task is given that task's list.
    let same = fork(&h, request(&source, ForkTarget::SameScope), cx)
        .await
        .unwrap();
    assert!(runtime_file(&h, &same.chat_id, "context.md").contains("docs/design.md"));
}

#[gpui::test]
async fn test_codex_without_fork_is_a_labeled_handoff_with_the_transcript_path(
    cx: &mut TestAppContext,
) {
    let h = ready(cx).await;
    let transcript = h
        .home
        .join(".codex/sessions/2026/11/08")
        .join(format!("rollout-2026-11-08T10-00-00-{CODEX_SESSION}.jsonl"));
    write(&transcript, "{}\n");
    let source = source(&h, Agent::Codex, cx, |_| {}).await;
    // What the parent's launch left: a fork copies and mentions none of it.
    crate::launch::write_runtime_dir(
        &h.runtime_root,
        &h.runtime_root.join(&source.id),
        &[(
            PathBuf::from("codex-instructions.md"),
            "PARENT-RUNTIME-MARKER\n".to_string(),
        )],
    )
    .unwrap();
    let help = "-m, --model <model>\n-c, --config <key=value>\nresume\nlogin\n";
    let mut asked = request(&source, ForkTarget::SameScope);
    asked.caps = Some(CodexAdapter::new("codex").capabilities_from_help("codex-cli 0.46.0", help));
    let forked = fork(&h, asked, cx).await.unwrap();

    assert_eq!(forked.fork, Fork::Handoff);
    assert_eq!(
        forked.toast(),
        format!("{}: {} Usage audit", forked.name, fork_label(Fork::Handoff))
    );
    assert!(forked.toast().contains("forked as new chat with handoff"));
    let record = record_of(&h.scope, &forked.chat_id);
    assert_eq!(record.fork, Some(Fork::Handoff));
    assert_eq!(record.parent.as_deref(), Some(source.id.as_str()));
    // The new session's id is captured once the CLI reports it, so it is not asserted here: the
    // start may have been captured already.
    wait_for(cx, "the fork to start", STARTUP, |_| {
        starts(&h, "codex") == 1
    })
    .await;
    let args = last_args(&h, "codex");
    assert!(!args.contains(&"fork".to_string()), "{args:?}");
    assert!(!args.contains(&"resume".to_string()), "{args:?}");

    let note = runtime_file(&h, &forked.chat_id, "codex-instructions.md");
    assert!(note.contains("# Handoff"), "{note}");
    assert!(note.contains("Usage audit"), "{note}");
    assert!(note.contains(&transcript.display().to_string()), "{note}");
    // No parent artifact: not the parent's launch files, and none of the task's private files.
    for text in [&note, &runtime_file(&h, &forked.chat_id, "context.md")] {
        assert!(!text.contains("PARENT-RUNTIME-MARKER"));
        for private in PRIVATE {
            assert!(!text.contains(private), "{private}");
        }
    }
    assert_eq!(
        std::fs::read_to_string(
            h.runtime_root
                .join(&source.id)
                .join("codex-instructions.md")
        )
        .unwrap(),
        "PARENT-RUNTIME-MARKER\n"
    );
}

#[gpui::test]
async fn test_codex_with_fork_forks_natively(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let source = source(&h, Agent::Codex, cx, |_| {}).await;
    let forked = fork(&h, request(&source, ForkTarget::SameScope), cx)
        .await
        .unwrap();
    assert_eq!(forked.fork, Fork::Native);
    wait_for(cx, "the fork to start", STARTUP, |_| {
        starts(&h, "codex") == 1
    })
    .await;
    let args = last_args(&h, "codex");
    assert_eq!(&args[..2], ["fork", CODEX_SESSION]);
    assert!(!runtime_file(&h, &forked.chat_id, "codex-instructions.md").contains("# Handoff"));
}

#[gpui::test]
async fn test_a_fork_without_a_known_session_is_a_handoff_that_says_so(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let source = source(&h, Agent::Claude, cx, |record| record.session_id = None).await;
    let forked = fork(&h, request(&source, ForkTarget::SameScope), cx)
        .await
        .unwrap();
    assert_eq!(forked.fork, Fork::Handoff);
    wait_for(cx, "the fork to start", STARTUP, |_| {
        starts(&h, "claude") == 1
    })
    .await;
    let args = last_args(&h, "claude");
    assert!(!args.contains(&"--fork-session".to_string()), "{args:?}");
    assert!(!args.contains(&"--resume".to_string()), "{args:?}");
    let note = runtime_file(&h, &forked.chat_id, "system.md");
    assert!(note.contains("transcript could not be found"), "{note}");

    // Asking for a handoff gets one even where a native fork could run.
    let source = self::source(&h, Agent::Claude, cx, |_| {}).await;
    let mut asked = request(&source, ForkTarget::SameScope);
    asked.force_handoff = true;
    let forced = fork(&h, asked, cx).await.unwrap();
    assert_eq!(forced.fork, Fork::Handoff);
}

#[gpui::test]
async fn test_a_refused_task_leaves_nothing_behind(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    add_tasks(&h, cx).await;
    let source = source(&h, Agent::Claude, cx, |_| {}).await;
    // Implement chats need a worktree: the pull request leaf `b-t1` has none.
    let target = ForkTarget::Task {
        task_id: "b-t1".to_string(),
        chat_type: None,
    };
    let error = fork(&h, request(&source, target), cx).await.unwrap_err();
    assert!(error.contains("Start the task first"), "{error}");
    let missing = ForkTarget::Task {
        task_id: "nope".to_string(),
        chat_type: None,
    };
    assert!(fork(&h, request(&source, missing), cx).await.is_err());
    let unknown = ForkRequest {
        source_id: "nope".to_string(),
        ..request(&source, ForkTarget::SameScope)
    };
    assert!(fork(&h, unknown, cx).await.is_err());
    assert_eq!(records_in(&h.scope).len(), 1);
    assert!(
        !h.root
            .join("projects/billing/tasks/backfill/chats")
            .exists()
    );
    assert!(starts(&h, "claude") == 0);
}

#[gpui::test]
async fn test_a_fork_does_nothing_while_chats_are_off(cx: &mut TestAppContext) {
    let h = harness(cx, false, 60, &[]).await;
    cx.update(|cx| ChatRuntime::set_global(h.runtime.clone(), cx));
    let record = ChatRecord::new(Agent::Claude, "plan", h.cwd.clone());
    let path = record_path(&h.scope, &record.id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_record(&path, &record).unwrap();
    let error = fork(&h, request(&record, ForkTarget::SameScope), cx)
        .await
        .unwrap_err();
    assert!(error.contains("off"), "{error}");
    assert_eq!(records_in(&h.scope).len(), 1);
    assert!(h.log_lines("claude").is_empty());
}

/// Waits for both CLIs to be probed, so a command finds their capabilities.
async fn probed(h: &Harness, cx: &mut TestAppContext) {
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
    let _ = h;
}

#[gpui::test]
async fn test_chat_fork_forks_the_shown_chat_without_moving_focus(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    probed(&h, cx).await;
    dispatch(cx, "chat::Fork");
    assert!(toast_shown(&workspace, cx), "no chat is shown yet");
    workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));

    let id = h.start(Agent::Claude, cx).await;
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    let panel = panel_of(&workspace, cx);
    assert!(panel.update_in(cx, |panel, window, cx| panel.show_chat(&id, window, cx)));
    dispatch(cx, "chat::Fork");
    wait_for(cx, "the fork in the panel", STARTUP, |cx| {
        shown(&workspace, cx).is_some_and(|shown| shown != id)
    })
    .await;
    let forked = shown(&workspace, cx).unwrap();
    let record = record_of(&h.scope, &forked);
    assert_eq!(record.parent.as_deref(), Some(id.as_str()));
    assert_eq!(record.fork, Some(Fork::Native));
    assert_eq!(h.state(&forked, cx), ChatState::Running);
    assert!(toast_shown(&workspace, cx));
    // The source still runs, and the terminal did not take focus.
    assert_eq!(h.state(&id, cx), ChatState::Running);
    let session = panel_of(&workspace, cx).read_with(cx, |panel, _| panel.session().unwrap());
    wait_for(cx, "the terminal view", STARTUP, |cx| {
        session.read_with(cx, |session, _| session.terminal_view().is_some())
    })
    .await;
    let view = session.read_with(cx, |session, _| session.terminal_view().cloned().unwrap());
    assert!(!view.update_in(cx, |view, window, cx| {
        view.focus_handle(cx).is_focused(window)
    }));
}

#[gpui::test]
async fn test_chat_fork_to_task_and_as_ask_the_task_and_the_type(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    add_tasks(&h, cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    probed(&h, cx).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    let panel = panel_of(&workspace, cx);
    assert!(panel.update_in(cx, |panel, window, cx| panel.show_chat(&id, window, cx)));

    dispatch(cx, "chat::ForkToTask");
    answer(&workspace, "Proration", cx).await;
    wait_for(cx, "the fork in the panel", STARTUP, |cx| {
        shown(&workspace, cx).is_some_and(|shown| shown != id)
    })
    .await;
    let first = shown(&workspace, cx).unwrap();
    let record = record_of(&spec_scope(&h), &first);
    assert_eq!(record.chat_type, "implement");
    assert_eq!(record.parent.as_deref(), Some(id.as_str()));
    assert_eq!(record.prompts_file, None);

    assert!(panel.update_in(cx, |panel, window, cx| panel.show_chat(&id, window, cx)));
    dispatch(cx, "chat::ForkToTaskAs");
    answer(&workspace, "Proration", cx).await;
    answer(&workspace, "review:", cx).await;
    wait_for(cx, "the second fork in the panel", STARTUP, |cx| {
        shown(&workspace, cx).is_some_and(|shown| shown != id && shown != first)
    })
    .await;
    let second = shown(&workspace, cx).unwrap();
    assert_eq!(record_of(&spec_scope(&h), &second).chat_type, "review");
    assert!(!has_modal(&workspace, cx));
}

#[gpui::test]
async fn test_a_gone_resume_directory_offers_a_fork_with_handoff(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    probed(&h, cx).await;
    let gone = h.base.join("gone");
    let mut record = ChatRecord::new(Agent::Claude, "plan", gone);
    record.name = "Old audit".to_string();
    record.prompted = true;
    record.session_id = Some(SESSION.to_string());
    let path = record_path(&h.scope, &record.id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_record(&path, &record).unwrap();
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store
            .read_with(cx, |store, _| store.find(&record.id).is_some())
    })
    .await;
    let before = std::fs::read(&path).unwrap();

    dispatch(cx, "chat::OpenAny");
    answer(&workspace, "Old audit", cx).await;
    // The offer comes, and nothing was resumed.
    answer(&workspace, "here", cx).await;
    wait_for(cx, "the fork in the panel", STARTUP, |cx| {
        shown(&workspace, cx).is_some_and(|shown| shown != record.id)
    })
    .await;
    let forked = record_of(&h.scope, &shown(&workspace, cx).unwrap());
    assert_eq!(forked.fork, Some(Fork::Handoff));
    assert_eq!(forked.parent.as_deref(), Some(record.id.as_str()));
    assert_eq!(forked.cwd, h.scope, "the place its task resolves to now");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(h.state(&record.id, cx), ChatState::Closed);
    assert!(
        h.log_lines("claude")
            .iter()
            .all(|line| line != "arg --resume"),
        "a handoff resumes nothing"
    );
}

#[gpui::test]
async fn test_a_same_scope_fork_of_a_chat_on_no_task_is_refused(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    // A directory under the project's tasks with no `task.toml`: its chats are indexed with no
    // task id.
    let loose = h.root.join("projects/invoicing/tasks/loose");
    let mut record = ChatRecord::new(Agent::Claude, "plan", h.cwd.clone());
    record.prompted = true;
    record.session_id = Some("8c1e4f8a-0000-4000-8000-0000000000cc".to_string());
    let path = record_path(&loose, &record.id);
    write(&path, "");
    write_record(&path, &record).unwrap();
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| {
            store
                .find(&record.id)
                .is_some_and(|chat| chat.task_id.is_none())
        })
    })
    .await;
    let request = ForkRequest {
        source_id: record.id.clone(),
        target: ForkTarget::SameScope,
        force_handoff: false,
        caps: Some(claude_caps()),
    };
    let projects = vec![h.project.clone()];
    let error = cx
        .update(|cx| fork_chat(request, projects, cx))
        .await
        .map(|forked| forked.chat_id)
        .unwrap_err();
    assert_eq!(error, "This chat is on no task");
    assert!(!h.runtime_root.join(&record.id).exists());
}
