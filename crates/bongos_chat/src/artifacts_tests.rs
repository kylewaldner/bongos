//! `chat: show artifacts`, its stale line and the row note, and `chat: edit session type`: a
//! running chat shows exactly the list it was given, a hidden artifact is never named (the buffer
//! text is searched for its path, name and description), human-only artifacts never appear, a
//! closed chat shows the next start's list, a view writes no record, and both commands dispatch by
//! name.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use bongos_chat_model::{
    Agent, AgentAdapter as _, ChatRecord, ClaudeAdapter, Fork, read_record, record_path,
    write_record,
};
use bongos_work_model::SharedArtifact;
use editor::Editor;
use gpui::{Entity, TestAppContext, VisualTestContext};
use workspace::Workspace;

use crate::commands_tests::{
    answer, command_window, dispatch, has_modal, panel_of, shown, toast_shown,
};
use crate::runtime_tests::{Harness, STARTUP, harness, wait_for, write};
use crate::{ROW_NOTE, STALE_LINE, artifacts_text, is_stale, stale_chat, type_files};

/// The human-only files of the test task: neither their paths nor their descriptions may ever
/// show.
pub(super) const HUMAN_ONLY: [&str; 6] = [
    "secret-root.md",
    "salary-review.md",
    "prompts.txt",
    "Root-level compensation numbers",
    "Salary review notes",
    "The private prompts Kyle typed",
];

pub(super) const EXTRA_PATH: &str = "docs/extra.md";
pub(super) const EXTRA_DESCRIPTION: &str = "Extra billing rules";

fn artifact(path: &str, description: &str) -> SharedArtifact {
    SharedArtifact {
        path: PathBuf::from(path),
        description: description.to_string(),
    }
}

/// The test task's manifest: `docs/design.md` shared, `docs/extra.md` shared or human-only, and
/// the human-only files the harness made. `description` is the extra artifact's.
pub(super) fn write_manifest(h: &Harness, extra_shared: bool, description: &str) {
    let visibility = if extra_shared { "shared" } else { "human-only" };
    let mut manifest = format!(
        "schema = 1\n\n[[artifact]]\npath = \"docs/design.md\"\ndescription = \"The backfill \
         design\"\nvisibility = \"shared\"\n\n[[artifact]]\npath = \"{EXTRA_PATH}\"\n\
         description = \"{description}\"\nvisibility = \"{visibility}\"\n"
    );
    for (path, description) in [
        ("secret-root.md", "Root-level compensation numbers"),
        ("notes/salary-review.md", "Salary review notes"),
        ("prompts.txt", "The private prompts Kyle typed"),
    ] {
        manifest.push_str(&format!(
            "\n[[artifact]]\npath = \"{path}\"\ndescription = \"{description}\"\n\
             visibility = \"human-only\"\n"
        ));
    }
    write(&h.scope.join("artifacts.toml"), &manifest);
}

/// A harness whose task shares `docs/design.md` and `docs/extra.md`, with the harness's runtime
/// as the global one.
pub(super) async fn ready(cx: &mut TestAppContext) -> Harness {
    let h = harness(cx, true, 60, &[]).await;
    write(&h.scope.join(EXTRA_PATH), "# extra\n");
    write_manifest(&h, true, EXTRA_DESCRIPTION);
    wait_for(cx, "the task in the tree", STARTUP, |cx| {
        h.work.read_with(cx, |work, _| {
            work.snapshot()
                .is_some_and(|tree| tree.task_by_id("t1").is_some())
        })
    })
    .await;
    h
}

/// A closed chat on the test task, written and waited for in the index.
async fn closed_chat(h: &Harness, cx: &mut TestAppContext) -> ChatRecord {
    let mut record = ChatRecord::new(Agent::Claude, "plan", h.cwd.clone());
    record.name = "Usage audit".to_string();
    record.prompted = true;
    let path = record_path(&h.scope, &record.id);
    write(&path, "");
    write_record(&path, &record).unwrap();
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store
            .read_with(cx, |store, _| store.find(&record.id).is_some())
    })
    .await;
    record
}

/// A running chat, shown in the panel.
async fn running_chat(
    h: &Harness,
    workspace: &Entity<Workspace>,
    cx: &mut VisualTestContext,
) -> String {
    let id = h.start(Agent::Claude, cx).await;
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    let panel = panel_of(workspace, cx);
    assert!(panel.update_in(cx, |panel, window, cx| panel.show_chat(&id, window, cx)));
    id
}

/// The editors in the center pane.
fn editors(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> Vec<Entity<Editor>> {
    workspace.read_with(cx, |workspace, cx| {
        workspace
            .active_pane()
            .read(cx)
            .items_of_type::<Editor>()
            .collect()
    })
}

/// Dispatches `chat: show artifacts` and returns the text of the buffer it opens.
async fn view_text(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> String {
    let before = editors(workspace, cx).len();
    dispatch(cx, "chat::ShowArtifacts");
    for _ in 0..400 {
        cx.run_until_parked();
        if editors(workspace, cx).len() > before {
            break;
        }
        cx.executor().timer(Duration::from_millis(25)).await;
    }
    let opened = editors(workspace, cx);
    assert!(opened.len() > before, "no artifacts view opened");
    let active = workspace.update_in(cx, |workspace, _, cx| {
        workspace.active_item_as::<Editor>(cx).unwrap()
    });
    active.read_with(cx, |editor, cx| {
        assert!(editor.read_only(cx), "the view is read-only");
        editor.text(cx)
    })
}

pub(super) fn no_human_only(text: &str) {
    for secret in HUMAN_ONLY {
        assert!(!text.contains(secret), "{secret} shows in:\n{text}");
    }
}

#[test]
fn test_the_list_text_and_the_digest_comparison() {
    let given = vec![artifact("/t/a.md", "A"), artifact("/t/b.md", "B")];
    // The same list, in any order, is not stale.
    assert!(!is_stale(&given, &[given[1].clone(), given[0].clone()]));
    // One gone, one changed, one more: stale.
    assert!(is_stale(&given, &given[..1]));
    assert!(is_stale(
        &given,
        &[given[0].clone(), artifact("/t/b.md", "B2")]
    ));
    let mut more = given.clone();
    more.push(artifact("/t/c.md", "C"));
    assert!(is_stale(&given, &more));

    let text = crate::artifacts_view::render("Audit", Some(&given), &given, &[]);
    assert!(
        text.contains("/t/a.md") && text.contains("/t/b.md"),
        "{text}"
    );
    assert!(!text.contains(STALE_LINE));
    // A hidden artifact is left out of a running chat's list, and the stale line names nothing.
    let text = crate::artifacts_view::render("Audit", Some(&given), &given[..1], &[]);
    assert!(text.contains("/t/a.md"), "{text}");
    assert!(
        !text.contains("/t/b.md") && !text.contains("\n  B\n"),
        "{text}"
    );
    assert!(text.contains(STALE_LINE));
    // A changed description keeps what the chat was given.
    let changed = [given[0].clone(), artifact("/t/b.md", "B2")];
    let text = crate::artifacts_view::render("Audit", Some(&given), &changed, &[]);
    assert!(text.contains("  B\n") && !text.contains("B2"), "{text}");
    assert!(text.contains(STALE_LINE));
    // A closed chat shows what a start gets, and is never stale.
    let text = crate::artifacts_view::render("Audit", None, &changed, &[]);
    assert!(text.contains("B2") && !text.contains(STALE_LINE), "{text}");
    let empty = crate::artifacts_view::render("Audit", None, &[], &[]);
    assert!(empty.contains("(none)"), "{empty}");
    assert_eq!(
        STALE_LINE,
        "The task's artifacts changed since this session started; the next resume gets the \
         current list"
    );
    assert_eq!(ROW_NOTE, "artifact list updates on next resume");
}

#[gpui::test]
async fn test_a_running_chat_shows_exactly_the_list_it_was_given(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    running_chat(&h, &workspace, cx).await;
    let text = view_text(&workspace, cx).await;
    let design = h.scope.join("docs/design.md");
    let extra = h.scope.join(EXTRA_PATH);
    assert!(text.contains(&format!("- {}\n  The backfill design\n", design.display())));
    assert!(text.contains(&format!("- {}\n  {EXTRA_DESCRIPTION}\n", extra.display())));
    assert_eq!(text.matches("\n- ").count(), 2, "{text}");
    assert!(!text.contains(STALE_LINE), "{text}");
    no_human_only(&text);
    // The Chats list's note: not stale.
    let id = shown(&workspace, cx).unwrap();
    assert!(!cx.update(|_, cx| stale_chat(&id, cx)).await);
    // The view wrote nothing: the record is what it was before the view, once a write that was
    // already on its way has landed.
    let path = record_path(&h.scope, &id);
    for _ in 0..10 {
        cx.run_until_parked();
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    let before = std::fs::read(&path).unwrap();
    view_text(&workspace, cx).await;
    for _ in 0..10 {
        cx.run_until_parked();
        cx.executor().timer(Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[gpui::test]
async fn test_a_hidden_artifact_makes_the_view_stale_and_is_never_named(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let id = running_chat(&h, &workspace, cx).await;

    // Hide the extra artifact after the chat started.
    write_manifest(&h, false, EXTRA_DESCRIPTION);
    let text = view_text(&workspace, cx).await;
    assert!(text.contains(STALE_LINE), "{text}");
    assert!(text.contains("docs/design.md"), "{text}");
    assert!(!text.contains("extra.md"), "{text}");
    assert!(!text.contains(EXTRA_PATH), "{text}");
    assert!(!text.contains(EXTRA_DESCRIPTION), "{text}");
    no_human_only(&text);
    assert!(cx.update(|_, cx| stale_chat(&id, cx)).await);

    // Hide it and change its description: still nothing of it on screen.
    write_manifest(&h, false, "Changed rules");
    let text = view_text(&workspace, cx).await;
    assert!(text.contains(STALE_LINE), "{text}");
    assert!(
        !text.contains("Changed rules") && !text.contains("extra.md"),
        "{text}"
    );
    // Showing it again as it was is not stale any more.
    write_manifest(&h, true, EXTRA_DESCRIPTION);
    let text = view_text(&workspace, cx).await;
    assert!(!text.contains(STALE_LINE), "{text}");
    assert!(!cx.update(|_, cx| stale_chat(&id, cx)).await);
    // A changed description is stale too, and shows what the chat was given.
    write_manifest(&h, true, "New wording");
    let text = view_text(&workspace, cx).await;
    assert!(text.contains(STALE_LINE), "{text}");
    assert!(
        text.contains(EXTRA_DESCRIPTION) && !text.contains("New wording"),
        "{text}"
    );
    assert!(cx.update(|_, cx| stale_chat(&id, cx)).await);
}

#[gpui::test]
async fn test_a_closed_chat_shows_the_next_starts_list_and_no_human_only_artifact(
    cx: &mut TestAppContext,
) {
    let h = ready(cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let record = closed_chat(&h, cx).await;
    let path = record_path(&h.scope, &record.id);
    let before = std::fs::read(&path).unwrap();
    let panel = panel_of(&workspace, cx);
    assert!(panel.update_in(cx, |panel, window, cx| {
        panel.show_chat(&record.id, window, cx)
    }));

    let text = view_text(&workspace, cx).await;
    assert!(text.contains(&h.scope.join("docs/design.md").display().to_string()));
    assert!(text.contains(&h.scope.join(EXTRA_PATH).display().to_string()));
    assert!(text.contains("next start"), "{text}");
    assert!(!text.contains(STALE_LINE), "{text}");
    no_human_only(&text);

    // Hiding one: the next start does not get it, so the view does not show it.
    write_manifest(&h, false, EXTRA_DESCRIPTION);
    let text = view_text(&workspace, cx).await;
    assert!(text.contains("docs/design.md"), "{text}");
    assert!(
        !text.contains("extra.md") && !text.contains(EXTRA_DESCRIPTION),
        "{text}"
    );
    assert!(!text.contains(STALE_LINE), "{text}");
    no_human_only(&text);
    // A chat that does not run is never stale.
    assert!(!cx.update(|_, cx| stale_chat(&record.id, cx)).await);
    // Neither view wrote the record.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(read_record(&path).unwrap().context_digest, None);
}

#[gpui::test]
async fn test_artifacts_text_reads_the_same_list_as_the_view(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let (_workspace, cx) = command_window(&h, cx).await;
    let record = closed_chat(&h, cx).await;
    let text = cx
        .update(|_, cx| artifacts_text(&record.id, cx))
        .await
        .unwrap();
    assert!(text.starts_with("Artifacts of Usage audit"), "{text}");
    assert!(text.contains("docs/extra.md"));
    let error = cx
        .update(|_, cx| artifacts_text("no-such-chat", cx))
        .await
        .unwrap_err();
    assert!(error.contains("no-such-chat"), "{error}");
}

#[gpui::test]
async fn test_edit_session_type_opens_both_files(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    dispatch(cx, "chat::EditSessionType");
    answer(&workspace, "review:", cx).await;
    for _ in 0..400 {
        cx.run_until_parked();
        if editors(&workspace, cx).len() >= 2 {
            break;
        }
        cx.executor().timer(Duration::from_millis(25)).await;
    }
    let opened: Vec<PathBuf> = editors(&workspace, cx)
        .into_iter()
        .filter_map(|editor| {
            editor.read_with(cx, |editor, cx| {
                let buffer = editor.buffer().read(cx).as_singleton()?;
                let file = buffer.read(cx).file()?.as_local()?;
                Some(file.abs_path(cx))
            })
        })
        .collect();
    let [type_toml, context] = type_files(&h.types_root, "review");
    assert!(opened.contains(&type_toml), "{opened:?}");
    assert!(opened.contains(&context), "{opened:?}");
    assert!(!has_modal(&workspace, cx));
}

#[gpui::test]
async fn test_edit_session_type_creates_a_missing_context_file_only(cx: &mut TestAppContext) {
    let h = ready(cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    write(
        &h.types_root.join("audit/type.toml"),
        "schema = 1\ndescription = \"Audit the code\"\n",
    );
    let [type_toml, context] = type_files(&h.types_root, "audit");
    let before = std::fs::read(&type_toml).unwrap();
    assert!(!context.exists());
    dispatch(cx, "chat::EditSessionType");
    answer(&workspace, "audit:", cx).await;
    wait_for(cx, "both files open", STARTUP, |cx| {
        editors(&workspace, cx).len() >= 2
    })
    .await;
    assert!(context.is_file());
    assert_eq!(std::fs::read(&context).unwrap(), b"");
    assert_eq!(std::fs::read(&type_toml).unwrap(), before);
}

/// Both commands dispatched by name: off, they only say so; on, with no chat shown, the artifacts
/// command says so and the type command asks.
#[gpui::test]
async fn test_both_commands_dispatch_by_name(cx: &mut TestAppContext) {
    let off = harness(cx, false, 60, &[]).await;
    let (workspace, cx) = command_window(&off, cx).await;
    for name in ["chat::ShowArtifacts", "chat::EditSessionType"] {
        workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));
        dispatch(cx, name);
        assert!(toast_shown(&workspace, cx), "{name}");
        assert!(!has_modal(&workspace, cx), "{name}");
    }
    assert!(editors(&workspace, cx).is_empty());
}

#[gpui::test]
async fn test_show_artifacts_with_no_shown_chat_says_so_and_edit_type_asks(
    cx: &mut TestAppContext,
) {
    let h = ready(cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    dispatch(cx, "chat::ShowArtifacts");
    assert!(toast_shown(&workspace, cx));
    assert!(editors(&workspace, cx).is_empty());
    workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));
    dispatch(cx, "chat::EditSessionType");
    assert!(has_modal(&workspace, cx));
}

/// Writes a closed chat forked from `parent`, as a record.
async fn closed_fork(
    h: &Harness,
    parent: &ChatRecord,
    fork: Fork,
    cx: &mut TestAppContext,
) -> ChatRecord {
    let mut record = ChatRecord::new(Agent::Claude, "plan", h.cwd.clone());
    record.name = "Forked audit".to_string();
    record.prompted = true;
    record.parent = Some(parent.id.clone());
    record.fork = Some(fork);
    let path = record_path(&h.scope, &record.id);
    write(&path, "");
    write_record(&path, &record).unwrap();
    wait_for(cx, "the fork in the index", STARTUP, |cx| {
        h.store
            .read_with(cx, |store, _| store.find(&record.id).is_some())
    })
    .await;
    record
}

#[gpui::test]
async fn test_a_handoff_fork_is_labeled_in_the_view_and_a_native_one_is_not_a_handoff(
    cx: &mut TestAppContext,
) {
    let h = ready(cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let parent = closed_chat(&h, cx).await;
    let handoff = closed_fork(&h, &parent, Fork::Handoff, cx).await;
    let panel = panel_of(&workspace, cx);
    assert!(panel.update_in(cx, |panel, window, cx| panel.show_chat(
        &handoff.id,
        window,
        cx
    )));
    write_manifest(&h, false, EXTRA_DESCRIPTION);
    let text = view_text(&workspace, cx).await;
    assert!(
        text.contains("forked as new chat with handoff from Usage audit"),
        "{text}"
    );
    assert!(text.contains("docs/design.md"), "{text}");
    assert!(
        !text.contains("extra.md") && !text.contains(EXTRA_DESCRIPTION),
        "{text}"
    );
    no_human_only(&text);

    let native = closed_fork(&h, &parent, Fork::Native, cx).await;
    assert!(panel.update_in(cx, |panel, window, cx| {
        panel.show_chat(&native.id, window, cx)
    }));
    let text = view_text(&workspace, cx).await;
    assert!(text.contains("forked from Usage audit"), "{text}");
    assert!(!text.contains("handoff"), "{text}");
    // A chat that is no fork has no such line.
    assert!(panel.update_in(cx, |panel, window, cx| {
        panel.show_chat(&parent.id, window, cx)
    }));
    let text = view_text(&workspace, cx).await;
    assert!(!text.contains("forked"), "{text}");
}

#[gpui::test]
async fn test_a_used_fallback_is_labeled_in_the_view_without_naming_a_hidden_artifact(
    cx: &mut TestAppContext,
) {
    let h = ready(cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    // A Claude without hooks, plugins and `--name`: the launch falls back three ways.
    let flags = "--session-id\n--resume\n--fork-session\n--append-system-prompt\n--model\n";
    let caps = ClaudeAdapter::new("claude").capabilities_from_help("2.1.319 (Claude Code)", flags);
    let mut request = h.request(Agent::Claude, "implement");
    request.caps = Some(caps);
    let id = request.record.id.clone();
    h.launch(request, &h.project, cx).await.unwrap();
    wait_for(cx, "the chat in the index", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| store.find(&id).is_some())
    })
    .await;
    let used: Vec<String> = h.runtime.read_with(cx, |runtime, _| {
        runtime
            .live(&id)
            .unwrap()
            .fallbacks()
            .iter()
            .map(|fallback| fallback.label().to_string())
            .collect()
    });
    assert!(!used.is_empty(), "the launch used no fallback");
    let panel = panel_of(&workspace, cx);
    assert!(panel.update_in(cx, |panel, window, cx| panel.show_chat(&id, window, cx)));
    write_manifest(&h, false, EXTRA_DESCRIPTION);
    let text = view_text(&workspace, cx).await;
    for label in &used {
        assert!(
            text.contains(&format!("Fallback used: {label}")),
            "{label} in\n{text}"
        );
    }
    assert!(text.contains(STALE_LINE), "{text}");
    assert!(
        !text.contains("extra.md") && !text.contains(EXTRA_DESCRIPTION),
        "{text}"
    );
    no_human_only(&text);
}
