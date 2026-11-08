//! The offer to archive a finished task's chats (design D17): when a task is closed or dropped, or
//! a project is archived, while chats of it are open, a notification says so and offers "Archive
//! its N open chats". Nothing happens without the click, and the click archives exactly the chats
//! that were open when the offer was made.
//!
//! The change is seen in the Work snapshot: a task whose status went from open to done or
//! cancelled, a project whose `archived` went from false to true. The first snapshot only sets the
//! baseline, so what was already closed when chats came on is never offered. While `chats.enabled`
//! is false the Work store is not even subscribed to.

use std::collections::HashMap;
use std::path::PathBuf;

use bongos_work::{WorkStore, WorkStoreEvent};
use bongos_work_model::Status;
use bongos_work_model::store::WorkTree;
use gpui::{App, AppContext as _, Context, Entity, Global, Subscription, WeakEntity};
use settings::{Settings as _, SettingsStore};
use workspace::notifications::NotificationId;
use workspace::{MultiWorkspace, Toast, Workspace};

use crate::commands::archive_chat;
use crate::{ChatStore, ChatsSettings};

/// The toast of the offer.
pub struct LifecycleToast;

/// What the Work snapshot said about tasks and projects the last time it was looked at.
#[derive(Default)]
struct Seen {
    /// Task id to whether the task is closed or dropped.
    tasks: HashMap<String, bool>,
    /// Project directory to whether the project is archived.
    projects: HashMap<PathBuf, bool>,
}

impl Seen {
    fn of(tree: &WorkTree) -> Self {
        let tasks = tree
            .tasks()
            .into_iter()
            .filter_map(|task| {
                let file = task.file.as_ref()?;
                Some((file.id.clone(), file.status != Status::Open))
            })
            .collect();
        let projects = tree
            .projects
            .iter()
            .map(|project| {
                let archived = project.file.as_ref().is_some_and(|file| file.archived);
                (project.task.dir.clone(), archived)
            })
            .collect();
        Self { tasks, projects }
    }
}

/// What ended, from one snapshot to the next.
#[derive(Debug, Default, PartialEq, Eq)]
struct Ended {
    /// Ids of the tasks whose chats count.
    task_ids: Vec<String>,
    /// How each was named in the offer.
    subjects: Vec<String>,
}

/// The tasks that were open and are closed or dropped now, and the projects that were not
/// archived and are now: with every task of such a project.
fn ended_between(before: &Seen, tree: &WorkTree) -> Ended {
    let mut ended = Ended::default();
    for task in tree.tasks() {
        let Some(file) = &task.file else { continue };
        if file.status != Status::Open && before.tasks.get(&file.id) == Some(&false) {
            let verb = if file.status == Status::Done {
                "closed"
            } else {
                "dropped"
            };
            ended.task_ids.push(file.id.clone());
            ended.subjects.push(format!("\"{}\" was {verb}", file.goal));
        }
    }
    for project in &tree.projects {
        let archived = project.file.as_ref().is_some_and(|file| file.archived);
        if archived && before.projects.get(&project.task.dir) == Some(&false) {
            let title = project
                .file
                .as_ref()
                .map_or(project.slug.as_str(), |file| file.title.as_str());
            ended
                .subjects
                .push(format!("Project \"{title}\" was archived"));
            ended.task_ids.extend(
                project
                    .task
                    .walk()
                    .into_iter()
                    .filter_map(|task| task.id().map(str::to_string)),
            );
        }
    }
    ended.task_ids.sort();
    ended.task_ids.dedup();
    ended
}

/// The chats offered for archiving.
struct Offer {
    chat_ids: Vec<String>,
}

/// Watches the Work snapshot for tasks and projects that ended and offers to archive their open
/// chats. Created once by [`init`].
pub(crate) struct Lifecycle {
    store: Entity<ChatStore>,
    seen: Option<Seen>,
    pending: Option<Offer>,
    work_subscription: Option<Subscription>,
    _settings: Subscription,
}

/// Holds the watcher for as long as the app runs; only tests look it up.
struct GlobalLifecycle(#[allow(dead_code)] Entity<Lifecycle>);

impl Global for GlobalLifecycle {}

/// Starts watching, once `chats.enabled` is true. Call after the chat store exists.
pub(crate) fn init(store: &Entity<ChatStore>, cx: &mut App) {
    let store = store.clone();
    let lifecycle = cx.new(|cx| {
        let mut this = Lifecycle {
            store,
            seen: None,
            pending: None,
            work_subscription: None,
            _settings: cx.observe_global::<SettingsStore>(|this: &mut Lifecycle, cx| this.sync(cx)),
        };
        this.sync(cx);
        this
    });
    cx.set_global(GlobalLifecycle(lifecycle));
}

impl Lifecycle {
    /// The watcher, once [`init`] has run.
    #[cfg(test)]
    pub(crate) fn global(cx: &App) -> Option<Entity<Lifecycle>> {
        cx.try_global::<GlobalLifecycle>()
            .map(|global| global.0.clone())
    }

    /// The chats of the offer waiting for its click.
    #[cfg(test)]
    pub(crate) fn offered(&self) -> Vec<String> {
        self.pending
            .as_ref()
            .map(|offer| offer.chat_ids.clone())
            .unwrap_or_default()
    }

    /// Whether the Work store is being watched.
    #[cfg(test)]
    pub(crate) fn is_watching(&self) -> bool {
        self.work_subscription.is_some()
    }

    fn sync(&mut self, cx: &mut Context<Self>) {
        let enabled = ChatsSettings::get_global(cx).enabled;
        if enabled == self.work_subscription.is_some() {
            return;
        }
        if !enabled {
            self.work_subscription = None;
            self.seen = None;
            self.pending = None;
            return;
        }
        let work = self.store.read(cx).work().clone();
        self.seen = work.read(cx).snapshot().map(|tree| Seen::of(&tree));
        self.work_subscription = Some(cx.subscribe(&work, |this, work, event, cx| {
            if *event == WorkStoreEvent::Changed {
                this.work_changed(&work, cx);
            }
        }));
    }

    fn work_changed(&mut self, work: &Entity<WorkStore>, cx: &mut Context<Self>) {
        let Some(tree) = work.read(cx).snapshot() else {
            return;
        };
        let Some(before) = self.seen.replace(Seen::of(&tree)) else {
            return;
        };
        let ended = ended_between(&before, &tree);
        if ended.task_ids.is_empty() {
            return;
        }
        let chat_ids: Vec<String> = self
            .store
            .read(cx)
            .index()
            .values()
            .filter_map(|chat| {
                let record = chat.entry.record()?;
                let task_id = chat.task_id.as_deref()?;
                (!record.archived && ended.task_ids.iter().any(|id| id == task_id))
                    .then(|| record.id.clone())
            })
            .collect();
        if chat_ids.is_empty() {
            return;
        }
        self.offer(ended.subjects, chat_ids, cx);
    }

    /// Shows the notification; the offer waits for its click.
    fn offer(&mut self, subjects: Vec<String>, chat_ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(workspace) = offer_workspace(cx) else {
            return;
        };
        let count = chat_ids.len();
        let noun = if count == 1 { "chat" } else { "chats" };
        let message = format!(
            "{}. Its {count} open {noun} stay open.",
            subjects.join("; ")
        );
        let label = format!("Archive its {count} open {noun}");
        self.pending = Some(Offer { chat_ids });
        let this: WeakEntity<Lifecycle> = cx.weak_entity();
        let toast = Toast::new(NotificationId::unique::<LifecycleToast>(), message).on_click(
            label,
            move |_, cx| {
                this.update(cx, |this, cx| this.accept(cx)).ok();
            },
        );
        workspace.update(cx, |workspace: &mut Workspace, cx| {
            workspace.show_toast(toast, cx)
        });
    }

    /// The click on the offer: archives the chats it named.
    pub(crate) fn accept(&mut self, cx: &mut Context<Self>) {
        let Some(offer) = self.pending.take() else {
            return;
        };
        let archives: Vec<_> = offer
            .chat_ids
            .iter()
            .map(|id| (id.clone(), archive_chat(id, cx)))
            .collect();
        cx.spawn(async move |_, _| {
            for (id, archive) in archives {
                if let Err(message) = archive.await {
                    log::warn!("chats: could not archive chat {id}: {message}");
                }
            }
        })
        .detach();
    }
}

/// A workspace to show the offer in: the active window's, else any.
fn offer_workspace(cx: &App) -> Option<Entity<Workspace>> {
    let active = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>());
    let others = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>());
    active
        .into_iter()
        .chain(others)
        .find_map(|window| Some(window.read(cx).ok()?.workspace().clone()))
}
