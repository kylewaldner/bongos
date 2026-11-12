use crate::{
    ClipboardSelection, Copy, Cut, KillRingCut, KillRingYank, Paste,
    editor_tests::init_test,
    kill_ring::{self, capture_clipboard, clipboard_item, ignore_clipboard_text},
    test::editor_test_context::EditorTestContext,
};
use bongos_emacs_model::kill_ring::{Entry, Piece};
use gpui::{ClipboardItem, TestAppContext};

fn ring_texts(cx: &mut EditorTestContext) -> Vec<String> {
    cx.update(|_, cx| {
        let Some(ring) = kill_ring::read(cx) else {
            return Vec::new();
        };
        (0..ring.len())
            .filter_map(|ix| ring.get(ix).map(|entry| entry.text.clone()))
            .collect()
    })
}

fn newest(cx: &mut EditorTestContext) -> Option<Entry> {
    cx.update(|_, cx| kill_ring::read(cx).and_then(|ring| ring.newest().cloned()))
}

fn shape(entry: &Entry) -> Vec<(usize, bool)> {
    entry
        .pieces
        .iter()
        .map(|piece| (piece.len, piece.is_entire_line))
        .collect()
}

fn piece(len: usize, is_entire_line: bool) -> Piece {
    Piece {
        len,
        is_entire_line,
        first_line_indent: 0,
        origin: None,
    }
}

#[gpui::test]
async fn test_kill_ring_cut_pushes_and_yank_inserts_newest(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("ˇone\ntwo\nthree\nend");
    let generation = cx.update(|_, cx| kill_ring::generation(cx));
    for _ in 0..3 {
        cx.update_editor(|e, window, cx| e.kill_ring_cut(&KillRingCut, window, cx));
        cx.update_editor(|e, window, cx| e.kill_ring_cut(&KillRingCut, window, cx));
    }
    cx.assert_editor_state("ˇend");
    assert_eq!(
        ring_texts(&mut cx),
        ["\n", "three", "\n", "two", "\n", "one"]
    );
    assert!(cx.update(|_, cx| kill_ring::generation(cx)) > generation);

    cx.update(|_, cx| kill_ring::update(cx, |ring| *ring = Default::default()));
    cx.set_state("ˇa");
    cx.update_editor(|e, window, cx| e.kill_ring_cut(&KillRingCut, window, cx));
    cx.set_state("ˇb");
    cx.update_editor(|e, window, cx| e.kill_ring_cut(&KillRingCut, window, cx));
    cx.set_state("ˇc");
    cx.update_editor(|e, window, cx| e.kill_ring_cut(&KillRingCut, window, cx));
    assert_eq!(ring_texts(&mut cx), ["c", "b", "a"]);

    cx.set_state("xˇy");
    cx.update_editor(|e, window, cx| e.kill_ring_yank(&KillRingYank, window, cx));
    cx.assert_editor_state("xcˇy");
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("c".to_string())
    );
}

#[gpui::test]
async fn test_set_kill_ring_text_pushes(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("ˇ");
    cx.update_editor(|e, _, cx| e.set_kill_ring_text("first", cx));
    cx.update_editor(|e, _, cx| e.set_kill_ring_text("second", cx));
    assert_eq!(ring_texts(&mut cx), ["second", "first"]);
    assert_eq!(newest(&mut cx), Some(Entry::plain("second".to_string())));
    assert_eq!(
        cx.update(|_, cx| kill_ring::yank_text(cx)),
        Some("second".to_string())
    );
}

#[gpui::test]
async fn test_cut_and_copy_push_with_metadata(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let mut cx = EditorTestContext::new(cx).await;

    cx.set_state("«one ˇ»two «three ˇ»four");
    cx.update_editor(|e, window, cx| e.cut(&Cut, window, cx));
    let entry = newest(&mut cx).unwrap();
    assert_eq!(entry.text, "one \nthree ");
    assert_eq!(shape(&entry), [(4, false), (6, false)]);
    assert!(!entry.secret);

    cx.set_state("  first\nsecˇond\nthiˇrd");
    cx.update_editor(|e, window, cx| e.copy(&Copy, window, cx));
    let entry = newest(&mut cx).unwrap();
    assert_eq!(entry.text, "second\nthird\n");
    assert_eq!(shape(&entry), [(7, true), (6, true)]);
    assert_eq!(ring_texts(&mut cx).len(), 2);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("second\nthird\n".to_string())
    );

    cx.set_state("«  indented\n  block ˇ»");
    cx.update_editor(|e, window, cx| e.copy_and_trim(&crate::CopyAndTrim, window, cx));
    assert_eq!(ring_texts(&mut cx).len(), 3);
    assert_eq!(
        newest(&mut cx).map(|entry| entry.text),
        cx.read_from_clipboard().and_then(|item| item.text())
    );
}

#[gpui::test]
async fn test_masked_editor_pushes_nothing(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let mut cx = EditorTestContext::new(cx).await;

    cx.update_editor(|e, _, cx| e.set_kill_ring_text("kept", cx));
    cx.update_editor(|e, _, cx| e.set_masked(true, cx));
    assert!(cx.update_editor(|e, _, cx| e.is_masked(cx)));

    cx.set_state("«secretˇ» rest");
    cx.update_editor(|e, window, cx| e.copy(&Copy, window, cx));
    assert_eq!(ring_texts(&mut cx), ["kept"]);
    assert!(!cx.update(|_, cx| capture_clipboard(cx)));
    assert_eq!(ring_texts(&mut cx), ["kept"]);

    cx.set_state("«hunter2ˇ» rest");
    cx.update_editor(|e, window, cx| e.cut(&Cut, window, cx));
    assert_eq!(ring_texts(&mut cx), ["kept"]);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("hunter2".to_string())
    );
    assert!(!cx.update(|_, cx| capture_clipboard(cx)));

    cx.set_state("ˇpassword\nnext");
    cx.update_editor(|e, window, cx| e.kill_ring_cut(&KillRingCut, window, cx));
    assert_eq!(ring_texts(&mut cx), ["kept"]);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("hunter2".to_string())
    );
}

#[gpui::test]
async fn test_capture_clipboard(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let mut cx = EditorTestContext::new(cx).await;

    // Nothing on the clipboard, then empty text.
    assert!(!cx.update(|_, cx| capture_clipboard(cx)));
    cx.write_to_clipboard(ClipboardItem::new_string(String::new()));
    assert!(!cx.update(|_, cx| capture_clipboard(cx)));
    assert!(ring_texts(&mut cx).is_empty());

    // New text is pushed once; equal to the newest is not pushed again.
    cx.write_to_clipboard(ClipboardItem::new_string("outside".to_string()));
    assert!(cx.update(|_, cx| capture_clipboard(cx)));
    assert!(!cx.update(|_, cx| capture_clipboard(cx)));
    assert_eq!(ring_texts(&mut cx), ["outside"]);

    // Ignored text is not pushed.
    cx.update(|_, cx| ignore_clipboard_text("private", cx));
    cx.write_to_clipboard(ClipboardItem::new_string("private".to_string()));
    assert!(!cx.update(|_, cx| capture_clipboard(cx)));
    assert_eq!(ring_texts(&mut cx), ["outside"]);

    // Metadata written by an editor is kept, with each piece's origin.
    let selections = vec![
        ClipboardSelection {
            len: 2,
            is_entire_line: false,
            first_line_indent: 4,
            file_path: Some("/root/a.rs".into()),
            line_range: Some(3..=3),
        },
        ClipboardSelection {
            len: 3,
            is_entire_line: true,
            first_line_indent: 0,
            file_path: None,
            line_range: None,
        },
    ];
    cx.write_to_clipboard(ClipboardItem::new_string_with_json_metadata(
        "ab\ncd\n".to_string(),
        selections,
    ));
    assert!(cx.update(|_, cx| capture_clipboard(cx)));
    let entry = newest(&mut cx).unwrap();
    assert_eq!(entry.text, "ab\ncd\n");
    assert_eq!(entry.piece_texts(), ["ab", "cd\n"]);
    assert_eq!(entry.pieces[0].first_line_indent, 4);
    let origin = entry.pieces[0].origin.clone().unwrap();
    assert_eq!(origin.path, std::path::PathBuf::from("/root/a.rs"));
    assert_eq!(origin.lines, 3..=3);
    assert!(entry.pieces[1].is_entire_line);
    assert_eq!(entry.pieces[1].origin, None);

    // Metadata that does not fit the text gives one plain piece.
    let selections = vec![ClipboardSelection {
        len: 99,
        is_entire_line: false,
        first_line_indent: 0,
        file_path: None,
        line_range: None,
    }];
    cx.write_to_clipboard(ClipboardItem::new_string_with_json_metadata(
        "short".to_string(),
        selections,
    ));
    assert!(cx.update(|_, cx| capture_clipboard(cx)));
    assert_eq!(newest(&mut cx), Some(Entry::plain("short".to_string())));
}

#[gpui::test]
async fn test_clipboard_item_is_pasted_per_caret(cx: &mut TestAppContext) {
    init_test(cx, |_| {});
    let mut cx = EditorTestContext::new(cx).await;

    let entry = Entry::from_pieces(
        &[
            "one".to_string(),
            "two\nlines".to_string(),
            "three".to_string(),
        ],
        vec![piece(0, false), piece(0, false), piece(0, false)],
    );
    let item = clipboard_item(&entry);
    assert_eq!(kill_ring::entry_from_clipboard(&item), Some(entry));
    cx.write_to_clipboard(item);

    cx.set_state("a ˇ\nb ˇ\nc ˇ");
    cx.update_editor(|e, window, cx| e.paste(&Paste, window, cx));
    cx.assert_editor_state("a oneˇ\nb two\nlinesˇ\nc threeˇ");
}
