use std::path::Path;

use fs::FakeFs;
use futures::StreamExt;
use gpui::TestAppContext;
use language::{CodeLabel, FakeLspAdapter, HighlightId, rust_lang};
use lsp::Uri;
use project::{Project, lsp_store::*};
use serde_json::json;
use util::path;

use crate::init_test;

/// Download progress a delegate reports reaches `LspStore` subscribers, and
/// `cancel_server_download` sets the flag of the download that started last.
#[gpui::test]
async fn test_server_download_progress_and_cancel(cx: &mut TestAppContext) {
    use language::LspAdapterDelegate;
    use lsp::LanguageServerName;
    use std::sync::atomic::Ordering;

    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;
    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let lsp_store = project.read_with(cx, |project, _| project.lsp_store());
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let _subscription = cx.update(|cx| {
        let events = events.clone();
        cx.subscribe(&lsp_store, move |_, event, _| {
            if let LspStoreEvent::LanguageServerDownloadProgress {
                name,
                worktree_id,
                done,
                total,
            } = event
            {
                events
                    .borrow_mut()
                    .push((name.clone(), *worktree_id, *done, *total));
            }
        })
    });
    let (delegate, worktree_id) = lsp_store.update(cx, |lsp_store, cx| {
        let local = lsp_store.as_local().unwrap();
        let worktree = local.worktree_store.read(cx).worktrees().next().unwrap();
        let worktree_id = worktree.read(cx).id();
        (
            LocalLspAdapterDelegate::from_local_lsp(local, &worktree, cx),
            worktree_id,
        )
    });
    let server = LanguageServerName::new_static("downloaded-server");

    delegate.report_download_progress(server.clone(), 0, Some(300));
    delegate.report_download_progress(server.clone(), 300, Some(300));
    delegate.report_download_progress(server.clone(), 7, None);
    cx.run_until_parked();
    assert_eq!(
        *events.borrow(),
        vec![
            (server.clone(), worktree_id, 0, Some(300)),
            (server.clone(), worktree_id, 300, Some(300)),
            (server.clone(), worktree_id, 7, None),
        ]
    );

    assert!(
        !lsp_store.read_with(cx, |lsp_store, _| lsp_store.cancel_server_download(&server)),
        "no download of the server started yet"
    );
    let first = delegate.download_cancel_flag(server.clone());
    assert!(lsp_store.read_with(cx, |lsp_store, _| lsp_store.cancel_server_download(&server)));
    assert!(first.load(Ordering::SeqCst));
    let second = delegate.download_cancel_flag(server.clone());
    assert!(
        !second.load(Ordering::SeqCst),
        "a new download starts uncancelled"
    );
    let other = delegate.download_cancel_flag(LanguageServerName::new_static("other-server"));
    lsp_store.read_with(cx, |lsp_store, _| lsp_store.cancel_server_download(&server));
    assert!(second.load(Ordering::SeqCst));
    assert!(
        !other.load(Ordering::SeqCst),
        "a cancel stops only its own server"
    );
}

#[gpui::test]
async fn test_removing_invisible_worktree_cleans_reused_lsp_bookkeeping(cx: &mut TestAppContext) {
    init_test(cx);
    cx.executor().allow_parking();

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/the-root"), json!({ "main.rs": "fn main() {}" }))
        .await;
    fs.insert_tree(
        path!("/the-registry"),
        json!({ "dep": { "src": { "dep.rs": "pub fn dep() {}" } } }),
    )
    .await;

    let project = Project::test(fs, [path!("/the-root").as_ref()], cx).await;
    let language_registry = project.read_with(cx, |project, _| project.languages().clone());
    language_registry.add(rust_lang());
    let mut fake_servers = language_registry.register_fake_lsp("Rust", FakeLspAdapter::default());

    let (_visible_buffer, _visible_handle) = project
        .update(cx, |project, cx| {
            project.open_local_buffer_with_lsp(path!("/the-root/main.rs"), cx)
        })
        .await
        .unwrap();
    fake_servers.next().await.unwrap();
    cx.run_until_parked();

    let server_id = project.read_with(cx, |project, cx| {
        project
            .lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .unwrap()
            .0
    });
    let external_buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer_via_lsp(
                Uri::from_file_path(path!("/the-registry/dep/src/dep.rs")).unwrap(),
                server_id,
                cx,
            )
        })
        .await
        .unwrap();
    cx.run_until_parked();

    let invisible_worktree_id =
        external_buffer.read_with(cx, |buffer, cx| buffer.file().unwrap().worktree_id(cx));
    project.read_with(cx, |project, cx| {
        let worktree = project.worktree_for_id(invisible_worktree_id, cx).unwrap();
        assert!(!worktree.read(cx).is_visible());
        assert!(
            project
                .lsp_store()
                .read(cx)
                .has_language_server_seed_for_worktree(invisible_worktree_id)
        );
    });

    project.update(cx, |project, cx| {
        project.remove_worktree(invisible_worktree_id, cx);
    });
    cx.run_until_parked();

    project.read_with(cx, |project, cx| {
        let lsp_store = project.lsp_store();
        let lsp_store = lsp_store.read(cx);
        assert!(
            lsp_store
                .language_server_statuses()
                .any(|(status_server_id, _)| status_server_id == server_id)
        );
        assert!(!lsp_store.has_language_server_seed_for_worktree(invisible_worktree_id));
    });
}

#[test]
fn test_glob_literal_prefix() {
    assert_eq!(glob_literal_prefix(Path::new("**/*.js")), Path::new(""));
    assert_eq!(
        glob_literal_prefix(Path::new("node_modules/**/*.js")),
        Path::new("node_modules")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
        Path::new("foo")
    );
    assert_eq!(
        glob_literal_prefix(Path::new("foo/bar/baz.js")),
        Path::new("foo/bar/baz.js")
    );

    #[cfg(target_os = "windows")]
    {
        assert_eq!(glob_literal_prefix(Path::new("**\\*.js")), Path::new(""));
        assert_eq!(
            glob_literal_prefix(Path::new("node_modules\\**/*.js")),
            Path::new("node_modules")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo/{bar,baz}.js")),
            Path::new("foo")
        );
        assert_eq!(
            glob_literal_prefix(Path::new("foo\\bar\\baz.js")),
            Path::new("foo/bar/baz.js")
        );
    }
}

#[test]
fn test_multi_len_chars_normalization() {
    let mut label = CodeLabel::new(
        "myElˇ (parameter) myElˇ: {\n    foo: string;\n}".to_string(),
        0..6,
        vec![(0..6, HighlightId::new(1))],
    );
    ensure_uniform_list_compatible_label(&mut label);
    assert_eq!(
        label,
        CodeLabel::new(
            "myElˇ (parameter) myElˇ: { foo: string; }".to_string(),
            0..6,
            vec![(0..6, HighlightId::new(1))],
        )
    );
}

#[test]
fn test_trailing_newline_in_completion_documentation() {
    let doc =
        lsp::Documentation::String("Inappropriate argument value (of correct type).\n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(
        matches!(completion_doc, CompletionDocumentation::SingleLine(s) if s == "Inappropriate argument value (of correct type).")
    );

    let doc = lsp::Documentation::String("  some value  \n".to_string());
    let completion_doc: CompletionDocumentation = doc.into();
    assert!(matches!(
        completion_doc,
        CompletionDocumentation::SingleLine(s) if s == "some value"
    ));
}
