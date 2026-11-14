//! The handlers of `project: convert`, which copies one old-format project (or the old
//! one-offs) into a new project repository, and `project: convert all`, which copies every one.
//! Each shows what it will make in a read-only buffer and asks before it runs; nothing is ever
//! converted without that. The conversion runs on the background executor, reads the old folder
//! only, and leaves nothing behind when it fails.

use std::path::{Path, PathBuf};

use anyhow::Result;
use bongos_project_repo::{
    ConversionPreview, ConvertError, ConvertOptions, ConvertedProject, Git, LayoutTemplates,
    ProjectRepositorySettings, RepositoryLocations, StateApplyHooks, convert, convert_all, preview,
    unconverted_sources,
};
use bongos_work_model::Timestamp;
use bongos_work_model::store::load;
use gpui::{AppContext as _, Context, Window};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::Workspace;

use crate::WorkStore;
use crate::actions::project;
use crate::backup_commands::open_read_only;
use crate::commands::{ready, show_toast};
use crate::modals;
use crate::projects::locations_path;

pub fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|w, _: &project::Convert, window, cx| start(w, false, window, cx))
        .register_action(|w, _: &project::ConvertAll, window, cx| start(w, true, window, cx));
}

/// Where conversions read from and make their projects.
#[derive(Clone, Debug)]
pub(crate) struct ConversionSetup {
    /// The old work tree's root.
    pub root: PathBuf,
    pub locations_path: PathBuf,
    pub projects_dir: PathBuf,
    pub layout: LayoutTemplates,
    /// The title of the project the old one-offs go in.
    pub oneoffs_title: String,
    pub now: Timestamp,
}

impl ConversionSetup {
    fn options<'a>(&'a self, hooks: &'a StateApplyHooks) -> ConvertOptions<'a> {
        ConvertOptions {
            projects_dir: &self.projects_dir,
            layout: &self.layout,
            hooks,
        }
    }
}

/// What converting each old-format project that is not converted yet would do.
pub(crate) fn previews(setup: &ConversionSetup) -> Result<Vec<ConversionPreview>> {
    let tree = load(&setup.root);
    let locations = RepositoryLocations::load(&setup.locations_path)?;
    let hooks = StateApplyHooks::new();
    Ok(
        unconverted_sources(&tree, &locations, &setup.oneoffs_title, setup.now)
            .iter()
            .map(|source| preview(source, setup.options(&hooks), &locations))
            .collect(),
    )
}

/// Converts the old project at `only`, or every unconverted one when `None`. Each result is
/// named by its old folder.
pub(crate) async fn run_conversion(
    setup: &ConversionSetup,
    only: Option<&Path>,
    git: &Git,
) -> Result<Vec<(PathBuf, Result<ConvertedProject, ConvertError>)>> {
    let tree = load(&setup.root);
    let mut locations = RepositoryLocations::load(&setup.locations_path)?;
    let hooks = StateApplyHooks::global();
    let options = setup.options(hooks);
    let new_id = bongos_work_model::new_id;
    let Some(only) = only else {
        return Ok(convert_all(
            &tree,
            &setup.oneoffs_title,
            setup.now,
            options,
            &mut locations,
            git,
            new_id,
        )
        .await);
    };
    let sources = unconverted_sources(&tree, &locations, &setup.oneoffs_title, setup.now);
    let Some(source) = sources.iter().find(|source| source.old_dir() == only) else {
        anyhow::bail!(
            "{} is not an old-format project that still needs converting",
            only.display()
        );
    };
    let result = convert(source, options, &mut locations, git, new_id).await;
    Ok(vec![(only.to_path_buf(), result)])
}

/// The preview buffer: what each conversion reads, makes, skips, and why it cannot run.
pub(crate) fn render_previews(previews: &[ConversionPreview]) -> String {
    let mut text = String::from(
        "project: convert copies an old-format project into a new project repository.\n\
         The old folder is only read: nothing in it is changed, moved or removed.\n\
         Afterwards the old project is hidden from the Work view.\n",
    );
    for preview in previews {
        let summary = &preview.summary;
        text.push_str(&format!(
            "\n{} ({})\n",
            summary.title,
            summary.old_dir.display()
        ));
        text.push_str(&format!(
            "  {} task{}, {} todo{}, {} workflow{}, {} chat{}, {} file{}\n",
            summary.tasks,
            plural(summary.tasks),
            summary.todos,
            plural(summary.todos),
            summary.workflows,
            plural(summary.workflows),
            summary.chats,
            plural(summary.chats),
            summary.files,
            plural(summary.files),
        ));
        if preview.is_blocked() {
            text.push_str("  Cannot be converted as it is:\n");
            for blocker in &summary.blockers {
                text.push_str(&format!("    - {blocker}\n"));
            }
        } else {
            text.push_str("  Will make:\n");
            for line in &summary.creates {
                text.push_str(&format!("    - {line}\n"));
            }
        }
        if !summary.rejected.is_empty() {
            text.push_str("  Skipped, not copied:\n");
            for rejected in &summary.rejected {
                text.push_str(&format!(
                    "    - {}: {}\n",
                    rejected.path.display(),
                    rejected.reason
                ));
            }
        }
    }
    text
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// What a person is told after converting.
pub(crate) fn conversion_message(
    results: &[(PathBuf, Result<ConvertedProject, ConvertError>)],
) -> String {
    let done: Vec<String> = results
        .iter()
        .filter_map(|(old, result)| {
            let converted = result.as_ref().ok()?;
            let mut line = format!(
                "{} into {}",
                old.display(),
                converted.location.folder.display()
            );
            if !converted.summary.rejected.is_empty() {
                line.push_str(&format!(
                    " ({} entr{} skipped)",
                    converted.summary.rejected.len(),
                    if converted.summary.rejected.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                ));
            }
            Some(line)
        })
        .collect();
    let failed: Vec<String> = results
        .iter()
        .filter_map(|(old, result)| {
            let error = result.as_ref().err()?;
            Some(format!("{}: {error}", old.display()))
        })
        .collect();
    let mut parts = Vec::new();
    if !done.is_empty() {
        parts.push(format!(
            "Converted {}; the old folders are kept as they were and hidden from the Work view",
            done.join(", ")
        ));
    }
    if !failed.is_empty() {
        parts.push(format!(
            "Not converted, nothing was made and the old folders are unchanged: {}",
            failed.join("; ")
        ));
    }
    if parts.is_empty() {
        return "There was no old-format project to convert".to_string();
    }
    parts.join(". ")
}

fn setup(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> Option<ConversionSetup> {
    let tree = ready(workspace, cx)?;
    let settings = ProjectRepositorySettings::get_global(cx).clone();
    if !settings.enabled {
        show_toast(
            workspace,
            "Project repositories are off (project_repository.enabled); turn them on to convert"
                .to_string(),
            cx,
        );
        return None;
    }
    Some(ConversionSetup {
        root: tree.root.clone(),
        locations_path: locations_path(cx),
        projects_dir: settings.projects_folder(),
        layout: settings.layout,
        oneoffs_title: settings.oneoffs_project,
        now: bongos_work_model::now(),
    })
}

fn start(workspace: &mut Workspace, all: bool, window: &mut Window, cx: &mut Context<Workspace>) {
    let Some(setup) = setup(workspace, cx) else {
        return;
    };
    let listing = setup.clone();
    let found = cx.background_spawn(async move { previews(&listing) });
    cx.spawn_in(window, async move |workspace, cx| {
        let found = match found.await {
            Ok(found) => found,
            Err(error) => {
                workspace
                    .update(cx, |workspace, cx| {
                        show_toast(workspace, format!("Convert: {error:#}"), cx)
                    })
                    .log_err();
                return;
            }
        };
        workspace
            .update_in(cx, |workspace, window, cx| {
                if found.is_empty() {
                    show_toast(
                        workspace,
                        "There is no old-format project to convert".to_string(),
                        cx,
                    );
                } else if all {
                    preview_and_ask(setup, None, found, workspace, window, cx);
                } else {
                    let labels = found
                        .iter()
                        .map(|preview| {
                            format!(
                                "{} ({})",
                                preview.summary.title,
                                preview.summary.old_dir.display()
                            )
                        })
                        .collect();
                    modals::pick(
                        workspace,
                        "Convert which old-format project?",
                        labels,
                        0,
                        Box::new(move |index, workspace, window, cx| {
                            if let Some(chosen) = found.get(index) {
                                let old = chosen.summary.old_dir.clone();
                                let chosen = vec![chosen.clone()];
                                preview_and_ask(setup, Some(old), chosen, workspace, window, cx);
                            }
                        }),
                        window,
                        cx,
                    );
                }
            })
            .log_err();
    })
    .detach();
}

/// Shows `shown` in a read-only buffer, then asks before converting `only` (or all).
fn preview_and_ask(
    setup: ConversionSetup,
    only: Option<PathBuf>,
    shown: Vec<ConversionPreview>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_read_only(workspace, "Convert", render_previews(&shown), window, cx);
    let ready = shown.iter().filter(|preview| !preview.is_blocked()).count();
    if ready == 0 {
        let blockers: Vec<String> = shown
            .iter()
            .flat_map(|preview| preview.summary.blockers.iter().cloned())
            .collect();
        show_toast(
            workspace,
            format!("Not converted: {}", blockers.join("; ")),
            cx,
        );
        return;
    }
    let question = match &only {
        Some(_) => format!("Convert {} as the preview shows?", shown[0].summary.title),
        None => format!(
            "Convert {ready} old-format project{} as the preview shows?",
            plural(ready)
        ),
    };
    modals::pick(
        workspace,
        question,
        vec!["Convert".to_string(), "Cancel".to_string()],
        1,
        Box::new(move |choice, _, _, cx| {
            if choice != 0 {
                return;
            }
            let converting = cx.background_spawn(async move {
                let git = Git::locate()?;
                run_conversion(&setup, only.as_deref(), &git).await
            });
            cx.spawn(async move |workspace, cx| {
                let message = match converting.await {
                    Ok(results) => conversion_message(&results),
                    Err(error) => format!("Convert: {error:#}"),
                };
                workspace
                    .update(cx, |workspace, cx| {
                        // Read the tree again, so converted projects leave the Work view.
                        if let Some(store) = WorkStore::global(cx) {
                            store.update(cx, |store, cx| store.refresh_sources(cx));
                        }
                        show_toast(workspace, message, cx)
                    })
                    .log_err();
            })
            .detach();
        }),
        window,
        cx,
    );
}
