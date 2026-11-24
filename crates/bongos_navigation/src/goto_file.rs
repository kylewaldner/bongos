//! The go to file picker and its four openers.
//!
//! The picker lists the files of the visible worktrees that are not ignored, by project-relative
//! path. With no input it lists the recent navigation history first and the rest in path order.
//! With input it narrows the paths with the minibuffer's engine (space-separated terms, any order,
//! smart case) and re-ranks its best 2,000 with `path_rank::score`. An input that starts with `/`
//! or `~/` lists that path alone when the file exists.
//!
//! Highlighting a row previews the file in the pane the pane choice resolves to (see
//! [`crate::preview`] and [`crate::target`]); confirming opens it in the pane the choice realizes.

use std::{
    collections::HashMap,
    ops::Range,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
};

use bongos_minibuffer::{
    Candidate, Outcome, Preview as SessionPreview, Session, SessionHandle, Source,
};
use bongos_minibuffer_model::{Cancel, Narrower, texts};
use bongos_navigation_model::path_rank;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, Context, Entity, Window};
use project::ProjectPath;
use util::paths::home_dir;
use workspace::Workspace;

use crate::goto_file_open::{self, FileTarget};
use crate::preview::{Preview, PreviewSession, PreviewTarget};
use crate::target::{PaneChoice, add_pane_choices, resolve};

/// The name the picker's session is routed by.
pub const HOST: &str = "go_to_file";

/// The key context of the picker's session.
pub const KEY_CONTEXT: &str = "NavGoToFile";

/// How many narrowed paths are re-ranked by `path_rank::score`.
const RANK_LIMIT: usize = 2_000;

struct IndexedPath {
    /// The project-relative path.
    text: String,
    /// Where the file name starts in `text`.
    name_start: usize,
    path: ProjectPath,
}

/// The files the picker lists, taken once when it opens.
pub struct PathIndex {
    /// Every file that is not ignored, in path order.
    entries: Vec<IndexedPath>,
    /// Positions in `entries` of the files in the recent navigation history, newest first.
    recent: Vec<usize>,
    narrower: Mutex<Narrower>,
}

impl PathIndex {
    /// Indexes every file entry of the visible worktrees' snapshots that is not ignored, and
    /// remembers the recent navigation history.
    pub fn new(workspace: &Workspace, cx: &App) -> Self {
        let mut entries = Vec::new();
        for worktree in workspace.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let worktree_id = worktree.id();
            for entry in worktree.snapshot().files(false, 0) {
                let text = entry.path.as_unix_str().to_string();
                let name_start = text.rfind('/').map_or(0, |at| at + 1);
                entries.push(IndexedPath {
                    text,
                    name_start,
                    path: ProjectPath {
                        worktree_id,
                        path: entry.path.clone(),
                    },
                });
            }
        }
        entries.sort_by(|a, b| {
            a.text
                .cmp(&b.text)
                .then(a.path.worktree_id.cmp(&b.path.worktree_id))
        });
        let positions: HashMap<&ProjectPath, usize> = entries
            .iter()
            .enumerate()
            .map(|(ix, entry)| (&entry.path, ix))
            .collect();
        let mut recent = Vec::new();
        for (path, _) in workspace.recent_navigation_history(None, cx) {
            if let Some(&ix) = positions.get(&path)
                && !recent.contains(&ix)
            {
                recent.push(ix);
            }
        }
        let narrower = Narrower::new(texts(entries.iter().map(|entry| entry.text.as_str())));
        Self {
            entries,
            recent,
            narrower: Mutex::new(narrower),
        }
    }

    /// The project-relative paths, in the order the rows list them for an empty input.
    pub fn listing(&self) -> Vec<&str> {
        self.order()
            .map(|ix| self.entries[ix].text.as_str())
            .collect()
    }

    fn order(&self) -> impl Iterator<Item = usize> + '_ {
        let rest = (0..self.entries.len()).filter(|ix| !self.recent.contains(ix));
        self.recent.iter().copied().chain(rest)
    }

    /// The rows for `query`, or `None` when `cancel` is set first.
    pub(crate) fn rows(&self, query: &str, cancel: &Cancel) -> Option<Vec<Candidate>> {
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.is_empty() {
            return Some(self.order().map(|ix| self.row(ix, &terms)).collect());
        }
        let matches = self
            .narrower
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .narrow(query, cancel)?;
        let mut ranked: Vec<(i64, usize)> = matches
            .iter()
            .take(RANK_LIMIT)
            .map(|found| {
                let ix = found.index as usize;
                let score = path_rank::score(&self.entries[ix].text, &terms).unwrap_or(i64::MIN);
                (score, ix)
            })
            .collect();
        // A stable sort: equal scores keep the order of the narrower.
        ranked.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        Some(
            ranked
                .into_iter()
                .map(|(_, ix)| self.row(ix, &terms))
                .collect(),
        )
    }

    fn row(&self, ix: usize, terms: &[&str]) -> Candidate {
        let entry = &self.entries[ix];
        let name = &entry.text[entry.name_start..];
        let directory = entry.text[..entry.name_start].trim_end_matches('/');
        let mut row = Candidate::new(name, FileTarget::Project(entry.path.clone()))
            .highlights(name_highlights(name, terms));
        if !directory.is_empty() {
            row = row.secondary(directory);
        }
        row
    }
}

/// The byte ranges of `name` that the terms match: a contiguous match, else the scattered
/// characters. Terms with a `/` match directories and are not drawn.
fn name_highlights(name: &str, terms: &[&str]) -> Vec<Range<usize>> {
    let lower = name.to_lowercase();
    if lower.len() != name.len() {
        return Vec::new();
    }
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for term in terms {
        let term = term.to_lowercase();
        if term.is_empty() || term.contains('/') {
            continue;
        }
        if let Some(at) = lower.find(&term) {
            ranges.push(at..at + term.len());
            continue;
        }
        let mut wanted = term.chars();
        let mut next = wanted.next();
        let mut found = Vec::new();
        for (at, c) in lower.char_indices() {
            if Some(c) == next {
                found.push(at..at + c.len_utf8());
                next = wanted.next();
                if next.is_none() {
                    break;
                }
            }
        }
        if next.is_none() {
            ranges.extend(found);
        }
    }
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// The path an input names when it starts with `/` or `~/`.
fn typed_path(input: &str) -> Option<PathBuf> {
    if input.starts_with('/') {
        Some(PathBuf::from(input))
    } else {
        input.strip_prefix("~/").map(|rest| home_dir().join(rest))
    }
}

/// Opens the picker; a chosen file opens in the current pane.
pub fn open_current(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace = cx.entity();
    // The preview session reads the workspace when it is made, which it cannot do while the
    // workspace is being updated.
    window.defer(cx, move |window, cx| {
        start(&workspace, PaneChoice::Default, window, cx);
    });
}

/// Opens the picker; a chosen file opens in another pane.
pub fn open_other_pane(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    open_with(PaneChoice::OtherPane, window, cx);
}
/// Opens the picker; a chosen file opens in a split to the right.
pub fn open_split_right(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    open_with(PaneChoice::SplitRight, window, cx);
}
/// Opens the picker; a chosen file opens in a split below.
pub fn open_split_down(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    open_with(PaneChoice::SplitDown, window, cx);
}

fn open_with(choice: PaneChoice, window: &mut Window, cx: &mut Context<Workspace>) {
    let workspace = cx.entity();
    // See `open_current`: the preview session cannot be made while the workspace is updated.
    window.defer(cx, move |window, cx| {
        start(&workspace, choice, window, cx);
    });
}

/// Starts the picker. `choice` says which pane the preview shows in and a chosen file opens in.
pub(crate) fn start(
    workspace: &Entity<Workspace>,
    choice: PaneChoice,
    window: &mut Window,
    cx: &mut App,
) -> Option<SessionHandle> {
    let preview = cx.new(|cx| PreviewSession::new(workspace, cx));
    let (index, project) = {
        let workspace = workspace.read(cx);
        (
            Arc::new(PathIndex::new(workspace, cx)),
            workspace.project().clone(),
        )
    };
    let fs = project.read(cx).fs().clone();
    let weak_workspace = workspace.downgrade();
    let preview_workspace = weak_workspace.clone();
    let preview_choice = choice.clone();
    let choice_workspace = weak_workspace.clone();
    let session = Session::completing_read(HOST, "Go to file: ")
        .key_context(KEY_CONTEXT)
        // C-j answers the input as typed and creates the file when it does not exist; an empty
        // input is refused.
        .free_input(true)
        .accept(|input| !input.trim().is_empty(), "Give the name of a file")
        .source(Source::Dynamic(Box::new(move |query, cancel, cx| {
            let (mut sender, receiver) = mpsc::channel(1);
            if let Some(abs_path) = typed_path(&query) {
                let project_path = project.read(cx).find_project_path(&abs_path, cx);
                let fs = fs.clone();
                cx.spawn(async move |_| {
                    let metadata = fs.metadata(&abs_path).await.ok().flatten();
                    if cancel.is_cancelled() {
                        return;
                    }
                    let rows = match metadata {
                        Some(metadata) if !metadata.is_dir => {
                            let target = match project_path {
                                Some(path) => FileTarget::Project(path),
                                None => FileTarget::Abs(abs_path),
                            };
                            vec![Candidate::new(query, target)]
                        }
                        _ => Vec::new(),
                    };
                    sender.try_send(rows).ok();
                })
                .detach();
            } else {
                let index = index.clone();
                cx.background_spawn(async move {
                    if let Some(rows) = index.rows(&query, &cancel) {
                        sender.try_send(rows).ok();
                    }
                })
                .detach();
            }
            receiver
        })))
        .on_preview(move |event, window, cx| {
            let event = match event {
                SessionPreview::Highlight(value) => {
                    let target = match value.downcast_ref::<FileTarget>() {
                        Some(FileTarget::Project(path)) => preview_workspace
                            .upgrade()
                            .map(|workspace| (path.clone(), workspace)),
                        _ => None,
                    };
                    match target {
                        Some((path, workspace)) => {
                            let pane =
                                resolve(workspace.read(cx), &preview_choice, cx).preview_pane;
                            Preview::Highlight(PreviewTarget::Path {
                                path,
                                row: None,
                                ranges: Vec::new(),
                                pane,
                            })
                        }
                        // A file outside the project has no preview.
                        None => Preview::NoCandidate,
                    }
                }
                SessionPreview::Confirmed => Preview::Confirmed,
                SessionPreview::Cancelled => Preview::Cancelled,
            };
            preview.update(cx, |preview, cx| preview.update(event, window, cx));
        })
        .on_confirm(move |outcome, window, cx| match outcome {
            Outcome::Candidate(value) => {
                let Some(target) = value.downcast_ref::<FileTarget>() else {
                    return;
                };
                weak_workspace
                    .update(cx, |workspace, cx| {
                        goto_file_open::open(workspace, target, &choice, window, cx)
                    })
                    .ok();
            }
            Outcome::Input(input) => {
                weak_workspace
                    .update(cx, |workspace, cx| {
                        goto_file_open::create_and_open(workspace, &input, &choice, window, cx)
                    })
                    .ok();
            }
        });
    let session = add_pane_choices(
        session,
        choice_workspace.clone(),
        move |value, pane, window, cx| {
            let Some(target) = value.downcast_ref::<FileTarget>() else {
                return;
            };
            choice_workspace
                .update(cx, |workspace, cx| {
                    goto_file_open::open(workspace, target, &PaneChoice::Letter(pane), window, cx)
                })
                .ok();
        },
    );
    Some(workspace.update(cx, |workspace, cx| {
        bongos_minibuffer::open(workspace, session, window, cx)
    }))
}

#[cfg(test)]
#[path = "goto_file_tests.rs"]
mod tests;
