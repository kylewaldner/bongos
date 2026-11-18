//! The chat channel: how an agent's own events reach bongos as small reports over a local socket.
//! [`report`] is the wire format (one request line and one reply line per connection) and
//! [`translate`] turns an agent's hook input into a report without keeping any conversation content.

pub mod report;
pub mod translate;

#[cfg(test)]
mod report_tests;

#[cfg(test)]
mod translate_tests;
