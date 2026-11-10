//! Tests of the row chips of a linked task: the words, their order, and that nothing shows while
//! the sync is off or for a task the sync has not read.

use gpui::TestAppContext;

use crate::actions_tests::{set_enabled, use_default_settings};
use crate::chips::chips_for;
use crate::sync::{self, ReadState};
use bongos_work::RowChipsRegistry;

fn words(read: Option<ReadState>, suggestions: usize) -> Vec<String> {
    chips_for(read, suggestions)
        .into_iter()
        .map(|chip| chip.to_string())
        .collect()
}

#[test]
fn each_state_has_its_chip() {
    assert!(words(None, 0).is_empty());
    assert!(words(Some(ReadState::Fresh { at: 1 }), 0).is_empty());
    assert_eq!(
        words(Some(ReadState::Unreachable), 0),
        ["linear unreachable"]
    );
    assert_eq!(
        words(Some(ReadState::RateLimited), 0),
        ["linear rate limited"]
    );
    assert_eq!(words(Some(ReadState::NotFound), 0), ["linear: not found"]);
}

#[test]
fn suggestions_are_counted_after_the_state() {
    assert_eq!(words(Some(ReadState::Fresh { at: 1 }), 1), ["1 suggestion"]);
    assert_eq!(words(None, 3), ["3 suggestions"]);
    assert_eq!(
        words(Some(ReadState::Unreachable), 2),
        ["linear unreachable", "2 suggestions"]
    );
}

#[gpui::test]
async fn the_registry_asks_the_sync_and_hides_while_off(cx: &mut TestAppContext) {
    use_default_settings(cx);
    set_enabled(cx, true);
    let linear = cx.update(|cx| sync::global(cx).unwrap());
    linear.update(cx, |linear, _| {
        linear.set_linked(["a".to_string(), "b".to_string()]);
        linear.set_read("a", ReadState::Unreachable, 1);
        linear.set_read("zzz", ReadState::Unreachable, 1);
    });
    let chips = |id: &str, cx: &mut TestAppContext| -> Vec<String> {
        cx.update(|cx| {
            RowChipsRegistry::chips(id, cx)
                .into_iter()
                .map(|chip| chip.to_string())
                .collect()
        })
    };
    assert_eq!(chips("a", cx), ["linear unreachable"]);
    assert!(chips("b", cx).is_empty());
    assert!(
        chips("zzz", cx).is_empty(),
        "a task that is not linked has no read state"
    );
    set_enabled(cx, false);
    assert!(chips("a", cx).is_empty());
}
