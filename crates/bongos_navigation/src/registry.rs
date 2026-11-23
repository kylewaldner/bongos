//! The open navigation sessions, one per workspace, and the echo for an in-session action when
//! none is open.

use std::{collections::HashMap, rc::Rc};

use gpui::{App, Context, EntityId, Global, Window};
use workspace::{MinibufferMessageKind, MinibufferRouting, Workspace};

/// What the echo area says when an in-session action runs with no picker open.
pub const NO_PICKER: &str = "No navigation picker";

/// A live navigation session, as the registry sees it.
pub trait SessionHandle {}

#[derive(Default)]
struct Registry {
    sessions: HashMap<EntityId, Rc<dyn SessionHandle>>,
}

impl Global for Registry {}

/// Records `session` as the open one of `workspace`, replacing any earlier one.
pub fn register(cx: &mut App, workspace: EntityId, session: Rc<dyn SessionHandle>) {
    cx.default_global::<Registry>()
        .sessions
        .insert(workspace, session);
}

/// Forgets the session of `workspace`.
pub fn unregister(cx: &mut App, workspace: EntityId) {
    cx.default_global::<Registry>().sessions.remove(&workspace);
}

/// The open session of `workspace`.
pub fn current(cx: &App, workspace: EntityId) -> Option<Rc<dyn SessionHandle>> {
    cx.try_global::<Registry>()?
        .sessions
        .get(&workspace)
        .cloned()
}

/// Runs `act` on the open session of `workspace`, or echoes that there is none.
pub fn in_session(
    window: &mut Window,
    cx: &mut Context<Workspace>,
    act: impl FnOnce(Rc<dyn SessionHandle>, &mut Window, &mut Context<Workspace>),
) {
    match current(cx, cx.entity_id()) {
        Some(session) => act(session, window, cx),
        None => {
            MinibufferRouting::message(window, cx, NO_PICKER, MinibufferMessageKind::Message);
        }
    }
}
