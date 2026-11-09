//! Tests of the gate: no request reaches the client unless `linear.enabled` is true, a token
//! exists and the request is for a linked task or an explicit Linear command.

use std::sync::Arc;

use bongos_linear_model::fake::{Call, FakeLinear};
use bongos_linear_model::{ApiError, LinearApi};
use gpui::{Entity, TestAppContext};

use crate::actions_tests::{set_enabled, use_default_settings};
use crate::sync::{self, LinearSync, ReadState, Refusal, Scope};

struct Fixture {
    sync: Entity<LinearSync>,
    fake: FakeLinear,
}

/// The sync with a client over a fake, a token present and the task `linked` linked.
fn fixture(cx: &mut TestAppContext, enabled: bool, token: bool) -> Fixture {
    use_default_settings(cx);
    set_enabled(cx, enabled);
    let fake = FakeLinear::new("acme");
    let sync = cx.update(|cx| sync::global(cx).expect("init installs the sync"));
    sync.update(cx, |sync, _| {
        let client = Arc::new(fake.clone()) as Arc<dyn LinearApi>;
        sync.set_token(token, Some(client));
        sync.set_linked(["linked".to_string()]);
    });
    Fixture { sync, fake }
}

fn call(
    fixture: &Fixture,
    scope: Scope,
    cx: &mut TestAppContext,
) -> Result<futures::future::BoxFuture<'static, Result<String, ApiError>>, Refusal> {
    fixture.sync.update(cx, |sync, cx| {
        sync.call(scope, cx, |client| client.viewer())
    })
}

const EXPLICIT: [Scope; 3] = [Scope::NewFromLinear, Scope::LinkToLinear, Scope::SetToken];

#[gpui::test]
async fn nothing_is_sent_while_disabled(cx: &mut TestAppContext) {
    let fixture = fixture(cx, false, true);
    for scope in [Scope::LinkedTask("linked")].into_iter().chain(EXPLICIT) {
        assert!(
            call(&fixture, scope, cx).err() == Some(Refusal::Disabled),
            "{scope:?}"
        );
    }
    let verdict = fixture
        .sync
        .update(cx, |sync, cx| sync.verify_token(&fixture.fake, cx));
    assert!(verdict.err() == Some(Refusal::Disabled));
    assert!(fixture.fake.calls().is_empty());
    let snapshot = fixture.sync.read_with(cx, |sync, cx| sync.snapshot(cx));
    assert!(!snapshot.enabled && snapshot.requests_allowed == 0 && snapshot.requests_refused == 5);
}

#[gpui::test]
async fn nothing_is_sent_without_a_token(cx: &mut TestAppContext) {
    let fixture = fixture(cx, true, false);
    for scope in [
        Scope::LinkedTask("linked"),
        Scope::NewFromLinear,
        Scope::LinkToLinear,
    ] {
        assert!(
            call(&fixture, scope, cx).err() == Some(Refusal::NoToken),
            "{scope:?}"
        );
    }
    assert!(fixture.fake.calls().is_empty());
}

#[gpui::test]
async fn a_task_that_is_not_linked_is_refused(cx: &mut TestAppContext) {
    let fixture = fixture(cx, true, true);
    assert!(call(&fixture, Scope::LinkedTask("other"), cx).err() == Some(Refusal::NotLinked));
    assert!(fixture.fake.calls().is_empty());

    assert!(
        call(&fixture, Scope::LinkedTask("linked"), cx)
            .unwrap()
            .await
            .is_ok()
    );
    assert!(fixture.fake.calls() == vec![Call::Viewer]);

    // A task that stopped being linked is refused again.
    fixture.sync.update(cx, |sync, _| sync.set_linked([]));
    assert!(call(&fixture, Scope::LinkedTask("linked"), cx).err() == Some(Refusal::NotLinked));
    assert!(fixture.fake.calls().len() == 1);
}

#[gpui::test]
async fn explicit_commands_pass_with_a_token_and_the_setting_on(cx: &mut TestAppContext) {
    let fixture = fixture(cx, true, true);
    for scope in [Scope::NewFromLinear, Scope::LinkToLinear] {
        assert!(
            call(&fixture, scope, cx).unwrap().await.is_ok(),
            "{scope:?}"
        );
    }
    assert!(fixture.fake.calls().len() == 2);
    let snapshot = fixture.sync.read_with(cx, |sync, cx| sync.snapshot(cx));
    assert!(snapshot.enabled && snapshot.token_present);
    assert!(snapshot.requests_allowed == 2 && snapshot.requests_refused == 0);

    // Turning the setting off stops the next one.
    set_enabled(cx, false);
    assert!(call(&fixture, Scope::NewFromLinear, cx).err() == Some(Refusal::Disabled));
    assert!(fixture.fake.calls().len() == 2);
}

#[gpui::test]
async fn a_token_being_checked_passes_the_gate_when_enabled(cx: &mut TestAppContext) {
    let fixture = fixture(cx, true, false);
    let check = fixture
        .sync
        .update(cx, |sync, cx| sync.verify_token(&fixture.fake, cx))
        .expect("the candidate token passes");
    assert!(check.await == Ok("acme".to_string()));
    assert!(fixture.fake.calls() == vec![Call::Viewer]);
}

#[gpui::test]
async fn a_token_without_a_client_is_refused(cx: &mut TestAppContext) {
    let fixture = fixture(cx, true, true);
    fixture
        .sync
        .update(cx, |sync, _| sync.set_token(true, None));
    assert!(call(&fixture, Scope::NewFromLinear, cx).err() == Some(Refusal::NoClient));
    // Clearing the token forgets the client too.
    fixture.sync.update(cx, |sync, _| {
        sync.set_token(false, Some(Arc::new(FakeLinear::new("x"))))
    });
    assert!(call(&fixture, Scope::NewFromLinear, cx).err() == Some(Refusal::NoToken));
}

#[gpui::test]
async fn reads_are_recorded_per_task(cx: &mut TestAppContext) {
    let fixture = fixture(cx, true, true);
    fixture.sync.update(cx, |sync, _| {
        sync.set_linked([
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]);
        sync.record_read("a", &Ok::<(), ApiError>(()), 42);
        sync.record_read("b", &Err::<(), _>(ApiError::Network("x".into())), 43);
        sync.record_read(
            "c",
            &Err::<(), _>(ApiError::RateLimited { reset_at: None }),
            44,
        );
        sync.record_read("d", &Err::<(), _>(ApiError::NotFound("d".into())), 45);
    });
    let states = fixture.sync.read_with(cx, |sync, _| {
        ["a", "b", "c", "d", "e"].map(|id| sync.read_state(id))
    });
    assert!(
        states
            == [
                Some(ReadState::Fresh { at: 42 }),
                Some(ReadState::Unreachable),
                Some(ReadState::RateLimited),
                Some(ReadState::NotFound),
                None
            ]
    );
    // An unlinked task's state is dropped.
    fixture
        .sync
        .update(cx, |sync, _| sync.set_linked(["a".to_string()]));
    let snapshot = fixture.sync.read_with(cx, |sync, cx| sync.snapshot(cx));
    assert!(snapshot.reads == vec![("a".to_string(), ReadState::Fresh { at: 42 })]);
}

#[test]
fn refusals_say_what_is_missing() {
    assert!(Refusal::Disabled.to_string() == "Linear sync is off (linear.enabled)");
    assert!(Refusal::NoToken.to_string() == crate::token::NO_TOKEN_MESSAGE);
}
