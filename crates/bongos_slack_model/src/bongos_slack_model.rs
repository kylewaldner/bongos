//! Slack data model for bongos: conversation and message types, the `SlackApi` trait, the token
//! (which never prints), the scopes the app asks for, the allowed list of destinations and the
//! approval rule that says whether a post is sent or asked about first.
//!
//! It also parses Slack's answers (`wire`), renders messages as text (`render`) and as a quoted
//! block for an agent (`quote`).
//!
//! This crate is plain Rust and does no I/O: no network, no files and no clock.

pub mod allowed;
pub mod api;
pub mod approval;
pub mod quote;
pub mod render;
pub mod scopes;
pub mod token;
pub mod types;
pub mod wire;

#[cfg(any(test, feature = "test-support"))]
pub mod fake;

pub use allowed::AllowedList;
pub use api::{ApiError, AuthInfo, PostRequest, PostedMessage, SlackApi, User};
pub use approval::{Decision, Origin, PostSummary, decide, question_hash};
pub use token::{SlackToken, TokenError, TokenKind, redact};
pub use types::{
    Conversation, ConversationId, ConversationKind, Cursor, Destination, DestinationError, Message,
    Page, Ts, UserId,
};

#[cfg(test)]
mod approval_tests;
#[cfg(test)]
mod fake_tests;
#[cfg(test)]
mod quote_tests;
#[cfg(test)]
mod render_tests;
#[cfg(test)]
mod token_tests;
#[cfg(test)]
mod types_tests;
#[cfg(test)]
mod wire_tests;
