//! Tests of the workflow steps `slack-read` and `slack-post`: the real built-ins over the fake
//! Slack, driven by the workflow runtime with a stand-in runner. Every answer is scripted and
//! ready, so nothing waits and no clock is read; the run logs live in fresh temporary
//! directories.

use std::sync::Arc;

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

use crate::allowed;
use crate::service_tests::{Fixture, setup};
use crate::workflow_node::{ProjectOf, SlackNodes, previous_try_warning};

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
    let fixture = setup(true, cx);
    let project = project.map(str::to_string);
    let project_of: ProjectOf = Arc::new(move |_: Option<&RunBinding>| project.clone());
    let dir = fixture.dir.clone();
    let nodes = cx.update(|cx| SlackNodes::new(&fixture.service, dir, project_of, cx));
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
