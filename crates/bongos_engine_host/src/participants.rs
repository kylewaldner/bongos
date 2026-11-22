//! Writer participants: parts of the app that work only in the process holding a project's
//! writer (the lanes' dispatcher, say). Each is told when this process becomes a project's writer
//! ([`WriterParticipant::started`]) and is drained, in registration order, when the writer moves
//! to another process, after the workflow runs and the task checks are drained, so no participant
//! drains while any node process runs. When the writer stays here after all (a cancelled
//! handover, or a take-back from an engine host that did not start),
//! [`WriterParticipants::undrain`] undrains them in reverse order and then every one is
//! [`WriterParticipant::started`] again.

use std::rc::Rc;

use anyhow::Result;
use gpui::{App, Global, Task};

/// A part of the app that works only in the process holding a project's writer.
///
/// `started` and `undrain` may come again for a project already started or not drained (a
/// handover cancelled before the participants drained, a second take-back): both must be
/// idempotent.
pub trait WriterParticipant {
    /// This process became the writer of `project` (a window that opened it first, the engine
    /// host once it took the lock, or a window that kept or took back the writer).
    fn started(&self, project: &str, cx: &mut App);
    /// The writer of `project` moves away: stop starting work; resolves once the work it began has
    /// stopped or reached a point the next writer continues from.
    fn drain(&self, project: &str, cx: &mut App) -> Task<()>;
    /// The writer of `project` stays here after a drain: work starts again. A failure is logged
    /// and the other participants undrain all the same.
    fn undrain(&self, project: &str, cx: &mut App) -> Result<()>;
}

/// The participants registered in this process, in registration order.
#[derive(Default)]
pub struct WriterParticipants(Vec<Rc<dyn WriterParticipant>>);

impl Global for WriterParticipants {}

impl WriterParticipants {
    /// Adds `participant` after those registered before it.
    pub fn register(participant: Rc<dyn WriterParticipant>, cx: &mut App) {
        cx.default_global::<Self>().0.push(participant);
    }

    fn all(cx: &App) -> Vec<Rc<dyn WriterParticipant>> {
        cx.try_global::<Self>()
            .map(|participants| participants.0.clone())
            .unwrap_or_default()
    }

    /// Tells every participant, in registration order, that this process became the writer of
    /// `project`.
    pub fn started(project: &str, cx: &mut App) {
        for participant in Self::all(cx) {
            participant.started(project, cx);
        }
    }

    /// Drains every participant for `project`, one after the other in registration order: the
    /// next is asked only once the one before resolved.
    pub fn drain(project: &str, cx: &mut App) -> Task<()> {
        let participants = Self::all(cx);
        if participants.is_empty() {
            return Task::ready(());
        }
        let project = project.to_string();
        cx.spawn(async move |cx| {
            for participant in participants {
                let drained = cx.update(|cx| participant.drain(&project, cx));
                drained.await;
            }
        })
    }

    /// Undrains every participant for `project`, in reverse registration order. One that fails is
    /// logged and the rest still undrain.
    pub fn undrain(project: &str, cx: &mut App) {
        for participant in Self::all(cx).into_iter().rev() {
            if let Err(error) = participant.undrain(project, cx) {
                log::error!("undraining a writer participant of {project}: {error:#}");
            }
        }
    }
}
