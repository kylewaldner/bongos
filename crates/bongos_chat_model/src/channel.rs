//! The chat channel: how an agent's own events reach bongos as small reports over a local socket.
//! [`report`] is the wire format (one request line and one reply line per connection),
//! [`translate`] turns an agent's hook input into a report without keeping any conversation content,
//! [`state`] folds reports and terminal signals into a chat's live state, exact or estimated, and
//! [`client`] is the reporter process that sends one report. [`request`] is the request line a chat
//! sends to ask bongos something and wait for the answer.

pub mod client;
pub mod report;
pub mod request;
pub mod state;
pub mod translate;

#[cfg(all(test, unix))]
mod client_tests;

#[cfg(test)]
mod report_tests;

#[cfg(test)]
mod request_tests;

#[cfg(test)]
mod state_tests;

#[cfg(test)]
mod translate_tests;
