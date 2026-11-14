//! The handlers of `project: show changes`, `project: show history`, `project: revert` and
//! `project: resolve split`, and the modal that shows the landing view and sends its keys to
//! [`LandingView::command`].

use std::path::PathBuf;

use bongos_project_repo::{AuthorFilter, EndedState, LandMode};
use bongos_project_repo::{
    ChangeState, HistoryEntry, HistoryFilter, ProjectRepo, ProjectRepositorySettings,
    SplitResolution,
};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use chrono::Utc;
use gpui::{
    App, AppContext as _, Context, DismissEvent, EventEmitter, FocusHandle, Focusable,
    KeyDownEvent, Render, WeakEntity, Window,
};
use settings::Settings as _;
use ui::{Label, LabelCommon as _, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, OpenOptions, Workspace};

use crate::actions::project;
use crate::commands::{keyboard_author, keyboard_context, show_toast};
use crate::landing_view::{LandingCommand, LandingView};
use crate::modals;
use crate::working_copy_commands::{
    approve_change, ask_and_land, now, open_in_new_workspace, short_commit, start_landing,
    with_repo,
};

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &project::ShowChanges, window, cx| show_changes(w, window, cx))
        .register_action(|w, _: &project::ShowHistory, window, cx| show_history(w, window, cx))
        .register_action(|w, _: &project::Revert, window, cx| revert(w, window, cx))
        .register_action(|w, _: &project::ResolveSplit, window, cx| {
            with_repo(w, window, cx, |repo, w, window, cx| {
                resolve_split(repo, w, window, cx)
            })
        });
}

fn toast_error(
    workspace: &WeakEntity<Workspace>,
    message: String,
    cx: &mut gpui::AsyncWindowContext,
) {
    workspace
        .update(cx, |workspace, cx| show_toast(workspace, message, cx))
        .log_err();
}

/// Writes `produce`'s text to `<state>/diffs/<name>` on the background executor and opens it.
fn show_text(
    repo: &ProjectRepo,
    name: String,
    produce: impl std::future::Future<Output = anyhow::Result<String>> + Send + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let directory: PathBuf = repo.location().state.join("diffs");
    let written = cx.background_spawn(async move {
        let text = produce.await?;
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(name);
        std::fs::write(&path, text)?;
        anyhow::Ok(path)
    });
    cx.spawn_in(window, async move |workspace, cx| match written.await {
        Ok(path) => {
            let opening = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(path, OpenOptions::default(), window, cx)
            });
            if let Ok(opening) = opening {
                opening.await.log_err();
            }
        }
        Err(error) => toast_error(&workspace, format!("{error:#}"), cx),
    })
    .detach();
}

/// Does what `command` says on the project of `repo`.
pub(crate) fn execute(
    repo: ProjectRepo,
    command: LandingCommand,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let row_of = |change_id: &str| repo.change(change_id).log_err().flatten();
    match command {
        LandingCommand::OpenTaskDiff { change_id } => {
            let reading = repo.clone();
            let id = change_id.clone();
            show_text(
                &repo,
                format!("{change_id}-tasks.txt"),
                async move { Ok(reading.task_diff(&id).await?.lines().join("\n")) },
                window,
                cx,
            );
        }
        LandingCommand::OpenRawDiff { change_id } => {
            let reading = repo.clone();
            let id = change_id.clone();
            show_text(
                &repo,
                format!("{change_id}.diff"),
                async move { reading.raw_diff(&id).await },
                window,
                cx,
            );
        }
        LandingCommand::OpenAmendDiff { change_id, commit } => {
            let reading = repo.clone();
            let (id, amend) = (change_id.clone(), commit);
            show_text(
                &repo,
                format!("{change_id}-amended.diff"),
                async move { reading.amend_diff(&id, &amend).await },
                window,
                cx,
            );
        }
        LandingCommand::CheckOut { change_id } => {
            let checking = repo.clone();
            let checked =
                cx.background_spawn(async move { checking.check_out_change(&change_id).await });
            cx.spawn_in(window, async move |workspace, cx| match checked.await {
                Ok(path) => {
                    workspace
                        .update_in(cx, |_, window, cx| {
                            open_in_new_workspace(path.join(DEFINITIONS_DIR), window, cx)
                        })
                        .log_err();
                }
                Err(error) => toast_error(&workspace, format!("{error:#}"), cx),
            })
            .detach();
        }
        LandingCommand::ResolveSplit => resolve_split(repo, workspace, window, cx),
        LandingCommand::Approve { change_id } => {
            if let Some(row) = row_of(&change_id) {
                approve_change(repo, row, window, cx);
            }
        }
        LandingCommand::Land { change_id } => {
            if let Some(row) = row_of(&change_id) {
                ask_and_land(repo, row, LandMode::Land, workspace, window, cx);
            }
        }
        LandingCommand::Abandon { change_id } => {
            let Some(row) = row_of(&change_id) else {
                return;
            };
            let name = row.name.clone();
            modals::pick(
                workspace,
                format!("Abandon {name}? Its branch and files are deleted; master is untouched"),
                vec![format!("Abandon {name}"), "Keep it".to_string()],
                1,
                Box::new(move |choice, _, _, cx| {
                    if choice != 0 {
                        return;
                    }
                    let abandoned = repo.abandon(&row.change_id, cx);
                    cx.spawn(async move |workspace, cx| {
                        let message = match abandoned.await {
                            Ok(()) => format!("Abandoned {name}"),
                            Err(error) => error.to_string(),
                        };
                        workspace
                            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                            .log_err();
                    })
                    .detach();
                }),
                window,
                cx,
            );
        }
        LandingCommand::Revert { change_id } => {
            revert_change(repo, change_id, workspace, window, cx)
        }
    }
}

/// `project: resolve split`: asks how, then resolves with the person at the keyboard.
fn resolve_split(
    repo: ProjectRepo,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match repo.split_master() {
        Ok(Some(_)) => {}
        Ok(None) => return show_toast(workspace, "Master is not split".to_string(), cx),
        Err(error) => return show_toast(workspace, format!("{error:#}"), cx),
    }
    modals::pick(
        workspace,
        "Resolve the split master: keep bongos' state over the outside commits",
        vec![
            "Keep bongos".to_string(),
            "Keep bongos and bring the outside change in as a working copy".to_string(),
        ],
        0,
        Box::new(move |choice, _, window, cx| {
            let keep = if choice == 0 {
                SplitResolution::KeepBongos
            } else {
                SplitResolution::KeepBongosAndBringIn
            };
            let Some(marker) = keyboard_context(window, cx).take_attestation() else {
                return;
            };
            let resolving =
                cx.background_spawn(async move { repo.resolve_split(keep, marker, now()).await });
            cx.spawn_in(window, async move |workspace, cx| {
                let message = match resolving.await {
                    Ok(resolved) => format!(
                        "Kept bongos over the outside commit {} ({} saved)",
                        short_commit(&resolved.outside),
                        resolved.saved.len()
                    ),
                    Err(error) => format!("{error:#}"),
                };
                toast_error(&workspace, message, cx);
            })
            .detach();
        }),
        window,
        cx,
    );
}

/// Asks to revert the landed change `change_id`, then makes the revert and lands it.
fn revert_change(
    repo: ProjectRepo,
    change_id: String,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let name = repo
        .change(&change_id)
        .log_err()
        .flatten()
        .map(|row| row.name)
        .unwrap_or_else(|| change_id.clone());
    modals::pick(
        workspace,
        format!("Revert {name}? A new change that undoes it is made and landed; history is kept"),
        vec![format!("Revert {name}"), "Keep it".to_string()],
        1,
        Box::new(move |choice, _, window, cx| {
            if choice != 0 {
                return;
            }
            let author = keyboard_author(window, cx);
            let making = {
                let repo = repo.clone();
                cx.background_spawn(async move {
                    let copy = repo.revert(&change_id, &author, now()).await?;
                    let meta = repo.revert_meta(&change_id, &author)?;
                    anyhow::Ok((copy, meta.subject))
                })
            };
            cx.spawn_in(window, async move |workspace, cx| match making.await {
                Ok((copy, subject)) => {
                    workspace
                        .update_in(cx, |_, window, cx| match repo.change(&copy.change_id) {
                            Ok(Some(row)) => {
                                start_landing(repo, row, LandMode::Land, subject, window, cx)
                            }
                            other => log::error!("the revert {}: {other:?}", copy.change_id),
                        })
                        .log_err();
                }
                Err(error) => toast_error(&workspace, format!("{error:#}"), cx),
            })
            .detach();
        }),
        window,
        cx,
    );
}

fn show_changes(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_repo(workspace, window, cx, |repo, _, window, cx| {
        let stale_days = ProjectRepositorySettings::get_global(cx).stale_after_days;
        let reading = repo.clone();
        let listing = cx.background_spawn(async move { reading.change_listing().await });
        cx.spawn_in(window, async move |workspace, cx| match listing.await {
            Ok(listing) => {
                let view = LandingView::build(&listing, now(), stale_days);
                let opened = workspace.update_in(cx, |workspace, window, cx| {
                    let weak = cx.entity().downgrade();
                    modals::show(workspace, window, cx, |window, cx| {
                        LandingModal::new(view, repo, weak, window, cx)
                    });
                });
                opened.log_err();
            }
            Err(error) => toast_error(&workspace, format!("{error:#}"), cx),
        })
        .detach();
    });
}

/// The landing view as a modal: up and down (or `n` and `p`) move, the keys of
/// [`LandingView::command`] act on the selected row, escape (or `q`) closes it.
pub(crate) struct LandingModal {
    view: LandingView,
    selected: usize,
    focus: FocusHandle,
    repo: ProjectRepo,
    workspace: WeakEntity<Workspace>,
}

impl LandingModal {
    fn new(
        view: LandingView,
        repo: ProjectRepo,
        workspace: WeakEntity<Workspace>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            view,
            selected: 0,
            focus: cx.focus_handle(),
            repo,
            workspace,
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event
            .keystroke
            .key_char
            .clone()
            .unwrap_or_else(|| event.keystroke.key.clone());
        let rows = self.view.rows().len();
        match key.as_str() {
            "escape" | "q" => cx.emit(DismissEvent),
            "down" | "n" => self.selected = (self.selected + 1).min(rows.saturating_sub(1)),
            "up" | "p" => self.selected = self.selected.saturating_sub(1),
            _ => match self.view.command(&key, self.selected) {
                Ok(command) => {
                    cx.emit(DismissEvent);
                    let repo = self.repo.clone();
                    let workspace = self.workspace.clone();
                    window.defer(cx, move |window, cx| {
                        workspace
                            .update(cx, |workspace, cx| {
                                execute(repo, command, workspace, window, cx)
                            })
                            .ok();
                    });
                }
                Err(message) => {
                    let workspace = self.workspace.clone();
                    workspace
                        .update(cx, |workspace, cx| show_toast(workspace, message, cx))
                        .ok();
                }
            },
        }
        cx.notify();
    }
}

impl ModalView for LandingModal {}

impl EventEmitter<DismissEvent> for LandingModal {}

impl Focusable for LandingModal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for LandingModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().gap_0p5();
        let mut index = 0;
        for block in &self.view.sections {
            list = list.child(Label::new(format!(
                "{} ({})",
                block.section.title(),
                block.rows.len()
            )));
            for row in &block.rows {
                let selected = index == self.selected;
                index += 1;
                list = list.child(
                    h_flex()
                        .px_2()
                        .when(selected, |this| {
                            this.bg(cx.theme().colors().element_selected)
                        })
                        .child(Label::new(row.text.clone()).color(if selected {
                            Color::Default
                        } else {
                            Color::Muted
                        })),
                );
            }
        }
        if index == 0 {
            list = list.child(Label::new("There are no changes").color(Color::Muted));
        }
        v_flex()
            .key_context("LandingView")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .elevation_3(cx)
            .p_2()
            .gap_1()
            .w(rems(48.))
            .child(list)
            .child(
                Label::new("d diff  D git diff  o check out  r resolve split  a approve  k abandon  l land  R revert  q close")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
    }
}

/// The ways `project: show history` offers to filter, with the filter each makes. The last,
/// by task, asks for the task id.
fn history_choices() -> Vec<(&'static str, HistoryFilter)> {
    let with = |change: fn(&mut HistoryFilter)| {
        let mut filter = HistoryFilter::default();
        change(&mut filter);
        filter
    };
    vec![
        ("Everything", HistoryFilter::default()),
        ("Landed", with(|f| f.state = EndedState::Landed)),
        ("Abandoned", with(|f| f.state = EndedState::Abandoned)),
        ("By people", with(|f| f.author = Some(AuthorFilter::Person))),
        ("By agents", with(|f| f.author = Some(AuthorFilter::Agent))),
        (
            "By workflows",
            with(|f| f.author = Some(AuthorFilter::Workflow)),
        ),
        ("Last 7 days", with(|f| f.within_days = Some(7))),
        ("Last 30 days", with(|f| f.within_days = Some(30))),
        ("Older than 30 days", with(|f| f.older_than_days = Some(30))),
        ("By task", HistoryFilter::default()),
    ]
}

/// The line of one history entry.
pub(crate) fn history_label(entry: &HistoryEntry) -> String {
    let state = match entry.state {
        ChangeState::Landed => "landed",
        ChangeState::Abandoned => "abandoned",
        ChangeState::Open => "open",
        ChangeState::Requested => "requested",
    };
    format!(
        "{} {} ({}, {state})",
        entry.ended.format("%Y-%m-%d"),
        entry.subject,
        entry.author.name
    )
}

fn show_history(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_repo(workspace, window, cx, |repo, workspace, window, cx| {
        let choices = history_choices();
        let labels = choices.iter().map(|(label, _)| label.to_string()).collect();
        modals::pick(
            workspace,
            "Show which history?",
            labels,
            0,
            Box::new(move |index, workspace, window, cx| {
                let Some((label, filter)) = choices.into_iter().nth(index) else {
                    return;
                };
                if label == "By task" {
                    modals::ask(
                        workspace,
                        "Id of the task",
                        "",
                        false,
                        Box::new(move |task, _, window, cx| {
                            let filter = HistoryFilter {
                                task: Some(task.trim().to_string()),
                                ..HistoryFilter::default()
                            };
                            pick_entry(
                                repo,
                                filter,
                                "Which change?",
                                window,
                                cx,
                                |repo, entry, workspace, window, cx| {
                                    open_entry(repo, entry, workspace, window, cx)
                                },
                            );
                        }),
                        window,
                        cx,
                    );
                } else {
                    pick_entry(
                        repo,
                        filter,
                        "Which change?",
                        window,
                        cx,
                        |repo, entry, workspace, window, cx| {
                            open_entry(repo, entry, workspace, window, cx)
                        },
                    );
                }
            }),
            window,
            cx,
        );
    });
}

/// Opens the task-aware diff of a history entry, when it has one.
fn open_entry(
    repo: ProjectRepo,
    entry: HistoryEntry,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if entry.state == ChangeState::Landed {
        execute(
            repo,
            LandingCommand::OpenTaskDiff {
                change_id: entry.change_id,
            },
            workspace,
            window,
            cx,
        );
    } else {
        show_toast(
            workspace,
            format!("{} was abandoned; it has no diff", entry.name),
            cx,
        );
    }
}

/// Reads the history through `filter` and lets a person pick one of its entries for `then`.
fn pick_entry(
    repo: ProjectRepo,
    filter: HistoryFilter,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    then: impl FnOnce(ProjectRepo, HistoryEntry, &mut Workspace, &mut Window, &mut Context<Workspace>)
    + 'static,
) {
    let reading = repo.clone();
    let history =
        cx.background_spawn(async move { reading.change_history(&filter, Utc::now()).await });
    cx.spawn_in(window, async move |workspace, cx| match history.await {
        Ok(entries) if entries.is_empty() => {
            toast_error(&workspace, "No change matches".to_string(), cx)
        }
        Ok(entries) => {
            let labels = entries.iter().map(history_label).collect();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    modals::pick(
                        workspace,
                        placeholder,
                        labels,
                        0,
                        Box::new(move |index, workspace, window, cx| {
                            if let Some(entry) = entries.into_iter().nth(index) {
                                then(repo, entry, workspace, window, cx);
                            }
                        }),
                        window,
                        cx,
                    );
                })
                .log_err();
        }
        Err(error) => toast_error(&workspace, format!("{error:#}"), cx),
    })
    .detach();
}

fn revert(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    with_repo(workspace, window, cx, |repo, _, window, cx| {
        let filter = HistoryFilter {
            state: EndedState::Landed,
            ..HistoryFilter::default()
        };
        pick_entry(
            repo,
            filter,
            "Revert which change?",
            window,
            cx,
            |repo, entry, workspace, window, cx| {
                revert_change(repo, entry.change_id, workspace, window, cx)
            },
        );
    });
}
