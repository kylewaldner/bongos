use bongos_emacs_model::kill_ring::{Entry, PERSIST_ENTRY_MAX_BYTES, Ring};
use db::AppDatabase;
use db::kvp::KeyValueStore;
use editor::test::editor_test_context::EditorTestContext;
use editor::{EditorSettings, kill_ring};
use gpui::{App, BorrowAppContext as _, KeyBinding, TestAppContext};
use settings::{Settings as _, SettingsStore};

use crate::actions::KillLine;
use crate::kill_ring_store::{KEY, NAMESPACE};

/// Starts the app over `db` the way launch does: settings (with `emacs.save_kill_ring` set to
/// `save` when given), then the app database, then `init`.
fn start(cx: &mut TestAppContext, db: &AppDatabase, save: Option<bool>) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        if let Some(save) = save {
            set_save(cx, save);
        }
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        cx.set_global(AppDatabase(db.0.clone()));
        crate::init(cx);
    });
    cx.run_until_parked();
}

fn set_save(cx: &mut App, save: bool) {
    cx.update_global::<SettingsStore, _>(|store, cx| {
        store.update_user_settings(cx, |content| {
            content.emacs.get_or_insert_default().save_kill_ring = Some(save);
        })
    });
}

fn stored(db: &AppDatabase) -> Option<String> {
    KeyValueStore::from_app_db(db)
        .scoped(NAMESPACE)
        .read(KEY)
        .unwrap()
}

fn stored_texts(db: &AppDatabase) -> Option<Vec<String>> {
    stored(db).map(|json| texts(&Ring::from_json(&json)))
}

fn texts(ring: &Ring) -> Vec<String> {
    (0..ring.len())
        .filter_map(|ix| ring.get(ix).map(|entry| entry.text.clone()))
        .collect()
}

fn ring_texts(cx: &App) -> Vec<String> {
    kill_ring::read(cx).map_or(Vec::new(), texts)
}

fn push(cx: &mut App, entry: Entry) {
    kill_ring::update(cx, |ring| ring.push(entry));
}

fn plain(text: &str) -> Entry {
    Entry::plain(text.to_string())
}

/// The entries as the saved form keeps them: no piece origins.
fn saved_form(ring: &Ring) -> Vec<Entry> {
    (0..ring.len())
        .filter_map(|ix| ring.get(ix).cloned())
        .map(|mut entry| {
            for piece in &mut entry.pieces {
                piece.origin = None;
            }
            entry
        })
        .collect()
}

#[gpui::test]
async fn kill_ring_survives_a_new_app_over_the_same_db(
    cx_a: &mut TestAppContext,
    cx_b: &mut TestAppContext,
) {
    let db = cx_a.update(|_| AppDatabase::test_new());
    start(cx_a, &db, None);
    let mut editor_cx = EditorTestContext::new(cx_a).await;
    editor_cx.update(|_, cx| {
        cx.bind_keys([KeyBinding::new("ctrl-k", KillLine::default(), Some("Editor"))])
    });
    editor_cx.set_state("ˇone\nˇtwo\nthree");
    editor_cx.simulate_keystrokes("ctrl-k");
    editor_cx.update(|_, cx| push(cx, plain("newest")));
    editor_cx.run_until_parked();
    let before = editor_cx.update(|_, cx| saved_form(kill_ring::read(cx).unwrap()));
    assert_eq!(before.len(), 2);
    assert_eq!(before[1].pieces.len(), 2, "the kill has one piece per caret");

    start(cx_b, &db, None);
    let after = cx_b.update(|cx| saved_form(kill_ring::read(cx).unwrap()));
    assert_eq!(after, before);
}

#[gpui::test]
async fn kill_ring_never_saves_secret_or_oversized_entries(cx: &mut TestAppContext) {
    let db = cx.update(|_| AppDatabase::test_new());
    start(cx, &db, None);
    let big = "x".repeat(2 << 20);
    assert!(big.len() > PERSIST_ENTRY_MAX_BYTES);
    cx.update(|cx| {
        push(cx, plain("kept"));
        let mut secret = plain("hunter2-password");
        secret.secret = true;
        push(cx, secret);
        push(cx, plain(&big));
    });
    cx.run_until_parked();

    let json = stored(&db).unwrap();
    assert!(!json.contains("hunter2-password"));
    assert!(!json.contains(&big[..1024]));
    assert_eq!(stored_texts(&db).unwrap(), ["kept"]);
    cx.update(|cx| {
        let in_memory = ring_texts(cx);
        assert_eq!(in_memory.len(), 3);
        assert_eq!(in_memory[1], "hunter2-password");
        assert_eq!(in_memory[0], big);
    });
}

#[gpui::test]
async fn kill_ring_setting_off_at_start_deletes_and_restores_nothing(cx: &mut TestAppContext) {
    let db = cx.update(|_| AppDatabase::test_new());
    let mut ring = Ring::default();
    ring.push(plain("saved earlier"));
    KeyValueStore::from_app_db(&db)
        .scoped(NAMESPACE)
        .write(KEY.to_string(), ring.to_json())
        .await
        .unwrap();

    start(cx, &db, Some(false));
    assert_eq!(stored(&db), None);
    assert!(cx.update(|cx| ring_texts(cx)).is_empty());

    cx.update(|cx| push(cx, plain("not saved")));
    cx.run_until_parked();
    assert_eq!(stored(&db), None);
}

#[gpui::test]
async fn kill_ring_switching_saving_off_deletes_and_on_writes(cx: &mut TestAppContext) {
    let db = cx.update(|_| AppDatabase::test_new());
    start(cx, &db, None);
    cx.update(|cx| push(cx, plain("a")));
    cx.run_until_parked();
    assert_eq!(stored_texts(&db).unwrap(), ["a"]);

    cx.update(|cx| set_save(cx, false));
    cx.run_until_parked();
    assert_eq!(stored(&db), None);
    cx.update(|cx| push(cx, plain("b")));
    cx.run_until_parked();
    assert_eq!(stored(&db), None);
    assert_eq!(cx.update(|cx| ring_texts(cx)), ["b", "a"]);

    cx.update(|cx| set_save(cx, true));
    cx.run_until_parked();
    assert_eq!(stored_texts(&db).unwrap(), ["b", "a"]);
}

#[gpui::test]
async fn kill_ring_burst_of_kills_stores_the_final_ring(cx: &mut TestAppContext) {
    let db = cx.update(|_| AppDatabase::test_new());
    start(cx, &db, None);
    cx.update(|cx| {
        for ix in 0..100 {
            push(cx, plain(&format!("kill {ix}")));
        }
    });
    cx.run_until_parked();
    let expected = cx.update(|cx| kill_ring::read(cx).unwrap().to_json());
    assert_eq!(stored(&db).unwrap(), expected);

    // One update per kill queues one message each before the writer runs; the last one wins.
    for ix in 100..200 {
        cx.update(|cx| push(cx, plain(&format!("kill {ix}"))));
    }
    cx.run_until_parked();
    let expected = cx.update(|cx| kill_ring::read(cx).unwrap().to_json());
    assert_eq!(stored(&db).unwrap(), expected);
    assert_eq!(stored_texts(&db).unwrap()[0], "kill 199");
}

#[gpui::test]
fn kill_ring_save_setting_defaults_to_on(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        assert!(EditorSettings::get_global(cx).save_kill_ring);
        set_save(cx, false);
        assert!(!EditorSettings::get_global(cx).save_kill_ring);
    });
}
