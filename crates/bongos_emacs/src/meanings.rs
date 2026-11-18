//! What the commands of the other features do with a prefix argument, declared by action name so
//! this crate depends on none of them. A name nothing registers is never built, so a declaration
//! is harmless until its action exists; the app's tests build every name.

use gpui::App;

use crate::prefix_arg::{Meaning, declare};

/// Actions with a variant that takes the place of the plain one when an argument is given.
pub const REDIRECTS: [(&str, &str); 7] = [
    ("task::Advance", "task::AdvanceOverride"),
    ("chat::New", "chat::NewWithOptions"),
    ("chat::Open", "chat::OpenAny"),
    ("chat::ForkToTask", "chat::ForkToTaskAs"),
    ("task::New", "task::NewWithoutPlan"),
    ("prompts::Send", "prompts::SendWithoutMarking"),
    ("workflow::RetryStep", "workflow::RetryStepFresh"),
];

/// Actions with a `count` field, each with the action a negative count swaps to.
pub const COUNT_FIELDS: [(&str, &str); 2] = [
    ("prompts::Previous", "prompts::Next"),
    ("prompts::Next", "prompts::Previous"),
];

/// Actions that have no use for an argument: it is cleared and they run once.
pub const IGNORED: [&str; 12] = [
    "isearch::Forward",
    "isearch::Backward",
    "isearch::ForwardRegexp",
    "isearch::BackwardRegexp",
    "emacs::PopGlobalMark",
    "query_replace::QueryReplace",
    "query_replace::QueryReplaceRegexp",
    "query_replace::ReplaceString",
    "query_replace::ReplaceRegexp",
    "query_replace::Abort",
    "isearch::QueryReplace",
    "isearch::QueryReplaceRegexp",
];

/// Declares every meaning above.
pub(crate) fn declare_all(cx: &mut App) {
    for (name, target) in REDIRECTS {
        declare(cx, name, Meaning::Redirect(target));
    }
    for (name, opposite) in COUNT_FIELDS {
        declare(cx, name, Meaning::CountFieldSwap(opposite));
    }
    for name in IGNORED {
        declare(cx, name, Meaning::Ignored);
    }
}
