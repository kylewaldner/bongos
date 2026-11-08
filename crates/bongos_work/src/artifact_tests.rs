//! The artifact commands dispatched by name, `task: think`, and the debug dump of the session
//! context. The model's own tests hold the leak checks of the shared set; these check that the
//! commands reach it.

use std::path::{Path, PathBuf};

use bongos_work_model::{Visibility, layout, read_artifacts, read_link, shared_artifacts};
use editor::Editor;
use gpui::{TestAppContext, VisualTestContext};
use tempfile::TempDir;

#[cfg(debug_assertions)]
use crate::artifacts::{DUMP_FILE, write_session_context};
use crate::command_tests::{Harness, Input, dispatch, harness, wait_until};
use crate::store_tests::write;

const SECRET: &str = "notes/salary-review.md";
const DESIGN: &str = "docs/design.md";

fn shared(dir: &Path) -> Vec<PathBuf> {
    shared_artifacts(dir)
        .into_iter()
        .map(|artifact| artifact.path)
        .collect()
}

/// Writes two artifacts into `backfill` and waits for the store to see them.
async fn with_artifacts(harness: &Harness, cx: &mut VisualTestContext) -> PathBuf {
    let backfill = harness.task_dir(&["backfill"]);
    write(&backfill.join(SECRET), "numbers\n");
    write(&backfill.join(DESIGN), "design\n");
    wait_until(cx, "the store to see the artifacts", |cx| {
        harness.tree(cx).task_by_dir(&backfill).is_some_and(|task| {
            task.files.contains_key(Path::new(SECRET)) && task.files.contains_key(Path::new(DESIGN))
        })
    })
    .await;
    backfill
}

#[gpui::test]
async fn artifact_commands_by_name(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let backfill = with_artifacts(&harness, cx).await;
    let secret = backfill.join(SECRET);
    let design = backfill.join(DESIGN);
    assert_eq!(shared(&backfill), [design.clone(), secret.clone()]);

    harness
        .run_and_wait(
            cx,
            "artifact::Hide",
            &[Input::Pick("backfill"), Input::Pick("salary")],
        )
        .await;
    assert_eq!(shared(&backfill), vec![design.clone()]);
    let manifest = read_artifacts(&layout::artifacts_file(&backfill)).unwrap();
    assert_eq!(manifest.artifacts.len(), 2);
    // Hiding never touches `task.toml`.
    assert!(harness.file(cx, &["backfill"]).history.is_empty());

    harness
        .run_and_wait(
            cx,
            "artifact::Share",
            &[Input::Pick("backfill"), Input::Pick("salary")],
        )
        .await;
    assert_eq!(shared(&backfill), [design.clone(), secret.clone()]);

    harness
        .run_and_wait(cx, "artifact::HideAll", &[Input::Pick("backfill")])
        .await;
    assert!(shared(&backfill).is_empty());
    harness
        .run_and_wait(cx, "artifact::ShareAll", &[Input::Pick("backfill")])
        .await;
    assert_eq!(shared(&backfill), [design.clone(), secret.clone()]);

    harness
        .run_and_wait(
            cx,
            "artifact::Link",
            &[
                Input::Pick("backfill"),
                Input::Text("https://notion.so/acme/Spec-1"),
                Input::Text("Billing spec"),
            ],
        )
        .await;
    let link = read_link(&backfill.join("billing-spec.link.toml")).unwrap();
    assert_eq!(link.url, "https://notion.so/acme/Spec-1");

    harness
        .run_and_wait(
            cx,
            "task::SetDefaultVisibility",
            &[Input::Pick("backfill"), Input::Pick("human")],
        )
        .await;
    assert_eq!(
        harness.file(cx, &["backfill"]).default_visibility,
        Visibility::HumanOnly
    );
    // A file an agent adds now stays out of the shared set.
    write(&backfill.join("notes/scratch.md"), "scratch\n");
    assert!(!shared(&backfill).contains(&backfill.join("notes/scratch.md")));
    assert!(!harness.has_toast(cx));
}

#[gpui::test]
async fn hiding_with_nothing_to_hide_shows_a_toast(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, false).await;
    for name in ["artifact::Hide", "artifact::Share", "artifact::HideAll"] {
        harness.clear_toasts(cx);
        dispatch(cx, name);
        assert!(harness.picker(cx).is_none(), "{name} opened a picker");
        assert!(harness.has_toast(cx), "{name} showed no toast");
    }
}

fn active_path(harness: &Harness, cx: &mut VisualTestContext) -> Option<PathBuf> {
    harness.workspace.read_with(cx, |workspace, cx| {
        let item = workspace.active_item(cx)?;
        item.downcast::<Editor>()?;
        let project_path = item.project_path(cx)?;
        harness.project.read(cx).absolute_path(&project_path, cx)
    })
}

#[gpui::test]
async fn think_opens_a_new_document_and_starts_nothing(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let backfill = harness.task_dir(&["backfill"]);
    let fs = harness
        .workspace
        .read_with(cx, |workspace, _| workspace.app_state().fs.clone());
    let writes = harness.writes(cx);

    for number in 1..=2 {
        harness
            .run(cx, "task::Think", &[Input::Pick("backfill")])
            .await;
        let path = backfill.join(format!("untitled-{number}.md"));
        wait_until(cx, "the new document", |cx| {
            active_path(&harness, cx).as_ref() == Some(&path)
        })
        .await;
        assert!(fs.is_file(&path).await, "{} was created", path.display());
    }

    // Two editors and nothing else: no terminal, chat or other item, no modal, no toast, and no
    // write to the work tree.
    let items = harness.workspace.read_with(cx, |workspace, cx| {
        workspace
            .items(cx)
            .map(|item| item.downcast::<Editor>().is_some())
            .collect::<Vec<_>>()
    });
    assert_eq!(items, [true, true]);
    assert!(harness.picker(cx).is_none() && harness.prompt(cx).is_none());
    assert!(!harness.has_toast(cx));
    assert_eq!(harness.writes(cx), writes);
    let terminals = harness
        .project
        .read_with(cx, |project, _| project.local_terminal_handles().len());
    assert_eq!(terminals, 0);
}

#[cfg(debug_assertions)]
#[gpui::test]
async fn the_session_context_dump_leaves_hidden_artifacts_out(cx: &mut TestAppContext) {
    let (harness, cx) = harness(cx, true, true).await;
    let backfill = with_artifacts(&harness, cx).await;
    harness
        .run_and_wait(
            cx,
            "artifact::Hide",
            &[Input::Pick("backfill"), Input::Pick("salary")],
        )
        .await;

    let logs = TempDir::new().unwrap();
    let path = write_session_context(&backfill, logs.path()).unwrap();
    assert_eq!(path, logs.path().join(DUMP_FILE));
    let json = std::fs::read_to_string(&path).unwrap();
    assert!(json.contains("design.md"), "{json}");
    assert!(!json.contains("salary"), "{json}");
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["goal"], "Backfill");

    // With no task focused, the action says so and writes nothing.
    harness.clear_toasts(cx);
    dispatch(cx, "work::DumpSessionContext");
    assert!(harness.has_toast(cx));
}
