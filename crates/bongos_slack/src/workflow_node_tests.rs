//! Tests of the workflow steps `slack-read` and `slack-post`: the real built-ins over the fake
//! Slack, driven by the workflow runtime with a stand-in runner. Every answer is scripted and
//! ready; a rate-limit wait runs on the test executor's clock, advanced by the test and never
//! slept. The run logs live in fresh temporary directories.

use std::sync::Arc;
use std::time::Duration;

use bongos_slack_model::fake::{Call, FakeSlack};
use bongos_slack_model::types::{
    Conversation, ConversationId, ConversationKind, Cursor, Message, Page, Ts, UserId,
};
use bongos_slack_model::{ApiError, PostRequest, PostedMessage, User};
use bongos_workflow::lifecycle::BY_PERSON;
use bongos_workflow::process::ProcessRecord;
use bongos_workflow::runtime::{Finish, RunBinding, RunSpec, Runner, Runtime};
use bongos_workflow::{BuiltinNode as _, BuiltinNodes, BuiltinStart, RunFacts};
use bongos_workflow_model::record::{EndState, StartedBy};
use bongos_workflow_model::scheduler::Branch;
use bongos_workflow_model::{
    Definition, Event, Failure, FailureReason, Limits, NodeKey, RunState, parse,
};
use gpui::TestAppContext;
use serde_json::Value;
use tempfile::TempDir;

use crate::service::{NOT_CONNECTED_MESSAGE, OFF_MESSAGE, Read};
use crate::service_tests::{Fixture, set_enabled, setup};
use crate::workflow_node::{HostFiles, ProjectOf, SlackNodes, previous_try_warning};
use crate::{allowed, connection};

const RUN: &str = "01890a5d-ac96-774b-bcce-b30200000001";
const PROJECT: &str = "project-1";

/// A read of `#incidents` whose count a post of `text` to `channel` names.
fn read_then_post(channel: &str, text: &str) -> String {
    format!(
        r##"
schema = 1
name = "report"
start = "read"

[nodes.read]
kind = "program"
use = "slack-read"
with = {{ channel = "#incidents", count = 2 }}
routes = [{{ to = "post" }}]

[nodes.post]
kind = "program"
use = "slack-post"
with = {{ channel = "{channel}", text = "{text}" }}
routes = [{{ to = "done" }}]

[ends.done]
kind = "success"
"##
    )
}

/// One post of `text` to `channel`.
fn post_only(channel: &str, text: &str) -> String {
    format!(
        r##"
schema = 1
name = "announce"
start = "post"

[nodes.post]
kind = "program"
use = "slack-post"
with = {{ channel = "{channel}", text = "{text}" }}
routes = [{{ to = "done" }}]

[ends.done]
kind = "success"
"##
    )
}

const GREEN: &str = "The build is green.";

/// Stands in for the agent and program runners: nothing but the built-ins runs.
struct NoRunner;

impl Runner for NoRunner {
    fn start(
        &mut self,
        _: &str,
        _: &Definition,
        _: &NodeKey,
        _: u32,
    ) -> Result<ProcessRecord, Failure> {
        Err(Failure::new(
            FailureReason::ProgramResultInvalid,
            None,
            "only built-ins run here",
        ))
    }

    fn poll(&mut self) -> Vec<Finish> {
        Vec::new()
    }

    fn stop(&mut self, _: &[(String, NodeKey)]) {}

    fn merge_branch(&mut self, _: &str, _: &str, _: &Branch) -> Result<String, Failure> {
        Err(Failure::new(
            FailureReason::MergeConflict,
            None,
            "no merges",
        ))
    }
}

struct World {
    fixture: Fixture,
    nodes: SlackNodes,
    runtime: Runtime<NoRunner>,
    logs: TempDir,
}

impl World {
    fn fake(&self) -> &FakeSlack {
        &self.fixture.fake
    }

    fn add(&mut self, text: &str, unattended: bool) {
        let spec = RunSpec {
            id: RUN.to_string(),
            definition: parse(text).unwrap(),
            attempt_dir: self.logs.path().join(format!("runs/{RUN}/attempts/1")),
            loop_bound: 5,
        };
        self.runtime.add(spec).unwrap();
        self.runtime.set_unattended(RUN, unattended);
        if unattended {
            self.runtime.set_started_by(RUN, StartedBy::Lane);
        }
    }

    /// Ticks the runtime and lets the app run until nothing more happens.
    fn drive(&mut self, cx: &mut TestAppContext) {
        for _ in 0..4 {
            self.runtime.tick();
            cx.run_until_parked();
        }
        self.runtime.tick();
    }

    fn state(&self) -> RunState {
        self.runtime.state(RUN).unwrap()
    }

    fn result(&self, node: &str) -> Value {
        let state = self.state();
        let finished = state.last_finish(&NodeKey::new(node, 1)).unwrap();
        assert!(finished.succeeded(), "{:?}", finished.failure);
        finished.result.clone().unwrap()
    }

    fn gates(&self) -> Vec<String> {
        self.runtime
            .events(RUN)
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::GateOpened { question, .. } => Some(question.clone()),
                _ => None,
            })
            .collect()
    }

    fn approve(&mut self, node: &str) {
        self.runtime
            .close_gate(RUN, NodeKey::new(node, 1), BY_PERSON, None, None)
            .unwrap();
    }
}

/// A connected Slack over the fake, the built-ins with `project` as every run's project, and a
/// runtime they are registered with.
fn world(project: Option<&str>, cx: &mut TestAppContext) -> World {
    world_with(project, None, |_, _| {}, cx)
}

/// [`world`] as in a process with no window: the built-ins read `connection.toml` and a user
/// settings file of `settings` again before every try.
fn host_world(settings: &str, cx: &mut TestAppContext) -> World {
    world_with(None, Some(settings), |_, _| {}, cx)
}

/// [`host_world`] whose app state `at_start` changes before the built-ins are made, as a host
/// that started with other files would have loaded it.
fn host_world_started(
    settings: &str,
    at_start: impl FnOnce(&Fixture, &mut TestAppContext),
    cx: &mut TestAppContext,
) -> World {
    world_with(None, Some(settings), at_start, cx)
}

fn world_with(
    project: Option<&str>,
    settings: Option<&str>,
    at_start: impl FnOnce(&Fixture, &mut TestAppContext),
    cx: &mut TestAppContext,
) -> World {
    let fixture = setup(true, cx);
    at_start(&fixture, cx);
    let project = project.map(str::to_string);
    let project_of: ProjectOf = Arc::new(move |_: Option<&RunBinding>| project.clone());
    let dir = fixture.dir.clone();
    let host = settings.map(|settings| {
        let settings_file = fixture.dir.parent().unwrap().join("settings.json");
        std::fs::write(&settings_file, settings).unwrap();
        HostFiles {
            dir: dir.clone(),
            settings_file,
        }
    });
    let nodes = cx.update(|cx| SlackNodes::new(&fixture.service, dir, project_of, host, cx));
    let builtins = BuiltinNodes::default();
    builtins.insert("slack-read", nodes.read.clone());
    builtins.insert("slack-post", nodes.post.clone());
    let mut runtime = Runtime::new(Limits::default(), NoRunner);
    runtime.set_builtins(builtins);
    World {
        fixture,
        nodes,
        runtime,
        logs: TempDir::new().unwrap(),
    }
}

fn id(text: &str) -> ConversationId {
    ConversationId(text.to_string())
}

fn ts(n: u64) -> Ts {
    Ts::new(format!("{}.000100", 1_730_000_000 + n))
}

fn message(n: u64, user: Option<&str>, text: &str) -> Message {
    Message {
        ts: ts(n),
        user: user.map(|user| UserId(user.to_string())),
        text: text.to_string(),
        thread_ts: None,
        reply_count: 0,
        subtype: None,
    }
}

fn channel(id_text: &str, name: &str) -> Conversation {
    Conversation {
        id: id(id_text),
        kind: ConversationKind::Channel,
        name: name.to_string(),
        user: None,
    }
}

fn push_channels(fake: &FakeSlack) {
    fake.push_conversations(Ok(Page {
        items: vec![channel("C0INC", "incidents"), channel("C0REL", "releases")],
        next: None,
    }));
}

/// Scripts `#incidents` with three messages, the newest two on the first page.
fn push_incidents(fake: &FakeSlack) {
    push_channels(fake);
    fake.push_history(Ok(Page {
        items: vec![
            message(3, None, "deploy <@U1> done"),
            message(2, Some("U1"), "is it down?\nstill?"),
        ],
        next: Some(Cursor("older".to_string())),
    }));
    fake.push_user(Ok(User {
        id: UserId("U1".to_string()),
        name: "alice".to_string(),
        display_name: "Alice".to_string(),
        real_name: String::new(),
    }));
}

fn push_posted(fake: &FakeSlack) {
    fake.push_post(Ok(PostedMessage {
        channel: id("C0REL"),
        ts: ts(9),
    }));
}

fn posts(fake: &FakeSlack) -> Vec<PostRequest> {
    fake.calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Post(request) => Some(request),
            _ => None,
        })
        .collect()
}

fn post_of(text: &str) -> PostRequest {
    PostRequest {
        channel: id("C0REL"),
        text: text.to_string(),
        thread_ts: None,
    }
}

#[gpui::test]
async fn a_slack_read_returns_messages_oldest_first_with_has_older_and_a_marked_quote(
    cx: &mut TestAppContext,
) {
    let mut world = world(Some(PROJECT), cx);
    push_incidents(world.fake());
    push_posted(world.fake());
    world.add(&read_then_post("C0REL", "{{nodes.read.count}} new"), false);
    world.drive(cx);

    let result = world.result("read");
    assert_eq!(result["count"], 2);
    assert_eq!(result["has_older"], "yes");
    let messages: Value = serde_json::from_str(result["messages"].as_str().unwrap()).unwrap();
    assert_eq!(
        messages,
        serde_json::json!([
            {
                "ts": "1730000002.000100",
                "time": "2024-10-27 04:33",
                "author": "Alice",
                "author_id": "U1",
                "text": "is it down?\nstill?",
                "thread_ts": null,
                "link": "https://acme.slack.com/archives/C0INC/p1730000002000100",
            },
            {
                "ts": "1730000003.000100",
                "time": "2024-10-27 04:33",
                "author": "bot",
                "author_id": null,
                "text": "deploy @Alice done",
                "thread_ts": null,
                "link": "https://acme.slack.com/archives/C0INC/p1730000003000100",
            },
        ])
    );
    let quoted = result["quoted"].as_str().unwrap();
    assert!(
        quoted.starts_with("Quoted Slack content from #incidents (workspace Acme)."),
        "{quoted}"
    );
    assert!(
        quoted.contains("<slack-messages channel=\"#incidents\" trust=\"untrusted\">\n"),
        "{quoted}"
    );
    assert!(
        quoted.contains("[2024-10-27 04:33] @Alice: is it down?\n    still?\n"),
        "{quoted}"
    );
    assert!(quoted.ends_with("</slack-messages>"), "{quoted}");
    // One page of exactly the count asked for, and the post rendered the read's count.
    assert!(world.fake().calls().contains(&Call::History {
        channel: id("C0INC"),
        latest: None,
        limit: 2,
    }));
    assert_eq!(posts(world.fake()), [post_of("2 new")]);
}

#[gpui::test]
async fn a_slack_post_of_an_attended_run_posts_with_no_gate(cx: &mut TestAppContext) {
    let mut world = world(None, cx);
    push_posted(world.fake());
    world.add(&post_only("C0REL", "deploy & <roll>"), false);
    world.drive(cx);

    assert!(world.gates().is_empty());
    assert_eq!(posts(world.fake()), [post_of("deploy &amp; &lt;roll&gt;")]);
    assert_eq!(
        world.result("post"),
        serde_json::json!({
            "link": "https://acme.slack.com/archives/C0REL/p1730000009000100",
            "channel": "C0REL",
            "ts": "1730000009.000100",
        })
    );
    assert_eq!(world.state().ended, Some((EndState::Succeeded, None)));
}

#[gpui::test]
async fn a_slack_post_of_an_unattended_run_asks_with_the_exact_text_and_posts_after_approval(
    cx: &mut TestAppContext,
) {
    let mut world = world(Some(PROJECT), cx);
    push_channels(world.fake());
    push_posted(world.fake());
    world.add(&post_only("#releases", GREEN), true);
    world.drive(cx);

    assert_eq!(
        world.gates(),
        ["Post to #releases in Acme as kyle (19 characters):\nThe build is green."]
    );
    assert!(
        posts(world.fake()).is_empty(),
        "nothing is sent before the approval"
    );
    assert!(
        world
            .state()
            .person_only_gates
            .contains(&NodeKey::new("post", 1))
    );

    world.approve("post");
    world.drive(cx);
    assert_eq!(posts(world.fake()), [post_of(GREEN)]);
    assert_eq!(world.gates().len(), 1);
    assert_eq!(world.state().ended, Some((EndState::Succeeded, None)));
}

#[gpui::test]
async fn a_slack_post_to_an_allowed_channel_posts_unattended(cx: &mut TestAppContext) {
    let mut world = world(Some(PROJECT), cx);
    allowed::add_in(&world.fixture.dir, PROJECT, &id("C0REL"), None).unwrap();
    push_posted(world.fake());
    world.add(&post_only("C0REL", GREEN), true);
    world.drive(cx);

    assert!(world.gates().is_empty());
    assert_eq!(posts(world.fake()), [post_of(GREEN)]);
    assert_eq!(world.state().ended, Some((EndState::Succeeded, None)));
}

#[gpui::test]
async fn a_slack_post_interrupted_asks_on_its_next_try_even_when_allowed(cx: &mut TestAppContext) {
    let world = world(Some(PROJECT), cx);
    allowed::add_in(&world.fixture.dir, PROJECT, &id("C0REL"), None).unwrap();
    let definition = parse(&post_only("C0REL", GREEN)).unwrap();
    let state = RunState::default();
    let key = NodeKey::new("post", 1);
    let start = |unattended: bool, try_number: u32| BuiltinStart {
        run: RUN,
        definition: &definition,
        state: &state,
        key: &key,
        try_number,
        binding: None,
        facts: RunFacts {
            unattended,
            started_by: StartedBy::Lane,
        },
    };
    let post = &world.nodes.post;
    assert_eq!(
        post.start_gate(&start(true, 1)),
        None,
        "allowed: no question"
    );

    post.recover(RUN, std::slice::from_ref(&key));
    let question = format!(
        "{}\n\nPost to C0REL in Acme as kyle (19 characters):\nThe build is green.",
        previous_try_warning("C0REL")
    );
    assert_eq!(post.start_gate(&start(true, 2)), Some(question.clone()));
    assert_eq!(post.start_gate(&start(false, 2)), Some(question));
    assert!(question_starts_with_warning(
        post.start_gate(&start(true, 2))
    ));

    // The approved try starts; the one after it is allowed again.
    push_posted(world.fake());
    post.start(start(true, 2)).unwrap();
    assert_eq!(post.start_gate(&start(true, 3)), None);
}

fn question_starts_with_warning(question: Option<String>) -> bool {
    question.is_some_and(|question| {
        question.starts_with("A previous try may already have posted this; check C0REL first.")
    })
}

#[gpui::test]
async fn a_slack_post_that_fails_fails_post_not_sent_and_is_not_retried(cx: &mut TestAppContext) {
    let mut world = world(None, cx);
    world.fake().push_post(Err(ApiError::Network));
    world.add(&post_only("C0REL", GREEN), false);
    world.drive(cx);
    world.drive(cx);

    assert_eq!(
        posts(world.fake()).len(),
        1,
        "a failed post is never repeated"
    );
    let (end, failure) = world.state().ended.unwrap();
    assert_eq!(end, EndState::Failed);
    let failure = failure.unwrap();
    assert_eq!(failure.reason, FailureReason::PostNotSent);
    assert!(
        failure.detail.contains("may not have been sent"),
        "{failure:?}"
    );
}

const ON: &str = r#"{ "slack": { "enabled": true } }"#;

/// The start of try 1 of `key` in an attended run of `definition`.
fn attended_start<'a>(
    definition: &'a Definition,
    state: &'a RunState,
    key: &'a NodeKey,
) -> BuiltinStart<'a> {
    BuiltinStart {
        run: RUN,
        definition,
        state,
        key,
        try_number: 1,
        binding: None,
        facts: RunFacts {
            unattended: false,
            started_by: StartedBy::Person,
        },
    }
}

fn remove_connection(world: &World) {
    std::fs::remove_file(connection::connection_path(&world.fixture.dir)).unwrap();
}

/// The failure of an attended post run in `world`, which must send nothing.
fn refused_post(mut world: World, cx: &mut TestAppContext) -> Failure {
    push_posted(world.fake());
    world.add(&post_only("C0REL", GREEN), false);
    world.drive(cx);
    assert!(
        world.fake().calls().is_empty(),
        "{:?}",
        world.fake().calls()
    );
    let (end, failure) = world.state().ended.unwrap();
    assert_eq!(end, EndState::Failed);
    failure.unwrap()
}

#[gpui::test]
async fn a_slack_post_in_a_host_fails_at_once_when_the_settings_file_turns_slack_off(
    cx: &mut TestAppContext,
) {
    let world = host_world(r#"{ "slack": { "enabled": false } }"#, cx);
    let failure = refused_post(world, cx);
    assert_eq!(failure.reason, FailureReason::PostNotSent);
    assert_eq!(failure.detail, OFF_MESSAGE);
}

#[gpui::test]
async fn a_slack_post_in_a_host_fails_at_once_when_another_process_disconnected(
    cx: &mut TestAppContext,
) {
    let world = host_world(ON, cx);
    remove_connection(&world);
    let failure = refused_post(world, cx);
    assert_eq!(failure.reason, FailureReason::PostNotSent);
    assert_eq!(failure.detail, NOT_CONNECTED_MESSAGE);
}

#[gpui::test]
async fn a_slack_read_in_a_host_fails_service_unavailable_when_not_connected(
    cx: &mut TestAppContext,
) {
    let world = host_world(ON, cx);
    remove_connection(&world);
    let definition = parse(&read_then_post("C0REL", GREEN)).unwrap();
    let state = RunState::default();
    let key = NodeKey::new("read", 1);
    let failure = world
        .nodes
        .read
        .start(attended_start(&definition, &state, &key))
        .unwrap_err();
    assert_eq!(failure.reason, FailureReason::ServiceUnavailable);
    assert_eq!(failure.detail, NOT_CONNECTED_MESSAGE);
    assert!(world.fake().calls().is_empty());
}

#[gpui::test]
async fn a_slack_post_in_a_host_refuses_when_the_connection_goes_after_its_start(
    cx: &mut TestAppContext,
) {
    let world = host_world(ON, cx);
    push_posted(world.fake());
    let definition = parse(&post_only("C0REL", GREEN)).unwrap();
    let state = RunState::default();
    let key = NodeKey::new("post", 1);
    let post = &world.nodes.post;
    post.start(attended_start(&definition, &state, &key))
        .unwrap();
    // Another process disconnects before the worker sends.
    remove_connection(&world);
    cx.run_until_parked();

    let finishes = post.poll();
    assert_eq!(finishes.len(), 1);
    let failure = finishes[0].outcome.clone().unwrap_err();
    assert_eq!(failure.reason, FailureReason::PostNotSent);
    assert_eq!(failure.detail, NOT_CONNECTED_MESSAGE);
    assert!(world.fake().calls().is_empty());
    assert_eq!(cx.update(|cx| connection::current(cx)), None);
}

#[gpui::test]
async fn a_slack_reply_links_to_the_message_in_its_thread(cx: &mut TestAppContext) {
    let mut world = world(None, cx);
    push_posted(world.fake());
    let definition = post_only("C0REL", GREEN).replace(
        "text = \"",
        &format!("thread = \"{}\", text = \"", ts(5).as_str()),
    );
    world.add(&definition, false);
    world.drive(cx);

    assert_eq!(
        posts(world.fake()),
        [PostRequest {
            thread_ts: Some(ts(5)),
            ..post_of(GREEN)
        }]
    );
    assert_eq!(
        world.result("post")["link"],
        "https://acme.slack.com/archives/C0REL/p1730000009000100\
         ?thread_ts=1730000005.000100&cid=C0REL"
    );
}

fn limited(seconds: u64) -> ApiError {
    ApiError::RateLimited {
        retry_after: Duration::from_secs(seconds),
    }
}

/// Starts try 1 of the attended post of `definition` in `world`, without the runtime.
fn start_post(world: &World, definition: &str) {
    let definition = parse(definition).unwrap();
    let state = RunState::default();
    let key = NodeKey::new("post", 1);
    world
        .nodes
        .post
        .start(attended_start(&definition, &state, &key))
        .unwrap();
}

/// The one failure the post reported.
fn post_failure(world: &World) -> Failure {
    let finishes = world.nodes.post.poll();
    assert_eq!(finishes.len(), 1);
    finishes[0].outcome.clone().unwrap_err()
}

#[gpui::test]
async fn a_slack_post_in_a_host_is_not_sent_again_when_the_connection_goes_during_its_rate_limit(
    cx: &mut TestAppContext,
) {
    let world = host_world(ON, cx);
    world.fake().push_post(Err(limited(10)));
    push_posted(world.fake());
    start_post(&world, &post_only("C0REL", GREEN));
    cx.run_until_parked();
    assert_eq!(posts(world.fake()).len(), 1, "the limited post");
    assert!(world.nodes.post.poll().is_empty(), "the post waits");

    // Another process disconnects during the wait.
    remove_connection(&world);
    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();

    let failure = post_failure(&world);
    assert_eq!(failure.reason, FailureReason::PostNotSent);
    assert_eq!(failure.detail, NOT_CONNECTED_MESSAGE);
    assert_eq!(posts(world.fake()).len(), 1, "nothing was sent again");
}

#[gpui::test]
async fn a_slack_post_in_a_host_queued_behind_a_wait_is_not_sent_once_slack_is_off(
    cx: &mut TestAppContext,
) {
    let world = host_world(ON, cx);
    // A read's rate limit starts the shared wait.
    world.fake().push_history(Err(limited(10)));
    world.fake().push_history(Ok(Page {
        items: Vec::new(),
        next: None,
    }));
    push_posted(world.fake());
    let read = world.fixture.service.update(cx, |service, cx| {
        service.call(
            Read::History {
                channel: id("C0INC"),
                latest: None,
                limit: 1,
            },
            cx,
        )
    });
    cx.run_until_parked();
    start_post(&world, &post_only("C0REL", GREEN));
    cx.run_until_parked();
    assert!(posts(world.fake()).is_empty(), "the post waits");

    // The window turns Slack off during the wait.
    std::fs::write(
        world.fixture.dir.parent().unwrap().join("settings.json"),
        r#"{ "slack": { "enabled": false } }"#,
    )
    .unwrap();
    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();

    let failure = post_failure(&world);
    assert_eq!(failure.reason, FailureReason::PostNotSent);
    assert_eq!(failure.detail, OFF_MESSAGE);
    assert!(posts(world.fake()).is_empty(), "{:?}", world.fake().calls());
    drop(read);
}

#[gpui::test]
async fn a_slack_post_in_a_host_is_not_sent_when_the_connection_goes_while_its_name_resolves(
    cx: &mut TestAppContext,
) {
    let world = host_world(ON, cx);
    world.fake().push_conversations(Err(limited(10)));
    push_channels(world.fake());
    push_posted(world.fake());
    start_post(&world, &post_only("#releases", GREEN));
    cx.run_until_parked();

    remove_connection(&world);
    cx.executor().advance_clock(Duration::from_secs(10));
    cx.run_until_parked();

    let failure = post_failure(&world);
    assert_eq!(failure.reason, FailureReason::PostNotSent);
    assert_eq!(failure.detail, NOT_CONNECTED_MESSAGE);
    assert!(posts(world.fake()).is_empty(), "{:?}", world.fake().calls());
}

/// Moves `connection.toml` aside and forgets the connection, as in a host that started before
/// the window connected; [`connect_later`] puts it back.
fn start_disconnected(fixture: &Fixture, cx: &mut TestAppContext) {
    let path = connection::connection_path(&fixture.dir);
    std::fs::rename(&path, path.with_extension("later")).unwrap();
    cx.update(|cx| connection::reload(None, cx));
}

fn connect_later(world: &World) {
    let path = connection::connection_path(&world.fixture.dir);
    std::fs::rename(path.with_extension("later"), &path).unwrap();
}

#[gpui::test]
async fn a_host_that_started_disconnected_posts_once_the_window_connects(cx: &mut TestAppContext) {
    let mut world = host_world_started(ON, start_disconnected, cx);
    let definition = parse(&post_only("C0REL", GREEN)).unwrap();
    let state = RunState::default();
    let key = NodeKey::new("post", 1);
    let failure = world
        .nodes
        .post
        .start(attended_start(&definition, &state, &key))
        .unwrap_err();
    assert_eq!(failure.detail, NOT_CONNECTED_MESSAGE);

    connect_later(&world);
    push_posted(world.fake());
    world.add(&post_only("C0REL", GREEN), false);
    world.drive(cx);

    assert_eq!(posts(world.fake()), [post_of(GREEN)]);
    assert_eq!(world.result("post")["channel"], "C0REL");
}

#[gpui::test]
async fn a_host_that_started_with_slack_off_posts_once_the_settings_file_turns_it_on(
    cx: &mut TestAppContext,
) {
    let mut world = host_world_started(ON, |_, cx| set_enabled(false, cx), cx);
    push_posted(world.fake());
    world.add(&post_only("C0REL", GREEN), false);
    world.drive(cx);

    assert_eq!(posts(world.fake()), [post_of(GREEN)]);
    assert_eq!(world.result("post")["channel"], "C0REL");
}
