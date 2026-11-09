//! `task: propose` and `task: toggle auto apply` in a real window over a temporary work tree read
//! by the real store (the harness of the view's tests).

#![cfg(unix)]

use std::path::Path;

use bongos_builder_model::LiveSet;
use bongos_builder_model::print::COMMENT;
use fs::RealFs;
use gpui::{TestAppContext, UpdateGlobal as _, VisualTestContext};
use settings::{Settings as _, SettingsStore};
use tempfile::TempDir;

use crate::actions::LastToast;
use crate::auto_apply::{auto_apply_allowed, effective_auto_apply, toggled_message};
use crate::propose::{LastPaste, hint, is_untouched, prefilled, propose_on, write_next};
use crate::view_tests::{LAYERS, dispatch, harness, proposal, snapshot, wait_for_in};

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn allowed(cx: &mut VisualTestContext) -> bool {
    let tree = snapshot(cx);
    cx.update(|_, cx| auto_apply_allowed(&tree, "b", cx))
}

fn set_global_auto_apply(cx: &mut VisualTestContext, on: bool) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_builder.get_or_insert_default().auto_apply = Some(on);
            });
        });
    });
}

async fn toggle_and_wait(cx: &mut VisualTestContext, on: bool) {
    cx.update(|_, cx| cx.set_global(LastToast(String::new())));
    dispatch(cx, "task::ToggleAutoApply");
    let expected = toggled_message("acme", on);
    wait_for_in(cx, &expected, |cx| {
        cx.update(|_, cx| cx.try_global::<LastToast>().map(|toast| toast.0.clone()))
            .as_deref()
            == Some(expected.as_str())
    })
    .await;
}

#[gpui::test]
async fn propose_writes_the_next_number_pre_filled_and_never_overwrites(cx: &mut TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[proposal(1, LAYERS, false)]).await;
    dispatch(cx, "task::Propose");
    let second = h.file(2);
    wait_for_in(cx, "proposal 2", |_| {
        read(&second).is_some_and(|text| !text.is_empty())
    })
    .await;

    assert_eq!(
        read(&h.file(1)).as_deref(),
        Some(LAYERS),
        "proposal 1 changed"
    );
    let text = read(&second).unwrap();
    assert!(
        text.starts_with(&format!("{COMMENT}\nproposal 2\nfor b\nby kyle\nwhy\n\n")),
        "{text}"
    );
    assert!(text.contains("\ntask billing: Billing | id b"), "{text}");
    assert!(text.contains("rates: Rates | id r"), "{text}");

    // Untouched, it changes nothing; with a new task it does.
    let tree = snapshot(cx);
    assert!(is_untouched(&text, &tree, &LiveSet::default()));
    let with_child = format!("{text}  task more: More work\n");
    assert!(
        !is_untouched(&with_child, &tree, &LiveSet::default()),
        "{with_child}"
    );

    // A second propose takes the next number again.
    dispatch(cx, "task::Propose");
    let third = h.file(3);
    wait_for_in(cx, "proposal 3", |_| {
        read(&third).is_some_and(|text| !text.is_empty())
    })
    .await;
    assert_eq!(read(&second).unwrap(), text, "proposal 2 changed");
    assert!(read(&third).unwrap().contains("\nproposal 3\n"));
}

#[gpui::test]
async fn write_next_skips_numbers_taken_meanwhile(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let task = temp.path().join("task");
    std::fs::create_dir_all(task.join("proposals")).unwrap();
    for number in [3, 4] {
        std::fs::write(
            task.join(format!("proposals/{number}.proposal.txt")),
            "taken",
        )
        .unwrap();
    }
    let fs = RealFs::new(None, cx.executor());
    let (number, path) = write_next(&fs, &task, 3, |n| format!("proposal {n}\n"))
        .await
        .unwrap();
    assert_eq!(number, 5);
    assert_eq!(path, task.join("proposals/5.proposal.txt"));
    assert_eq!(read(&path).as_deref(), Some("proposal 5\n"));
    for number in [3, 4] {
        assert_eq!(
            read(&task.join(format!("proposals/{number}.proposal.txt"))).as_deref(),
            Some("taken")
        );
    }
}

#[test]
fn prefilled_rewrites_only_the_header() {
    let printed = "# c\nproposal 4\nfor b\nby kyle\nwhy\n\ntask billing: Billing | id b\n  \
                   todo by kyle\n";
    assert_eq!(
        prefilled(printed, 6, "by chat c-1"),
        "# c\nproposal 6\nfor b\nby chat c-1\nwhy\n\ntask billing: Billing | id b\n  \
         todo by kyle\n"
    );
}

#[gpui::test]
async fn a_proposal_for_a_chat_names_it_and_the_hint_is_pasted_without_submitting(
    cx: &mut TestAppContext,
) {
    let (h, workspace, cx) = harness(cx, true, Some("b"), &[]).await;
    cx.update(|_, cx| bongos_chat::ChatsSettings::register(cx));
    workspace.update_in(cx, |workspace, window, cx| {
        propose_on(
            "b".to_string(),
            Some("chat-7".to_string()),
            workspace,
            window,
            cx,
        )
    });
    wait_for_in(cx, "the paste", |cx| {
        cx.update(|_, cx| cx.has_global::<LastPaste>())
    })
    .await;
    let (chat, text, submitted) = cx.update(|_, cx| {
        let paste = cx.global::<LastPaste>();
        (paste.0.clone(), paste.1.clone(), paste.2)
    });
    assert_eq!(chat, "chat-7");
    assert_eq!(text, hint(1, &h.file(1)));
    assert!(!submitted, "the hint was submitted");
    let written = read(&h.file(1)).unwrap();
    assert!(
        written.contains("\nproposal 1\nfor b\nby chat chat-7\n"),
        "{written}"
    );
    assert!(is_untouched(&written, &snapshot(cx), &LiveSet::default()));
}

#[test]
fn the_project_value_wins_over_the_setting() {
    let mut project = bongos_work::model::ProjectFile::new("Acme");
    assert!(effective_auto_apply(None, true));
    assert!(!effective_auto_apply(None, false));
    assert!(effective_auto_apply(Some(&project), true));
    assert!(!effective_auto_apply(Some(&project), false));
    project.auto_apply_proposals = Some(false);
    assert!(!effective_auto_apply(Some(&project), true));
    project.auto_apply_proposals = Some(true);
    assert!(effective_auto_apply(Some(&project), false));
}

#[gpui::test]
async fn the_project_switch_and_the_setting_decide_auto_apply_allowed(cx: &mut TestAppContext) {
    let (h, _workspace, cx) = harness(cx, true, Some("b"), &[]).await;
    let project_toml = h.billing.ancestors().nth(2).unwrap().join("project.toml");
    // The setting alone, while the project says nothing.
    assert!(allowed(cx));
    set_global_auto_apply(cx, false);
    assert!(!allowed(cx));
    set_global_auto_apply(cx, true);
    assert!(allowed(cx));

    // The project's switch wins over the setting.
    toggle_and_wait(cx, false).await;
    assert!(
        read(&project_toml)
            .unwrap()
            .contains("auto_apply_proposals = false"),
        "{:?}",
        read(&project_toml)
    );
    assert!(!allowed(cx));
    set_global_auto_apply(cx, false);
    toggle_and_wait(cx, true).await;
    assert!(
        read(&project_toml)
            .unwrap()
            .contains("auto_apply_proposals = true")
    );
    assert!(allowed(cx));

    // Nothing applies on its own while the builder is off.
    crate::view_tests::set_settings_in(cx, false);
    assert!(!allowed(cx));
}
