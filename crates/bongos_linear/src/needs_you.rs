//! The chip `3 need tickets` and the needs-you reason of a linked group whose tasks an agent or
//! a workflow added (an applied proposal with `by` other than `kyle`) and that have no ticket yet.
//! Both last until `task: create missing subtickets` runs on the group and is answered. What they
//! show is computed from the work tree by `subtickets` on every store change; nothing here is
//! written, and nothing here sends a request.

use std::rc::Rc;

use bongos_work::{NeedsYou, NeedsYouRegistry, NeedsYouSource};
use gpui::App;
use settings::Settings as _;

use crate::LinearSettings;
use crate::subtickets;

/// The name of the source in the needs-you registry.
pub(crate) const SOURCE: &str = "linear subtickets";

/// The chip and the needs-you reason of a group with `count` tasks that need a ticket.
pub(crate) fn need_tickets(count: usize) -> String {
    if count == 1 {
        "1 needs a ticket".to_string()
    } else {
        format!("{count} need tickets")
    }
}

struct NeedTickets;

impl NeedsYouSource for NeedTickets {
    fn needs_you(&self, cx: &App) -> Vec<NeedsYou> {
        if !LinearSettings::get_global(cx).enabled {
            return Vec::new();
        }
        subtickets::need_tickets(cx)
            .into_iter()
            .map(|(task_id, count)| NeedsYou {
                task_id,
                reason: need_tickets(count),
            })
            .collect()
    }
}

/// Registers the source.
pub(crate) fn install(cx: &mut App) {
    NeedsYouRegistry::add_source(SOURCE, Rc::new(NeedTickets), cx);
}

/// Tells the registry that what the source reports may have changed.
pub(crate) fn changed(cx: &mut App) {
    NeedsYouRegistry::changed(cx);
}
