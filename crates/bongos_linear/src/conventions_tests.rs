//! Tests of the PR conventions glue: the section a linked leaf's chats get in `context.md` (and
//! that no other task gets one), the context extension while the sync is on and off, and
//! `task: copy pr title`.

use bongos_chat::ContextExtension;
use gpui::TestAppContext;

use crate::actions_tests::{dispatch, set_enabled_in};
use crate::conventions::{LinearContext, NO_LINK, section_for};
use crate::stacks_tests::{Spec, id, tree};
use crate::status_tests::start;

#[test]
fn a_linked_leaf_gets_the_title_rule() {
    let (_dir, tree) = tree(&[Spec::leaf("a", "Proration engine").linked("SEQ-121")]);
    let section = section_for(&tree, &id("a")).unwrap();
    assert!(section.starts_with("## Linear\n\n"), "{section}");
    assert!(
        section.contains("This task is Linear SEQ-121."),
        "{section}"
    );
    assert!(
        section.contains(
            "Start the PR title with `SEQ-121 ` (for example `SEQ-121 Proration engine`)"
        ),
        "{section}"
    );
    assert!(!section.contains("Closes"), "nothing is stacked on it");
}

#[test]
fn an_unlinked_pending_or_hand_set_leaf_gets_no_section() {
    let (_dir, tree) = tree(&[
        Spec::leaf("plain", "Plain"),
        Spec::leaf("pending", "Pending").pending(),
    ]);
    assert_eq!(section_for(&tree, &id("plain")), None);
    assert_eq!(section_for(&tree, &id("pending")), None);
    assert_eq!(section_for(&tree, "no-such-task"), None);
}

#[test]
fn a_parent_lists_the_leaves_stacked_on_it() {
    let (_dir, tree) = tree(&[
        Spec::leaf("a", "Base").linked("SEQ-121"),
        Spec::leaf("b", "Next").linked("SEQ-122").stacked_on("a"),
        Spec::leaf("c", "Last").linked("SEQ-123").stacked_on("b"),
        Spec::leaf("u", "Not linked").stacked_on("a"),
    ]);
    let section = section_for(&tree, &id("a")).unwrap();
    assert!(section.contains("SEQ-122, SEQ-123"), "{section}");
    assert!(section.contains("`Closes SEQ-122`"), "{section}");
    assert!(!section.contains("Not linked"), "{section}");
    // The top of the stack has nobody stacked on it.
    assert!(!section_for(&tree, &id("c")).unwrap().contains("Closes"));
}

#[test]
fn a_section_holds_identifiers_and_goals_only() {
    let (_dir, tree) = tree(&[Spec::leaf("a", "Proration engine").linked("SEQ-121")]);
    let section = section_for(&tree, &id("a")).unwrap();
    assert!(!section.contains("linear.app"), "no URL: {section}");
    assert!(!section.contains(".md"), "no artifact: {section}");
}

#[gpui::test]
async fn the_extension_adds_a_section_only_for_a_linked_leaf_while_on(cx: &mut TestAppContext) {
    let (_env, cx) = start(cx).await;
    // `beta` (l2) is linked to SEQ-5; `alpha` (l1) is not linked; `hand` (l4) has a hand-set link.
    let section = |task: Option<&str>, cx: &mut gpui::VisualTestContext| {
        let task = task.map(str::to_string);
        cx.update(|_, cx| LinearContext.section("code", task.as_deref(), cx))
    };
    let linked = section(Some("l2"), cx).expect("a linked leaf has a section");
    assert!(linked.contains("Linear SEQ-5"), "{linked}");
    assert_eq!(section(Some("l1"), cx), None);
    assert_eq!(section(Some("l4"), cx), None);
    assert_eq!(section(None, cx), None);

    set_enabled_in(cx, false);
    assert_eq!(
        section(Some("l2"), cx),
        None,
        "nothing while the sync is off"
    );
}

fn clipboard(cx: &mut gpui::VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

#[gpui::test]
async fn copy_pr_title_copies_the_title_of_a_linked_task(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".to_string()));

    env.focus("l2", cx);
    env.clear_toast(cx);
    dispatch("task::CopyPrTitle", cx);
    let copied = clipboard(cx).unwrap_or_default();
    assert!(copied.starts_with("SEQ-5 ") && copied.len() > 6, "{copied}");
    assert!(
        env.last_toast(cx).starts_with("Copied PR title"),
        "{}",
        env.last_toast(cx)
    );

    // An unlinked task and a hand-set link have no title: the clipboard stays.
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".to_string()));
    for focus in ["l1", "l4"] {
        env.focus(focus, cx);
        env.clear_toast(cx);
        dispatch("task::CopyPrTitle", cx);
        assert_eq!(clipboard(cx).as_deref(), Some("before"), "{focus}");
        assert_eq!(env.last_toast(cx), NO_LINK, "{focus}");
    }
}

#[gpui::test]
async fn copy_pr_title_says_the_sync_is_off(cx: &mut TestAppContext) {
    let (env, cx) = start(cx).await;
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("before".to_string()));
    set_enabled_in(cx, false);
    env.focus("l2", cx);
    dispatch("task::CopyPrTitle", cx);
    assert_eq!(clipboard(cx).as_deref(), Some("before"));
    assert_eq!(env.last_toast(cx), crate::actions::OFF_MESSAGE);
}
