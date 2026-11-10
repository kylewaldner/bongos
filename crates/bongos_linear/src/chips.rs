//! The chips of a linked task's row: `1 suggestion`, `linear unreachable`, `linear rate limited`
//! and `linear: not found`, from what the sync knows about the last read of its ticket. They show
//! through `RowChipsRegistry`, after the identifier, and only while `linear.enabled` is true.

use std::rc::Rc;

use bongos_work::{RowChipsRegistry, RowChipsSource};
use gpui::{App, SharedString};
use settings::Settings as _;

use crate::LinearSettings;
use crate::sync::{self, ReadState};

/// The name of the source in the registry.
pub(crate) const SOURCE: &str = "linear";

/// The chips for a task with this read state and this many suggestions: the state first.
pub(crate) fn chips_for(read: Option<ReadState>, suggestions: usize) -> Vec<SharedString> {
    let mut chips = Vec::new();
    match read {
        Some(ReadState::Unreachable) => chips.push("linear unreachable".into()),
        Some(ReadState::RateLimited) => chips.push("linear rate limited".into()),
        Some(ReadState::NotFound) => chips.push("linear: not found".into()),
        Some(ReadState::Fresh { .. }) | None => {}
    }
    match suggestions {
        0 => {}
        1 => chips.push("1 suggestion".into()),
        n => chips.push(format!("{n} suggestions").into()),
    }
    chips
}

struct LinearChips;

impl RowChipsSource for LinearChips {
    fn chips(&self, task_id: &str, cx: &App) -> Vec<SharedString> {
        if !LinearSettings::get_global(cx).enabled {
            return Vec::new();
        }
        let Some(linear) = sync::global(cx) else {
            return Vec::new();
        };
        let linear = linear.read(cx);
        chips_for(
            linear.read_state(task_id),
            linear.suggestions(task_id).len(),
        )
    }
}

/// Registers the source. Call after [`sync::install`].
pub(crate) fn install(cx: &mut App) {
    RowChipsRegistry::add_source(SOURCE, Rc::new(LinearChips), cx);
}

/// Tells the Work view that the chips may have changed.
pub(crate) fn changed(cx: &mut App) {
    RowChipsRegistry::changed(cx);
}
