//! The builder's "needs you" source (D13, D16): every proposal that waits for Kyle, reported on
//! its root task, which feature 03 rolls up to every ancestor row. It reads only what the last
//! scan of [`crate::later`] found: a proposal applied or rejected as it is, one whose diff is
//! empty (an untouched file of `task: propose`) and one being applied on its own are skipped.
//! While `task_builder.enabled` is false it reports nothing.

use std::rc::Rc;

use bongos_work::{NeedsYou, NeedsYouRegistry, NeedsYouSource};
use gpui::App;
use settings::Settings as _;

use crate::TaskBuilderSettings;
use crate::later::{Entry, Later, Status};

/// The name of the source in the registry.
pub(crate) const SOURCE: &str = "task builder proposals";

/// The reason a proposal gives, or `None` when it does not wait for Kyle.
pub(crate) fn reason(entry: &Entry) -> Option<String> {
    let number = entry.number;
    match &entry.status {
        Status::Pending => Some(format!("proposal {number} pending")),
        Status::Problems(1) => Some(format!("proposal {number} has 1 problem")),
        Status::Problems(count) => Some(format!("proposal {number} has {count} problems")),
        Status::Failed(why) => Some(format!("proposal {number} pending: {why}")),
        Status::Settled | Status::Untouched | Status::Applying => None,
    }
}

struct Proposals;

impl NeedsYouSource for Proposals {
    fn needs_you(&self, cx: &App) -> Vec<NeedsYou> {
        if !TaskBuilderSettings::get_global(cx).enabled {
            return Vec::new();
        }
        let Some(later) = cx.try_global::<Later>() else {
            return Vec::new();
        };
        later
            .entries
            .values()
            .filter_map(|entry| {
                Some(NeedsYou {
                    task_id: entry.task_id.clone(),
                    reason: reason(entry)?,
                })
            })
            .collect()
    }
}

/// Registers the source.
pub(crate) fn install(cx: &mut App) {
    NeedsYouRegistry::add_source(SOURCE, Rc::new(Proposals), cx);
}
