//! Tests of the hand-off to query replace: the starter receives the search and the session
//! closes; without one the echo area says so and the session stays.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{Entity, TestAppContext, VisualTestContext};
use multi_buffer::ToOffset as _;

use crate::actions::{Forward, QueryReplace, QueryReplaceRegexp};
use crate::seams::{QueryReplaceSeed, set_query_replace_starter};
use crate::session_tests::{
    cursor, input, keys, session_open, set_selections, start, text, type_slowly, workspace_text,
};

const TEXT: &str = "one Two\ntwo three\nTwo\n";

fn register(cx: &mut VisualTestContext) -> Rc<RefCell<Vec<QueryReplaceSeed>>> {
    let seeds = Rc::new(RefCell::new(Vec::new()));
    cx.update(|_, cx| {
        let seeds = seeds.clone();
        set_query_replace_starter(Rc::new(move |seed, _, _| seeds.borrow_mut().push(seed)), cx);
    });
    seeds
}

#[gpui::test]
async fn a_starter_receives_the_search_and_the_session_closes(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(TEXT, true, cx).await;
    let cx = &mut cx;
    let seeds = register(cx);
    set_selections(&editor, vec![0..0], cx);
    start(Forward, &workspace, cx);
    type_slowly("Two", cx);
    cx.dispatch_action(QueryReplace);
    cx.run_until_parked();
    assert!(!session_open(&workspace, cx));
    let seeds = seeds.borrow();
    assert_eq!(seeds.len(), 1);
    let seed = &seeds[0];
    assert_eq!(seed.from, "Two");
    assert!(!seed.regexp);
    assert!(!seed.symbol);
    // An uppercase letter turns smart case into a case-sensitive search; the seed carries the
    // mode as chosen, which is smart case.
    assert_eq!(seed.case_sensitive, None);
    assert_eq!(seed.editor, editor.downgrade());
    // Point is at the match start, "Two" at offset 4.
    assert_eq!(cursor(&editor, cx), 4);
    let start = editor.read_with(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        seed.start.to_offset(&snapshot).0
    });
    assert_eq!(start, 4);
    assert_eq!(text(&editor, cx), TEXT);
}

#[gpui::test]
async fn the_regexp_action_asks_for_a_regexp(cx: &mut TestAppContext) {
    let (workspace, _editor, mut cx) = workspace_text(TEXT, true, cx).await;
    let cx = &mut cx;
    let seeds = register(cx);
    start(Forward, &workspace, cx);
    type_slowly("two", cx);
    cx.dispatch_action(QueryReplaceRegexp);
    cx.run_until_parked();
    assert!(!session_open(&workspace, cx));
    let seeds = seeds.borrow();
    assert_eq!(seeds.len(), 1);
    assert_eq!(seeds[0].from, "two");
    assert!(seeds[0].regexp);
}

#[gpui::test]
async fn without_a_starter_the_echo_area_says_so_and_the_session_stays(cx: &mut TestAppContext) {
    let (workspace, editor, mut cx) = workspace_text(TEXT, true, cx).await;
    let cx = &mut cx;
    cx.update(|_, cx| {
        cx.default_global::<workspace::MinibufferRouting>()
            .set_routes(vec![
                crate::session::HOST.to_string(),
                bongos_minibuffer::MESSAGES.to_string(),
            ]);
    });
    start(Forward, &workspace, cx);
    type_slowly("two", cx);
    cx.dispatch_action(QueryReplace);
    cx.run_until_parked();
    assert!(session_open(&workspace, cx), "the session stays");
    assert_eq!(input(&workspace, cx), "two");
    let strip = cx
        .update(|window, cx| bongos_minibuffer::minibuffer_for_window(window.window_handle(), cx))
        .expect("a strip");
    let said: Vec<String> = strip.read_with(cx, |strip, _| {
        strip
            .messages()
            .into_iter()
            .map(|line| line.text.to_string())
            .collect()
    });
    assert!(
        said.iter()
            .any(|line| line == "Query replace is not available yet"),
        "{said:?}"
    );
    keys("ctrl-g", cx);
    keys("ctrl-g", cx);
    assert!(!session_open(&workspace, cx));
    let _: Entity<_> = editor;
}
