use crate::unsandboxed::*;

const MODES: [Mode; 4] = [Mode::Fallback, Mode::Ask, Mode::Refuse, Mode::Allow];

fn consent() -> AllowedBy {
    AllowedBy::RunConsent {
        at: "2026-01-02T03:04:05Z".to_string(),
        by: "person".to_string(),
    }
}

fn permits() -> [Permit; 4] {
    [
        Permit::Refuse,
        Permit::Ask,
        Permit::Allowed(consent()),
        Permit::Allowed(AllowedBy::Setting),
    ]
}

fn missing() -> Availability {
    Err("bwrap not found".to_string())
}

fn unsandboxed(note: &str) -> Launch {
    Launch::Unsandboxed {
        note: note.to_string(),
    }
}

fn refused() -> Launch {
    Launch::Refuse {
        message: "refused: no OS sandbox (bwrap not found)".to_string(),
    }
}

#[test]
fn a_working_sandbox_never_asks() {
    for mode in MODES {
        for permit in permits() {
            assert_eq!(
                decide(&Ok(()), &permit, mode),
                Launch::Sandboxed,
                "{mode:?} {permit:?}"
            );
        }
    }
    assert_eq!(Permit::default(), Permit::Refuse);
    assert_eq!(Mode::default(), Mode::Fallback);
}

#[test]
fn without_a_sandbox_the_mode_and_permit_decide() {
    let fallback = unsandboxed("not sandboxed: bwrap not found");
    let approved = unsandboxed("unsandboxed (approved 2026-01-02T03:04:05Z)");
    let by_setting = unsandboxed("unsandboxed (allowed by setting)");
    let ask = Launch::Ask {
        message: "no OS sandbox is available (bwrap not found)".to_string(),
    };
    let [refuse, ask_permit, consented, setting] = permits();
    let table = [
        // Fallback is today's behavior whatever the permit.
        (Mode::Fallback, refuse.clone(), fallback.clone()),
        (Mode::Fallback, ask_permit.clone(), fallback.clone()),
        (Mode::Fallback, consented.clone(), fallback.clone()),
        (Mode::Fallback, setting.clone(), fallback),
        (Mode::Ask, refuse.clone(), refused()),
        (Mode::Ask, ask_permit.clone(), ask),
        (Mode::Ask, consented.clone(), approved.clone()),
        (Mode::Ask, setting.clone(), by_setting.clone()),
        // Refuse never runs, whatever the permit.
        (Mode::Refuse, refuse.clone(), refused()),
        (Mode::Refuse, ask_permit.clone(), refused()),
        (Mode::Refuse, consented.clone(), refused()),
        (Mode::Refuse, setting.clone(), refused()),
        // Allow still refuses a caller that must never run unsandboxed.
        (Mode::Allow, refuse, refused()),
        (Mode::Allow, ask_permit, by_setting.clone()),
        (Mode::Allow, consented, approved),
        (Mode::Allow, setting, by_setting),
    ];
    for (mode, permit, launch) in table {
        assert_eq!(
            decide(&missing(), &permit, mode),
            launch,
            "{mode:?} {permit:?}"
        );
    }
    assert_eq!(REFUSED_TEXT, "refused: no OS sandbox");
    assert_eq!(
        ask_text("lint", "bwrap not found"),
        "Program node 'lint' was not run: no OS sandbox is available (bwrap not found). Install \
         Bubblewrap (Linux) or approve this run once. The script's own audit checks are not a \
         security boundary."
    );
}

#[test]
fn a_project_refuse_beats_allow() {
    for mode in MODES {
        assert_eq!(effective_mode(mode, true), Mode::Refuse, "{mode:?}");
        assert_eq!(effective_mode(mode, false), mode, "{mode:?}");
    }
    for permit in permits() {
        assert_eq!(
            decide(&missing(), &permit, effective_mode(Mode::Allow, true)),
            refused(),
            "{permit:?}"
        );
    }
}
