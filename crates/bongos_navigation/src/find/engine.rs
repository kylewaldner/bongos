//! The search behind find in files, and its scope.
//!
//! [`search`] is the session's dynamic source: it turns a [`FindRequest`] into a project search
//! and streams its matches as rows, one batch per file, while the search runs. The search ends,
//! and Zed's search with it, when its [`Cancel`] is set, when the receiver is gone, or when every
//! file has been searched. Nothing is written anywhere.

use std::ops::Range;
use std::rc::Rc;

use bongos_minibuffer::Candidate;
use bongos_minibuffer_model::Cancel;
use bongos_navigation_model::flags::Flags;
use bongos_navigation_model::rows;
use bongos_navigation_model::scope::{self, ScopeToken};
use futures::channel::mpsc;
use futures::{FutureExt as _, select_biased};
use gpui::{App, AsyncApp, Context, Entity, SharedString, Window};
use language::{Bias, Buffer, Point};
use project::Project;
use project::project_search::SearchResults;
use project::search::{SearchQuery, SearchResult};
use text::{Anchor, ToPoint as _};
use util::paths::{PathMatcher, PathStyle};
use workspace::Workspace;

/// Rows sent at most for one query; the matches past it are counted in a last [`FindRow::More`].
pub const MAX_ROWS: usize = 2_000;
/// Characters of a line kept around its match.
pub const SNIPPET_CHARS: usize = 200;
/// Batches the receiver may hold before the search waits for it.
const BATCHES_IN_FLIGHT: usize = 16;

/// Limits the search to the open buffers.
pub fn scope_open_buffers(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}
/// Limits the search to a directory.
pub fn scope_directory(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    crate::registry::in_session(window, cx, |_, _, _| {})
}

/// What to search for, and where.
#[derive(Clone, Debug, Default)]
pub struct FindRequest {
    /// The text (or pattern, with `flags.regex`) to find. Empty finds nothing.
    pub text: String,
    pub flags: Flags,
    /// Directories, globs and negated globs from after ` -- `.
    pub scope: Vec<ScopeToken>,
    /// A directory the whole search stays in (`M-d`), as a project path.
    pub base_dir: Option<String>,
    /// The buffers searched when `flags.open_buffers` is set: those of every editor in the panes,
    /// as the caller collects them.
    pub buffers: Vec<Entity<Buffer>>,
}

impl FindRequest {
    /// Parses `input` as `text -- scope tokens`.
    pub fn new(input: &str, flags: Flags) -> Self {
        let parts = scope::parse(input);
        Self {
            text: parts.text,
            flags,
            scope: parts.scope,
            base_dir: None,
            buffers: Vec::new(),
        }
    }

    pub fn base_dir(mut self, dir: impl Into<String>) -> Self {
        self.base_dir = Some(dir.into());
        self
    }

    pub fn buffers(mut self, buffers: Vec<Entity<Buffer>>) -> Self {
        self.buffers = buffers;
        self
    }

    /// The include globs. Directory tokens and globs narrow together (`src/main *.kt` is the
    /// Kotlin files under `src/main`), and the base directory narrows every one of them.
    fn include_globs(&self) -> Vec<String> {
        let base = self
            .base_dir
            .as_deref()
            .map(|dir| dir.trim_end_matches('/'))
            .filter(|dir| !dir.is_empty());
        let join = |dir: &str| match base {
            Some(base) => format!("{base}/{}", dir.trim_end_matches('/')),
            None => dir.trim_end_matches('/').to_string(),
        };
        let mut dirs: Vec<String> = self
            .scope
            .iter()
            .filter_map(|token| match token {
                ScopeToken::Directory(dir) => Some(join(dir)),
                _ => None,
            })
            .collect();
        if dirs.is_empty()
            && let Some(base) = base
        {
            dirs.push(base.to_string());
        }
        let globs: Vec<&str> = self
            .scope
            .iter()
            .filter_map(|token| match token {
                ScopeToken::Include(glob) => Some(glob.as_str()),
                _ => None,
            })
            .collect();
        // A glob with a `/` is already a path pattern: it is not joined under a directory token
        // (only under the base directory, which narrows everything).
        let (paths, names): (Vec<&str>, Vec<&str>) =
            globs.into_iter().partition(|glob| glob.contains('/'));
        let mut include: Vec<String> = paths
            .into_iter()
            .map(|glob| join_base(base, glob))
            .collect();
        match (dirs.is_empty(), names.is_empty()) {
            (true, _) => include.extend(names.into_iter().map(str::to_string)),
            (false, true) if include.is_empty() => {
                include.extend(dirs.into_iter().map(|dir| format!("{dir}/**")))
            }
            (false, true) => {}
            (false, false) => include.extend(
                dirs.iter()
                    .flat_map(|dir| names.iter().map(move |glob| format!("{dir}/{glob}"))),
            ),
        }
        include
    }

    fn exclude_globs(&self) -> Vec<String> {
        self.scope
            .iter()
            .filter(|token| token.is_exclude())
            .map(ScopeToken::glob)
            .collect()
    }

    /// The project search for this request, or the reason there is none.
    fn query(&self, full_paths: bool, path_style: PathStyle) -> Result<SearchQuery, SharedString> {
        let include = PathMatcher::new(self.include_globs(), path_style)
            .map_err(|error| SharedString::from(format!("Invalid glob: {error}")))?;
        let exclude = PathMatcher::new(self.exclude_globs(), path_style)
            .map_err(|error| SharedString::from(format!("Invalid glob: {error}")))?;
        let case_sensitive = self.flags.case_sensitive(&self.text);
        let buffers = self.flags.open_buffers.then(|| self.buffers.clone());
        if self.flags.regex {
            SearchQuery::regex(
                &self.text,
                self.flags.whole_word,
                case_sensitive,
                false,
                false,
                include,
                exclude,
                full_paths,
                buffers,
            )
            .map_err(|error| format!("Invalid regexp: {error}").into())
        } else {
            SearchQuery::text(
                &self.text,
                self.flags.whole_word,
                case_sensitive,
                false,
                include,
                exclude,
                full_paths,
                buffers,
            )
            .map_err(|error| format!("Invalid search: {error}").into())
        }
    }
}

/// The value of a find row.
#[derive(Clone, Debug)]
pub enum FindRow {
    /// One match: its buffer, its range and its 1-based line.
    Match {
        buffer: Entity<Buffer>,
        range: Range<Anchor>,
        line: u32,
    },
    /// The matches past [`MAX_ROWS`], which are not shown. `at_least` is set when Zed's search
    /// hit its own cap, so `count` is a floor.
    More { count: usize, at_least: bool },
}

fn join_base(base: Option<&str>, glob: &str) -> String {
    match base {
        Some(base) => format!("{base}/{glob}"),
        None => glob.to_string(),
    }
}

/// The text of the [`FindRow::More`] row.
fn more_label(count: usize, at_least: bool) -> String {
    let plus = if at_least { "+" } else { "" };
    format!("{count}{plus} more matches: RET opens them all")
}

/// Where a search stands, for the session's status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindStatus {
    /// The search runs; rows may still come.
    Searching,
    /// The search ended (or never started, for empty text).
    Done,
    /// No search was started: the request is not valid (`Invalid regexp: <reason>`).
    Invalid(SharedString),
}

impl FindStatus {
    /// The text shown after the input.
    pub fn label(&self) -> SharedString {
        match self {
            FindStatus::Searching => "searching".into(),
            FindStatus::Done => SharedString::default(),
            FindStatus::Invalid(reason) => reason.clone(),
        }
    }
}

/// Reports the status of one search. It is never called once the search's [`Cancel`] is set or
/// its receiver is gone, and never from inside `search` itself (the first report is deferred).
pub type StatusHandler = Rc<dyn Fn(FindStatus, &mut App)>;

/// Starts the search for `request` at once and returns its rows as they are found: one batch
/// per file, `path:line  snippet` rows with the match highlighted, at most [`MAX_ROWS`] rows then
/// one [`FindRow::More`] row counting the rest. `on_status` hears [`FindStatus::Searching`] then
/// [`FindStatus::Done`] when the stream ends, or only [`FindStatus::Invalid`] (nothing is sent)
/// or [`FindStatus::Done`] (empty text) when no search starts.
///
/// The search stops, dropping Zed's search, as soon as `cancel` is set; a dropped receiver is
/// noticed at the search's next result or its end. No row is sent once `cancel` is set.
pub fn search(
    project: &Entity<Project>,
    request: FindRequest,
    cancel: Cancel,
    on_status: impl Fn(FindStatus, &mut App) + 'static,
    cx: &mut App,
) -> mpsc::Receiver<Vec<Candidate>> {
    let on_status: StatusHandler = Rc::new(on_status);
    let (tx, rx) = mpsc::channel(BATCHES_IN_FLIGHT);
    if request.text.is_empty() {
        report_deferred(FindStatus::Done, &cancel, &on_status, cx);
        return rx;
    }
    let (full_paths, path_style) = {
        let project = project.read(cx);
        (
            project.visible_worktrees(cx).nth(1).is_some(),
            project.path_style(cx),
        )
    };
    let query = match request.query(full_paths, path_style) {
        Ok(query) => query,
        Err(reason) => {
            report_deferred(FindStatus::Invalid(reason), &cancel, &on_status, cx);
            return rx;
        }
    };
    report_deferred(FindStatus::Searching, &cancel, &on_status, cx);
    let results = project.update(cx, |project, cx| project.search(query, cx));
    cx.spawn(async move |cx| stream(results, tx, cancel, full_paths, on_status, cx).await)
        .detach();
    rx
}

fn report_deferred(status: FindStatus, cancel: &Cancel, on_status: &StatusHandler, cx: &mut App) {
    let cancel = cancel.clone();
    let on_status = on_status.clone();
    cx.defer(move |cx| {
        if !cancel.is_cancelled() {
            on_status(status, cx);
        }
    });
}

/// Turns the results of a project search into batches of rows on `tx`. It holds `results`, so
/// Zed's search stops when this returns: when the search ends, when `cancel` is set (at once),
/// or when the receiver is gone (at the next result).
pub(crate) async fn stream(
    results: SearchResults<SearchResult>,
    mut tx: mpsc::Sender<Vec<Candidate>>,
    cancel: Cancel,
    full_paths: bool,
    on_status: StatusHandler,
    cx: &mut AsyncApp,
) {
    let SearchResults { task_handle, rx } = results;
    let _search = task_handle;
    let stale = |tx: &mpsc::Sender<Vec<Candidate>>| cancel.is_cancelled() || tx.is_closed();
    let mut shown = 0;
    let mut more = 0;
    // Zed stops its search at a cap of matches; the count of the rest is then a floor.
    let mut limit_reached = false;
    loop {
        let next = select_biased! {
            _ = cancel.cancelled().fuse() => return,
            next = rx.recv().fuse() => next,
        };
        if stale(&tx) {
            return;
        }
        let Ok(result) = next else {
            break;
        };
        let (buffer, ranges) = match result {
            SearchResult::Buffer { buffer, ranges } => (buffer, ranges),
            SearchResult::LimitReached => {
                limit_reached = true;
                continue;
            }
            SearchResult::WaitingForScan | SearchResult::Searching => continue,
        };
        let room = MAX_ROWS - shown;
        let batch = cx.update(|cx| buffer_rows(&buffer, &ranges, room, full_paths, cx));
        shown += batch.len();
        more += ranges.len() - batch.len();
        if !batch.is_empty() && !send(&mut tx, batch, &cancel).await {
            return;
        }
    }
    if more > 0 {
        let row = Candidate::new(
            more_label(more, limit_reached),
            FindRow::More {
                count: more,
                at_least: limit_reached,
            },
        );
        if !send(&mut tx, vec![row], &cancel).await {
            return;
        }
    }
    if !stale(&tx) {
        cx.update(|cx| on_status(FindStatus::Done, cx));
    }
}

/// Sends `batch` when the receiver has room for it, unless `cancel` is set first or the
/// receiver is gone. Returns whether it was sent.
async fn send(
    tx: &mut mpsc::Sender<Vec<Candidate>>,
    batch: Vec<Candidate>,
    cancel: &Cancel,
) -> bool {
    let ready = select_biased! {
        _ = cancel.cancelled().fuse() => false,
        ready = futures::future::poll_fn(|cx| tx.poll_ready(cx)).fuse() => ready.is_ok(),
    };
    // Checked again right before sending: nothing of a cancelled query is ever sent.
    ready && !cancel.is_cancelled() && tx.start_send(batch).is_ok()
}

/// The rows of one file's matches, at most `room` of them.
fn buffer_rows(
    buffer: &Entity<Buffer>,
    ranges: &[Range<Anchor>],
    room: usize,
    full_paths: bool,
    cx: &App,
) -> Vec<Candidate> {
    if room == 0 || ranges.is_empty() {
        return Vec::new();
    }
    let buffer_ref = buffer.read(cx);
    let path: String = match buffer_ref.file() {
        Some(file) if full_paths => file.full_path(cx).to_string_lossy().into_owned(),
        Some(file) => file.path().display(file.path_style(cx)).into_owned(),
        None => "untitled".to_string(),
    };
    let snapshot = buffer_ref.snapshot();
    ranges
        .iter()
        .take(room)
        .map(|range| {
            let start = range.start.to_point(&snapshot);
            let end = range.end.to_point(&snapshot);
            let line_len = snapshot.line_len(start.row);
            let match_end = if end.row == start.row {
                end.column
            } else {
                line_len
            };
            // Only the bytes around the match are copied, so a huge line costs no more than a
            // short one. The window holds more than the snippet can use on each side (a character
            // is at most 4 bytes), so the snippet reads as it would from the whole line.
            let reach = (SNIPPET_CHARS * 4 + 4) as u32;
            let match_end = match_end.min(start.column.saturating_add(reach));
            let from = snapshot.clip_point(
                Point::new(start.row, start.column.saturating_sub(reach)),
                Bias::Right,
            );
            let to = snapshot.clip_point(
                Point::new(start.row, match_end.saturating_add(reach).min(line_len)),
                Bias::Left,
            );
            let line: String = snapshot.text_for_range(from..to).collect();
            let snippet = rows::snippet(
                &line,
                (start.column - from.column) as usize..(match_end - from.column) as usize,
                SNIPPET_CHARS,
            );
            let line_number = start.row + 1;
            let primary = rows::find_row(&path, line_number, &snippet.text);
            let prefix = primary.len() - snippet.text.len();
            let highlights = snippet
                .ranges
                .iter()
                .map(|range| range.start + prefix..range.end + prefix)
                .collect();
            Candidate::new(
                primary,
                FindRow::Match {
                    buffer: buffer.clone(),
                    range: range.clone(),
                    line: line_number,
                },
            )
            .highlights(highlights)
        })
        .collect()
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod engine_tests;
