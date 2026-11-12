//! The repository overview (D13): root sources and previews, the problems a root can have, the
//! old-root task list, and `slow_poll` from the store's decider. The rows are checked on their
//! own with real temporary directories, and through the store for what needs its recompute.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use bongos_work_model::store::load;
use bongos_work_model::worktree_path::{MAX_WORKTREE_PATH, TemplateError};
use bongos_work_model::{Kind, Status, layout};
use gpui::{TestAppContext, UpdateGlobal as _, VisualTestContext};
use settings::{SettingsStore, WorkRepositoryContent};
use tempfile::TempDir;

use crate::command_tests::{harness, seed, wait_until};
use crate::overview::{RepositoryRow, RootProblem, RootSource, dump_text, old_root_tasks, row};
use crate::worktree_tests::{init_repository, leaf, test_repository};
use crate::worktrees::Layout;
use crate::{Repository, Stats};

fn layout_in(base: &Path, default_root: Option<&str>) -> Layout {
    Layout {
        default_root: default_root.map(str::to_string),
        home: base.join("home"),
        central: base.join("central"),
    }
}

fn base() -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    std::fs::create_dir_all(base.join("src").join("invoicing")).unwrap();
    std::fs::create_dir_all(base.join("home")).unwrap();
    (temp, base)
}

fn invoicing(base: &Path, worktree_root: Option<&str>) -> Repository {
    let path = base.join("src").join("invoicing");
    test_repository("invoicing", &path.to_string_lossy(), worktree_root)
}

fn row_of(repository: &Repository, layout: &Layout) -> RepositoryRow {
    row(repository, layout, Vec::new(), false)
}

#[test]
fn an_own_root_wins_over_the_default_and_previews_the_path() {
    let (_temp, base) = base();
    let layout = layout_in(&base, Some("~/trees/{repo}"));
    let row = row_of(&invoicing(&base, Some("{repo}.worktrees")), &layout);
    assert_eq!(row.root_source, RootSource::Own);
    assert_eq!(row.template.as_deref(), Some("{repo}.worktrees"));
    assert_eq!(
        row.preview,
        base.join("src")
            .join("invoicing.worktrees")
            .join("<task>")
            .to_string_lossy()
    );
    assert_eq!(row.problem, None);
    assert_eq!(row.name, "invoicing");
    assert_eq!(row.path, base.join("src").join("invoicing"));
}

#[test]
fn the_default_root_applies_without_an_own_one() {
    let (_temp, base) = base();
    let layout = layout_in(&base, Some("~/trees/{repo}"));
    let row = row_of(&invoicing(&base, None), &layout);
    assert_eq!(row.root_source, RootSource::Default);
    assert_eq!(row.template.as_deref(), Some("~/trees/{repo}"));
    assert_eq!(
        row.preview,
        base.join("home")
            .join("trees")
            .join("invoicing")
            .join("<task>")
            .to_string_lossy()
    );
    assert_eq!(row.problem, None);
}

#[test]
fn no_template_is_the_central_directory() {
    let (_temp, base) = base();
    let row = row_of(&invoicing(&base, None), &layout_in(&base, None));
    assert_eq!(row.root_source, RootSource::Central);
    assert_eq!(row.template, None);
    assert_eq!(
        row.preview,
        base.join("central")
            .join("invoicing")
            .join("<task>")
            .to_string_lossy()
    );
    assert_eq!(
        row.problem, None,
        "a central directory that does not exist yet is fine"
    );
}

#[test]
fn an_unknown_token_is_a_problem_with_no_preview() {
    let (_temp, base) = base();
    let row = row_of(
        &invoicing(&base, Some("{repo}-{branch}")),
        &layout_in(&base, None),
    );
    assert_eq!(row.root_source, RootSource::Own);
    assert_eq!(row.preview, "");
    assert_eq!(
        row.problem,
        Some(RootProblem::Template(TemplateError::UnknownToken(
            "branch".to_string()
        )))
    );
}

#[test]
fn an_unwritable_parent_is_a_problem_even_below_directories_that_do_not_exist() {
    let (_temp, base) = base();
    let locked = base.join("locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
    let in_locked = format!("{}/{{slug}}", locked.display());
    let below = format!("{}/not/yet/{{slug}}", locked.display());
    let rows = [
        row_of(&invoicing(&base, Some(&in_locked)), &layout_in(&base, None)),
        row_of(&invoicing(&base, Some(&below)), &layout_in(&base, None)),
    ];
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    for row in rows {
        assert_eq!(
            row.problem,
            Some(RootProblem::Unwritable {
                directory: locked.clone()
            })
        );
    }
    // The same roots are fine once the directory is writable, and the probe left nothing.
    let row = row_of(&invoicing(&base, Some(&in_locked)), &layout_in(&base, None));
    assert_eq!(row.problem, None);
    assert_eq!(std::fs::read_dir(&locked).unwrap().count(), 0);
}

#[test]
fn a_root_too_long_for_a_full_slug_and_one_inside_the_checkout_are_problems() {
    let (_temp, base) = base();
    let long = format!("{}/{}/{{slug}}", base.display(), "d".repeat(160));
    let row = row_of(&invoicing(&base, Some(&long)), &layout_in(&base, None));
    let Some(RootProblem::TooLong { length }) = row.problem else {
        panic!("expected too long, got {:?}", row.problem);
    };
    assert!(length > MAX_WORKTREE_PATH);

    let inside = format!("{}/src/invoicing/trees/{{slug}}", base.display());
    let row = row_of(&invoicing(&base, Some(&inside)), &layout_in(&base, None));
    assert_eq!(row.problem, Some(RootProblem::InsideCheckout));
}

/// Writes the leaves of the seeded tree `invoicing` with the recorded worktrees given as
/// `(directory of the task, id, status, worktree)`.
fn record_worktrees(root: &Path, tasks: &[(&[&str], &str, Status, &Path)]) {
    let mut project = layout::project_dir(root, "invoicing");
    for (path, id, status, worktree) in tasks {
        for slug in *path {
            project = layout::child_dir(&project, slug);
        }
        let worktree = worktree.to_string_lossy().into_owned();
        leaf(&project, id, Kind::PrShip, |file| {
            file.status = *status;
            file.worktree = Some(worktree);
        });
        project = layout::project_dir(root, "invoicing");
    }
}

fn names(rows: &[(String, String)]) -> BTreeSet<String> {
    rows.iter().map(|(id, _)| id.clone()).collect()
}

#[test]
fn old_root_tasks_lists_every_status_under_the_old_directory_only() {
    let (_temp, base) = base();
    let root = base.join("work");
    let repository = invoicing(&base, None);
    let old = base.join("src").join("invoicing").join(".bongos-worktrees");
    let elsewhere = base.join("central").join("invoicing").join("beta");
    seed(&root);
    record_worktrees(
        &root,
        &[
            (&["backfill"], "l1", Status::Open, &old.join("backfill")),
            (&["phase", "alpha"], "l2", Status::Done, &old.join("alpha")),
            (&["phase", "beta"], "l3", Status::Open, &elsewhere),
        ],
    );
    let tree = load(&root);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    // A sibling repository whose path is a prefix of this one's must not claim these.
    let sibling = test_repository(
        "other",
        &base.join("src").join("invoicing-2").to_string_lossy(),
        None,
    );
    let found = old_root_tasks(&tree, &[repository, sibling]);
    let ids: BTreeSet<_> = found[0].iter().map(|task| task.id.clone()).collect();
    assert_eq!(ids, BTreeSet::from(["l1".to_string(), "l2".to_string()]));
    assert!(found[1].is_empty());
    let alpha = found[0].iter().find(|task| task.id == "l2").unwrap();
    assert_eq!(alpha.slug, "alpha");
}

fn set_repositories(cx: &mut VisualTestContext, repositories: &[(&str, &Path)]) {
    let repositories: Vec<WorkRepositoryContent> = repositories
        .iter()
        .map(|(name, path)| WorkRepositoryContent {
            name: name.to_string(),
            path: path.to_string_lossy().into_owned(),
            setup_command: None,
            merge_is_ship: None,
            deployments: None,
            branch_template: None,
            worktree_root: None,
        })
        .collect();
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().repositories = Some(repositories);
            });
        });
    });
}

fn overview(
    harness: &crate::command_tests::Harness,
    cx: &mut VisualTestContext,
) -> Vec<RepositoryRow> {
    harness
        .store
        .read_with(cx, |store, _| store.repository_overview().to_vec())
}

#[gpui::test]
async fn the_store_keeps_old_root_tasks_whose_directory_exists(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let (_temp, base) = base();
    let repository = base.join("src").join("invoicing");
    let old = repository.join(".bongos-worktrees");
    for name in ["backfill", "alpha"] {
        std::fs::create_dir_all(old.join(name)).unwrap();
    }
    set_repositories(cx, &[("invoicing", &repository)]);
    record_worktrees(
        &harness.root,
        &[
            // Open, directory there.
            (&["backfill"], "l1", Status::Open, &old.join("backfill")),
            // Closed, directory there: still listed.
            (&["phase", "alpha"], "l2", Status::Done, &old.join("alpha")),
            // Directory gone: left out.
            (&["phase", "beta"], "l3", Status::Open, &old.join("beta")),
        ],
    );
    wait_until(cx, "the old-root tasks", |cx| {
        overview(&harness, cx)
            .first()
            .is_some_and(|row| !row.old_root.is_empty())
    })
    .await;
    // The row waits for the whole tree before the assertion: the third task has been read.
    wait_until(cx, "the third task", |cx| {
        harness.tree(cx).task_by_id("l3").is_some_and(|task| {
            task.file
                .as_ref()
                .is_some_and(|file| file.worktree.is_some())
        })
    })
    .await;
    cx.run_until_parked();
    let rows = overview(&harness, cx);
    assert_eq!(
        names(&rows[0].old_root),
        BTreeSet::from(["l1".to_string(), "l2".to_string()])
    );
    assert!(
        rows[0]
            .old_root
            .contains(&("l2".to_string(), "alpha".to_string()))
    );
}

#[gpui::test]
async fn slow_poll_follows_the_decider_and_a_settings_change_recomputes_the_rows(
    cx: &mut TestAppContext,
) {
    fn poll_api(common_dir: &Path) -> bool {
        common_dir
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "api")
    }
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .store
        .update(cx, |store, _| store.set_poll_decider(poll_api));
    let (_temp, base) = base();
    let invoicing = base.join("invoicing");
    let api = base.join("api");
    init_repository(&invoicing);
    init_repository(&api);
    set_repositories(cx, &[("invoicing", &invoicing), ("api", &api)]);
    wait_until(cx, "both rows", |cx| overview(&harness, cx).len() == 2).await;
    let rows = overview(&harness, cx);
    assert!(!rows[0].slow_poll, "invoicing is watched");
    assert!(rows[1].slow_poll, "api is polled");
    assert_eq!(rows[0].root_source, RootSource::Central);

    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().default_worktree_root =
                    Some("{repo}.trees".to_string());
            });
        });
    });
    wait_until(cx, "the default root", |cx| {
        overview(&harness, cx)
            .iter()
            .all(|row| row.root_source == RootSource::Default)
    })
    .await;
    let rows = overview(&harness, cx);
    assert!(rows[1].slow_poll);
    assert_eq!(
        rows[1].preview,
        base.join("api.trees").join("<task>").to_string_lossy()
    );

    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(false);
            });
        });
    });
    wait_until(cx, "the overview to clear", |cx| {
        overview(&harness, cx).is_empty()
    })
    .await;
}

#[test]
fn the_dump_lists_the_counters_and_every_row() {
    let (_temp, base) = base();
    let mut row = row_of(
        &invoicing(&base, Some("{repo}-{x}")),
        &layout_in(&base, None),
    );
    row.slow_poll = true;
    row.old_root = vec![("l1".to_string(), "alpha".to_string())];
    let stats = Stats {
        rebase_probes: 3,
        ..Stats::default()
    };
    let text = dump_text(&[row], &stats);
    for expected in [
        "  rebase_probes: 3\n",
        "repository invoicing\n",
        "  root: own {repo}-{x}\n",
        "  problem: the worktree root has an unknown token {x}",
        "  slow poll: true\n",
        "  old root: alpha (l1)\n",
    ] {
        assert!(text.contains(expected), "{expected:?} missing from\n{text}");
    }
}
