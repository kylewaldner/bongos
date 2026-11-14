//! The data of a landing check's run: when it runs, what it says, and the stamp its result is
//! stored under.

use std::fmt;
use std::path::PathBuf;

/// When a landing check runs: before the change enters the queue (slow checks, D11 step 3) or in
/// the queue on the rebased tree (cheap checks, D11 step 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CheckPhase {
    BeforeQueue,
    InQueue,
}

impl CheckPhase {
    /// The word a `[[landing.checks]]` entry's `phase` key uses.
    pub fn word(self) -> &'static str {
        match self {
            CheckPhase::BeforeQueue => "before-queue",
            CheckPhase::InQueue => "in-queue",
        }
    }

    /// The phase a `phase` key names; `None` for any other word, `"amending"` included.
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "before-queue" => Some(CheckPhase::BeforeQueue),
            "in-queue" => Some(CheckPhase::InQueue),
            _ => None,
        }
    }
}

/// What a landing check says about one tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Pass,
    Fail {
        message: String,
        /// A file the check wrote, shown in place in the landing view.
        output: Option<PathBuf>,
    },
    /// Neither pass nor fail: the change waits with `reason` and the check is retried.
    NoVerdict {
        reason: String,
        output: Option<PathBuf>,
    },
}

impl CheckOutcome {
    pub fn fail(message: impl Into<String>) -> Self {
        CheckOutcome::Fail {
            message: message.into(),
            output: None,
        }
    }

    pub fn no_verdict(reason: impl Into<String>) -> Self {
        CheckOutcome::NoVerdict {
            reason: reason.into(),
            output: None,
        }
    }

    /// `Pass` when `refusals` is empty, otherwise `Fail` with them one per line.
    pub fn from_refusals(refusals: Vec<String>) -> Self {
        if refusals.is_empty() {
            CheckOutcome::Pass
        } else {
            CheckOutcome::fail(refusals.join("\n"))
        }
    }

    /// The word a trailer and the landing view show.
    pub fn word(&self) -> &'static str {
        match self {
            CheckOutcome::Pass => "pass",
            CheckOutcome::Fail { .. } => "fail",
            CheckOutcome::NoVerdict { .. } => "no-verdict",
        }
    }

    /// The failure message, if the outcome is `Fail`.
    pub fn message(&self) -> Option<&str> {
        match self {
            CheckOutcome::Fail { message, .. } => Some(message),
            _ => None,
        }
    }
}

/// What an amending check did to the working copy's branch checkout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AmendOutcome {
    Unchanged,
    /// The check wrote files; the engine commits them as "amend: <check> <message>".
    Amended {
        message: String,
    },
    NoVerdict {
        reason: String,
        output: Option<PathBuf>,
    },
}

impl AmendOutcome {
    pub fn no_verdict(reason: impl Into<String>) -> Self {
        AmendOutcome::NoVerdict {
            reason: reason.into(),
            output: None,
        }
    }
}

/// What a check's result is stored under besides the change, check and tree; a result whose
/// stamp differs from the current one is rerun. By default a check's stamp is the tree id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp(pub String);

impl Stamp {
    /// The default stamp: the tree id itself.
    pub fn tree(tree_id: impl Into<String>) -> Self {
        Stamp(tree_id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
