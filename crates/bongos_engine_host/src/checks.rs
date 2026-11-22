//! Task checks' writing commands served by the writer: `checks` extension commands run in the
//! writer's checks engine, trusted to speak for a person only from a window of the same
//! executable.

use std::rc::Rc;

use anyhow::Result;
use bongos_checks::host::CHECKS_NAMESPACE;
use gpui::{App, Task};

use crate::commands::{HostCommands, HostHandler};

/// The `checks` handler around `handle`, which takes the payload and whether the peer may speak
/// for the person at the keyboard (a command-line peer never may).
pub fn checks_handler(
    handle: impl Fn(serde_json::Value, bool, &mut App) -> Task<Result<serde_json::Value>> + 'static,
) -> HostHandler {
    Rc::new(move |payload, peer, cx| handle(payload, peer.may_attest_person(), cx))
}

/// Serves `checks` commands with task checks' own handler.
pub fn register_checks(cx: &mut App) {
    HostCommands::register(
        CHECKS_NAMESPACE,
        checks_handler(bongos_checks::host::handle),
        cx,
    );
}
