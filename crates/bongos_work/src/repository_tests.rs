//! The `work.repositories` setting, finding a task's repository through its parents, the kind a
//! new task defaults to, and `work: add repository`.

use std::path::{Path, PathBuf};

use bongos_work_model::store::{WorkTree, load};
use bongos_work_model::{Kind, ProjectFile, TaskFile, layout, serialize_project, serialize_task};
use gpui::{TestAppContext, UpdateGlobal as _};
use settings::{Settings as _, SettingsContent, SettingsStore, WorkRepositoryContent, parse_json};
use tempfile::TempDir;

use crate::command_tests::{Input, dispatch, harness, wait_until};
use crate::store_tests::{canonical, write};
use crate::{Repository, WorkSettings, find_repository, merge_is_ship, resolve_repository};

fn repository(name: &str, deployments: bool) -> Repository {
    Repository {
        name: name.to_string(),
        path: format!("~/src/{name}"),
        setup_command: None,
        merge_is_ship: true,
        deployments,
        branch_template: "{slug}".to_string(),
        worktree_root: None,
    }
}

fn task(dir: &Path, id: &str, repository: Option<&str>) {
    let mut file = TaskFile::new(id, Kind::PrShip);
    file.id = id.to_string();
    file.repository = repository.map(str::to_string);
    write(&layout::task_file(dir), &serialize_task(&file).unwrap());
}

fn project(root: &Path, slug: &str, repositories: &[&str]) -> PathBuf {
    let dir = layout::project_dir(root, slug);
    let mut file = ProjectFile::new(slug);
    file.repositories = repositories.iter().map(|name| name.to_string()).collect();
    write(
        &layout::project_file(&dir),
        &serialize_project(&file).unwrap(),
    );
    task(&dir, &format!("{slug}-top"), None);
    dir
}

/// Project `p` (repository `proj`) holds the group `g` (repository `grp`), which holds `inherit`
/// (none), `own` (repository `own`), `unknown` (repository `nope`) and the group `sub` (none)
/// with the leaf `deep` (none); `plain` (none) is directly under the project. Project `q`
/// names two repositories and holds `multi` (none).
fn tree() -> (TempDir, WorkTree) {
    let temp = TempDir::new().unwrap();
    let root = canonical(&temp).join("work");
    let p = project(&root, "p", &["proj"]);
    let g = layout::child_dir(&p, "g");
    task(&g, "g", Some("grp"));
    task(&layout::child_dir(&g, "inherit"), "inherit", None);
    task(&layout::child_dir(&g, "own"), "own", Some("own"));
    task(&layout::child_dir(&g, "unknown"), "unknown", Some("nope"));
    let sub = layout::child_dir(&g, "sub");
    task(&sub, "sub", None);
    task(&layout::child_dir(&sub, "deep"), "deep", None);
    task(&layout::child_dir(&p, "plain"), "plain", None);
    let q = project(&root, "q", &["a", "b"]);
    task(&layout::child_dir(&q, "multi"), "multi", None);
    let tree = load(&root);
    assert!(tree.errors.is_empty(), "{:?}", tree.errors);
    (temp, tree)
}

#[test]
fn repositories_resolve_from_the_task_then_the_nearest_ancestor_then_the_project() {
    let (_temp, tree) = tree();
    let repositories: Vec<Repository> = ["proj", "grp", "own", "a", "b"]
        .into_iter()
        .map(|name| repository(name, true))
        .collect();
    let name = |id: &str| {
        resolve_repository(&tree, id, &repositories).map(|repository| repository.name.as_str())
    };
    // Its own field wins over everything above.
    assert_eq!(name("own"), Some("own"));
    // Else the nearest ancestor that names one, however far up.
    assert_eq!(name("inherit"), Some("grp"));
    assert_eq!(name("sub"), Some("grp"));
    assert_eq!(name("deep"), Some("grp"));
    assert_eq!(name("g"), Some("grp"));
    // Else the project's, when it names exactly one.
    assert_eq!(name("plain"), Some("proj"));
    assert_eq!(name("p-top"), Some("proj"));
    // Else none: a project with two repositories decides nothing.
    assert_eq!(name("multi"), None);
    // A name that is not in the settings list, and a task that does not exist, resolve to none.
    assert_eq!(name("unknown"), None);
    assert_eq!(name("no-such-task"), None);
    // The list decides what is found.
    assert_eq!(resolve_repository(&tree, "deep", &repositories[..1]), None);
    assert_eq!(find_repository(&repositories, "own").unwrap().name, "own");
}

#[test]
fn entries_have_the_documented_defaults() {
    let text = r#"{ "work": { "repositories": [
        { "name": "invoicing", "path": "~/src/invoicing" },
        { "name": "site", "path": "/srv/site", "setup_command": "npm ci",
          "merge_is_ship": false, "deployments": false, "branch_template": "kyle/{slug}" }
    ] } }"#;
    let (content, _) = parse_json::<SettingsContent>(text);
    let entries = content.unwrap().work.unwrap().repositories.unwrap();
    let invoicing = Repository::from(&entries[0]);
    assert_eq!(
        invoicing,
        Repository {
            name: "invoicing".to_string(),
            path: "~/src/invoicing".to_string(),
            setup_command: None,
            merge_is_ship: true,
            deployments: true,
            branch_template: "{slug}".to_string(),
            worktree_root: None,
        }
    );
    let site = Repository::from(&entries[1]);
    assert_eq!(site.setup_command.as_deref(), Some("npm ci"));
    assert!(!site.merge_is_ship && !site.deployments);
    assert_eq!(site.branch_template, "kyle/{slug}");
}

#[gpui::test]
async fn the_defaults_list_no_repository_and_dock_the_panel_left(cx: &mut TestAppContext) {
    let (_harness, cx) = harness(cx, true, false).await;
    cx.update(|_, cx| {
        let settings = WorkSettings::get_global(cx);
        assert_eq!(settings.dock, settings::DockPosition::Left);
        assert_eq!(settings.default_width, 320.0);
        assert_eq!(settings.fade_after_days, 1.5);
        assert_eq!(settings.stale_after_days, 3.0);
        assert!(settings.repositories.is_empty());
    });
}

fn set_repositories(cx: &mut gpui::VisualTestContext, deployments: Option<bool>) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().repositories =
                    Some(vec![WorkRepositoryContent {
                        name: "invoicing".to_string(),
                        path: "~/src/invoicing".to_string(),
                        setup_command: None,
                        merge_is_ship: None,
                        deployments,
                        branch_template: None,
                        worktree_root: None,
                    }]);
            });
        });
    });
}

#[gpui::test]
async fn a_task_in_a_repository_without_deployments_defaults_to_pr_merge(cx: &mut TestAppContext) {
    // The seeded project `invoicing` names the repository `invoicing`.
    let (harness, cx) = harness(cx, true, true).await;
    set_repositories(cx, Some(false));
    harness
        .run_and_wait(
            cx,
            "task::NewChild",
            &[Input::Pick("phase"), Input::Text("Gamma")],
        )
        .await;
    assert_eq!(harness.file(cx, &["phase", "gamma"]).kind, Kind::PrMerge);
}

#[gpui::test]
async fn deployments_and_unlisted_repositories_keep_the_default_kind(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    // Not in the list at all.
    harness
        .run_and_wait(
            cx,
            "task::NewChild",
            &[Input::Pick("phase"), Input::Text("Gamma")],
        )
        .await;
    assert_eq!(harness.file(cx, &["phase", "gamma"]).kind, Kind::PrShip);

    // Listed, with deployments left at their default.
    set_repositories(cx, None);
    harness
        .run_and_wait(
            cx,
            "task::NewChild",
            &[Input::Pick("phase"), Input::Text("Delta")],
        )
        .await;
    assert_eq!(harness.file(cx, &["phase", "delta"]).kind, Kind::PrShip);

    // A one-off belongs to no repository.
    set_repositories(cx, Some(false));
    harness
        .run_and_wait(cx, "task::New", &[Input::Text("Proration spec")])
        .await;
    let tree = harness.tree(cx);
    let spec = crate::command_tests::find(&tree, "Proration spec");
    assert_eq!(spec.file.as_ref().unwrap().kind, Kind::Spike);
}

#[gpui::test]
async fn add_repository_is_refused_while_work_is_off(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, false, false).await;
    dispatch(cx, "work::AddRepository");
    assert!(harness.has_toast(cx));
    assert!(!cx.did_prompt_for_paths());
}

#[gpui::test]
async fn add_repository_picks_a_folder_asks_and_writes_the_settings(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, false).await;
    dispatch(cx, "work::AddRepository");
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|options| {
        assert!(options.directories && !options.files && !options.multiple);
        Some(vec![PathBuf::from("/srv/billing-service")])
    });
    cx.run_until_parked();

    // The name starts as the folder's.
    wait_until(cx, "the name prompt", |cx| harness.prompt(cx).is_some()).await;
    let prompt = harness.prompt(cx).unwrap();
    let suggestion = prompt.update_in(cx, |prompt, _, cx| prompt.editor.read(cx).text(cx));
    assert_eq!(suggestion, "billing-service");
    harness.answer(cx, &Input::Text("billing")).await;
    harness.answer(cx, &Input::Text("npm ci")).await;

    wait_until(cx, "the repository in the settings", |cx| {
        cx.update(|_, cx| !WorkSettings::get_global(cx).repositories.is_empty())
    })
    .await;
    cx.update(|_, cx| {
        let repositories = &WorkSettings::get_global(cx).repositories;
        assert_eq!(repositories.len(), 1);
        let added = &repositories[0];
        assert_eq!(added.name, "billing");
        assert_eq!(added.path, "/srv/billing-service");
        assert_eq!(added.setup_command.as_deref(), Some("npm ci"));
        assert!(added.merge_is_ship && added.deployments);
        assert_eq!(added.branch_template, "{slug}");
    });
    assert!(!harness.has_toast(cx));
}

#[gpui::test]
async fn add_repository_refuses_a_name_in_use_and_cancelling_adds_nothing(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, false).await;
    set_repositories(cx, None);

    dispatch(cx, "work::AddRepository");
    cx.simulate_path_prompt_response(|_| Some(vec![PathBuf::from("/srv/other")]));
    cx.run_until_parked();
    harness.answer(cx, &Input::Text("invoicing")).await;
    assert!(harness.has_toast(cx));
    assert!(harness.prompt(cx).is_none(), "no setup command is asked");
    cx.update(|_, cx| assert_eq!(WorkSettings::get_global(cx).repositories.len(), 1));

    harness.clear_toasts(cx);
    dispatch(cx, "work::AddRepository");
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
    assert!(harness.prompt(cx).is_none());
    assert!(!harness.has_toast(cx));
}

fn set_merge_is_ship(cx: &mut gpui::VisualTestContext, value: Option<bool>) {
    cx.update(|_, cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |content| {
                content.work.get_or_insert_default().repositories =
                    Some(vec![WorkRepositoryContent {
                        name: "invoicing".to_string(),
                        path: "~/src/invoicing".to_string(),
                        setup_command: None,
                        merge_is_ship: value,
                        deployments: None,
                        branch_template: None,
                        worktree_root: None,
                    }]);
            });
        });
    });
}

#[gpui::test]
async fn merge_is_ship_is_the_task_override_then_the_repository_then_true(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let task = harness.tree(cx).task_by_id("l1").unwrap().clone();
    let mut overridden = task.clone();
    let ask = |task: &bongos_work_model::store::Task, cx: &mut gpui::VisualTestContext| {
        cx.update(|_, cx| merge_is_ship(task, cx))
    };

    // No repository listed: the default.
    assert!(ask(&task, cx));

    // The repository the project names says false; true with no value.
    set_merge_is_ship(cx, Some(false));
    assert!(!ask(&task, cx));
    set_merge_is_ship(cx, None);
    assert!(ask(&task, cx));

    // The task's own setting wins over the repository's, both ways.
    set_merge_is_ship(cx, Some(false));
    overridden.file.as_mut().unwrap().merge_is_ship = Some(true);
    assert!(ask(&overridden, cx));
    set_merge_is_ship(cx, Some(true));
    overridden.file.as_mut().unwrap().merge_is_ship = Some(false);
    assert!(!ask(&overridden, cx));
}
