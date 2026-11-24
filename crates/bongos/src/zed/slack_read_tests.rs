//! The Slack read actions in the whole app: each has a handler on the workspace, so running one
//! by name while `slack.enabled` is false says Slack is off instead of doing nothing.

use gpui::TestAppContext;
use util::path;
use workspace::MultiWorkspace;

use super::tests::init_test;

#[gpui::test]
async fn read_channel_load_older_and_read_thread_have_handlers(cx: &mut TestAppContext) {
    let app_state = init_test(cx);
    cx.update(bongos_slack::init);
    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), serde_json::json!({ "a": null }))
        .await;
    cx.update(|cx| {
        workspace::open_paths(
            &[path!("/root/a").into()],
            app_state.clone(),
            workspace::OpenOptions::default(),
            cx,
        )
    })
    .await
    .unwrap();
    let multi_workspace = cx.windows()[0].downcast::<MultiWorkspace>().unwrap();

    for name in [
        "slack::ReadChannel",
        "slack::LoadOlderMessages",
        "slack::ReadThread",
    ] {
        let available = multi_workspace
            .update(cx, |_, window, cx| {
                let action = cx.build_action(name, None).unwrap();
                window.is_action_available(action.as_ref(), cx)
            })
            .unwrap();
        assert!(available, "{name} has a handler");
    }
}
