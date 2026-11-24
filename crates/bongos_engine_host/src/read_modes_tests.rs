//! The registry of read modes and the stand-ins for the commands of later features.

use std::rc::Rc;

use anyhow::{Result, anyhow};
use bongos_project_repo::{StateReader, StateStore};
use futures::channel::oneshot;
use gpui::{App, BorrowAppContext as _, Global, Task, TestAppContext};
use serde_json::json;

use crate::engine_host_tests::fixture;
use crate::{
    HostCommands, ReadModes, ReadProject, STUB_NAMESPACES, local_identity, register_stub_commands,
};

fn reader() -> (crate::engine_host_tests::Fixture, StateReader) {
    let fixture = fixture();
    let reader = StateStore::open_reader(fixture.location()).unwrap();
    (fixture, reader)
}

fn echo(args: &[String], reader: &StateReader, _cx: &mut App) -> Result<String> {
    let generation = reader.read(|read| read.generation())?;
    Ok(format!("echo {} at {generation}", args.join(" ")))
}

fn failing(_args: &[String], _reader: &StateReader, _cx: &mut App) -> Result<String> {
    Err(anyhow!("this mode fails"))
}

/// The end the test holds to let the asynchronous mode finish.
struct Gate(Option<oneshot::Receiver<()>>);

impl Global for Gate {}

fn after_the_gate(_args: &[String], _reader: &StateReader, cx: &mut App) -> Task<Result<String>> {
    let gate = cx.update_global::<Gate, _>(|gate, _| gate.0.take());
    cx.spawn(async move |_| {
        gate.ok_or_else(|| anyhow!("the gate was taken already"))?
            .await
            .map_err(|_| anyhow!("the gate was dropped"))?;
        Ok("released".to_string())
    })
}

#[gpui::test]
async fn a_name_nothing_is_registered_under_runs_nothing(cx: &mut TestAppContext) {
    let (_fixture, reader) = reader();
    cx.update(|cx| {
        assert!(!ReadModes::is_registered("profile", cx));
        assert!(ReadModes::run("profile", &[], &reader, cx).is_none());
        ReadModes::register("status", echo, cx);
        assert!(ReadModes::is_registered("status", cx));
        assert!(ReadModes::run("profile", &[], &reader, cx).is_none());
    });
}

#[gpui::test]
async fn a_registered_mode_gets_its_arguments_and_the_reader_and_a_failure_comes_back(
    cx: &mut TestAppContext,
) {
    let (_fixture, reader) = reader();
    let task = cx.update(|cx| {
        ReadModes::register("profile", echo, cx);
        ReadModes::register("broken", failing, cx);
        let args = vec!["show".to_string(), "--json".to_string()];
        ReadModes::run("profile", &args, &reader, cx).expect("registered")
    });
    assert!(task.await.unwrap().starts_with("echo show --json at "));
    let broken = cx
        .update(|cx| ReadModes::run("broken", &[], &reader, cx))
        .expect("registered");
    assert_eq!(broken.await.unwrap_err().to_string(), "this mode fails");
}

#[gpui::test]
async fn an_asynchronous_mode_is_awaited_for_its_answer(cx: &mut TestAppContext) {
    let (_fixture, reader) = reader();
    let (open, gate) = oneshot::channel();
    let task = cx.update(|cx| {
        cx.set_global(Gate(Some(gate)));
        ReadModes::register_async("slow", after_the_gate, cx);
        ReadModes::run("slow", &[], &reader, cx).expect("registered")
    });
    cx.run_until_parked();
    open.send(()).unwrap();
    assert_eq!(task.await.unwrap(), "released");
}

#[gpui::test]
async fn the_project_a_mode_runs_for_is_kept_for_it(cx: &mut TestAppContext) {
    let (fixture, _reader) = reader();
    cx.update(|cx| {
        assert!(ReadProject::get(cx).is_none());
        ReadProject::set(fixture.location().clone(), cx);
        assert_eq!(ReadProject::get(cx).unwrap().id, "project-1");
    });
}

#[gpui::test]
async fn the_stand_ins_answer_until_a_feature_registers_its_own_handler(cx: &mut TestAppContext) {
    let peer = local_identity().unwrap();
    let ask = |namespace: &str, cx: &mut TestAppContext, peer: &_| {
        cx.update(|cx| {
            let handler = HostCommands::handler(namespace, cx).expect("a handler");
            handler(json!({ "verb": "fire", "name": "nightly" }), peer, cx)
        })
    };
    cx.update(|cx| {
        for namespace in STUB_NAMESPACES {
            assert!(HostCommands::handler(namespace, cx).is_none());
        }
        register_stub_commands(cx);
    });
    for namespace in STUB_NAMESPACES {
        let reply = ask(namespace, cx, &peer).await.unwrap();
        assert_eq!(
            reply["message"],
            format!("{namespace} fire: not implemented yet")
        );
        assert_eq!(reply["echo"]["name"], "nightly");
    }

    // A feature that registers its own handler, before or after the stand-ins, is the one asked.
    cx.update(|cx| {
        HostCommands::register(
            "trigger",
            Rc::new(|_, _, _| Task::ready(Ok(json!({ "message": "real" })))),
            cx,
        );
        register_stub_commands(cx);
    });
    assert_eq!(ask("trigger", cx, &peer).await.unwrap()["message"], "real");
    assert_eq!(
        ask("queue", cx, &peer).await.unwrap()["message"],
        "queue fire: not implemented yet"
    );
}
