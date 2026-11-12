//! The large-file gate: typing a search string one character at a time into a session on a
//! 100 MB buffer. The synchronous handler of the input is what the main thread pays for, so it
//! is measured on its own; the search runs on the background executor and is only awaited.

use crate::actions::Forward;
use crate::session::test_session;
use crate::session_tests::{input, selections, start, status, type_slowly, workspace_with};
use editor::Editor;
use gpui::{AppContext as _, TestAppContext};
use language::Buffer;
use multi_buffer::MultiBuffer;
use std::time::Duration;

const SIZE: usize = 100 * 1024 * 1024;
const BLOCK: usize = 1024 * 1024;
const TYPED: &str = "largefileneedlezzzzz";

/// 100 MB of lowercase lines of 79 letters and a newline, a random 1 MB block from a fixed seed
/// repeated, ending in `TYPED` and a newline: the only place the whole string occurs.
fn large_text() -> String {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut block = String::with_capacity(BLOCK);
    while block.len() < BLOCK {
        for _ in 0..79 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            block.push(char::from(b'a' + ((state >> 33) % 26) as u8));
        }
        block.push('\n');
    }
    let mut text = String::with_capacity(SIZE);
    while text.len() + BLOCK <= SIZE {
        text.push_str(&block);
    }
    let end = text.len() - 1 - TYPED.len();
    text.replace_range(end..end + TYPED.len(), TYPED);
    text
}

/// Twenty characters typed one by one into a session on 100 MB: no `on_input` takes over 2 ms on
/// the main thread, and once the background search is done the view is on the only match, at the
/// end of the buffer.
#[gpui::test]
async fn test_large_file_typing_stays_responsive(cx: &mut TestAppContext) {
    assert_eq!(TYPED.len(), 20);
    let text = large_text();
    let length = text.len();
    assert!(length > 99 * 1024 * 1024, "{length} bytes");
    let (workspace, editor, mut cx) = workspace_with(false, cx, move |project, window, cx| {
        let buffer = cx.new(|cx| Buffer::local(text, cx));
        let multibuffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
        cx.new(|cx| Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx))
    })
    .await;
    let cx = &mut cx;

    start(Forward, &workspace, cx);
    type_slowly(TYPED, cx);
    cx.run_until_parked();

    assert_eq!(input(&workspace, cx), TYPED, "every character is kept");
    let state = test_session(&workspace, cx).expect("the session is open");
    let stats = state.borrow().stats.clone();
    assert_eq!(stats.inputs.len(), TYPED.len());
    let slowest = stats.inputs.iter().max().copied().unwrap_or_default();
    assert!(
        slowest < Duration::from_millis(2),
        "on_input took {slowest:?}"
    );
    let end = length - 1;
    assert_eq!(selections(&editor, cx), vec![end..end]);
    assert_eq!(status(&workspace, cx), "1/1");
    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();
}
