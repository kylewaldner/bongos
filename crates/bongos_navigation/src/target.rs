//! Where a chosen row opens: the current pane or another one.
//!
//! [`resolve`] only reads: it says which pane a picker previews in and how a confirm would open,
//! and it never splits or adds a pane. [`realize`] does the split, once, when the row is
//! confirmed.

use std::rc::Rc;

use bongos_minibuffer::{Session, Value};
use gpui::{App, Context, Entity, WeakEntity, Window};
use workspace::{OpenTarget, Pane, SplitDirection, Workspace};

/// Which pane a row opens in, as the picker's M-o menu or its default asks.
#[derive(Clone)]
pub enum PaneChoice {
    /// The pane an open goes to when none is named.
    Default,
    /// The next center pane after the active one, or a split to its right when there is one pane.
    OtherPane,
    /// A new pane to the right of the active one.
    SplitRight,
    /// A new pane below the active one.
    SplitDown,
    /// This pane.
    Letter(Entity<Pane>),
}

/// How a confirmed row opens.
#[derive(Clone)]
pub enum OpenAt {
    /// In a pane that exists.
    Pane(Entity<Pane>),
    /// In a new pane split off `of` in `direction`.
    Split {
        of: Entity<Pane>,
        direction: SplitDirection,
    },
}

/// What a [`PaneChoice`] comes to.
#[derive(Clone)]
pub struct Resolved {
    /// The pane the preview shows in: the target pane, or the pane that would be split.
    pub preview_pane: Entity<Pane>,
    pub open: OpenAt,
}

/// Resolves `choice` without changing the workspace.
pub fn resolve(workspace: &Workspace, choice: &PaneChoice, cx: &App) -> Resolved {
    let preferred = workspace.active_pane().clone();
    let split = |direction| Resolved {
        preview_pane: preferred.clone(),
        open: OpenAt::Split {
            of: preferred.clone(),
            direction,
        },
    };
    match choice {
        PaneChoice::Default => match workspace.open_target(&preferred, cx) {
            OpenTarget::Pane(pane) => existing(pane),
            OpenTarget::SplitRightOf(of) => Resolved {
                preview_pane: of.clone(),
                open: OpenAt::Split {
                    of,
                    direction: SplitDirection::Right,
                },
            },
        },
        PaneChoice::OtherPane => {
            let panes = workspace.center_panes();
            match panes.iter().position(|pane| *pane == preferred) {
                Some(ix) if panes.len() > 1 => existing(panes[(ix + 1) % panes.len()].clone()),
                None if !panes.is_empty() => existing(panes[0].clone()),
                _ => split(SplitDirection::Right),
            }
        }
        PaneChoice::SplitRight => split(SplitDirection::Right),
        PaneChoice::SplitDown => split(SplitDirection::Down),
        PaneChoice::Letter(pane) => existing(pane.clone()),
    }
}

fn existing(pane: Entity<Pane>) -> Resolved {
    Resolved {
        preview_pane: pane.clone(),
        open: OpenAt::Pane(pane),
    }
}

/// The pane a confirmed row opens in. This is the only place a choice splits: an existing pane is
/// returned as it is, a pending split is made now.
pub fn realize(
    workspace: &mut Workspace,
    resolved: Resolved,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<Pane> {
    match resolved.open {
        OpenAt::Pane(pane) => pane,
        OpenAt::Split { of, direction } => workspace.split_pane(of, direction, window, cx),
    }
}

type OnChoice = Rc<dyn Fn(Value, Entity<Pane>, &mut Window, &mut App)>;

/// Adds the pane entries of the M-o menu to `session`: Other pane (o), Split right (r), Split
/// down (d) and Pane by letter (p). Each realizes its choice at the active pane and calls
/// `on_choice` with the selected row's value and the pane to open it in. Pane by letter asks
/// with the pane picker, and a cancel there calls nothing.
pub fn add_pane_choices(
    session: Session,
    workspace: WeakEntity<Workspace>,
    on_choice: impl Fn(Value, Entity<Pane>, &mut Window, &mut App) + 'static,
) -> Session {
    let on_choice: OnChoice = Rc::new(on_choice);
    let entry = |session: Session, label: &'static str, key: char, choice: PaneChoice| {
        let workspace = workspace.clone();
        let on_choice = on_choice.clone();
        session.action(label, key, move |value, window, cx| {
            let pane = workspace
                .update(cx, |workspace, cx| {
                    let resolved = resolve(workspace, &choice, cx);
                    realize(workspace, resolved, window, cx)
                })
                .ok();
            if let Some(pane) = pane {
                on_choice(value, pane, window, cx);
            }
        })
    };
    let session = entry(session, "Other pane", 'o', PaneChoice::OtherPane);
    let session = entry(session, "Split right", 'r', PaneChoice::SplitRight);
    let session = entry(session, "Split down", 'd', PaneChoice::SplitDown);
    session.action("Pane by letter", 'p', move |value, window, cx| {
        let on_choice = on_choice.clone();
        workspace
            .update(cx, |workspace, cx| {
                bongos_window::picker::pick_pane(workspace, window, cx, move |pane, window, cx| {
                    // The picker may answer while the workspace is being updated.
                    window.defer(cx, move |window, cx| on_choice(value, pane, window, cx));
                })
            })
            .ok();
    })
}

/// Opens the highlighted row in a chosen pane.
pub fn open_in_pane(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}

#[cfg(test)]
#[path = "target_tests.rs"]
mod tests;
