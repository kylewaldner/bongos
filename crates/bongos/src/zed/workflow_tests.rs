//! The workflow actions in the whole app: with `workflows.enabled` false (the default) a run
//! started the way `task: mode` starts it, by the name `workflow::Run`, finds a handler and says
//! that workflows are off, instead of "Workflows arrive with feature 20".

use gpui::TestAppContext;
use serde_json::json;
use util::path;
use workspace::MultiWorkspace;
use workspace::notifications::NotificationId;

use super::tests::init_test;

#[gpui::test]
async fn run_by_name_is_handled_and_says_workflows_are_off(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    cx.update(bongos_workflow::init);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({ "a": null }))
        .await;
    cx.update(|cx| {
        crate::zed::open_paths(
            &[path!("/root/a").into()],
            app_state.clone(),
            workspace::OpenOptions::default(),
            cx,
        )
    })
    .await
    .unwrap();

    let multi_workspace = cx.windows()[0].downcast::<MultiWorkspace>().unwrap();
    let workspace = multi_workspace
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let id = NotificationId::unique::<bongos_workflow::WorkflowToast>();
    assert!(!workspace.read_with(cx, |workspace, _| {
        workspace.notification_ids().contains(&id)
    }));

    // The two checks `task: mode` makes before it would show "Workflows arrive with feature 20".
    let available = multi_workspace
        .update(cx, |_, window, cx| {
            let action = cx.build_action("workflow::Run", None).unwrap();
            let available = window.is_action_available(action.as_ref(), cx);
            window.dispatch_action(action, cx);
            available
        })
        .unwrap();
    cx.run_until_parked();
    assert!(available, "workflow::Run has a handler");
    assert!(workspace.read_with(cx, |workspace, _| {
        workspace.notification_ids().contains(&id)
    }));
}
