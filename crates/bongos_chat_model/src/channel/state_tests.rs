use super::report::{
    AgentName, ChatId, Detail, Event, Report, ReportedState, SessionSource, Token,
};
use super::state::{
    BELL_GRACE, ChannelCoverage, ChannelStatus, ChatLive, ChatState, FROM_TERMINAL, Input,
    MID_TURN, NO_TURN_END, Source, apply, is_mid_turn,
};

const ESTIMATED: Source = Source::Estimated(FROM_TERMINAL);
const NO_END: Source = Source::Estimated(NO_TURN_END);

fn report(at: u64, event: Event) -> Input {
    Input::Report(Report {
        chat: ChatId::new("chat-1").unwrap(),
        token: Token::new("9f0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e").unwrap(),
        at,
        agent: AgentName::claude(),
        event,
        session: None,
    })
}

fn needs_input(at: u64, detail: Option<&str>) -> Input {
    report(
        at,
        Event::NeedsInput {
            detail: detail.map(|detail| Detail::new(detail).unwrap()),
        },
    )
}

fn chat(
    channel: ChannelStatus,
    coverage: ChannelCoverage,
    state: ChatState,
    source: Source,
) -> ChatLive {
    ChatLive {
        state,
        source,
        channel,
        coverage,
        last_report_at: None,
        last_bell_at: None,
        last_output_at: None,
    }
}

fn live(state: ChatState, source: Source) -> ChatLive {
    chat(
        ChannelStatus::Live,
        ChannelCoverage::claude(),
        state,
        source,
    )
}

fn no_channel(state: ChatState) -> ChatLive {
    chat(
        ChannelStatus::Off("no channel"),
        ChannelCoverage::none(),
        state,
        ESTIMATED,
    )
}

/// A channel that reports waiting and idle but never running.
fn no_running() -> ChannelCoverage {
    ChannelCoverage {
        running: false,
        ..ChannelCoverage::claude()
    }
}

/// The state and source after `input`, or the unchanged ones.
fn after(mut chat: ChatLive, input: Input) -> (ChatState, Source) {
    chat.step(input);
    (chat.state, chat.source)
}

// The table, one row per test.

#[test]
fn session_start_goes_idle_except_compact() {
    for source in [
        SessionSource::Startup,
        SessionSource::Resume,
        SessionSource::Fork,
        SessionSource::Clear,
    ] {
        let input = report(1, Event::SessionStart { source });
        assert_eq!(
            after(live(ChatState::Running, ESTIMATED), input),
            (ChatState::Idle, Source::Exact)
        );
    }
    let compact = report(
        1,
        Event::SessionStart {
            source: SessionSource::Compact,
        },
    );
    assert_eq!(
        after(live(ChatState::Running, Source::Exact), compact.clone()),
        (ChatState::Running, Source::Exact)
    );
    let change = apply(&live(ChatState::Running, Source::Exact), compact).unwrap();
    assert_eq!(change.state, ChatState::Running);
    assert_eq!(change.last_report_at, Some(1));
}

#[test]
fn prompt_submitted_goes_running() {
    assert_eq!(
        after(
            live(ChatState::Idle, Source::Exact),
            report(1, Event::PromptSubmitted)
        ),
        (ChatState::Running, Source::Exact)
    );
}

#[test]
fn turn_done_goes_idle() {
    assert_eq!(
        after(
            live(ChatState::Running, Source::Exact),
            report(1, Event::TurnDone)
        ),
        (ChatState::Idle, Source::Exact)
    );
}

#[test]
fn idle_prompt_from_idle_stays_exact() {
    assert_eq!(
        after(
            live(ChatState::Idle, Source::Exact),
            needs_input(1, Some("idle_prompt"))
        ),
        (ChatState::Idle, Source::Exact)
    );
    assert_eq!(
        after(
            live(ChatState::Idle, ESTIMATED),
            needs_input(1, Some("idle_prompt"))
        ),
        (ChatState::Idle, Source::Exact)
    );
}

#[test]
fn idle_prompt_from_running_or_waiting_is_estimated_idle() {
    for state in [ChatState::Running, ChatState::Waiting] {
        assert_eq!(
            after(
                live(state, Source::Exact),
                needs_input(1, Some("idle_prompt"))
            ),
            (ChatState::Idle, NO_END)
        );
    }
}

#[test]
fn needs_input_otherwise_goes_waiting() {
    for detail in [Some("permission_prompt"), Some("elicitation_dialog"), None] {
        assert_eq!(
            after(
                live(ChatState::Running, Source::Exact),
                needs_input(1, detail)
            ),
            (ChatState::Waiting, Source::Exact)
        );
    }
}

#[test]
fn tool_done_goes_exact_running_except_from_idle() {
    for state in [ChatState::Waiting, ChatState::Running] {
        assert_eq!(
            after(live(state, Source::Exact), report(1, Event::ToolDone)),
            (ChatState::Running, Source::Exact)
        );
    }
    assert_eq!(
        after(
            live(ChatState::Idle, Source::Exact),
            report(1, Event::ToolDone)
        ),
        (ChatState::Idle, Source::Exact)
    );
}

#[test]
fn state_report_sets_the_named_state() {
    for (reported, state) in [
        (ReportedState::Running, ChatState::Running),
        (ReportedState::Waiting, ChatState::Waiting),
        (ReportedState::Idle, ChatState::Idle),
    ] {
        let input = report(1, Event::State { state: reported });
        assert_eq!(
            after(live(ChatState::Running, ESTIMATED), input),
            (state, Source::Exact)
        );
    }
}

#[test]
fn bell_goes_waiting() {
    for state in [ChatState::Running, ChatState::Idle] {
        assert_eq!(
            after(no_channel(state), Input::Bell { now: 10 }),
            (ChatState::Waiting, ESTIMATED)
        );
    }
}

#[test]
fn output_goes_running_from_waiting_or_idle_outside_the_bell_grace() {
    for state in [ChatState::Waiting, ChatState::Idle] {
        assert_eq!(
            after(no_channel(state), Input::Output { now: 10 }),
            (ChatState::Running, ESTIMATED)
        );
    }

    let grace = u64::try_from(BELL_GRACE.as_millis()).unwrap();
    let mut chat = no_channel(ChatState::Running);
    chat.step(Input::Bell { now: 1_000 });
    assert_eq!(
        chat.step(Input::Output {
            now: 1_000 + grace - 1
        }),
        None
    );
    assert_eq!(chat.state, ChatState::Waiting);
    chat.step(Input::Output { now: 1_000 + grace });
    assert_eq!(chat.state, ChatState::Running);
}

#[test]
fn idle_timer_goes_idle_only_from_running() {
    assert_eq!(
        after(no_channel(ChatState::Running), Input::IdleTimer),
        (ChatState::Idle, ESTIMATED)
    );
    assert_eq!(
        apply(&no_channel(ChatState::Waiting), Input::IdleTimer),
        None
    );
}

#[test]
fn keyboard_input_and_send_enter_go_running_from_waiting_or_idle() {
    for input in [Input::KeyboardInput, Input::SendEnter] {
        for state in [ChatState::Waiting, ChatState::Idle] {
            assert_eq!(
                after(no_channel(state), input.clone()),
                (ChatState::Running, ESTIMATED)
            );
        }
    }
}

#[test]
fn exit_goes_exited_from_every_state() {
    for state in [ChatState::Running, ChatState::Waiting, ChatState::Idle] {
        for source in [Source::Exact, ESTIMATED] {
            assert_eq!(
                after(live(state, source), Input::Exit { code: Some(3) }),
                (ChatState::Exited { code: Some(3) }, Source::Exact)
            );
        }
    }
}

// Precedence.

#[test]
fn rule_1_nothing_changes_an_exited_chat() {
    let exited = live(ChatState::Exited { code: Some(0) }, Source::Exact);
    for input in [
        report(u64::MAX, Event::PromptSubmitted),
        report(
            u64::MAX,
            Event::State {
                state: ReportedState::Running,
            },
        ),
        Input::Bell { now: 1 },
        Input::Output { now: 1 },
        Input::IdleTimer,
        Input::KeyboardInput,
        Input::SendEnter,
        Input::Exit { code: Some(1) },
    ] {
        assert_eq!(apply(&exited, input), None);
    }
    assert_eq!(
        apply(
            &live(ChatState::Closed, Source::Exact),
            Input::KeyboardInput
        ),
        None
    );
}

#[test]
fn rule_2_an_older_report_is_dropped() {
    let mut chat = live(ChatState::Idle, Source::Exact);
    chat.step(report(20, Event::PromptSubmitted));
    assert_eq!(chat.step(report(19, Event::TurnDone)), None);
    assert_eq!(chat.state, ChatState::Running);
    assert_eq!(chat.last_report_at, Some(20));
}

#[test]
fn rule_2_a_tied_report_applies_in_arrival_order() {
    let mut chat = live(ChatState::Idle, Source::Exact);
    chat.step(report(20, Event::PromptSubmitted));
    chat.step(report(20, Event::TurnDone));
    assert_eq!((chat.state, chat.source), (ChatState::Idle, Source::Exact));
}

#[test]
fn rule_2_a_confirming_report_makes_an_estimate_exact() {
    let chat = live(ChatState::Running, ESTIMATED);
    let change = apply(&chat, report(1, Event::PromptSubmitted)).unwrap();
    assert_eq!(
        (change.state, change.source),
        (ChatState::Running, Source::Exact)
    );
}

#[test]
fn rule_2_a_report_outside_the_coverage_still_applies() {
    let chat = chat(
        ChannelStatus::Live,
        no_running(),
        ChatState::Idle,
        Source::Exact,
    );
    let input = report(
        1,
        Event::State {
            state: ReportedState::Running,
        },
    );
    assert_eq!(after(chat, input), (ChatState::Running, Source::Exact));
}

#[test]
fn rule_3_a_signal_applies_only_where_the_channel_cannot_answer() {
    // Live, covered, exact: ignored.
    assert_eq!(
        apply(&live(ChatState::Idle, Source::Exact), Input::SendEnter),
        None
    );
    // Not live.
    let pending = chat(
        ChannelStatus::Pending,
        ChannelCoverage::claude(),
        ChatState::Idle,
        Source::Exact,
    );
    assert_eq!(
        after(pending, Input::SendEnter),
        (ChatState::Running, ESTIMATED)
    );
    // Target not covered.
    let uncovered = chat(
        ChannelStatus::Live,
        no_running(),
        ChatState::Idle,
        Source::Exact,
    );
    assert_eq!(
        after(uncovered, Input::SendEnter),
        (ChatState::Running, ESTIMATED)
    );
    // Current state estimated.
    assert_eq!(
        after(live(ChatState::Idle, NO_END), Input::SendEnter),
        (ChatState::Running, ESTIMATED)
    );
}

#[test]
fn claude_live_ignores_bell_and_output() {
    let mut chat = live(ChatState::Running, Source::Exact);
    assert_eq!(chat.step(Input::Bell { now: 10 }), None);
    let mut idle = live(ChatState::Idle, Source::Exact);
    assert_eq!(idle.step(Input::Output { now: 5_000 }), None);
    let mut waiting = live(ChatState::Waiting, Source::Exact);
    assert_eq!(waiting.step(Input::Output { now: 5_000 }), None);
    assert_eq!(waiting.step(Input::IdleTimer), None);
    assert_eq!(chat.state, ChatState::Running);
}

#[test]
fn live_without_running_coverage_estimates_running_and_decays_it() {
    let mut chat = chat(
        ChannelStatus::Live,
        no_running(),
        ChatState::Idle,
        Source::Exact,
    );
    chat.step(report(1, Event::TurnDone));
    chat.step(Input::Output { now: 10 });
    assert_eq!((chat.state, chat.source), (ChatState::Running, ESTIMATED));
    chat.step(Input::IdleTimer);
    assert_eq!((chat.state, chat.source), (ChatState::Idle, ESTIMATED));
    chat.step(Input::Output { now: 20 });
    assert_eq!(chat.state, ChatState::Running);
    chat.step(report(2, Event::TurnDone));
    assert_eq!((chat.state, chat.source), (ChatState::Idle, Source::Exact));
}

#[test]
fn pending_and_off_behave_as_no_channel() {
    for channel in [ChannelStatus::Pending, ChannelStatus::Off("no channel")] {
        let mut chat = chat(
            channel,
            ChannelCoverage::claude(),
            ChatState::Running,
            ESTIMATED,
        );
        chat.step(Input::Bell { now: 0 });
        assert_eq!((chat.state, chat.source), (ChatState::Waiting, ESTIMATED));
        chat.step(Input::Output { now: 1_000 });
        assert_eq!((chat.state, chat.source), (ChatState::Running, ESTIMATED));
        chat.step(Input::IdleTimer);
        assert_eq!((chat.state, chat.source), (ChatState::Idle, ESTIMATED));
        chat.step(Input::KeyboardInput);
        assert_eq!((chat.state, chat.source), (ChatState::Running, ESTIMATED));
    }
}

#[test]
fn no_channel_results_are_always_estimated() {
    let mut chat = ChatLive::start(ChannelStatus::Off("no channel"), ChannelCoverage::none(), 0);
    for input in [
        Input::Bell { now: 0 },
        Input::Output { now: 1_000 },
        Input::IdleTimer,
        Input::SendEnter,
        Input::Bell { now: 2_000 },
        Input::KeyboardInput,
    ] {
        chat.step(input);
        assert_eq!(chat.source, ESTIMATED);
    }
}

// Rule 4: turn ends a live channel does not report.

#[test]
fn rule_4_interrupted_turn_ends_in_estimated_idle() {
    let mut chat = live(ChatState::Idle, Source::Exact);
    chat.step(report(1, Event::PromptSubmitted));
    chat.step(Input::IdleTimer);
    assert_eq!((chat.state, chat.source), (ChatState::Idle, NO_END));
}

#[test]
fn rule_4_denied_permission_ends_in_estimated_idle() {
    let mut chat = live(ChatState::Running, Source::Exact);
    chat.step(needs_input(1, Some("permission_prompt")));
    assert_eq!(
        (chat.state, chat.source),
        (ChatState::Waiting, Source::Exact)
    );
    chat.step(Input::KeyboardInput);
    assert_eq!((chat.state, chat.source), (ChatState::Running, NO_END));
    chat.step(Input::IdleTimer);
    assert_eq!((chat.state, chat.source), (ChatState::Idle, NO_END));
}

#[test]
fn rule_4_approval_tool_done_after_keyboard_input_is_exact_running() {
    let mut chat = live(ChatState::Waiting, Source::Exact);
    chat.step(Input::KeyboardInput);
    assert_eq!((chat.state, chat.source), (ChatState::Running, NO_END));
    chat.step(report(1, Event::ToolDone));
    assert_eq!(
        (chat.state, chat.source),
        (ChatState::Running, Source::Exact)
    );
}

#[test]
fn rule_4_idle_timer_never_ends_exact_waiting() {
    let mut chat = live(ChatState::Waiting, Source::Exact);
    for _ in 0..3 {
        assert_eq!(chat.step(Input::IdleTimer), None);
    }
    assert_eq!(
        (chat.state, chat.source),
        (ChatState::Waiting, Source::Exact)
    );
}

#[test]
fn rule_4_idle_prompt_gives_estimated_idle() {
    let mut chat = live(ChatState::Waiting, Source::Exact);
    chat.step(needs_input(1, Some("idle_prompt")));
    assert_eq!((chat.state, chat.source), (ChatState::Idle, NO_END));
}

#[test]
fn rule_4_next_report_restores_exact() {
    let mut chat = live(ChatState::Running, Source::Exact);
    chat.step(Input::IdleTimer);
    assert_eq!(chat.source, NO_END);
    chat.step(report(1, Event::PromptSubmitted));
    assert_eq!(
        (chat.state, chat.source),
        (ChatState::Running, Source::Exact)
    );
    chat.step(Input::IdleTimer);
    chat.step(report(2, Event::TurnDone));
    assert_eq!((chat.state, chat.source), (ChatState::Idle, Source::Exact));
}

#[test]
fn promise_out_of_order_reports_never_move_backward() {
    // Reports of one turn arrive shuffled; only those at or after the newest applied one count.
    let events = [
        (10, Event::PromptSubmitted),
        (30, Event::TurnDone),
        (20, Event::NeedsInput { detail: None }),
        (25, Event::ToolDone),
        (15, Event::PromptSubmitted),
    ];
    let mut chat = live(ChatState::Idle, Source::Exact);
    let mut newest = 0;
    for (at, event) in events {
        chat.step(report(at, event));
        newest = newest.max(at);
        assert_eq!(chat.last_report_at, Some(newest));
    }
    assert_eq!((chat.state, chat.source), (ChatState::Idle, Source::Exact));
}

#[test]
fn promise_no_exact_state_sticks_after_an_unreported_turn_end() {
    for state in [ChatState::Running, ChatState::Waiting] {
        let mut chat = live(state, Source::Exact);
        // The user interrupts or denies; no report follows, only the terminal.
        chat.step(Input::KeyboardInput);
        chat.step(Input::IdleTimer);
        assert_eq!((chat.state, chat.source), (ChatState::Idle, NO_END));
    }
}

#[test]
fn mid_turn_is_exact_running_when_live_otherwise_recent_output() {
    let window = u64::try_from(MID_TURN.as_millis()).unwrap();
    let exact = live(ChatState::Running, Source::Exact);
    assert!(is_mid_turn(&exact, 1_000_000));
    assert!(!is_mid_turn(&live(ChatState::Idle, Source::Exact), 0));

    let mut guessed = no_channel(ChatState::Running);
    guessed.last_output_at = Some(1_000);
    assert!(is_mid_turn(&guessed, 1_000 + window - 1));
    assert!(!is_mid_turn(&guessed, 1_000 + window));
    guessed.state = ChatState::Waiting;
    assert!(!is_mid_turn(&guessed, 1_000));
}
