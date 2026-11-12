//! Occur: lists the lines that match the search in a read-only results tab. The session closes
//! first, leaving the cursor at the match; then a count-style pass runs in the background,
//! groups the matches by buffer row and stops at [`LINE_CAP`] lines, and the tab opens when it
//! lands. Each matching line is an excerpt with no context, so consecutive lines merge into one
//! excerpt and `editor::OpenExcerpts` jumps from a result to its line in the buffer.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use bongos_isearch_model::{SearchSpec, compile};
use editor::{Editor, HighlightKey};
use gpui::{App, AppContext as _, Entity, Window};
use language::{Buffer, BufferId, Capability, Point};
use multi_buffer::{MultiBuffer, MultiBufferOffset, MultiBufferSnapshot, PathKey};
use project::search::SearchQuery;

use crate::engine::{Cancel, build_query, count, in_range};

/// The most matching lines a results tab lists.
pub(crate) const LINE_CAP: usize = 10_000;

/// The matching lines of each buffer, in the order the buffers first match.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Lines {
    pub(crate) buffers: Vec<(BufferId, BTreeSet<u32>)>,
    /// The cap was hit: there are more matching lines.
    pub(crate) capped: bool,
}

impl Lines {
    pub(crate) fn len(&self) -> usize {
        self.buffers.iter().map(|(_, rows)| rows.len()).sum()
    }
}

/// Counts every match of `query` and groups them by the buffer row they start on, up to
/// [`LINE_CAP`] lines.
pub(crate) fn group(snapshot: &MultiBufferSnapshot, query: &SearchQuery) -> Lines {
    let Some(matches) = count(snapshot, query, usize::MAX, &Cancel::new()) else {
        return Lines::default();
    };
    let mut order: Vec<BufferId> = Vec::new();
    let mut rows: HashMap<BufferId, BTreeSet<u32>> = HashMap::new();
    let mut total = 0;
    let mut capped = false;
    for start in matches.starts {
        let point = snapshot.offset_to_point(start);
        let Some((buffer, point)) = snapshot.point_to_buffer_point(point) else {
            continue;
        };
        let id = buffer.remote_id();
        if rows.get(&id).is_some_and(|rows| rows.contains(&point.row)) {
            continue;
        }
        if total >= LINE_CAP {
            capped = true;
            break;
        }
        total += 1;
        if !rows.contains_key(&id) {
            order.push(id);
        }
        rows.entry(id).or_default().insert(point.row);
    }
    Lines {
        buffers: order
            .into_iter()
            .filter_map(|id| rows.remove(&id).map(|rows| (id, rows)))
            .collect(),
        capped,
    }
}

/// The title of the results tab.
pub(crate) fn title(string: &str) -> String {
    format!("Occur: {string}")
}

/// Runs occur for `spec` over `editor`'s text in the background, and opens the results tab in
/// the editor's workspace when it is done.
pub(crate) fn start(editor: &Entity<Editor>, spec: SearchSpec, window: &mut Window, cx: &mut App) {
    if spec.string.is_empty() {
        return;
    }
    let query = match build_query(&compile(&spec)) {
        Ok(query) => query,
        Err(invalid) => {
            bongos_minibuffer::message(window, cx, invalid.0);
            return;
        }
    };
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let source = editor.read(cx).buffer().clone();
    let Some(workspace) = editor.read(cx).workspace() else {
        return;
    };
    let grouped = cx.background_spawn({
        let query = query.clone();
        async move { group(&snapshot, &query) }
    });
    window
        .spawn(cx, async move |cx| {
            let lines = grouped.await;
            cx.update(move |window, cx| {
                if lines.len() == 0 {
                    let message = format!("No matches for \"{}\"", spec.string);
                    bongos_minibuffer::message(window, cx, message);
                    return;
                }
                let capped = lines.capped;
                open(
                    &source,
                    &workspace,
                    &lines,
                    &spec.string,
                    &query,
                    window,
                    cx,
                );
                if capped {
                    bongos_minibuffer::message(
                        window,
                        cx,
                        format!("Occur stopped at {LINE_CAP} lines"),
                    );
                }
            })
            .ok();
        })
        .detach();
}

/// Builds the read-only multibuffer of `lines`, one excerpt per run of consecutive rows.
fn results(
    source: &Entity<MultiBuffer>,
    lines: &Lines,
    string: &str,
    cx: &mut App,
) -> Entity<MultiBuffer> {
    cx.new(|cx| {
        let mut results = MultiBuffer::new(Capability::ReadOnly).with_title(title(string));
        for (id, rows) in &lines.buffers {
            let Some(buffer) = source.read(cx).buffer(*id) else {
                continue;
            };
            let snapshot = buffer.read(cx).snapshot();
            let ranges = rows
                .iter()
                .map(|row| Point::new(*row, 0)..Point::new(*row, snapshot.line_len(*row)))
                .collect::<Vec<_>>();
            let path = PathKey::for_buffer(&buffer, cx);
            results.set_excerpts_for_path(path, buffer as Entity<Buffer>, ranges, 0, cx);
        }
        results
    })
}

fn open(
    source: &Entity<MultiBuffer>,
    workspace: &Entity<workspace::Workspace>,
    lines: &Lines,
    string: &str,
    query: &Arc<SearchQuery>,
    window: &mut Window,
    cx: &mut App,
) {
    let results = results(source, lines, string, cx);
    let project = workspace.read(cx).project().clone();
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_multibuffer(results.clone(), Some(project), window, cx);
        editor.set_read_only(true);
        editor
    });
    workspace.update(cx, |workspace, cx| {
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
    });
    let snapshot = results.read(cx).snapshot(cx);
    let ranges = in_range(&snapshot, query, MultiBufferOffset(0)..snapshot.len());
    editor.update(cx, |editor, cx| {
        editor.highlight_background(
            HighlightKey::Editor,
            &ranges,
            |_, theme| theme.colors().search_match_background,
            cx,
        );
    });
}
