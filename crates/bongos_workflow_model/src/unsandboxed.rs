//! How a program starts when no OS sandbox is available: one pure decision over the sandbox probe's
//! answer, the run's permit and the effective mode. Nothing here writes or remembers anything; a
//! grant to run unsandboxed lives in the writer's memory only, never in the run record.
//!
//! The script's own audit checks are defense in depth, not a security boundary: the OS sandbox is.

/// The per-start sandbox probe's answer: `Ok(())` when a sandbox works for this node's policy,
/// otherwise why not.
pub type Availability = Result<(), String>;

/// The run's word on an unsandboxed start, built by the caller. Anything not built by a caller that
/// knows the run (watchers, test cases, unattended and heal runs) is `Refuse`, so a forgotten caller
/// fails closed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Permit {
    #[default]
    Refuse,
    Ask,
    Allowed(AllowedBy),
}

/// Who allowed an unsandboxed start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowedBy {
    /// A person approved this run once, at `at` (RFC 3339).
    RunConsent { at: String, by: String },
    /// The user setting is `allow`.
    Setting,
}

/// The effective mode: the user setting `workflows.programs.unsandboxed`, tightened by the project.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Ask a person to approve the run once.
    #[default]
    Ask,
    Refuse,
    Allow,
}

/// What a start does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    Sandboxed,
    Unsandboxed { note: String },
    Ask { message: String },
    Refuse { message: String },
}

/// What the run record and the run view say when an unsandboxed start is refused.
pub const REFUSED_TEXT: &str = "refused: no OS sandbox";

/// The effective mode: a project's `refuse` wins over every user setting, `allow` included; the
/// project can only tighten.
pub fn effective_mode(user: Mode, project_refuse: bool) -> Mode {
    if project_refuse { Mode::Refuse } else { user }
}

/// Decide how a program starts. With a working sandbox it is always sandboxed and nothing is asked.
pub fn decide(availability: &Availability, permit: &Permit, mode: Mode) -> Launch {
    let reason = match availability {
        Ok(()) => return Launch::Sandboxed,
        Err(reason) => reason,
    };
    let refuse = || Launch::Refuse {
        message: format!("{REFUSED_TEXT} ({reason})"),
    };
    match (mode, permit) {
        (Mode::Refuse, _) => refuse(),
        // A `Refuse` permit means a caller that must never run unsandboxed (an unattended or heal
        // run, or one that did not build a permit). It refuses even under `allow`: fail closed.
        (Mode::Allow | Mode::Ask, Permit::Refuse) => refuse(),
        (Mode::Allow, Permit::Ask | Permit::Allowed(AllowedBy::Setting)) => Launch::Unsandboxed {
            note: allowed_note(&AllowedBy::Setting),
        },
        (Mode::Allow | Mode::Ask, Permit::Allowed(by)) => Launch::Unsandboxed {
            note: allowed_note(by),
        },
        (Mode::Ask, Permit::Ask) => Launch::Ask {
            message: format!("no OS sandbox is available ({reason})"),
        },
    }
}

fn allowed_note(by: &AllowedBy) -> String {
    match by {
        AllowedBy::RunConsent { at, .. } => format!("unsandboxed (approved {at})"),
        AllowedBy::Setting => "unsandboxed (allowed by setting)".to_string(),
    }
}

/// The question a person sees when a program node needs an answer to run without a sandbox.
pub fn ask_text(node: &str, reason: &str) -> String {
    format!(
        "Program node '{node}' was not run: no OS sandbox is available ({reason}). Install \
         Bubblewrap (Linux) or approve this run once. The script's own audit checks are not a \
         security boundary."
    )
}
