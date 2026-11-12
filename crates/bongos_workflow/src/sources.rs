//! What workflows tell the Work view (D15): the `workflows` [`LiveModesSource`], which reports the
//! active run of a task (queued, running, pausing, paused, halted, waiting on you or failed), so
//! `can_start_run` refuses a second run on the leaf everywhere, `task: mode` included.

use std::rc::Rc;

use bongos_work::{LiveModes, LiveModesRegistry, LiveModesSource};
use gpui::App;

use crate::engine::WorkflowEngine;

/// The name of the source in the registry.
pub const SOURCE_NAME: &str = "workflows";

/// Reports the active run of a task from the engine's lifecycle.
struct WorkflowLiveModes {
    engine: WorkflowEngine,
}

impl LiveModesSource for WorkflowLiveModes {
    fn live_modes(&self, task_id: &str, _: &App) -> LiveModes {
        LiveModes {
            open_chats: 0,
            run: self.engine.active_run(task_id),
        }
    }
}

/// Registers the `workflows` source over `engine`, replacing an earlier one.
pub fn register(engine: &WorkflowEngine, cx: &mut App) {
    LiveModesRegistry::add_source(
        SOURCE_NAME,
        Rc::new(WorkflowLiveModes {
            engine: engine.clone(),
        }),
        cx,
    );
}
