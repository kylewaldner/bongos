//! Stacked leaves (D16): what Kyle is told about a linked leaf stacked on another leaf, and the
//! row chip `stacked: check Linear rule`.
//!
//! Linear may mark a ticket Done when its PR merges into the parent's branch, which is not the
//! default branch. Bongos changes no Linear setting and writes no status for a stacked leaf (see
//! `status`); it says what Kyle can do in Linear's GitHub settings instead. A [`Notices`] looks at
//! the work tree on every store change:
//!
//! * a stacked toast, once per task per session, when a linked leaf gains `stacked on` or its plan
//!   begins while it is stacked;
//! * an open parent PR toast whenever a linked leaf gains `stacked on` a leaf whose result is a
//!   PR (Kyle adds the `Closes` line to that PR himself: bongos has no GitHub write path).
//!
//! Both are one toast when they happen together, because a toast of the sync replaces the one
//! before it. The first look only notes what is stacked already, and nothing is said while
//! `linear.enabled` is false (the look still goes on, so turning it on does not announce the past).
//! Nothing here sends a request and nothing here writes the work tree.

use std::collections::{BTreeMap, BTreeSet};

use bongos_linear_model::conventions::{branch_segment, open_pr_warning};
use bongos_linear_model::status::plan_begun;
use bongos_linear_model::sync::linked_identifier;
use bongos_work::{DEFAULT_BRANCH_TEMPLATE, WorkStore, WorkStoreEvent};
use bongos_work_model::derive::{self, Shape};
use bongos_work_model::rules::Place;
use bongos_work_model::store::WorkTree;
use gpui::{App, AppContext as _, Context, Entity, Global, Subscription};

use crate::refresh::{enabled, linked_ids, snapshot, toast_active};

/// The row chip of a linked stacked leaf.
pub(crate) const CHIP: &str = "stacked: check Linear rule";

/// The regular expression of the target branches Kyle gives a 'no action' rule in Linear.
const RULE_BRANCHES: &str = "^[a-z]+-[0-9]+-";

/// Whether `task_id` is a linked leaf with `stacked on`.
fn is_linked_stacked_leaf(tree: &WorkTree, task_id: &str) -> bool {
    let Ok(place) = Place::locate(tree, task_id) else {
        return false;
    };
    place.task.file.as_ref().is_some_and(|file| {
        linked_identifier(file).is_some()
            && file.stacked_on.is_some()
            && derive::shape(place.task, tree) == Shape::Leaf
    })
}

/// The chip of `task_id` on `tree`: [`CHIP`] for a linked stacked leaf.
pub(crate) fn chip_for(tree: &WorkTree, task_id: &str) -> Option<&'static str> {
    is_linked_stacked_leaf(tree, task_id).then_some(CHIP)
}

/// The chip of `task_id` on the store's tree.
pub(crate) fn chip(task_id: &str, cx: &App) -> Option<&'static str> {
    let tree = snapshot(cx)?;
    chip_for(&tree, task_id)
}

/// The stacked toast for the linked leaf `task_id`: who it is stacked on, which branch its PR
/// targets, and the rule to add in Linear. The branch is the parent's identifier and slug under
/// the default branch template; `None` when the task is not stacked on a sibling.
pub(crate) fn stacked_notice(tree: &WorkTree, task_id: &str) -> Option<String> {
    let place = Place::locate(tree, task_id).ok()?;
    let file = place.task.file.as_ref()?;
    let child = linked_identifier(file)?;
    let parent = place.sibling(&file.stacked_on.as_ref()?.task)?;
    let (parent_name, target) = match parent.file.as_ref().and_then(linked_identifier) {
        Some(identifier) => (
            identifier.to_string(),
            branch_segment(identifier, DEFAULT_BRANCH_TEMPLATE, &parent.slug),
        ),
        None => (
            parent.slug.clone(),
            format!("the branch of {}", parent.slug),
        ),
    };
    Some(format!(
        "{child} is stacked on {parent_name}; its PR targets {target}. Linear may mark {child} \
         Done when that PR merges into the parent branch. Add a 'no action' rule in Linear's \
         GitHub settings for target branches matching `{RULE_BRANCHES}`. Bongos writes no status \
         for stacked leaves."
    ))
}

/// What a look remembers about a linked stacked leaf.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    /// The slug of the sibling it is stacked on.
    parent: String,
    /// Whether its plan had begun.
    begun: bool,
}

/// What Kyle was told, and what the tree looked like the last time.
#[derive(Default)]
pub(crate) struct Notices {
    /// The linked stacked leaves at the last look; `None` before the first.
    seen: Option<BTreeMap<String, Seen>>,
    /// The tasks whose stacked toast was shown in this session.
    told: BTreeSet<String>,
}

impl Notices {
    /// Looks at `tree` and returns the toasts to show, in task order. With `speak` false it only
    /// notes the tree.
    pub(crate) fn look(&mut self, tree: &WorkTree, speak: bool) -> Vec<String> {
        let mut now = BTreeMap::new();
        for id in linked_ids(tree) {
            if !is_linked_stacked_leaf(tree, &id) {
                continue;
            }
            let Ok(place) = Place::locate(tree, &id) else {
                continue;
            };
            let Some(file) = place.task.file.as_ref() else {
                continue;
            };
            let Some(edge) = file.stacked_on.as_ref() else {
                continue;
            };
            now.insert(
                id,
                Seen {
                    parent: edge.task.clone(),
                    begun: plan_begun(file),
                },
            );
        }
        let before = self.seen.replace(now.clone());
        let (Some(before), true) = (before, speak) else {
            return Vec::new();
        };
        let mut messages = Vec::new();
        for (id, seen) in &now {
            let previous = before.get(id);
            let gained = previous.is_none_or(|previous| previous.parent != seen.parent);
            let started = seen.begun && !previous.is_some_and(|previous| previous.begun);
            let mut parts = Vec::new();
            if (gained || started)
                && !self.told.contains(id)
                && let Some(notice) = stacked_notice(tree, id)
            {
                self.told.insert(id.clone());
                parts.push(notice);
            }
            if gained && let Some(warning) = open_pr_warning(tree, id) {
                parts.push(warning);
            }
            if !parts.is_empty() {
                messages.push(parts.join("\n\n"));
            }
        }
        messages
    }
}

struct Watcher {
    subscription: Option<Subscription>,
    notices: Notices,
}

struct GlobalWatcher(Entity<Watcher>);

impl Global for GlobalWatcher {}

/// Creates the watcher; the store is attached now when it exists, else by [`connect`]. Call after
/// [`crate::sync::install`].
pub(crate) fn install(cx: &mut App) {
    let entity = cx.new(|_| Watcher {
        subscription: None,
        notices: Notices::default(),
    });
    cx.set_global(GlobalWatcher(entity));
    connect(cx);
}

/// Attaches to the work store when it exists and is not attached yet.
pub(crate) fn connect(cx: &mut App) {
    if let Some(entity) = cx.try_global::<GlobalWatcher>().map(|g| g.0.clone()) {
        entity.update(cx, |watcher, cx| watcher.attach(cx));
    }
}

impl Watcher {
    fn attach(&mut self, cx: &mut Context<Self>) {
        if self.subscription.is_some() {
            return;
        }
        let Some(store) = WorkStore::global(cx) else {
            return;
        };
        self.subscription = Some(cx.subscribe(&store, |this, _, event, cx| {
            if matches!(event, WorkStoreEvent::Changed) {
                this.scan(cx);
            }
        }));
        self.scan(cx);
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(tree) = snapshot(cx) else {
            return;
        };
        let messages = self.notices.look(&tree, enabled(cx));
        if !messages.is_empty() {
            let message = messages.join("\n\n");
            cx.defer(move |cx| toast_active(message, cx));
        }
    }
}
