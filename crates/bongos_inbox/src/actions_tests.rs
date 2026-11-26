//! Actions and targets in a real window over a real inbox directory: `a` runs the first action of
//! the item that is registered (`accept` and `done` archive, `overturn` opens the target in a
//! writable editor at its line, `open-task` focuses a task), an action nobody registered is not
//! shown and `a` says there is no primary action, `a` on a group row says to expand it, a
//! registered handler runs by name, and `o` says a stale target is gone.

use super::badge_tests::{Env, Win, env, window_in};
use super::targets::{Parsed, parse};
use super::tests::{item, wait_for};
use super::view_tests::{fixed_now, messages, open_view};
use super::*;
use editor::Editor;
use gpui::TestAppContext;
use language::Point;
use std::cell::{Cell, RefCell};

const SCHEMES: &[&str] = &["task", "run", "jira"];

fn parsed(target: &str) -> Parsed {
    parse(target, &|scheme| SCHEMES.contains(&scheme))
}

#[test]
fn targets_read_as_paths_with_a_line_schemes_or_unknown() {
    let path = |text: &str, line| Parsed::Path {
        path: PathBuf::from(text),
        line,
    };
    let relative = |text: &str, line| Parsed::Relative {
        path: PathBuf::from(text),
        line,
    };
    assert_eq!(parsed("/a/b.md"), path("/a/b.md", None));
    assert_eq!(parsed("/a/b.md:12"), path("/a/b.md", Some(12)));
    assert_eq!(parsed("plan.md:12"), relative("plan.md", Some(12)));
    assert_eq!(parsed("docs/plan.md:3"), relative("docs/plan.md", Some(3)));
    assert_eq!(parsed("plan.md"), relative("plan.md", None));
    assert_eq!(
        parsed("task:7"),
        Parsed::Scheme {
            scheme: "task".into(),
            rest: "7".into()
        }
    );
    assert_eq!(
        parsed("jira:ABC-1"),
        Parsed::Scheme {
            scheme: "jira".into(),
            rest: "ABC-1".into()
        }
    );
    assert_eq!(
        parsed("linear:ABC-1"),
        Parsed::Unknown {
            scheme: "linear".into()
        }
    );
    let Parsed::Path { path, line } = parsed("~/notes.md:4") else {
        panic!("a home path is a path");
    };
    assert!(path.ends_with("notes.md") && path.is_absolute());
    assert_eq!(line, Some(4));
}

/// Delivers an item of kind `kind` with `actions` and `target`, created `minutes_ago` before the
/// tests' clock.
fn deliver_with(
    env: &Env,
    summary: &str,
    actions: &[&str],
    target: Option<&str>,
    group: Option<&str>,
    minutes_ago: i64,
) {
    let mut item = item(summary);
    item.kind = "decision-default".into();
    item.source = format!("agent/{summary}");
    item.actions = actions.iter().map(|action| action.to_string()).collect();
    item.target = target.map(str::to_string);
    item.group = group.map(str::to_string);
    item.created = fixed_now() - chrono::Duration::minutes(minutes_ago);
    maildir::deliver(&env.root, &item).unwrap();
}

/// An enabled inbox, a fixed clock and a window whose project has `<temp>/proj` open, where
/// `plan.md` (20 lines) is on the real disk and in the window's file system.
async fn setup(cx: &mut TestAppContext) -> (Env, Win, PathBuf) {
    let env = env(cx).await;
    cx.update(|cx| set_clock(fixed_now, cx));
    let proj = env.root.parent().unwrap().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let text: String = (1..=20).map(|line| format!("line {line}\n")).collect();
    std::fs::write(proj.join("plan.md"), &text).unwrap();
    let fake = env.app_state.fs.as_fake();
    fake.create_dir(&proj).await.unwrap();
    fake.insert_file(proj.join("plan.md"), text.into_bytes())
        .await;
    let win = window_in(cx, &env, false, &[proj.as_path()]).await;
    (env, win, proj)
}

async fn wait_unread(cx: &mut TestAppContext, env: &Env, count: usize) {
    env.wait_unread(cx, count).await;
}

async fn wait_archived(cx: &mut TestAppContext, env: &Env) {
    wait_for(cx, &env.inbox, "the item archived", |inbox| {
        inbox.snapshot().is_some_and(|snapshot| snapshot.is_empty())
    })
    .await;
    let archived = std::fs::read_dir(env.root.join(ARCHIVE)).unwrap().count();
    assert_eq!(archived, 1, "the item is in archive/");
}

fn action_names(win: &Win, view: &Entity<InboxView>) -> Vec<String> {
    view.read_with(&win.cx, |view, cx| {
        view.detail(cx)
            .unwrap()
            .actions
            .into_iter()
            .map(|cap| cap.name)
            .collect()
    })
}

fn editor_state(win: &mut Win) -> Option<(bool, u32)> {
    let editor = win.workspace.read_with(&win.cx, |workspace, cx| {
        workspace.active_item_as::<Editor>(cx)
    })?;
    Some(editor.update(&mut win.cx, |editor, cx| {
        let snapshot = editor.display_snapshot(cx);
        let row = editor.selections.newest::<Point>(&snapshot).head().row;
        (editor.read_only(cx), row)
    }))
}

#[gpui::test]
async fn a_runs_accept_on_a_decision_default_item_and_archives_it(cx: &mut TestAppContext) {
    let (env, mut win, _) = setup(cx).await;
    deliver_with(&env, "pick", &["accept", "overturn"], None, None, 5);
    wait_unread(cx, &env, 1).await;
    let view = open_view(&mut win);
    assert_eq!(
        action_names(&win, &view),
        ["accept", "overturn", "open", "dismiss", "mark-unread"]
    );
    let keys: Vec<(String, String)> = view.read_with(&win.cx, |view, cx| {
        let actions = view.detail(cx).unwrap().actions;
        actions
            .into_iter()
            .map(|cap| (cap.key, cap.label))
            .collect()
    });
    assert_eq!(keys[0], ("a".into(), "Accept".into()));
    assert_eq!(keys[1], ("o".into(), "Overturn".into()));
    assert_eq!(keys[3], ("d".into(), "Dismiss".into()));

    win.cx.dispatch_action(Accept);
    wait_archived(cx, &env).await;
}

#[gpui::test]
async fn done_archives(cx: &mut TestAppContext) {
    let (env, mut win, _) = setup(cx).await;
    deliver_with(&env, "check", &["done"], None, None, 5);
    wait_unread(cx, &env, 1).await;
    open_view(&mut win);

    win.cx.dispatch_action(Accept);
    wait_archived(cx, &env).await;
}

#[gpui::test]
async fn overturn_opens_the_target_writable_at_its_line_and_marks_read(cx: &mut TestAppContext) {
    let (env, mut win, proj) = setup(cx).await;
    let target = format!("{}:7", proj.join("plan.md").display());
    deliver_with(&env, "revisit", &["overturn"], Some(&target), None, 5);
    wait_unread(cx, &env, 1).await;
    open_view(&mut win);

    win.cx.dispatch_action(Accept);
    win.cx.run_until_parked();
    win.cx.run_until_parked();
    assert_eq!(
        editor_state(&mut win),
        Some((false, 6)),
        "writable, on line 7"
    );
    wait_unread(cx, &env, 0).await;
    let snapshot = env
        .inbox
        .read_with(cx, |inbox, _| inbox.snapshot().unwrap());
    assert_eq!(snapshot.len(), 1, "the item stays until it is dismissed");
}

#[gpui::test]
async fn overturn_resolves_a_relative_target_against_an_open_worktree(cx: &mut TestAppContext) {
    let (env, mut win, _) = setup(cx).await;
    deliver_with(&env, "revisit", &["overturn"], Some("plan.md:12"), None, 5);
    wait_unread(cx, &env, 1).await;
    let snapshot = env
        .inbox
        .read_with(cx, |inbox, _| inbox.snapshot().unwrap());
    assert_eq!(snapshot.target(0), Some(TargetState::Live));
    open_view(&mut win);

    win.cx.dispatch_action(Accept);
    win.cx.run_until_parked();
    win.cx.run_until_parked();
    assert_eq!(
        editor_state(&mut win),
        Some((false, 11)),
        "writable, on line 12"
    );
    wait_unread(cx, &env, 0).await;
}

#[gpui::test]
async fn open_task_focuses_the_task_of_the_target(cx: &mut TestAppContext) {
    let (env, mut win, _) = setup(cx).await;
    let focused = Rc::new(RefCell::new(Vec::<String>::new()));
    cx.update(|cx| {
        let focused = focused.clone();
        InboxActions::set_task_focus(
            move |id, _, _, _| focused.borrow_mut().push(id.to_string()),
            cx,
        );
    });
    deliver_with(&env, "look", &["open-task"], Some("task:t1"), None, 5);
    wait_unread(cx, &env, 1).await;
    open_view(&mut win);

    win.cx.dispatch_action(Accept);
    win.cx.run_until_parked();
    assert_eq!(*focused.borrow(), ["t1"]);
    wait_unread(cx, &env, 0).await;
}

#[gpui::test]
async fn an_unregistered_action_is_not_shown_and_a_says_no_primary_action(cx: &mut TestAppContext) {
    let (env, mut win, _) = setup(cx).await;
    deliver_with(&env, "promote", &["promote-to-profile"], None, None, 5);
    wait_unread(cx, &env, 1).await;
    let view = open_view(&mut win);
    assert_eq!(
        action_names(&win, &view),
        ["open", "dismiss", "mark-unread"],
        "only open and dismiss besides mark unread"
    );

    win.cx.dispatch_action(Accept);
    win.cx.run_until_parked();
    assert_eq!(messages(&mut win), [actions::NO_PRIMARY_ACTION]);
    let snapshot = env
        .inbox
        .read_with(cx, |inbox, _| inbox.snapshot().unwrap());
    assert_eq!(snapshot.len(), 1, "nothing ran");
}

#[gpui::test]
async fn a_registered_handler_runs_by_name(cx: &mut TestAppContext) {
    let (env, mut win, _) = setup(cx).await;
    let calls = Rc::new(Cell::new(0));
    let seen = Rc::new(RefCell::new(None));
    cx.update(|cx| {
        let (calls, seen) = (calls.clone(), seen.clone());
        InboxActions::register(
            "ping",
            move |item, _, _| {
                calls.set(calls.get() + 1);
                *seen.borrow_mut() = Some(item.actions.clone());
                ActionOutcome::Keep
            },
            cx,
        );
    });
    deliver_with(&env, "ping me", &["ping", "accept"], None, None, 5);
    wait_unread(cx, &env, 1).await;
    let view = open_view(&mut win);
    assert_eq!(
        action_names(&win, &view),
        ["ping", "accept", "open", "dismiss", "mark-unread"]
    );

    win.cx.dispatch_action(Accept);
    win.cx.run_until_parked();
    assert_eq!(calls.get(), 1);
    assert_eq!(
        *seen.borrow(),
        Some(vec!["ping".to_string(), "accept".to_string()])
    );
    let snapshot = env
        .inbox
        .read_with(cx, |inbox, _| inbox.snapshot().unwrap());
    assert_eq!(snapshot.len(), 1, "Keep leaves the item");
}

#[gpui::test]
async fn o_on_a_stale_target_says_it_is_gone(cx: &mut TestAppContext) {
    let (env, mut win, proj) = setup(cx).await;
    let target = proj.join("missing.md").display().to_string();
    deliver_with(&env, "moved", &["overturn"], Some(&target), None, 5);
    wait_unread(cx, &env, 1).await;
    let snapshot = env
        .inbox
        .read_with(cx, |inbox, _| inbox.snapshot().unwrap());
    assert_eq!(snapshot.target(0), Some(TargetState::Stale));
    open_view(&mut win);

    win.cx.dispatch_action(OpenTarget);
    win.cx.run_until_parked();
    assert_eq!(
        messages(&mut win),
        [format!("the target is gone: {target}")]
    );
    assert_eq!(editor_state(&mut win), None, "nothing opened");
}

#[gpui::test]
async fn o_on_an_unknown_scheme_says_it_cannot_resolve_it(cx: &mut TestAppContext) {
    let (env, mut win, _) = setup(cx).await;
    deliver_with(&env, "odd", &["accept"], Some("linear:ABC-1"), None, 5);
    wait_unread(cx, &env, 1).await;
    open_view(&mut win);

    win.cx.dispatch_action(OpenTarget);
    win.cx.run_until_parked();
    let said = messages(&mut win);
    assert_eq!(said.len(), 1);
    assert!(said[0].contains("linear:"), "{said:?}");
}

#[gpui::test]
async fn a_on_a_group_row_says_to_expand_the_group_first(cx: &mut TestAppContext) {
    let (env, mut win, _) = setup(cx).await;
    deliver_with(&env, "first", &["accept"], None, Some("batch"), 5);
    deliver_with(&env, "second", &["accept"], None, Some("batch"), 6);
    wait_unread(cx, &env, 2).await;
    let view = open_view(&mut win);
    let group = view.read_with(&win.cx, |view, _| view.selected().unwrap().item.is_none());
    assert!(group, "the cursor is on the group row");

    win.cx.dispatch_action(Accept);
    win.cx.run_until_parked();
    assert_eq!(messages(&mut win), [actions::EXPAND_FIRST]);
    let snapshot = env
        .inbox
        .read_with(cx, |inbox, _| inbox.snapshot().unwrap());
    assert_eq!(snapshot.len(), 2, "nothing was archived");
}
