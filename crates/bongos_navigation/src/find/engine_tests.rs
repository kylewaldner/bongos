use std::cell::{Cell, RefCell};

use bongos_navigation_model::flags::Flags;
use fs::FakeFs;
use futures::StreamExt as _;
use gpui::{Task, TestAppContext};
use serde_json::{Map, Value as Json, json};
use settings::SettingsStore;
use util::path;

use super::*;

type Statuses = Rc<RefCell<Vec<FindStatus>>>;

async fn project(tree: Json, cx: &mut TestAppContext) -> Entity<Project> {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
    });
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), tree).await;
    Project::test(fs, [path!("/root").as_ref()], cx).await
}

fn start(
    project: &Entity<Project>,
    request: FindRequest,
    cancel: &Cancel,
    cx: &mut TestAppContext,
) -> (mpsc::Receiver<Vec<Candidate>>, Statuses) {
    let statuses = Statuses::default();
    let sink = statuses.clone();
    let cancel = cancel.clone();
    let rx = cx.update(|cx| {
        search(
            project,
            request,
            cancel,
            move |status, _| sink.borrow_mut().push(status),
            cx,
        )
    });
    (rx, statuses)
}

fn find(input: &str) -> FindRequest {
    FindRequest::new(input, Flags::default())
}

fn texts(rows: &[Candidate]) -> Vec<String> {
    rows.iter().map(|row| row.primary.to_string()).collect()
}

fn paths(batches: &[Vec<Candidate>]) -> Vec<String> {
    let mut paths: Vec<String> = batches
        .iter()
        .flatten()
        .filter_map(|row| row.primary.split(':').next().map(str::to_string))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn find_row(row: &Candidate) -> FindRow {
    row.value
        .downcast_ref::<FindRow>()
        .expect("a find row")
        .clone()
}

async fn open(project: &Entity<Project>, path: &str, cx: &mut TestAppContext) -> Entity<Buffer> {
    project
        .update(cx, |project, cx| project.open_local_buffer(path, cx))
        .await
        .expect("the buffer opens")
}

fn needles(buffer: &Entity<Buffer>, cx: &TestAppContext) -> Vec<Range<Anchor>> {
    buffer.read_with(cx, |buffer, _| {
        buffer
            .text()
            .match_indices("needle")
            .map(|(ix, found)| buffer.anchor_before(ix)..buffer.anchor_after(ix + found.len()))
            .collect()
    })
}

/// Sets its flag when dropped: it stands in for Zed's search inside a fake search task.
struct DropGuard(Rc<Cell<bool>>);

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

/// A search whose results the test sends by hand, and whose task records being dropped.
struct FakeSearch {
    results: async_channel::Sender<SearchResult>,
    dropped: Rc<Cell<bool>>,
    rows: mpsc::Receiver<Vec<Candidate>>,
    statuses: Statuses,
    cancel: Cancel,
}

fn fake_search(cx: &mut TestAppContext) -> FakeSearch {
    let (results, rx) = async_channel::unbounded();
    let dropped = Rc::new(Cell::new(false));
    let guard = DropGuard(dropped.clone());
    let task_handle: Task<()> = cx.spawn(async move |_| {
        let _guard = guard;
        futures::future::pending::<()>().await
    });
    let (tx, rows) = mpsc::channel(BATCHES_IN_FLIGHT);
    let cancel = Cancel::new();
    let statuses = Statuses::default();
    let sink = statuses.clone();
    let on_status: StatusHandler = Rc::new(move |status, _| sink.borrow_mut().push(status));
    let stream_cancel = cancel.clone();
    cx.spawn(async move |mut cx| {
        stream(
            SearchResults { task_handle, rx },
            tx,
            stream_cancel,
            false,
            on_status,
            &mut cx,
        )
        .await
    })
    .detach();
    FakeSearch {
        results,
        dropped,
        rows,
        statuses,
        cancel,
    }
}

#[gpui::test]
async fn rows_read_path_line_and_snippet_with_the_match_highlighted(cx: &mut TestAppContext) {
    let project = project(
        json!({ "src": { "a.txt": "first line\nlet needle = 1;\n" } }),
        cx,
    )
    .await;
    let (rx, statuses) = start(&project, find("needle"), &Cancel::new(), cx);
    let batches: Vec<Vec<Candidate>> = rx.collect().await;
    cx.run_until_parked();

    assert_eq!(batches.len(), 1);
    let row = &batches[0][0];
    assert_eq!(texts(&batches[0]), ["src/a.txt:2  let needle = 1;"]);
    assert_eq!(row.highlights.len(), 1);
    assert_eq!(&row.primary[row.highlights[0].clone()], "needle");
    let FindRow::Match { line, .. } = find_row(row) else {
        panic!("a match row");
    };
    assert_eq!(line, 2);
    assert_eq!(
        *statuses.borrow(),
        [FindStatus::Searching, FindStatus::Done]
    );
    assert_eq!(FindStatus::Searching.label(), "searching");
}

#[gpui::test]
async fn the_first_files_rows_arrive_before_the_search_completes(cx: &mut TestAppContext) {
    let project = project(json!({ "a.txt": "a needle\n", "b.txt": "b needle\n" }), cx).await;
    let a = open(&project, path!("/root/a.txt"), cx).await;
    let b = open(&project, path!("/root/b.txt"), cx).await;
    let mut search = fake_search(cx);

    search
        .results
        .send(SearchResult::Buffer {
            ranges: needles(&a, cx),
            buffer: a,
        })
        .await
        .unwrap();
    let first = search.rows.next().await.expect("the first file's batch");
    assert_eq!(texts(&first), ["a.txt:1  a needle"]);
    cx.run_until_parked();
    assert!(search.rows.try_recv().is_err(), "nothing else yet");
    assert!(!search.results.is_closed(), "the task still runs");
    assert!(!search.statuses.borrow().contains(&FindStatus::Done));

    // The second file is released only now that the first batch has been received.
    search
        .results
        .send(SearchResult::Buffer {
            ranges: needles(&b, cx),
            buffer: b,
        })
        .await
        .unwrap();
    search.results.close();
    let second = search.rows.next().await.expect("the second file's batch");
    assert_eq!(texts(&second), ["b.txt:1  b needle"]);
    assert!(search.rows.next().await.is_none());
    cx.run_until_parked();
    assert_eq!(*search.statuses.borrow(), [FindStatus::Done]);
    assert!(search.dropped.get());
}

#[gpui::test]
async fn setting_cancel_or_dropping_the_receiver_ends_the_task_and_drops_the_search(
    cx: &mut TestAppContext,
) {
    let project = project(json!({ "a.txt": "a needle\n", "b.txt": "b needle\n" }), cx).await;
    let a = open(&project, path!("/root/a.txt"), cx).await;
    let b = open(&project, path!("/root/b.txt"), cx).await;

    // Cancel: the task ends while it waits for the next result, and sends nothing more.
    let mut search = fake_search(cx);
    search
        .results
        .send(SearchResult::Buffer {
            ranges: needles(&a, cx),
            buffer: a.clone(),
        })
        .await
        .unwrap();
    assert!(search.rows.next().await.is_some());
    cx.run_until_parked();
    assert!(!search.dropped.get());
    search.cancel.cancel();
    cx.run_until_parked();
    assert!(search.dropped.get(), "the search is dropped");
    assert!(search.results.is_closed(), "the task has ended");
    assert!(
        search
            .results
            .try_send(SearchResult::Buffer {
                ranges: needles(&b, cx),
                buffer: b.clone(),
            })
            .is_err()
    );
    assert!(search.rows.next().await.is_none(), "no row after cancel");
    assert!(search.statuses.borrow().is_empty());

    // A dropped receiver: the task ends at the search's next result, without reading it.
    let search = fake_search(cx);
    cx.run_until_parked();
    drop(search.rows);
    search
        .results
        .send(SearchResult::Buffer {
            ranges: needles(&b, cx),
            buffer: b,
        })
        .await
        .unwrap();
    cx.run_until_parked();
    assert!(search.dropped.get(), "the search is dropped");
    assert!(search.results.is_closed(), "the task has ended");
    assert!(search.statuses.borrow().is_empty());
}

#[gpui::test]
async fn matches_past_2000_rows_are_counted_in_one_more_row(cx: &mut TestAppContext) {
    // 41 files of 50 matching lines: 2,050 matches.
    let lines = "needle\n".repeat(50);
    let mut tree = Map::new();
    for ix in 0..41 {
        tree.insert(format!("f{ix:02}.txt"), Json::String(lines.clone()));
    }
    let project = project(Json::Object(tree), cx).await;
    let (rx, _) = start(&project, find("needle"), &Cancel::new(), cx);
    let batches: Vec<Vec<Candidate>> = rx.collect().await;

    let rows: Vec<&Candidate> = batches.iter().flatten().collect();
    assert_eq!(rows.len(), 2_001);
    let matches = rows
        .iter()
        .filter(|row| matches!(find_row(row), FindRow::Match { .. }))
        .count();
    assert_eq!(matches, 2_000);
    let last = rows.last().unwrap();
    assert!(matches!(find_row(last), FindRow::More { count: 50 }));
    assert_eq!(last.primary.as_ref(), "50 more matches: RET opens them all");
    assert_eq!(batches.len(), 41, "one batch per file, then the more row");
}

#[gpui::test]
async fn a_directory_a_glob_and_a_negated_glob_filter_the_files(cx: &mut TestAppContext) {
    let project = project(
        json!({
            "src": {
                "main": {
                    "A.kt": "needle",
                    "ATest.kt": "needle",
                    "B.java": "needle",
                    "deep": { "D.kt": "needle" },
                },
                "other": { "C.kt": "needle" },
            },
        }),
        cx,
    )
    .await;
    let (rx, _) = start(
        &project,
        find("needle -- src/main *.kt !*Test.kt"),
        &Cancel::new(),
        cx,
    );
    let batches: Vec<Vec<Candidate>> = rx.collect().await;
    assert_eq!(paths(&batches), ["src/main/A.kt", "src/main/deep/D.kt"]);
}

#[gpui::test]
async fn open_buffers_only_searches_the_given_buffers(cx: &mut TestAppContext) {
    let project = project(
        json!({ "a.txt": "needle", "b.txt": "needle", "c.txt": "needle" }),
        cx,
    )
    .await;
    let a = open(&project, path!("/root/a.txt"), cx).await;
    let _b = open(&project, path!("/root/b.txt"), cx).await;
    let flags = Flags {
        open_buffers: true,
        ..Flags::default()
    };
    let request = FindRequest::new("needle", flags).buffers(vec![a]);
    let (rx, _) = start(&project, request, &Cancel::new(), cx);
    let batches: Vec<Vec<Candidate>> = rx.collect().await;
    assert_eq!(paths(&batches), ["a.txt"]);
}

#[gpui::test]
async fn an_invalid_regex_sends_nothing_and_reports_the_reason(cx: &mut TestAppContext) {
    let project = project(json!({ "a.txt": "needle(" }), cx).await;
    let flags = Flags {
        regex: true,
        ..Flags::default()
    };
    let (rx, statuses) = start(
        &project,
        FindRequest::new("needle(", flags),
        &Cancel::new(),
        cx,
    );
    let batches: Vec<Vec<Candidate>> = rx.collect().await;
    cx.run_until_parked();
    assert!(batches.is_empty());
    let statuses = statuses.borrow();
    assert_eq!(statuses.len(), 1);
    let label = statuses[0].label();
    assert!(
        matches!(statuses[0], FindStatus::Invalid(_)),
        "{:?}",
        statuses[0]
    );
    let reason = label
        .strip_prefix("Invalid regexp: ")
        .expect("the status names the invalid regexp");
    assert!(!reason.trim().is_empty(), "the reason is given: {label}");
}

mod perf_tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[gpui::test]
    async fn the_first_batch_of_a_20000_file_project_arrives_within_100_ms(
        cx: &mut TestAppContext,
    ) {
        let mut tree = Map::new();
        for dir in 0..200 {
            let mut files = Map::new();
            for file in 0..100 {
                // One file in a hundred matches, the first one among them.
                let comment = if file == 0 { "needle" } else { "nothing" };
                files.insert(
                    format!("f{file:03}.kt"),
                    Json::String(format!("fun f{file}() {{}}\n// {comment} {dir}\n")),
                );
            }
            tree.insert(format!("d{dir:03}"), Json::Object(files));
        }
        let project = project(Json::Object(tree), cx).await;
        cx.run_until_parked();

        let mut times = Vec::new();
        for _ in 0..3 {
            let cancel = Cancel::new();
            let started = Instant::now();
            let (mut rx, _) = start(&project, find("needle"), &cancel, cx);
            assert!(
                rx.try_recv().is_err(),
                "search returns its receiver before anything is found"
            );
            let first = rx.next().await.expect("a first batch");
            times.push(started.elapsed());
            assert!(!first.is_empty());
            cancel.cancel();
            cx.run_until_parked();
        }
        let best = times.iter().min().copied().unwrap_or(Duration::MAX);
        eprintln!("first batch: best {best:?} of {times:?}");
        // The budget holds in an optimized build (the perf set is built with the `perf-test`
        // profile). In the dev profile the unoptimized project search and buffer opening make the
        // first batch take about a second here.
        if cfg!(not(debug_assertions)) {
            assert!(
                best < Duration::from_millis(100),
                "best of three was {best:?} ({times:?})"
            );
        }
    }
}
