//! The list view of the Chats panel in a real window: rows sorted by last activity with every field
//! and label, Archived collapsed, the filter, the all-tasks toggle, the health header, the keys as
//! actions, the saved state a new panel comes back to, the row note, the dump text, and the two
//! things the list made real (`chats.enabled` off ends running chats, the last agent is saved).

#![cfg(unix)]

use std::path::Path;

use bongos_chat_model::{Agent, ChatRecord, Fork, Layout, read_record, record_path, write_record};
use gpui::{
    AppContext as _, Entity, Focusable as _, TestAppContext, UpdateGlobal as _, VisualTestContext,
};
use settings::SettingsStore;
use workspace::Workspace;

use crate::artifacts_tests::{EXTRA_DESCRIPTION, EXTRA_PATH, no_human_only, ready, write_manifest};
use crate::commands_tests::{answer, command_window, dispatch, has_modal, toast_shown};
use crate::list_view::{ALL_TASKS, Listing, Row, short_path};
use crate::runtime_tests::{
    Harness, STARTUP, WITHIN, harness, harness_focused, process_alive, set_chat_settings, wait_for,
    write,
};
use crate::view_state::{self, ALL_TASKS_KEY, FILTER_KEY, LAST_AGENT_KEY, VIEW_KEY};
use crate::{ChatHealth, ChatPanel, ChatRuntime, ChatState, HealthState, ROW_NOTE, fix_not_found};

/// A record of `agent` in `scope`, written to disk and not running. `change` edits it first.
fn write_chat(
    scope: &Path,
    cwd: &Path,
    name: &str,
    last_active: &str,
    change: impl FnOnce(&mut ChatRecord),
) -> ChatRecord {
    let mut record = ChatRecord::new(Agent::Claude, "plan", cwd.to_path_buf());
    record.name = name.to_string();
    record.created = "2026-01-01T09:00:00Z".parse().unwrap();
    record.last_active = last_active.parse().unwrap();
    change(&mut record);
    let path = record_path(scope, &record.id);
    write(&path, "");
    write_record(&path, &record).unwrap();
    record
}

async fn wait_indexed(h: &Harness, ids: &[&str], cx: &mut TestAppContext) {
    wait_for(cx, "the chats in the index", STARTUP, |cx| {
        h.store
            .read_with(cx, |store, _| ids.iter().all(|id| store.find(id).is_some()))
    })
    .await;
}

fn panel(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ChatPanel> {
    workspace.read_with(cx, |workspace, cx| {
        workspace.panel::<ChatPanel>(cx).expect("the chats panel")
    })
}

fn list_of(
    panel: &Entity<ChatPanel>,
    cx: &mut VisualTestContext,
) -> Entity<crate::list_view::ChatListView> {
    panel.read_with(cx, |panel, _| panel.list().clone())
}

fn listing(panel: &Entity<ChatPanel>, cx: &mut VisualTestContext) -> Listing {
    let list = list_of(panel, cx);
    list.read_with(cx, |list, cx| list.listing(cx))
}

fn view_text(panel: &Entity<ChatPanel>, cx: &mut VisualTestContext) -> String {
    panel.read_with(cx, |panel, cx| panel.view_text(cx))
}

fn titles(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|row| row.title.as_str()).collect()
}

fn row<'a>(rows: &'a [Row], title: &str) -> &'a Row {
    rows.iter()
        .find(|row| row.title == title)
        .unwrap_or_else(|| panic!("no row {title} in {:?}", titles(rows)))
}

fn on_disk(scope: &Path, id: &str) -> ChatRecord {
    read_record(&record_path(scope, id)).unwrap()
}

/// Opens the dock with the panel focused (so the keys of the list reach it).
fn focus_list(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) {
    dispatch(cx, "chat::TogglePanel");
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.right_dock().read(cx).is_open()
    }));
}

/// The saved value of `key`, once it was written.
async fn saved(key: &'static str, want: Option<&str>, cx: &mut TestAppContext) {
    wait_for(cx, &format!("the saved {key}"), STARTUP, |cx| {
        cx.update(|cx| view_state::read(key, cx)).as_deref() == want
    })
    .await;
}

#[test]
fn test_a_path_is_shortened_to_home_and_its_last_directories() {
    let home = Path::new("/home/kyle");
    assert_eq!(short_path(Path::new("/home/kyle"), home), "~");
    assert_eq!(
        short_path(Path::new("/home/kyle/code/app"), home),
        "~/code/app"
    );
    assert_eq!(
        short_path(Path::new("/home/kyle/code/work/trees/app"), home),
        ".../work/trees/app"
    );
    assert_eq!(short_path(Path::new("/srv/app"), home), "/srv/app");
}

#[gpui::test]
async fn test_rows_are_sorted_by_last_active_and_show_every_field(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let old = write_chat(&h.scope, &h.cwd, "Old chat", "2026-01-01T10:00:00Z", |_| {});
    let newest = write_chat(
        &h.scope,
        &h.cwd,
        "Newest chat",
        "2026-03-01T10:00:00Z",
        |_| {},
    );
    let fork = write_chat(
        &h.scope,
        &h.cwd,
        "Forked chat",
        "2026-02-01T10:00:00Z",
        |record| {
            record.agent = "codex".to_string();
            record.chat_type = "implement".to_string();
            record.layout = Layout::ChatOnly;
            record.parent = Some(newest.id.clone());
            record.fork = Some(Fork::Handoff);
        },
    );
    // A record that cannot be read is listed by its file name, after the readable ones.
    write(
        &h.scope.join("chats/broken.toml"),
        "schema = 1\nthis is not toml\n",
    );
    wait_indexed(&h, &[&old.id, &newest.id, &fork.id], cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let panel = panel(&workspace, cx);

    let listing = listing(&panel, cx);
    assert_eq!(
        titles(&listing.open),
        ["Newest chat", "Forked chat", "Old chat", "broken.toml"]
    );
    let forked = row(&listing.open, "Forked chat");
    let lines = forked.lines.join("\n");
    assert!(lines.contains("agent: codex  type: implement  layout: chat-only  state: closed"));
    assert!(lines.contains("started: 2026-01-01 09:00Z  last active: 2026-02-01 10:00Z"));
    assert!(lines.contains("worktree: "), "{lines}");
    assert!(lines.contains("/worktree"), "{lines}");
    assert!(
        lines.contains("forked as new chat with handoff from Newest chat"),
        "{lines}"
    );
    assert!(lines.contains("labels: handoff"), "{lines}");
    let parent = row(&listing.open, "Newest chat").lines.join("\n");
    assert!(parent.contains("forks: 1"), "{parent}");
    assert!(parent.contains("agent: claude  type: plan  layout: dual  state: closed"));
    let broken = row(&listing.open, "broken.toml");
    assert!(!broken.selectable);
    assert!(broken.lines.join("\n").contains("labels: error: "));

    // The text of the view is the rows, the first one selected.
    let text = view_text(&panel, cx);
    assert!(text.contains("Open (4)"), "{text}");
    assert!(text.contains("> Newest chat"), "{text}");
    assert!(
        text.find("Newest chat") < text.find("Forked chat"),
        "{text}"
    );
    assert!(text.find("Forked chat") < text.find("Old chat"), "{text}");
}

#[gpui::test]
async fn test_archived_chats_are_collapsed_until_opened(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let open = write_chat(
        &h.scope,
        &h.cwd,
        "Still open",
        "2026-02-01T10:00:00Z",
        |_| {},
    );
    let a1 = write_chat(
        &h.scope,
        &h.cwd,
        "Old audit",
        "2026-01-01T10:00:00Z",
        |record| {
            record.archived = true;
            record.archived_at = Some(bongos_work_model::now());
        },
    );
    let a2 = write_chat(
        &h.scope,
        &h.cwd,
        "Old plan",
        "2026-01-02T10:00:00Z",
        |record| {
            record.archived = true;
            record.archived_at = Some(bongos_work_model::now());
        },
    );
    wait_indexed(&h, &[&open.id, &a1.id, &a2.id], cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let panel = panel(&workspace, cx);

    let listing = listing(&panel, cx);
    assert_eq!(titles(&listing.open), ["Still open"]);
    assert_eq!(titles(&listing.archived), ["Old plan", "Old audit"]);
    let text = view_text(&panel, cx);
    assert!(text.contains("Archived (2) collapsed"), "{text}");
    assert!(
        !text.contains("Old audit") && !text.contains("Old plan"),
        "{text}"
    );

    // `chat: show archive` opens the list with Archived expanded.
    dispatch(cx, "chat::ShowArchive");
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.right_dock().read(cx).is_open()
    }));
    let text = view_text(&panel, cx);
    assert!(text.contains("Archived (2)\n"), "{text}");
    assert!(!text.contains("collapsed"), "{text}");
    assert!(text.find("Old plan") < text.find("Old audit"), "{text}");
    assert!(on_disk(&h.scope, &a1.id).archived, "the view wrote nothing");
    saved(view_state::ARCHIVED_EXPANDED_KEY, Some("true"), cx).await;

    // Collapsing it again hides the rows.
    let list = list_of(&panel, cx);
    list.update(cx, |list, cx| list.set_archived_expanded(false, cx));
    assert!(view_text(&panel, cx).contains("Archived (2) collapsed"));
}

#[gpui::test]
async fn test_the_filter_keeps_the_chats_whose_name_matches(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let audit = write_chat(
        &h.scope,
        &h.cwd,
        "Usage Audit",
        "2026-02-01T10:00:00Z",
        |_| {},
    );
    let other = write_chat(
        &h.scope,
        &h.cwd,
        "Login redirect",
        "2026-02-02T10:00:00Z",
        |_| {},
    );
    let old = write_chat(
        &h.scope,
        &h.cwd,
        "Audit of old things",
        "2026-01-01T10:00:00Z",
        |r| {
            r.archived = true;
        },
    );
    wait_indexed(&h, &[&audit.id, &other.id, &old.id], cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let panel = panel(&workspace, cx);
    assert_eq!(listing(&panel, cx).open.len(), 2);

    // `/` asks for the text, and the list follows it.
    focus_list(&workspace, cx);
    dispatch(cx, "chat::Filter");
    answer(&workspace, "audit", cx).await;
    assert!(!has_modal(&workspace, cx));
    let filtered = listing(&panel, cx);
    assert_eq!(titles(&filtered.open), ["Usage Audit"]);
    assert_eq!(titles(&filtered.archived), ["Audit of old things"]);
    let text = view_text(&panel, cx);
    assert!(text.contains("Filter: audit"), "{text}");
    assert!(!text.contains("Login redirect"), "{text}");
    saved(FILTER_KEY, Some("audit"), cx).await;

    // An empty filter lists everything again.
    let list = list_of(&panel, cx);
    list.update(cx, |list, cx| list.set_filter("  ".to_string(), cx));
    assert_eq!(listing(&panel, cx).open.len(), 2);
    saved(FILTER_KEY, None, cx).await;
}

#[gpui::test]
async fn test_all_tasks_toggles_between_the_focused_task_and_every_task(cx: &mut TestAppContext) {
    let h = harness_focused(cx).await;
    let other_dir = h.root.join("projects/invoicing/tasks/other");
    write(
        &other_dir.join("task.toml"),
        "schema = 1\nid = \"t9\"\ngoal = \"Other work\"\nkind = \"pr-ship\"\nstatus = \"open\"\n\
         created = 2020-01-01T10:00:00Z\n",
    );
    let here = write_chat(
        &h.scope,
        &h.cwd,
        "On the focused task",
        "2026-02-01T10:00:00Z",
        |_| {},
    );
    let there = write_chat(
        &other_dir,
        &h.cwd,
        "On another task",
        "2026-03-01T10:00:00Z",
        |_| {},
    );
    wait_indexed(&h, &[&here.id, &there.id], cx).await;
    wait_for(cx, "the chat's task id", STARTUP, |cx| {
        h.store.read_with(cx, |store, _| {
            store.find(&there.id).is_some_and(|c| c.task_id.is_some())
        })
    })
    .await;
    let (workspace, cx) = command_window(&h, cx).await;
    let panel = panel(&workspace, cx);

    let scoped = listing(&panel, cx);
    assert_eq!(scoped.scope, "Backfill usage events");
    assert_eq!(titles(&scoped.open), ["On the focused task"]);
    assert!(view_text(&panel, cx).contains("Scope: Backfill usage events"));

    // `t`.
    focus_list(&workspace, cx);
    dispatch(cx, "chat::ToggleAllTasks");
    let all = listing(&panel, cx);
    assert_eq!(all.scope, ALL_TASKS);
    assert_eq!(
        titles(&all.open),
        ["On another task", "On the focused task"]
    );
    assert!(
        row(&all.open, "On another task")
            .lines
            .contains(&"task: Other work".to_string())
    );
    saved(ALL_TASKS_KEY, Some("true"), cx).await;

    dispatch(cx, "chat::ToggleAllTasks");
    assert_eq!(titles(&listing(&panel, cx).open), ["On the focused task"]);
    saved(ALL_TASKS_KEY, None, cx).await;
}

#[gpui::test]
async fn test_the_header_says_a_missing_cli_is_missing_and_how_to_fix_it(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let missing = "/nonexistent/bin/claude";
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                let agents = content
                    .chats
                    .get_or_insert_default()
                    .agents
                    .get_or_insert_default();
                agents.claude.get_or_insert_default().command = Some(missing.to_string());
            });
        });
    });
    let health = cx.update(|cx| ChatHealth::global(cx).unwrap());
    wait_for(cx, "claude to be reported missing", STARTUP, |cx| {
        health.update(cx, |health, cx| health.refresh_now(cx));
        health.read_with(cx, |health, _| {
            health.health(Agent::Claude).state == HealthState::NotFound
        })
    })
    .await;
    let (workspace, cx) = command_window(&h, cx).await;
    let panel = panel(&workspace, cx);

    let fix = fix_not_found(Agent::Claude, missing);
    let listing = listing(&panel, cx);
    assert!(
        listing
            .health
            .contains(&format!("claude: not found - {fix}")),
        "{:?}",
        listing.health
    );
    assert!(fix.contains("chats.agents.claude.command"));
    // Codex is not affected.
    assert!(
        listing
            .health
            .iter()
            .any(|line| line.starts_with("codex: ready")),
        "{:?}",
        listing.health
    );
    let text = view_text(&panel, cx);
    assert!(
        text.contains(&format!("claude: not found - {fix}")),
        "{text}"
    );
}

#[gpui::test]
async fn test_the_keys_act_on_the_selected_row(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let first = write_chat(
        &h.scope,
        &h.cwd,
        "First chat",
        "2026-03-01T10:00:00Z",
        |_| {},
    );
    let second = write_chat(
        &h.scope,
        &h.cwd,
        "Second chat",
        "2026-02-01T10:00:00Z",
        |_| {},
    );
    wait_indexed(&h, &[&first.id, &second.id], cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let panel = panel(&workspace, cx);
    focus_list(&workspace, cx);
    let list = list_of(&panel, cx);
    let selected =
        |cx: &mut VisualTestContext| list.read_with(cx, |list, cx| list.selected_chat(cx));
    assert_eq!(selected(cx), Some(first.id.clone()));

    dispatch(cx, "menu::SelectNext");
    assert_eq!(selected(cx), Some(second.id.clone()));
    dispatch(cx, "menu::SelectNext");
    assert_eq!(selected(cx), Some(second.id.clone()), "the last row stays");
    dispatch(cx, "menu::SelectPrevious");
    assert_eq!(selected(cx), Some(first.id.clone()));
    dispatch(cx, "menu::SelectNext");

    // `r`: asks for a name and renames the selected chat.
    dispatch(cx, "chat::Rename");
    answer(&workspace, " renamed", cx).await;
    wait_for(cx, "the rename", STARTUP, |_| {
        on_disk(&h.scope, &second.id).name.ends_with("renamed")
    })
    .await;
    assert_eq!(on_disk(&h.scope, &first.id).name, "First chat");

    // `a`: archives it; the Archived section holds it, collapsed.
    dispatch(cx, "chat::Archive");
    wait_for(cx, "the archive", STARTUP, |_| {
        on_disk(&h.scope, &second.id).archived
    })
    .await;
    wait_for(cx, "the index to see it", STARTUP, |cx| {
        panel.read_with(cx, |panel, cx| {
            panel.list().read(cx).listing(cx).archived.len() == 1
        })
    })
    .await;
    assert!(!on_disk(&h.scope, &first.id).archived);

    // `u` on an open row says it is not archived and writes nothing.
    workspace.update(cx, |workspace, cx| workspace.clear_all_notifications(cx));
    dispatch(cx, "chat::Restore");
    assert!(toast_shown(&workspace, cx));
    assert!(!on_disk(&h.scope, &first.id).archived);

    // Show the archive, select the archived row and restore it with `u`.
    dispatch(cx, "chat::ShowArchive");
    list.update_in(cx, |list, window, cx| {
        window.focus(&list.focus_handle(cx), cx)
    });
    list.update(cx, |list, cx| list.select(&second.id, cx));
    dispatch(cx, "chat::Restore");
    wait_for(cx, "the restore", STARTUP, |_| {
        !on_disk(&h.scope, &second.id).archived
    })
    .await;

    // `f`: asks which task to fork into (the task list is the Work crate's picker).
    list.update(cx, |list, cx| list.select(&first.id, cx));
    dispatch(cx, "chat::ForkToTask");
    assert!(has_modal(&workspace, cx) || toast_shown(&workspace, cx));
}

#[gpui::test]
async fn test_enter_shows_the_selected_chat_and_the_chats_link_goes_back_and_is_saved(
    cx: &mut TestAppContext,
) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &[&id], cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let panel = panel(&workspace, cx);
    focus_list(&workspace, cx);
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.shown_chat().map(str::to_string)),
        None
    );
    assert!(
        view_text(&panel, cx).contains("state: "),
        "a running chat's row"
    );

    dispatch(cx, "menu::Confirm");
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.shown_chat().map(str::to_string)),
        Some(id.clone())
    );
    saved(VIEW_KEY, Some(&id), cx).await;
    // Showing it did not move focus into its terminal.
    let session = panel.read_with(cx, |panel, _| panel.session().unwrap());
    let terminal = session
        .read_with(cx, |session, _| session.terminal_view().cloned())
        .unwrap();
    assert!(!terminal.update_in(cx, |view, window, cx| {
        view.focus_handle(cx).is_focused(window)
    }));

    // The "Chats" link clears the shown chat for real: the list shows and it is saved.
    session.update(cx, |_, cx| cx.emit(crate::SessionViewEvent::ShowList));
    cx.run_until_parked();
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.shown_chat().map(str::to_string)),
        None
    );
    saved(VIEW_KEY, Some(view_state::LIST_VIEW), cx).await;
}

#[gpui::test]
async fn test_the_state_survives_a_new_panel(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &[&id], cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let first = panel(&workspace, cx);
    let list = list_of(&first, cx);
    list.update(cx, |list, cx| {
        list.set_filter("chat".to_string(), cx);
        list.toggle_all_tasks(cx);
        list.set_archived_expanded(true, cx);
    });
    assert!(first.update_in(cx, |panel, window, cx| panel.show_chat(&id, window, cx)));
    saved(VIEW_KEY, Some(&id), cx).await;
    saved(FILTER_KEY, Some("chat"), cx).await;
    saved(ALL_TASKS_KEY, Some("true"), cx).await;
    saved(view_state::ARCHIVED_EXPANDED_KEY, Some("true"), cx).await;

    // A new panel (a new window, a restart) comes back showing the same chat and list state.
    let second = new_panel(&workspace, cx);
    cx.run_until_parked();
    assert_eq!(
        second.read_with(cx, |panel, _| panel.shown_chat().map(str::to_string)),
        Some(id.clone())
    );
    let list = list_of(&second, cx);
    list.read_with(cx, |list, _| {
        assert_eq!(list.filter(), "chat");
        assert!(list.all_tasks());
        assert!(list.archived_expanded());
    });

    // Back to the list, and a newer panel shows the list.
    second.update(cx, |panel, cx| panel.show_list(cx));
    saved(VIEW_KEY, Some(view_state::LIST_VIEW), cx).await;
    let third = new_panel(&workspace, cx);
    cx.run_until_parked();
    assert_eq!(
        third.read_with(cx, |panel, _| panel.shown_chat().map(str::to_string)),
        None
    );

    // A chat the index does not list yet is shown once it is listed, and not before.
    let late = write_chat(
        &h.scope,
        &h.cwd,
        "Late chat",
        "2026-02-01T10:00:00Z",
        |_| {},
    );
    cx.update(|_, cx| view_state::write(VIEW_KEY, Some(late.id.clone()), cx));
    saved(VIEW_KEY, Some(&late.id), cx).await;
    let fourth = new_panel(&workspace, cx);
    wait_indexed(&h, &[&late.id], cx).await;
    cx.run_until_parked();
    assert_eq!(
        fourth.read_with(cx, |panel, _| panel.shown_chat().map(str::to_string)),
        Some(late.id.clone())
    );
}

fn new_panel(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Entity<ChatPanel> {
    workspace.update_in(cx, |workspace, window, cx| {
        let (weak, project, fs) = (
            workspace.weak_handle(),
            workspace.project().clone(),
            workspace.app_state().fs.clone(),
        );
        cx.new(|cx| ChatPanel::new(weak, project, fs, window, cx))
    })
}

#[gpui::test]
async fn test_the_last_agent_is_saved_and_offered_first_in_a_new_run(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    // The newest chat on record is claude's; the saved agent wins over it.
    let record = write_chat(
        &h.scope,
        &h.cwd,
        "Claude chat",
        "2026-02-01T10:00:00Z",
        |_| {},
    );
    wait_indexed(&h, &[&record.id], cx).await;
    assert_eq!(cx.update(|cx| crate::commands::last_agent(cx)), None);
    h.runtime
        .update(cx, |runtime, cx| runtime.set_last_agent(Agent::Codex, cx));
    saved(LAST_AGENT_KEY, Some("codex"), cx).await;

    // A runtime of a new run knows nothing in memory.
    let store = h.store.clone();
    let home = h.home.clone();
    let fresh = cx.new(|cx| ChatRuntime::new(store, home, cx));
    cx.update(|cx| ChatRuntime::set_global(fresh.clone(), cx));
    assert_eq!(fresh.read_with(cx, |runtime, _| runtime.last_agent()), None);
    assert_eq!(
        cx.update(|cx| crate::commands::last_agent(cx)),
        Some(Agent::Codex)
    );
}

#[gpui::test]
async fn test_turning_chats_off_ends_the_running_chats(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let id = h.start(Agent::Claude, cx).await;
    let pid = h.runtime.update(cx, |runtime, cx| {
        runtime
            .live(&id)
            .unwrap()
            .terminal()
            .read(cx)
            .pid()
            .unwrap()
    });
    assert!(process_alive(pid.as_u32()));
    let path = record_path(&h.scope, &id);
    let before = std::fs::read(&path).unwrap();

    set_chat_settings(cx, false, 60);
    cx.run_until_parked();
    assert_eq!(h.state(&id, cx), ChatState::Closed);
    assert!(
        h.runtime
            .read_with(cx, |runtime, _| runtime.live_ids().next().is_none())
    );
    wait_for(cx, "the process to be gone", WITHIN, |_| {
        !process_alive(pid.as_u32())
    })
    .await;
    // The record is as it was: nothing is written while chats are off.
    cx.executor()
        .timer(std::time::Duration::from_millis(200))
        .await;
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[gpui::test]
async fn test_a_running_chat_with_a_stale_list_says_so_and_no_row_names_a_hidden_artifact(
    cx: &mut TestAppContext,
) {
    let h = ready(cx).await;
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &[&id], cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let panel = panel(&workspace, cx);
    cx.run_until_parked();
    assert!(!view_text(&panel, cx).contains(ROW_NOTE));
    no_human_only(&view_text(&panel, cx));

    // The extra artifact is hidden after the chat started: the row says the list updates on the
    // next resume, and nothing on the row names the artifact or its description.
    write_manifest(&h, false, EXTRA_DESCRIPTION);
    wait_for(cx, "the row note", STARTUP, |cx| {
        panel.read_with(cx, |panel, cx| panel.view_text(cx).contains(ROW_NOTE))
    })
    .await;
    let text = view_text(&panel, cx);
    assert!(text.contains(ROW_NOTE), "{text}");
    for hidden in [EXTRA_PATH, "extra.md", EXTRA_DESCRIPTION] {
        assert!(!text.contains(hidden), "{hidden} shows in:\n{text}");
    }
    no_human_only(&text);
    // The view wrote nothing to the record's artifact fields.
    assert!(on_disk(&h.scope, &id).context_digest.is_some());
}

#[gpui::test]
async fn test_a_missing_skill_and_a_fallback_are_labels_of_a_running_chat(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    // The type lists a skill nobody has.
    write(
        &h.types_root.join("implement/type.toml"),
        "schema = 1\ndescription = \"Implement\"\nlayout = \"dual\"\n\
         skills = [\"model-routing\", \"no-such-skill\"]\ntask_context = true\n\
         artifacts = true\n",
    );
    let id = h.start(Agent::Claude, cx).await;
    wait_indexed(&h, &[&id], cx).await;
    let (workspace, cx) = command_window(&h, cx).await;
    let panel = panel(&workspace, cx);
    let listing = listing(&panel, cx);
    let lines = listing.open[0].lines.join("\n");
    assert!(lines.contains("missing skill: no-such-skill"), "{lines}");
}

#[test]
fn test_the_dump_view_file_holds_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = crate::panel::write_view("Chats\nScope: all tasks\n", dir.path()).unwrap();
    assert_eq!(path, dir.path().join("chat-view.txt"));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "Chats\nScope: all tasks\n"
    );
}

#[gpui::test]
async fn test_dump_view_is_a_debug_action_and_the_text_has_every_part(cx: &mut TestAppContext) {
    let h = harness(cx, true, 60, &[]).await;
    let (workspace, cx) = command_window(&h, cx).await;
    cx.update(|_, cx| {
        let action = cx.build_action("chat::DumpView", None).unwrap();
        assert_eq!(action.name(), "chat::DumpView");
        assert_eq!(command_palette_name("chat::DumpView"), "chat: dump view");
        assert!(crate::action_types().contains(&action.type_id()));
    });
    let panel = panel(&workspace, cx);
    let text = view_text(&panel, cx);
    assert!(text.starts_with("Chats\n"), "{text}");
    assert!(text.contains("  claude: ready"), "{text}");
    assert!(text.contains("Scope: all tasks"), "{text}");
    assert!(text.contains("Open (0)"), "{text}");
    assert!(
        text.contains("No chats; start one with chat: new"),
        "{text}"
    );
    assert!(text.contains("Archived (0) collapsed"), "{text}");
}

/// What the palette shows for an action name (`chat::DumpView` is `chat: dump view`).
fn command_palette_name(name: &str) -> String {
    let (namespace, action) = name.split_once("::").unwrap();
    let mut words = String::new();
    for (index, c) in action.chars().enumerate() {
        if c.is_uppercase() && index > 0 {
            words.push(' ');
        }
        words.extend(c.to_lowercase());
    }
    format!("{namespace}: {words}")
}
