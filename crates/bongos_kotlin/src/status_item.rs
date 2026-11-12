//! The Kotlin status bar item: the server's state as text, and a menu of what to do about it. It
//! reads the managers' state and never blocks: all text comes from `status.rs`.

use gpui::{
    Action, Anchor, App, Context, Entity, EntityId, IntoElement, Render, Subscription, Task,
    TaskExt as _, WeakEntity, Window, div,
};
use project::{Event as ProjectEvent, Project};
use settings::WorktreeId;
use ui::{
    Button, ContextMenu, ContextMenuEntry, LabelSize, ParentElement as _, PopoverMenu,
    PopoverMenuHandle, Styled as _, Tooltip,
};
use util::truncate_and_trailoff;
use workspace::{OpenOptions, StatusItemView, Workspace};

use crate::{
    CancelInstall, KotlinLog, KotlinProjectManager, KotlinRegistry, KotlinSetup, KotlinUpdates,
    LicenseActivate, LicenseStatus, LicenseTrial, ReimportProject, RestartServer,
    ReviewIntellijEula, ShowLogs, SwitchServer,
    status::{self, Command, MAX_TEXT_CHARS, MenuItem, Snapshot},
};

/// Zed's language server log viewer, which has the Kotlin server's protocol messages.
const PROTOCOL_LOG_ACTION: &str = "dev::OpenLanguageServerLogs";

pub struct KotlinStatusItem {
    project: WeakEntity<Project>,
    menu_handle: PopoverMenuHandle<ContextMenu>,
    active_worktree: Option<WorktreeId>,
    managers: Vec<(Entity<KotlinProjectManager>, Subscription)>,
    _project_subscription: Subscription,
    _updates_subscription: Option<Subscription>,
}

impl KotlinStatusItem {
    pub fn new(
        project: Entity<Project>,
        menu_handle: PopoverMenuHandle<ContextMenu>,
        cx: &mut Context<Self>,
    ) -> Self {
        let _project_subscription = cx.subscribe(&project, |this, _, event, cx| {
            if matches!(
                event,
                ProjectEvent::WorktreeAdded(_) | ProjectEvent::WorktreeRemoved(_)
            ) {
                this.refresh_managers(cx);
            }
        });
        let _updates_subscription =
            KotlinUpdates::global(cx).map(|updates| cx.observe(&updates, |_, _, cx| cx.notify()));
        let mut this = Self {
            project: project.downgrade(),
            menu_handle,
            active_worktree: None,
            managers: Vec::new(),
            _project_subscription,
            _updates_subscription,
        };
        this.refresh_managers(cx);
        this
    }

    /// Follows the managers of the project's worktrees, which register when a worktree is added.
    fn refresh_managers(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.project.upgrade() else {
            return;
        };
        let found = managers_of(project.read(cx), cx);
        let same = found
            .iter()
            .map(Entity::entity_id)
            .eq(self.managers.iter().map(|(manager, _)| manager.entity_id()));
        if same {
            return;
        }
        self.managers = found
            .into_iter()
            .map(|manager| {
                let subscription = cx.observe(&manager, |_, _, cx| cx.notify());
                (manager, subscription)
            })
            .collect();
        cx.notify();
    }

    /// The state to show: the active file's worktree if a server has started for it, else the first
    /// worktree with one. `None` hides the item.
    fn snapshot(&self, cx: &App) -> Option<Snapshot> {
        let updates = KotlinUpdates::global(cx);
        let download = updates
            .as_ref()
            .and_then(|updates| updates.read(cx).download_progress().cloned());
        let checking = updates
            .as_ref()
            .is_some_and(|updates| updates.read(cx).is_checking());
        let mut first = None;
        for (manager, _) in &self.managers {
            for (worktree_id, state) in manager.read(cx).worktrees() {
                let Some(mut snapshot) = Snapshot::of(state) else {
                    continue;
                };
                snapshot.download = download.clone();
                snapshot.checking = checking;
                if Some(worktree_id) == self.active_worktree {
                    return Some(snapshot);
                }
                first.get_or_insert(snapshot);
            }
        }
        first
    }
}

pub(crate) fn managers_of(project: &Project, cx: &App) -> Vec<Entity<KotlinProjectManager>> {
    let Some(registry) = cx.try_global::<KotlinRegistry>() else {
        return Vec::new();
    };
    let mut seen: Vec<EntityId> = Vec::new();
    let mut managers = Vec::new();
    for worktree in project.visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        if !worktree.is_local() {
            continue;
        }
        if let Some(manager) = registry.manager(&worktree.abs_path())
            && !seen.contains(&manager.entity_id())
        {
            seen.push(manager.entity_id());
            managers.push(manager);
        }
    }
    managers
}

impl StatusItemView for KotlinStatusItem {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_worktree = active_pane_item
            .and_then(|item| item.project_path(cx))
            .map(|path| path.worktree_id);
        // A worktree can be added before its manager registers; look again on every change.
        self.refresh_managers(cx);
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        // The item appears only for a project that runs a Kotlin server.
        None
    }
}

impl Render for KotlinStatusItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(snapshot) = self.snapshot(cx) else {
            return div().hidden().into_any_element();
        };
        let line = status::status_line(&snapshot);
        let this = cx.weak_entity();
        div()
            .child(
                PopoverMenu::new("kotlin-status-menu")
                    .menu(move |window, cx| {
                        let snapshot = this
                            .read_with(cx, |this, cx| this.snapshot(cx))
                            .ok()
                            .flatten()?;
                        Some(build_menu(&snapshot, window, cx))
                    })
                    .anchor(Anchor::BottomLeft)
                    .with_handle(self.menu_handle.clone())
                    .trigger_with_tooltip(
                        Button::new(
                            "kotlin-status",
                            truncate_and_trailoff(&line.text, MAX_TEXT_CHARS),
                        )
                        .label_size(LabelSize::Small)
                        .color(line.color)
                        .loading(line.busy),
                        Tooltip::text(line.text.clone()),
                    ),
            )
            .into_any_element()
    }
}

/// The action a menu command runs; `None` when it cannot be built.
fn action_for(command: &Command, cx: &App) -> Option<Box<dyn Action>> {
    match command {
        Command::ShowLogs => Some(ShowLogs.boxed_clone()),
        Command::CancelInstall => Some(CancelInstall.boxed_clone()),
        Command::SetJdkHome => Some(zed_actions::OpenSettingsFile.boxed_clone()),
        Command::OpenProtocolLog => cx.build_action(PROTOCOL_LOG_ACTION, None).ok(),
        Command::SwitchServer => Some(SwitchServer.boxed_clone()),
        Command::RestartServer => Some(RestartServer.boxed_clone()),
        Command::ReimportProject => Some(ReimportProject.boxed_clone()),
        Command::LicenseStatus => Some(LicenseStatus.boxed_clone()),
        Command::LicenseActivate => Some(LicenseActivate.boxed_clone()),
        Command::LicenseTrial => Some(LicenseTrial.boxed_clone()),
        Command::ReviewEula => Some(ReviewIntellijEula.boxed_clone()),
    }
}

fn build_menu(snapshot: &Snapshot, window: &mut Window, cx: &mut App) -> Entity<ContextMenu> {
    let items = status::menu_items(snapshot);
    ContextMenu::build(window, cx, move |mut menu, _, cx| {
        for item in &items {
            menu = match item {
                MenuItem::Separator => menu.separator(),
                MenuItem::Note(text) => menu.label(text.clone()),
                MenuItem::Entry { label, command } => match action_for(command, cx) {
                    Some(action) => menu.action(label.clone(), action),
                    None => menu.item(ContextMenuEntry::new(label.clone()).disabled(true)),
                },
            };
        }
        menu
    })
}

/// `kotlin: cancel install`: cancels every install running for the project's worktrees and the
/// background download of a newer build. Returns whether one was. The installer stops at its next chunk, removes its partial files and the
/// status becomes `Failed("install cancelled")`; the same code serves both servers.
pub fn cancel_installs(project: &Entity<Project>, cx: &mut App) -> bool {
    let mut cancelled = false;
    for manager in managers_of(project.read(cx), cx) {
        manager.update(cx, |manager, _| {
            let ids: Vec<WorktreeId> = manager.worktrees().map(|(id, _)| id).collect();
            for id in ids {
                cancelled |= manager.cancel_install(id);
            }
        });
    }
    if let Some(updates) = KotlinUpdates::global(cx) {
        cancelled |= updates.update(cx, |updates, cx| updates.cancel_download(cx));
    }
    cancelled
}

/// `kotlin: show logs`: opens `kotlin.log`, creating it when nothing was logged yet.
pub fn show_logs(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(logs_dir) = cx
        .try_global::<KotlinSetup>()
        .map(|setup| setup.dirs.logs_dir.clone())
    else {
        return;
    };
    if let Err(error) = KotlinLog::new(&logs_dir).ensure_exists() {
        ::log::error!("cannot create the Kotlin log: {error}");
        return;
    }
    let task: Task<anyhow::Result<_>> = workspace.open_abs_path(
        KotlinLog::path_in(&logs_dir),
        OpenOptions::default(),
        window,
        cx,
    );
    task.detach_and_log_err(cx);
}
