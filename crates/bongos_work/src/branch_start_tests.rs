//! Worktree roots a registered resolver decides, ahead of the settings, and the branch-only start
//! a lane uses for a leaf it runs in its own tree. Real git in temporary directories.

use std::path::Path;
use std::sync::Arc;

use bongos_work_model::commands::{self as model, Write};
use bongos_work_model::fixtures::ctx;
use bongos_work_model::store::load;
use bongos_work_model::worktree_path::Source;
use bongos_work_model::{Kind, layout};
use fs::RealFs;
use gpui::{App, TestAppContext, VisualTestContext};
use parking_lot::Mutex;
use tempfile::TempDir;

use crate::command_tests::{Input, seed, wait_until};
use crate::worktree_tests::{
    central, git, init_repository, set_repository_root, setup, test_layout, test_repository,
};
use crate::worktrees::{Plan, branch_start, make_branch, plan, plan_with, writable_spot};
use crate::{WorktreeRootResolver, WorktreeRootResolvers};

/// A resolver that answers every repository with `answer` and records what it was asked.
struct FakeResolver {
    answer: Option<(String, Source)>,
    asked: Mutex<Vec<(String, String)>>,
}

impl FakeResolver {
    fn new(answer: Option<(String, Source)>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: Mutex::new(Vec::new()),
        })
    }
}

impl WorktreeRootResolver for FakeResolver {
    fn root(&self, project_id: &str, repo: &str, _cx: &App) -> Option<(String, Source)> {
        self.asked
            .lock()
            .push((project_id.to_string(), repo.to_string()));
        self.answer.clone()
    }
}

fn profile(name: &str) -> Source {
    Source::Profile {
        name: name.to_string(),
    }
}

fn register(cx: &mut VisualTestContext, resolver: Arc<FakeResolver>) {
    cx.update(|_, cx| WorktreeRootResolvers::register(resolver, cx));
}

/// A regular file: no directory can be made below it, whoever runs the test.
fn blocked(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("blocked");
    std::fs::write(&path, "").unwrap();
    path
}

#[gpui::test]
async fn a_profile_root_from_a_resolver_holds_the_hand_started_worktree(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let trees = setup.real.parent().unwrap().join("profile-trees");
    let resolver = FakeResolver::new(Some((
        format!("{}/{{repo}}/{{slug}}", trees.display()),
        profile("fast-disk"),
    )));
    register(cx, resolver.clone());

    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    let worktree = trees.join("invoicing").join("backfill");
    assert_eq!(
        harness.file(cx, &["backfill"]).worktree,
        Some(worktree.to_string_lossy().into_owned())
    );
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "backfill");
    assert!(!central(harness, "backfill").exists());
    // Asked with the project's id (its top group's) and the repository's name.
    assert_eq!(
        resolver.asked.lock().first().cloned(),
        Some(("g1".to_string(), "invoicing".to_string()))
    );
}

#[gpui::test]
async fn an_unwritable_profile_root_refuses_the_start_with_no_fallback(cx: &mut TestAppContext) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    let blocked = blocked(setup.real.parent().unwrap());
    register(
        cx,
        FakeResolver::new(Some((
            format!("{}/{{slug}}", blocked.display()),
            profile("fast-disk"),
        ))),
    );

    let writes = harness.writes(cx);
    harness.clear_toasts(cx);
    harness
        .run(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    wait_until(cx, "the refusal", |cx| harness.has_toast(cx)).await;
    assert_eq!(harness.writes(cx), writes);
    assert_eq!(harness.file(cx, &["backfill"]).worktree, None);
    assert_eq!(git(&setup.real, &["branch", "--list", "backfill"]), "");
    assert!(!central(harness, "backfill").exists());
    assert!(
        !central(harness, "backfill").parent().unwrap().exists(),
        "nothing central was made"
    );
}

#[gpui::test]
async fn with_no_resolver_answer_the_settings_and_the_central_fallback_apply(
    cx: &mut TestAppContext,
) {
    let (setup, cx) = setup(cx, None).await;
    let harness = &setup.harness;
    // Lanes off: the resolver answers nothing, so the repository's own root decides, and an
    // unwritable one falls back to the central directory as it always did.
    let resolver = FakeResolver::new(None);
    register(cx, resolver.clone());
    let blocked = blocked(setup.real.parent().unwrap());
    set_repository_root(
        cx,
        &setup.repository,
        None,
        Some(&format!("{}/{{slug}}", blocked.display())),
    );

    harness
        .run_and_wait(cx, "task::Start", &[Input::Pick("backfill")])
        .await;
    let file = harness.file(cx, &["backfill"]);
    let worktree = central(harness, "backfill");
    assert_eq!(file.worktree, Some(worktree.to_string_lossy().into_owned()));
    assert_eq!(
        file.history.last().unwrap().event,
        "worktree created in the central directory"
    );
    assert!(!resolver.asked.lock().is_empty(), "the resolver was asked");
}

#[test]
fn a_resolver_root_names_its_layer_and_never_falls_back() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("work");
    seed(&root);
    let tree = load(&root);
    let repositories = [test_repository("invoicing", "/src/invoicing", None)];
    let layout = test_layout(&temp.path().join("data"));
    let blocked = blocked(temp.path());
    let template = format!("{}/{{slug}}", blocked.display());

    for (source, layer) in [
        (profile("fast-disk"), "by project profile fast-disk"),
        (Source::Project, "by the project"),
        (Source::Lane, "by the lane binding"),
    ] {
        let answer = (template.clone(), source.clone());
        let planned = plan_with(&tree, "l1", &repositories, &layout, &|_, _| {
            Some(answer.clone())
        });
        let Ok(Plan::Ready(request)) = planned else {
            panic!("expected a ready plan");
        };
        assert_eq!(request.source(), &source);
        assert_eq!(request.worktree(), blocked.join("backfill"));
        let error = writable_spot(&request, || blocked.to_string_lossy().into_owned())
            .unwrap_err()
            .to_string();
        assert!(error.contains(&*blocked.to_string_lossy()), "{error}");
        assert!(error.contains(layer), "{error}");
        assert!(error.ends_with("nothing was created"), "{error}");
        assert!(!temp.path().join("data").exists(), "nothing central was made");
    }

    // A bad template from a resolver names the root and its layer.
    let planned = plan_with(&tree, "l1", &repositories, &layout, &|_, _| {
        Some(("/trees/{user}".to_string(), profile("fast-disk")))
    });
    let Err(message) = planned else {
        panic!("expected a refusal");
    };
    assert!(
        message.starts_with("the worktree root /trees/{user} of invoicing (from project profile fast-disk layer)"),
        "{message}"
    );
}

#[gpui::test]
async fn a_branch_only_start_cuts_the_branch_records_it_and_makes_no_worktree(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    let temp = TempDir::new().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let repository = base.join("invoicing");
    init_repository(&repository);
    let main = git(&repository, &["rev-parse", "main"]);
    let root = base.join("work");
    seed(&root);
    let tree = load(&root);
    let repository_text = repository.to_string_lossy().into_owned();
    let repositories = [test_repository("invoicing", &repository_text, None)];
    let layout = test_layout(&base.join("data"));

    let start = branch_start(plan(&tree, "l1", &repositories, &layout)).unwrap();
    assert_eq!(start.branch(), "backfill");
    let fs = Arc::new(RealFs::new(None, cx.executor()));
    let started = make_branch(fs.clone(), &start).await.unwrap();
    assert_eq!(started.branch, "backfill");
    assert_eq!(started.base, "main");
    assert_eq!(started.base_sha, main);
    assert_eq!(git(&repository, &["rev-parse", "refs/heads/backfill"]), main);
    // No worktree, and the checkout is still on main and clean.
    let worktrees = git(&repository, &["worktree", "list", "--porcelain"]);
    assert_eq!(worktrees.lines().filter(|l| l.starts_with("worktree ")).count(), 1);
    assert_eq!(git(&repository, &["branch", "--show-current"]), "main");
    assert_eq!(git(&repository, &["status", "--porcelain"]), "");
    assert!(!base.join("data").exists());

    // The branch and its base are recorded, with no worktree.
    let change = model::record_branch(&tree, &ctx(), "l1", started.clone()).unwrap();
    let Some(Write::Task { file, .. }) = change.writes.first() else {
        panic!("expected the task's own write first");
    };
    assert_eq!(file.branch.as_deref(), Some("backfill"));
    assert_eq!(file.base.as_deref(), Some("main"));
    assert_eq!(file.base_sha.as_deref(), Some(main.as_str()));
    assert_eq!(file.worktree, None);
    assert!(file.todos[0].started.is_some());
    model::apply(&root, &change).unwrap();
    let tree = load(&root);

    // A second branch-only start of the same leaf is refused and changes nothing.
    assert!(model::record_branch(&tree, &ctx(), "l1", started).is_err());
    let again = make_branch(fs, &start).await.unwrap_err().to_string();
    assert!(again.contains("already exists"), "{again}");
    assert_eq!(git(&repository, &["rev-parse", "refs/heads/backfill"]), main);

    // A babysit leaf starts on a PR's head, so it is refused here.
    let babysit = layout::child_dir(&layout::project_dir(&root, "invoicing"), "watch");
    crate::worktree_tests::leaf(&babysit, "l9", Kind::Babysit, |_| {});
    let tree = load(&root);
    assert!(branch_start(plan(&tree, "l9", &repositories, &layout)).is_err());
}
