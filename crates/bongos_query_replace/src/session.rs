//! A replace running in an editor. For now this is the batch of `replace-string` and
//! `replace-regexp`: every match of the scope is found on the background executor and replaced
//! on the main thread in chunks of [`crate::engine::SCAN_CHUNK`], one undo entry each, yielding
//! between chunks so the window keeps drawing. At the end the entries are merged into one undo
//! step, the cursor goes to the end of the last replacement and the count goes to the echo area.
//!
//! While a batch runs, its editor has the [`BATCH_KEY_CONTEXT`] key context (through an editor
//! addon), where `C-g` is [`Abort`]: the batch stops before its next chunk and the replacements
//! already made stay.

use std::any::Any;
use std::future::poll_fn;
use std::ops::Range;
use std::sync::Arc;
use std::task::Poll;

use bongos_isearch::engine::Cancel;
use bongos_query_replace_model::session::{Effect, Found, Key, Session};
use bongos_query_replace_model::template::Template;
use bongos_query_replace_model::text::end_message;
use editor::{Addon, Editor, SelectionEffects, scroll::Autoscroll};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, Entity, KeyContext, Window};
use multi_buffer::{Anchor, MultiBufferOffset};
use project::search::SearchQuery;

use crate::actions::Abort;
use crate::edits::Edits;
use crate::engine::{self, Replacer, ScanEnd};

/// The key context of an editor while a batch replaces in it.
pub const BATCH_KEY_CONTEXT: &str = "QueryReplaceBatch";

/// Marks an editor whose batch is running, and holds the flag that stops it.
struct Batch {
    cancel: Cancel,
}

impl Addon for Batch {
    fn extend_key_context(&self, context: &mut KeyContext, _: &App) {
        context.add(BATCH_KEY_CONTEXT);
    }

    fn to_any(&self) -> &dyn Any {
        self
    }
}

/// Registers [`Abort`] on every editor: it stops the editor's running batch, and passes the key on
/// when there is none.
pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, cx| {
        let weak = cx.entity().downgrade();
        editor
            .register_action(move |_: &Abort, _, cx| {
                let cancel = weak.upgrade().and_then(|editor| {
                    editor
                        .read(cx)
                        .addon::<Batch>()
                        .map(|batch| batch.cancel.clone())
                });
                match cancel {
                    Some(cancel) => cancel.cancel(),
                    None => cx.propagate(),
                }
            })
            .detach();
    })
    .detach();
}

/// Whether a batch is replacing in `editor` now.
pub fn batch_running(editor: &Editor) -> bool {
    editor.addon::<Batch>().is_some()
}

/// What a batch replaces: every match of `query` in `scope` from `first`, the first match, found
/// on the editor's current text. `preserve_case` and `template` make the replacements.
pub(crate) struct BatchRequest {
    pub query: Arc<SearchQuery>,
    pub scope: Range<Anchor>,
    pub first: Found,
    pub preserve_case: bool,
    pub template: Template,
}

/// Replaces every match of `request` in `editor` without asking, as one undo step.
pub(crate) fn run_batch(
    editor: &Entity<Editor>,
    request: BatchRequest,
    window: &mut Window,
    cx: &mut App,
) {
    let BatchRequest {
        query,
        scope,
        first,
        preserve_case,
        template,
    } = request;
    let mut model = Session::new(preserve_case, template.clone());
    model.on_match(Some(first));
    let from = match model.on_key(Key::All).as_slice() {
        [Effect::StartBatch { from }] => from.start,
        _ => return,
    };

    let cancel = Cancel::new();
    let (mut edits, snapshot) = editor.update(cx, |editor, cx| {
        editor.register_addon(Batch {
            cancel: cancel.clone(),
        });
        cx.notify();
        (
            Edits::start(editor, cx),
            editor.buffer().read(cx).snapshot(cx),
        )
    });

    let (sender, mut chunks) = mpsc::unbounded();
    let scan = cx.background_spawn({
        let cancel = cancel.clone();
        async move {
            // The model session that computes the replacements, a twin of the one that counts.
            let session = Session::new(preserve_case, template);
            let replacer = Replacer {
                session: &session,
                counter: 0,
            };
            engine::scan(
                &snapshot,
                &query,
                MultiBufferOffset(from),
                &scope,
                &cancel,
                Some(replacer),
                |chunk| {
                    sender.unbounded_send(chunk).ok();
                },
            )
        }
    });

    let editor = editor.downgrade();
    window
        .spawn(cx, async move |cx| {
            let mut replaced = 0;
            let mut end = None;
            let mut stopped = false;
            let mut read_only = None;
            while let Some(chunk) = chunks.next().await {
                if cancel.is_canceled() {
                    stopped = true;
                    break;
                }
                let applied = editor.update_in(cx, |editor, window, cx| {
                    edits.apply_chunk(editor, chunk, window, cx)
                });
                match applied {
                    Ok(Ok(applied)) => {
                        replaced += applied.replaced;
                        end = applied.end.or(end);
                        model.on_batch_progress(replaced);
                    }
                    Ok(Err(error)) => {
                        read_only = Some(error);
                        break;
                    }
                    // The editor or its window is gone.
                    Err(_) => {
                        cancel.cancel();
                        return;
                    }
                }
                yield_now().await;
            }
            let interrupted = if stopped || read_only.is_some() {
                cancel.cancel();
                stopped
            } else {
                matches!(scan.await, ScanEnd::Canceled { .. })
            };

            let finished = editor.update_in(cx, |editor, window, cx| {
                editor.unregister_addon::<Batch>();
                // The cursor goes to the end of the last replacement, which also ends the region.
                if let Some(end) = end {
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::fit()),
                        window,
                        cx,
                        |s| s.select_anchor_ranges([end..end]),
                    );
                }
                edits.finish(editor, cx);
                cx.notify();
            });
            if finished.is_err() {
                return;
            }
            let message = match read_only {
                Some(error) => error.to_string(),
                None => model
                    .on_batch_end(interrupted)
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::Finish(end) => Some(end_message(end)),
                        _ => None,
                    })
                    .unwrap_or_default(),
            };
            cx.update(|window, cx| bongos_minibuffer::message(window, cx, message))
                .ok();
        })
        .detach();
}

/// Lets the other tasks of the main thread run (and the window draw) before going on.
async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}
