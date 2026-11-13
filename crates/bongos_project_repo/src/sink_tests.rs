use bongos_project_repo_model::Author;
use futures::executor::block_on;

use crate::{RefusingSink, RepoReply, StateOp, StateSink};

#[test]
fn a_refusing_sink_refuses_with_the_pid() {
    let sink = RefusingSink::new(4242);
    let error = block_on(sink.apply(
        vec![StateOp::Delete { task: "a".into() }],
        Author::agent("runner"),
    ))
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "this project is open in another bongos process (pid 4242); work there or close it"
    );
}

#[test]
fn repo_reply_round_trips_through_json() {
    let json = serde_json::to_string(&RepoReply::Done).unwrap();
    assert_eq!(serde_json::from_str::<RepoReply>(&json).unwrap(), RepoReply::Done);
}
