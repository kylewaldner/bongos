//! Tests of the `checks` host command and of the window side that forwards to it: a process
//! without the state writer sends its commands and starts nothing itself, and the process that
//! holds the writer runs them in its engine.

use std::cell::RefCell;
use std::rc::Rc;

use futures::channel::oneshot;
use gpui::{BorrowAppContext as _, Task, TestAppContext, VisualTestContext};
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use workspace::{AppState, MultiWorkspace};

use crate::actions::{LastToast, cancel_checks, run_checks};
use crate::engine::{ChecksEngine, ChecksEvent};
use crate::engine_tests::World;
use crate::host::{
    CHECKS_NAMESPACE, ChecksAccess, ChecksCommand, ChecksForwarderRegistry, ChecksReply,
    checks_access, handle,
};

fn last_toast(cx: &mut VisualTestContext) -> String {
    cx.update(|_, cx| cx.global::<LastToast>().0.clone())
}

fn is_local(cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| matches!(checks_access(cx), Some(ChecksAccess::Local(_))))
}

#[gpui::test]
async fn checks_a_run_in_a_process_without_the_writer_is_forwarded_and_starts_nothing_here(
    cx: &mut TestAppContext,
) {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        crate::init(cx);
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.task_checks.get_or_insert_default().enabled = Some(true);
            });
        });
        app_state
    });
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi, _| multi.workspace().clone())
        .unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    let engine = cx.update(|_, cx| ChecksEngine::global(cx).expect("the engine is installed"));
    let events = Rc::new(RefCell::new(Vec::<ChecksEvent>::new()));
    let seen = events.clone();
    cx.update(|_, cx| {
        cx.subscribe(&engine, move |_, event: &ChecksEvent, _| {
            seen.borrow_mut().push(event.clone())
        })
        .detach()
    });

    // Until a forwarder is registered this process holds the writer.
    assert!(is_local(cx));

    let sent = Rc::new(RefCell::new(Vec::<serde_json::Value>::new()));
    let log = sent.clone();
    cx.update(|_, cx| {
        ChecksForwarderRegistry::set(
            Rc::new(move |payload: serde_json::Value, _: &mut gpui::App| {
                log.borrow_mut().push(payload.clone());
                let reply = match payload["command"].as_str() {
                    Some("run-checks") => ChecksReply::Started,
                    Some("cancel-checks") => ChecksReply::Cancelled { stopped: true },
                    _ => ChecksReply::Hold { reason: None },
                };
                Task::ready(reply.to_value())
            }),
            cx,
        )
    });
    assert!(!is_local(cx));

    workspace.update_in(cx, |workspace, window, cx| {
        run_checks("l1".to_string(), workspace, window, cx)
    });
    cx.run_until_parked();

    assert_eq!(
        *sent.borrow(),
        [json!({ "command": "run-checks", "task_id": "l1", "checks": null, "clean": false })]
    );
    assert_eq!(last_toast(cx), "Running the checks of l1");
    // Nothing started here: no run, no event.
    assert!(!engine.read_with(cx, |engine, _| engine.is_running("l1")));
    assert!(events.borrow().is_empty(), "{:?}", events.borrow());

    workspace.update_in(cx, |workspace, window, cx| {
        cancel_checks("l1".to_string(), workspace, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        sent.borrow()[1],
        json!({ "command": "cancel-checks", "task_id": "l1" })
    );
    assert_eq!(last_toast(cx), "Stopping the checks of l1");

    // Taking the writer back makes the commands local again: this engine answers.
    cx.update(|_, cx| ChecksForwarderRegistry::clear(cx));
    assert!(is_local(cx));
    workspace.update_in(cx, |workspace, window, cx| {
        run_checks("l1".to_string(), workspace, window, cx)
    });
    cx.run_until_parked();
    assert_eq!(sent.borrow().len(), 2, "nothing more was forwarded");
    assert_eq!(last_toast(cx), "l1 has no checks");
}

#[gpui::test]
async fn checks_the_host_command_runs_checks_in_the_writers_engine(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let world = World::new(cx, true);
    assert_eq!(CHECKS_NAMESPACE, "checks");
    let (finished, finishing) = oneshot::channel();
    let finished = RefCell::new(Some(finished));
    cx.update(|cx| {
        cx.subscribe(&world.engine, move |_, event: &ChecksEvent, _| {
            if matches!(event, ChecksEvent::Finished { .. })
                && let Some(finished) = finished.borrow_mut().take()
            {
                finished.send(()).ok();
            }
        })
        .detach()
    });
    let command = ChecksCommand::RunChecks {
        task_id: "l1".to_string(),
        checks: Some(vec!["docs".to_string()]),
        clean: false,
    };
    let payload = serde_json::to_value(&command).unwrap();

    // The second request reaches the engine while the first run is live.
    let (first, second) = cx.update(|cx| {
        (
            handle(payload.clone(), true, cx),
            handle(payload.clone(), false, cx),
        )
    });
    let first = ChecksReply::from_value(first.await.unwrap()).unwrap();
    let second = ChecksReply::from_value(second.await.unwrap()).unwrap();

    assert_eq!(first, ChecksReply::Started);
    assert_eq!(
        second,
        ChecksReply::Refused {
            reason: "Backfill already has checks running".to_string()
        }
    );
    finishing.await.unwrap();
    assert_eq!(world.lines("order"), ["docs"]);
    let records = world.records("docs");
    assert_eq!(records.len(), 1);
    assert!(records[0].verdict.is_pass());

    let ask = |command: ChecksCommand, cx: &mut TestAppContext| {
        cx.update(|cx| handle(serde_json::to_value(&command).unwrap(), true, cx))
    };
    let cancel = ask(
        ChecksCommand::CancelChecks {
            task_id: "nobody".to_string(),
        },
        cx,
    );
    assert_eq!(
        ChecksReply::from_value(cancel.await.unwrap()).unwrap(),
        ChecksReply::Cancelled { stopped: false }
    );
    // No done decider exists yet: no task is held.
    let hold = ask(
        ChecksCommand::WouldHold {
            task_id: "l1".to_string(),
        },
        cx,
    );
    assert_eq!(
        ChecksReply::from_value(hold.await.unwrap()).unwrap(),
        ChecksReply::Hold { reason: None }
    );
    let wrong = cx.update(|cx| handle(json!({ "command": "explode" }), true, cx));
    assert!(wrong.await.is_err());
}
