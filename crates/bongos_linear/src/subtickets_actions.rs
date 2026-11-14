//! The action handlers of the subtickets offer (D11): `task: create missing subtickets`, and the
//! preview, checklist and confirm of an offer, which a person answers. Each write they make is
//! attributed to the person at the keyboard; the background sync's writes stay in `subtickets`.

use bongos_linear_model::subtickets::{Missing, Scope as Listed, Unoffered, missing, unoffered};
use bongos_work_model::commands::Context as WorkContext;
use bongos_work_model::{PersonAtKeyboard, TaskId};
use gpui::{Context, EntityId, Window};
use workspace::Workspace;

use crate::actions::show_toast;
use crate::checklist;
use crate::preview;
use crate::refresh;
use crate::subtickets::{
    Confirmed, NO_GROUP, Offer, claim, confirm, confirm_question, create, enabled, group_for,
    identifier_of, offer_question, preview_text, preview_title, record, release, rows, select,
};
use crate::token;

// ---------------------------------------------------------------------------------------------
// The offer: preview, checklist, confirm. Nothing here sends a request.

/// Opens the preview of `offer`, then the checklist over it.
pub(crate) fn open_offer(
    workspace: &mut Workspace,
    offer: Offer,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let text = preview_text(&offer.items, &offer.identifier);
    preview::open(
        preview_title(&offer.identifier),
        text,
        workspace,
        window,
        cx,
        Box::new(move |workspace, item, window, cx| {
            open_checklist(workspace, offer, item, window, cx)
        }),
    );
}

fn open_checklist(
    workspace: &mut Workspace,
    offer: Offer,
    item: EntityId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let title = offer_question(&offer.items, &offer.identifier);
    let rows = rows(&offer.items);
    checklist::open(
        workspace,
        title,
        rows,
        Box::new(move |answer, workspace, window, cx| match answer {
            Some(checked) => {
                let (chosen, declined) = select(&offer.items, &checked);
                if chosen.is_empty() {
                    not_now(workspace, offer, item, window, cx);
                } else {
                    ask_confirm(workspace, offer, chosen, declined, item, window, cx);
                }
            }
            None => not_now(workspace, offer, item, window, cx),
        }),
        window,
        cx,
    );
}

fn ask_confirm(
    workspace: &mut Workspace,
    offer: Offer,
    chosen: Vec<Missing>,
    declined: Vec<TaskId>,
    item: EntityId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = cx.entity().downgrade();
    let question = confirm_question(chosen.len(), &offer.identifier);
    bongos_minibuffer::yes_or_no(
        workspace,
        bongos_work::MINIBUFFER_NAME,
        question,
        false,
        move |yes, window, cx| {
            let ctx = WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx));
            weak.update(cx, |workspace, cx| {
                match confirm::answer(yes, &offer, chosen) {
                    Some(confirmed) => {
                        preview::close(workspace, item, window, cx);
                        record(workspace, &offer, declined, ctx.clone(), cx);
                        create_after_token(confirmed, ctx, cx);
                    }
                    None => not_now(workspace, offer, item, window, cx),
                }
            })
            .ok();
        },
        window,
        cx,
    );
}

/// "Not now": the records are offered and every leaf of the offer is declined. Nothing is sent.
fn not_now(
    workspace: &mut Workspace,
    offer: Offer,
    item: EntityId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    preview::close(workspace, item, window, cx);
    let declined = offer
        .items
        .iter()
        .filter(|item| !item.is_group)
        .map(|item| item.task_id.clone())
        .collect();
    let ctx = WorkContext::at_keyboard(PersonAtKeyboard::from_action(window, cx));
    record(workspace, &offer, declined, ctx, cx);
    release(&offer.group_id, cx);
}

/// Reads the token (a toast and nothing more without one), then creates.
fn create_after_token(confirmed: Confirmed, ctx: WorkContext, cx: &mut Context<Workspace>) {
    token::with_token_state(cx, move |workspace, present, cx| {
        if !present {
            show_toast(workspace, token::NO_TOKEN_MESSAGE.to_string(), cx);
            release(&confirmed.group_id, cx);
            return;
        }
        if !enabled(cx) {
            release(&confirmed.group_id, cx);
            return;
        }
        cx.spawn(async move |workspace, cx| {
            let group_id = confirmed.group_id.clone();
            let message = create(confirmed, ctx, cx).await;
            cx.update(|cx| {
                release(&group_id, cx);
                refresh::sync_linked(cx);
            });
            workspace
                .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                .ok();
        })
        .detach();
    });
}

// ---------------------------------------------------------------------------------------------
// The command

/// `task: create missing subtickets`: every task under the linked group at or above the task at
/// point that has no ticket, declined ones included, then the same preview, checklist and
/// confirm. The token is read first; no token means a toast and nothing else.
pub(crate) fn create_missing_subtickets(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(tree) = bongos_work::ready(workspace, cx) else {
        return;
    };
    let group_id =
        bongos_work::task_at_point(workspace, window, cx).and_then(|id| group_for(&tree, &id));
    let Some(group_id) = group_id else {
        show_toast(workspace, NO_GROUP.to_string(), cx);
        return;
    };
    let Some(identifier) = identifier_of(&tree, &group_id) else {
        show_toast(workspace, NO_GROUP.to_string(), cx);
        return;
    };
    let items = match missing(&tree, &group_id, Listed::Command) {
        Ok(items) => items,
        Err(reason) => {
            show_toast(workspace, reason, cx);
            return;
        }
    };
    if items.is_empty() {
        show_toast(
            workspace,
            format!("Every task under {identifier} has a ticket"),
            cx,
        );
        return;
    }
    let offers: Vec<Unoffered> = unoffered(&tree)
        .into_iter()
        .filter(|offer| offer.group_id == group_id)
        .collect();
    let handle = window.window_handle();
    token::with_token(cx, move |_, _, cx| {
        if !enabled(cx) || !claim(&group_id, cx) {
            return;
        }
        let offer = Offer {
            group_id,
            identifier,
            items,
            offers,
        };
        let workspace = cx.entity().downgrade();
        // The token read has no window; the preview needs one.
        cx.defer(move |cx| {
            let group_id = offer.group_id.clone();
            let opened = handle.update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| open_offer(workspace, offer, window, cx))
            });
            if !matches!(opened, Ok(Ok(()))) {
                release(&group_id, cx);
            }
        });
    });
}
