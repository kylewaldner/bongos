//! Closing the last window with live runs, through a recording [`LastWindowClose`].

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use futures::channel::oneshot;
use gpui::{App, AppContext as _, Task, TestAppContext};

use crate::{
    CloseFirst, CloseOutcome, LAST_WINDOW_CHOICES, LanesOnLastWindowClose, LastWindowAnswer,
    LastWindowChoice, LastWindowClose, answer_of, close_first, close_last_window,
};

/// Records what closing did, in order; its handover resolves when the test says.
struct FakeCloser {
    log: Rc<RefCell<Vec<String>>>,
    answer: LastWindowAnswer,
    handover: RefCell<Option<oneshot::Receiver<Result<()>>>>,
}

impl LastWindowClose for FakeCloser {
    fn ask(&self, _cx: &mut App) -> Task<LastWindowAnswer> {
        self.log.borrow_mut().push("ask".to_string());
        Task::ready(self.answer)
    }

    fn remember(&self, setting: LanesOnLastWindowClose, _cx: &mut App) {
        self.log.borrow_mut().push(format!("remember {setting:?}"));
    }

    fn hand_over(&self, cx: &mut App) -> Task<Result<()>> {
        self.log.borrow_mut().push("hand over".to_string());
        let handover = self.handover.borrow_mut().take().expect("one handover");
        cx.background_spawn(async move { handover.await.unwrap_or(Ok(())) })
    }

    fn stop(&self, _cx: &mut App) {
        self.log.borrow_mut().push("stop".to_string());
    }
}

struct Closing {
    log: Rc<RefCell<Vec<String>>>,
    outcome: Rc<RefCell<Option<CloseOutcome>>>,
    /// Ends the handover.
    handed: Option<oneshot::Sender<Result<()>>>,
}

/// Closes the last window with `first`, the person answering `answer` if asked; "close" is
/// logged when closing resolves.
fn close(first: CloseFirst, answer: LastWindowAnswer, cx: &mut TestAppContext) -> Closing {
    let log = Rc::new(RefCell::new(Vec::new()));
    let (handed, handover) = oneshot::channel();
    let closer = Rc::new(FakeCloser {
        log: log.clone(),
        answer,
        handover: RefCell::new(Some(handover)),
    });
    let outcome = Rc::new(RefCell::new(None));
    let (ended, ended_log) = (outcome.clone(), log.clone());
    cx.update(|cx| {
        let closing = close_last_window(first, closer, cx);
        cx.spawn(async move |_| {
            let result = closing.await;
            ended_log.borrow_mut().push("close".to_string());
            *ended.borrow_mut() = Some(result);
        })
        .detach();
    });
    cx.run_until_parked();
    Closing {
        log,
        outcome,
        handed: Some(handed),
    }
}

fn logged(closing: &Closing) -> Vec<String> {
    closing.log.borrow().clone()
}

const NO_ANSWER: LastWindowAnswer = LastWindowAnswer {
    choice: LastWindowChoice::Cancel,
    remember: false,
};

// `keep`: the handover starts with no question, and the window closes only once it ended.
#[gpui::test]
fn closing_the_last_window_with_keep_hands_over_first(cx: &mut TestAppContext) {
    let first = close_first(LanesOnLastWindowClose::Keep, true, 1);
    assert_eq!(first, CloseFirst::Keep);
    let mut closing = close(first, NO_ANSWER, cx);
    assert_eq!(logged(&closing), ["hand over"]);
    assert_eq!(
        *closing.outcome.borrow(),
        None,
        "not closed during the handover"
    );

    closing.handed.take().unwrap().send(Ok(())).unwrap();
    cx.run_until_parked();
    assert_eq!(logged(&closing), ["hand over", "close"]);
    assert_eq!(*closing.outcome.borrow(), Some(CloseOutcome::Close));
}

// A handover that failed (the writer was taken back) keeps the window open and says why.
#[gpui::test]
fn a_failed_handover_keeps_the_last_window_open(cx: &mut TestAppContext) {
    let mut closing = close(CloseFirst::Keep, NO_ANSWER, cx);
    closing
        .handed
        .take()
        .unwrap()
        .send(Err(anyhow::anyhow!(
            "the engine host did not start; see host.log"
        )))
        .unwrap();
    cx.run_until_parked();
    let Some(CloseOutcome::StayOpen(Some(message))) = closing.outcome.borrow().clone() else {
        panic!("the window closed after a failed handover");
    };
    assert!(message.contains("did not start; see host.log"), "{message}");
}

// `stop`: the runs are interrupted and the window closes, with no question and no handover.
#[gpui::test]
fn closing_the_last_window_with_stop_interrupts_and_closes(cx: &mut TestAppContext) {
    let first = close_first(LanesOnLastWindowClose::Stop, true, 2);
    assert_eq!(first, CloseFirst::Stop);
    let closing = close(first, NO_ANSWER, cx);
    assert_eq!(logged(&closing), ["stop", "close"]);
    assert_eq!(*closing.outcome.borrow(), Some(CloseOutcome::Close));
}

// `ask`: "Always keep running" writes the setting, then hands over before closing.
#[gpui::test]
fn always_keep_running_remembers_then_hands_over_before_closing(cx: &mut TestAppContext) {
    let first = close_first(LanesOnLastWindowClose::Ask, true, 1);
    assert_eq!(first, CloseFirst::Ask);
    let mut closing = close(first, answer_of(Some(2)), cx);
    assert_eq!(logged(&closing), ["ask", "remember Keep", "hand over"]);
    closing.handed.take().unwrap().send(Ok(())).unwrap();
    cx.run_until_parked();
    assert_eq!(
        logged(&closing),
        ["ask", "remember Keep", "hand over", "close"]
    );
}

// `ask`: Stop interrupts and closes without remembering; Cancel leaves the window open.
#[gpui::test]
fn asked_stop_interrupts_and_cancel_keeps_the_window(cx: &mut TestAppContext) {
    let closing = close(CloseFirst::Ask, answer_of(Some(1)), cx);
    assert_eq!(logged(&closing), ["ask", "stop", "close"]);
    assert_eq!(*closing.outcome.borrow(), Some(CloseOutcome::Close));

    let closing = close(CloseFirst::Ask, answer_of(Some(4)), cx);
    assert_eq!(logged(&closing), ["ask", "close"]);
    assert_eq!(
        *closing.outcome.borrow(),
        Some(CloseOutcome::StayOpen(None))
    );
}

// Only the last window with live runs does anything first.
#[test]
fn only_the_last_window_with_live_runs_does_anything_first() {
    for setting in [
        LanesOnLastWindowClose::Ask,
        LanesOnLastWindowClose::Keep,
        LanesOnLastWindowClose::Stop,
    ] {
        assert_eq!(close_first(setting, false, 3), CloseFirst::Nothing);
        assert_eq!(close_first(setting, true, 0), CloseFirst::Nothing);
    }
}

// Each choice of the prompt reads as its answer; a dismissed prompt cancels.
#[test]
fn the_prompt_choices_read_as_answers() {
    let read: Vec<_> = (0..LAST_WINDOW_CHOICES.len())
        .map(|index| answer_of(Some(index)))
        .map(|answer| (answer.choice, answer.remember))
        .collect();
    assert_eq!(
        read,
        [
            (LastWindowChoice::Keep, false),
            (LastWindowChoice::Stop, false),
            (LastWindowChoice::Keep, true),
            (LastWindowChoice::Stop, true),
            (LastWindowChoice::Cancel, false),
        ]
    );
    assert_eq!(answer_of(None).choice, LastWindowChoice::Cancel);
}
