//! The event-driven rebase check (D9, D10, D11) against real git repositories in temporary
//! directories, watched with the real file system. Stacks are written straight into the task
//! files and the branches made with the git binary, so the only git activity is the test's own.
//!
//! Nothing here calls `refresh_rebase` to see a ref change: the states follow the watcher, or
//! the slow poll when the fake clock passes its interval.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bongos_work_model::{Kind, StackedOn, TaskFile, UnblockPoint, layout, serialize_task};
use gpui::{TestAppContext, UpdateGlobal as _, VisualTestContext};
use parking_lot::Mutex;
use settings::{SettingsStore, WorkRepositoryContent};
use tempfile::TempDir;

use crate::command_tests::{Harness, group_toml, harness, leaf_toml, wait_until};
use crate::rebase::RebaseState;
use crate::ref_watch::{REF_WATCH_LATENCY, SLOW_POLL_INTERVAL, names_a_ref};
use crate::store_tests::write;
use crate::worktree_tests::{git, init_repository};

/// Two repositories, `invoicing` (the seeded project's: `phase/alpha` `l2` and `phase/beta`
/// `l3`) and `api` (the project `api`: `gamma` `l8` and `delta` `l9`), none stacked yet.
struct Fixture {
    harness: Harness,
    invoicing: PathBuf,
    api: PathBuf,
    _temp: TempDir,
}

/// One stacked pair: the child's id, the parent's branch and the repository.
struct Stack {
    child: &'static str,
    parent_branch: &'static str,
    repository: PathBuf,
}

/// The decider that puts no repository on the slow poll.
fn watch_all(_: &Path) -> bool {
    false
}

/// The decider that puts every repository on the slow poll.
fn poll_all(_: &Path) -> bool {
    true
}

/// The decider that puts the repository at a directory named `api` on the slow poll.
fn poll_api(common_dir: &Path) -> bool {
    common_dir
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "api")
}

async fn fixture(
    cx: &mut TestAppContext,
    poll_decider: fn(&Path) -> bool,
) -> (Fixture, &mut VisualTestContext) {
    let (harness, cx) = harness(cx, true, true).await;
    harness
        .store
        .update(cx, |store, _| store.set_poll_decider(poll_decider));
    let temp = TempDir::new().unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let invoicing = base.join("invoicing");
    let api = base.join("api");
    init_repository(&invoicing);
    init_repository(&api);
    set_repositories(cx, &[("invoicing", &invoicing), ("api", &api)]);

    let project = layout::project_dir(&harness.root, "api");
    write(
        &layout::project_file(&project),
        "schema = 1\ntitle = \"Api\"\nrepositories = [\"api\"]\n",
    );
    write(&layout::task_file(&project), &group_toml("g9", "Api"));
    for (id, slug) in [("l8", "gamma"), ("l9", "delta")] {
        write(
            &layout::task_file(&layout::child_dir(&project, slug)),
            &leaf_toml(id, slug, Kind::PrShip),
        );
    }
    wait_until(cx, "the api project to load", |cx| {
        harness.tree(cx).task_by_id("l9").is_some()
    })
    .await;
    (
        Fixture {
            harness,
            invoicing,
            api,
            _temp: temp,
        },
        cx,
    )
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

fn set_enabled(cx: &mut VisualTestContext, enabled: bool) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
}

impl Fixture {
    fn dir(&self, project: &str, path: &[&str]) -> PathBuf {
        let mut dir = layout::project_dir(&self.harness.root, project);
        for slug in path {
            dir = layout::child_dir(&dir, slug);
        }
        dir
    }

    fn file(&self, cx: &mut VisualTestContext, dir: &Path) -> TaskFile {
        self.harness
            .tree(cx)
            .task_by_dir(dir)
            .and_then(|task| task.file.clone())
            .unwrap_or_else(|| panic!("no task at {}", dir.display()))
    }

    /// Stacks `child` on its sibling `parent` in `project`: the parent's branch is cut from
    /// `main`, the child's from the parent's, and the child records a worktree and its base.
    async fn stack(
        &self,
        cx: &mut VisualTestContext,
        project: &str,
        group: &[&str],
        (parent, parent_branch): (&str, &'static str),
        (child, child_id, child_branch): (&str, &'static str, &str),
        repository: &Path,
    ) -> Stack {
        git(repository, &["branch", parent_branch, "main"]);
        git(repository, &["branch", child_branch, parent_branch]);
        let base = git(repository, &["rev-parse", parent_branch]);

        let parent_dir = self.dir(project, &[group, &[parent][..]].concat());
        let mut parent_file = self.file(cx, &parent_dir);
        parent_file.branch = Some(parent_branch.to_string());
        write(
            &layout::task_file(&parent_dir),
            &serialize_task(&parent_file).unwrap(),
        );

        let child_dir = self.dir(project, &[group, &[child][..]].concat());
        let mut child_file = self.file(cx, &child_dir);
        child_file.stacked_on = Some(StackedOn {
            task: parent.to_string(),
            unblock: UnblockPoint::Implemented,
            extra: Default::default(),
        });
        child_file.branch = Some(child_branch.to_string());
        child_file.base_sha = Some(base);
        child_file.worktree = Some(repository.to_string_lossy().into_owned());
        write(
            &layout::task_file(&child_dir),
            &serialize_task(&child_file).unwrap(),
        );

        let common_dir = repository.join(".git");
        wait_until(
            cx,
            "the stack to load and its repository to be watched",
            |cx| {
                self.harness.store.read_with(cx, |store, _| {
                    store.watched_git_dirs().contains_key(&common_dir)
                        && store.snapshot().is_some_and(|tree| {
                            tree.task_by_id(child_id)
                                .and_then(|task| task.file.as_ref())
                                .is_some_and(|file| file.stacked_on.is_some())
                        })
                })
            },
        )
        .await;
        Stack {
            child: child_id,
            parent_branch,
            repository: repository.to_path_buf(),
        }
    }

    /// `alpha` (`kyle/alpha` when `nested`) under `beta` in `invoicing`.
    async fn stack_invoicing(&self, cx: &mut VisualTestContext, nested: bool) -> Stack {
        let (parent_branch, child_branch) = if nested {
            ("kyle/alpha", "kyle/beta")
        } else {
            ("alpha", "beta")
        };
        self.stack(
            cx,
            "invoicing",
            &["phase"],
            ("alpha", parent_branch),
            ("beta", "l3", child_branch),
            &self.invoicing,
        )
        .await
    }

    /// `delta` stacked on `gamma` in `api`.
    async fn stack_api(&self, cx: &mut VisualTestContext) -> Stack {
        self.stack(
            cx,
            "api",
            &[],
            ("gamma", "gamma"),
            ("delta", "l9", "delta"),
            &self.api,
        )
        .await
    }

    fn state(&self, cx: &mut VisualTestContext, id: &str) -> Option<RebaseState> {
        self.harness
            .store
            .read_with(cx, |store, _| store.rebase_state(id).cloned())
    }

    fn probes(&self, cx: &mut VisualTestContext) -> usize {
        self.harness
            .store
            .read_with(cx, |store, _| store.stats().rebase_probes)
    }

    fn ref_events(&self, cx: &mut VisualTestContext) -> usize {
        self.harness
            .store
            .read_with(cx, |store, _| store.stats().ref_events)
    }

    /// Waits until the watchers have delivered what earlier git commands caused and every
    /// check those started has finished.
    async fn settle(&self, cx: &mut VisualTestContext) {
        for _ in 0..2 {
            cx.executor().timer(REF_WATCH_LATENCY * 3).await;
            wait_until(cx, "the rebase checks to finish", |cx| {
                self.harness
                    .store
                    .read_with(cx, |store, _| store.rebase_checks.idle())
            })
            .await;
        }
    }

    async fn until_state(
        &self,
        cx: &mut VisualTestContext,
        id: &str,
        what: &str,
        wanted: Option<RebaseState>,
    ) {
        wait_until(cx, what, |cx| self.state(cx, id) == wanted).await;
    }
}

/// Moves `branch` one empty commit ahead, without touching any checkout. Returns the new head.
fn advance(repository: &Path, branch: &str, message: &str) -> String {
    let tree = git(repository, &["rev-parse", &format!("{branch}^{{tree}}")]);
    let commit = git(
        repository,
        &["commit-tree", &tree, "-p", branch, "-m", message],
    );
    git(
        repository,
        &["update-ref", &format!("refs/heads/{branch}"), &commit],
    );
    commit
}

fn needed(head: String) -> Option<RebaseState> {
    Some(RebaseState::RebaseNeeded { parent_head: head })
}

#[test]
fn only_ref_paths_matter() {
    for path in [
        "refs/heads/alpha",
        "refs/heads/kyle/alpha",
        "refs/remotes/origin/main",
        "refs",
        "reftable/tables.list",
        "packed-refs",
        "HEAD",
        "FETCH_HEAD",
        "ORIG_HEAD",
    ] {
        assert!(names_a_ref(Path::new(path)), "{path} should matter");
    }
    for path in [
        "refs/heads/alpha.lock",
        "packed-refs.lock",
        "reftable/tables.list.lock",
        "HEAD.lock",
        "index",
        "index.lock",
        "objects/ab/cdef",
        "logs/HEAD",
        "worktrees/beta/HEAD",
        "config",
    ] {
        assert!(!names_a_ref(Path::new(path)), "{path} should not matter");
    }
}

#[gpui::test]
async fn a_tree_with_no_stacked_task_watches_nothing(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx, watch_all).await;
    fixture
        .harness
        .store
        .update(cx, |store, cx| store.refresh_rebase(cx));
    fixture.settle(cx).await;
    fixture.harness.store.read_with(cx, |store, _| {
        assert!(store.watched_git_dirs().is_empty());
        assert_eq!(store.stats().rebase_probes, 0);
    });

    // A commit in a listed repository changes nothing either.
    advance(&fixture.invoicing, "main", "main moves");
    fixture.settle(cx).await;
    assert_eq!(fixture.ref_events(cx), 0);
    assert_eq!(fixture.probes(cx), 0);
}

#[gpui::test]
async fn moving_the_parent_flags_the_child_and_moving_the_child_clears_it(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx, watch_all).await;
    let stack = fixture.stack_invoicing(cx, false).await;
    fixture.settle(cx).await;
    assert_eq!(fixture.state(cx, stack.child), None);
    let events = fixture.ref_events(cx);

    let moved = advance(&stack.repository, stack.parent_branch, "alpha moves");
    fixture
        .until_state(cx, stack.child, "the flag", needed(moved.clone()))
        .await;
    assert!(fixture.ref_events(cx) > events);

    // The child now contains the parent's head, as after a rebase.
    git(
        &stack.repository,
        &["update-ref", "refs/heads/beta", &moved],
    );
    fixture
        .until_state(cx, stack.child, "the flag to clear", None)
        .await;
}

#[gpui::test]
async fn two_repositories_changing_together_both_flag(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx, watch_all).await;
    let invoicing = fixture.stack_invoicing(cx, false).await;
    let api = fixture.stack_api(cx).await;
    fixture.settle(cx).await;

    // Both inside one latency window: no timer runs between the two commands.
    let invoicing_head = advance(&invoicing.repository, invoicing.parent_branch, "alpha");
    let api_head = advance(&api.repository, api.parent_branch, "gamma");
    wait_until(cx, "both children to flag", |cx| {
        fixture.state(cx, invoicing.child) == needed(invoicing_head.clone())
            && fixture.state(cx, api.child) == needed(api_head.clone())
    })
    .await;
}

#[gpui::test]
async fn nested_branches_and_packed_refs_still_trigger(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx, watch_all).await;
    let stack = fixture.stack_invoicing(cx, true).await;
    fixture.settle(cx).await;

    let moved = advance(&stack.repository, stack.parent_branch, "kyle/alpha moves");
    fixture
        .until_state(
            cx,
            stack.child,
            "the nested branch's flag",
            needed(moved.clone()),
        )
        .await;
    git(
        &stack.repository,
        &["update-ref", "refs/heads/kyle/beta", &moved],
    );
    fixture
        .until_state(cx, stack.child, "the flag to clear", None)
        .await;
    fixture.settle(cx).await;

    // Packing moves every loose ref into `packed-refs` and prunes `refs/heads/kyle`.
    let events = fixture.ref_events(cx);
    git(&stack.repository, &["pack-refs", "--all"]);
    wait_until(cx, "the pack to be seen", |cx| {
        fixture.ref_events(cx) > events
    })
    .await;
    fixture.settle(cx).await;

    // The branch moves again, its directory made anew.
    let moved_again = advance(&stack.repository, stack.parent_branch, "kyle/alpha again");
    fixture
        .until_state(
            cx,
            stack.child,
            "the flag after packing",
            needed(moved_again),
        )
        .await;
}

#[gpui::test]
async fn idle_minutes_run_no_probe(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx, watch_all).await;
    fixture.stack_invoicing(cx, false).await;
    fixture.settle(cx).await;
    let probes = fixture.probes(cx);
    let events = fixture.ref_events(cx);

    cx.executor().advance_clock(Duration::from_secs(10 * 60));
    cx.run_until_parked();
    fixture.settle(cx).await;
    assert_eq!(fixture.probes(cx), probes);
    assert_eq!(fixture.ref_events(cx), events);
}

#[gpui::test]
async fn a_slow_poll_repository_is_checked_every_five_minutes(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx, poll_api).await;
    let invoicing = fixture.stack_invoicing(cx, false).await;
    let api = fixture.stack_api(cx).await;
    fixture.harness.store.read_with(cx, |store, _| {
        let watched = store.watched_git_dirs();
        assert_eq!(
            watched.get(&invoicing.repository.join(".git")),
            Some(&false)
        );
        assert_eq!(watched.get(&api.repository.join(".git")), Some(&true));
    });
    fixture.settle(cx).await;
    let probes = fixture.probes(cx);
    let events = fixture.ref_events(cx);

    // The api parent moves: no watcher sees it, the next poll does.
    let moved = advance(&api.repository, api.parent_branch, "gamma moves");
    fixture.settle(cx).await;
    assert_eq!(fixture.state(cx, api.child), None);
    assert_eq!(fixture.probes(cx), probes);

    cx.executor().advance_clock(SLOW_POLL_INTERVAL);
    cx.run_until_parked();
    fixture
        .until_state(cx, api.child, "the polled flag", needed(moved))
        .await;
    fixture.settle(cx).await;
    // One probe: api's one stacked task. The watched invoicing ran none.
    assert_eq!(fixture.probes(cx), probes + 1);

    cx.executor().advance_clock(SLOW_POLL_INTERVAL);
    cx.run_until_parked();
    fixture.settle(cx).await;
    assert_eq!(fixture.probes(cx), probes + 2);
    assert_eq!(fixture.ref_events(cx), events);
}

#[gpui::test]
async fn a_repository_check_during_a_full_one_waits_and_wins(cx: &mut TestAppContext) {
    // Every repository on the slow poll, so no watcher event interferes.
    let (fixture, cx) = fixture(cx, poll_all).await;
    let stack = fixture.stack_invoicing(cx, false).await;
    fixture.settle(cx).await;
    assert_eq!(fixture.state(cx, stack.child), None);
    let probes = fixture.probes(cx);

    // The parent moves after the full check probed and before it publishes.
    let moved = Arc::new(Mutex::new(None));
    let repository = stack.repository.clone();
    let common_dir = repository.join(".git");
    fixture.harness.store.update(cx, |store, cx| {
        let moved = moved.clone();
        store.after_full_probe = Some(Box::new(move || {
            *moved.lock() = Some(advance(&repository, "alpha", "alpha moves late"));
        }));
        store.refresh_rebase(cx);
        store.refresh_rebase_for(&common_dir, cx);
        // The repository's check did not cancel the full one; it waits for it.
        assert!(store.rebase_checks.full_running());
        assert!(!store.rebase_checks.repo_running(&common_dir));
        assert!(store.rebase_checks.is_pending(&common_dir));
    });
    fixture.settle(cx).await;

    let moved = moved.lock().clone().expect("the full check probed");
    // The full check saw the old head; the repository's check ran after it and saw the new one,
    // and the full result did not overwrite it.
    assert_eq!(fixture.state(cx, stack.child), needed(moved));
    assert_eq!(fixture.probes(cx), probes + 2);
}

#[gpui::test]
async fn disabling_work_drops_watches_and_timers(cx: &mut TestAppContext) {
    let (fixture, cx) = fixture(cx, poll_api).await;
    let invoicing = fixture.stack_invoicing(cx, false).await;
    let api = fixture.stack_api(cx).await;
    fixture.settle(cx).await;
    assert_eq!(
        fixture
            .harness
            .store
            .read_with(cx, |store, _| store.watched_git_dirs().len()),
        2
    );

    set_enabled(cx, false);
    cx.run_until_parked();
    fixture.harness.store.read_with(cx, |store, _| {
        assert!(store.watched_git_dirs().is_empty());
        assert!(store.rebase_checks.idle());
        assert!(store.rebase_states().is_empty());
    });
    let probes = fixture.probes(cx);
    let events = fixture.ref_events(cx);

    advance(&invoicing.repository, invoicing.parent_branch, "alpha");
    advance(&api.repository, api.parent_branch, "gamma");
    cx.executor().advance_clock(Duration::from_secs(10 * 60));
    cx.run_until_parked();
    fixture.settle(cx).await;
    assert_eq!(fixture.probes(cx), probes);
    assert_eq!(fixture.ref_events(cx), events);
}
