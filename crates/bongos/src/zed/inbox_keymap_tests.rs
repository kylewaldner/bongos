//! The keys of the Inbox: `C-c i` opens it under the Emacs base, the mode keys of the view (key
//! context `InboxView`) and of the item modal (`InboxItemModal`) reach their actions in a real
//! window, `n` typed in an editor is still text, and the palette lists the inbox's commands only
//! while `inbox.enabled`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone as _, Utc};
use futures::{FutureExt as _, StreamExt as _, channel::mpsc, select_biased};
use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, KeyBinding, KeyContext, Keymap, Keystroke,
    TestAppContext, VisualTestContext,
};
use serde_json::json;
use settings::{KeymapFile, SettingsStore};
use tempfile::TempDir;
use util::path;
use workspace::{MultiWorkspace, Workspace};

use bongos_inbox::{Inbox, InboxDirs, InboxEvent, InboxState, InboxView, Layout};
use bongos_inbox_model::format::Item;
use bongos_inbox_model::maildir;

use super::tests::{init_keymap_test, init_test};

const LINUX: [&str; 3] = [
    "keymaps/default-linux.json",
    "keymaps/linux/emacs.json",
    "keymaps/specific-overrides.json",
];
const MACOS: [&str; 3] = [
    "keymaps/default-macos.json",
    "keymaps/macos/emacs.json",
    "keymaps/specific-overrides-macos.json",
];

/// The keys of the view and what each asks for; the Emacs base adds the keys of `EMACS_VIEW_KEYS`.
const VIEW_KEYS: &[(&str, &str)] = &[
    ("n", "inbox::Next"),
    ("p", "inbox::Previous"),
    ("enter", "inbox::OpenItem"),
    ("a", "inbox::Accept"),
    ("d", "inbox::Dismiss"),
    ("o", "inbox::OpenTarget"),
    ("u", "inbox::MarkUnread"),
    ("/", "inbox::Filter"),
    ("g", "inbox::Refresh"),
    ("right", "inbox::FocusItem"),
    ("left", "inbox::FocusList"),
    ("escape", "menu::Cancel"),
];

const EMACS_VIEW_KEYS: &[(&str, &str)] = &[
    ("ctrl-n", "inbox::Next"),
    ("ctrl-p", "inbox::Previous"),
    ("ctrl-f", "inbox::FocusItem"),
    ("ctrl-b", "inbox::FocusList"),
];

const MODAL_KEYS: &[(&str, &str)] = &[
    ("n", "inbox::Next"),
    ("p", "inbox::Previous"),
    ("a", "inbox::Accept"),
    ("d", "inbox::Dismiss"),
    ("o", "inbox::OpenTarget"),
    ("escape", "menu::Cancel"),
];

const VIEW: [&str; 3] = ["Workspace", "Pane", "InboxView"];
const ITEM_PANE: [&str; 4] = ["Workspace", "Pane", "InboxView", "InboxItem"];
const MODAL: [&str; 3] = ["Workspace", "Modal", "InboxItemModal"];
const EDITOR: [&str; 3] = ["Workspace", "Pane", "Editor mode=full"];

fn load(paths: &[&str], cx: &TestAppContext) -> Keymap {
    let bindings: Vec<KeyBinding> = cx.update(|cx| {
        let mut all = Vec::new();
        for path in paths {
            all.extend(
                KeymapFile::load_asset(path, None, cx)
                    .unwrap_or_else(|error| panic!("{path}: {error}")),
            );
        }
        all
    });
    Keymap::new(bindings)
}

fn resolve(keymap: &Keymap, input: &str, contexts: &[&str]) -> Vec<&'static str> {
    let keys: Vec<Keystroke> = input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect();
    let stack: Vec<KeyContext> = contexts
        .iter()
        .map(|context| KeyContext::parse(context).unwrap())
        .collect();
    let (bindings, _) = keymap.bindings_for_input(&keys, &stack);
    bindings
        .iter()
        .map(|binding| binding.action().name())
        .collect()
}

/// Whether `input` runs `action` in `contexts`: some binding that matches there asks for it, and
/// every one in front of it is one the view does not handle, so the key falls through to it.
fn runs(keymap: &Keymap, input: &str, contexts: &[&str], action: &str) -> bool {
    resolve(keymap, input, contexts).contains(&action)
}

#[gpui::test]
async fn the_view_and_the_modal_resolve_their_keys_in_both_profiles(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    for (paths, emacs_base) in [
        (&LINUX[..], true),
        (&MACOS[..], true),
        (&LINUX[..1], false),
        (&MACOS[..1], false),
    ] {
        let keymap = load(paths, cx);
        let profile = paths[0];
        for (input, action) in VIEW_KEYS {
            for contexts in [&VIEW[..], &ITEM_PANE[..]] {
                assert!(
                    runs(&keymap, input, contexts, action),
                    "{profile}: {input} in {contexts:?}"
                );
            }
        }
        for (input, action) in MODAL_KEYS {
            assert!(
                runs(&keymap, input, &MODAL, action),
                "{profile}: {input} in the modal"
            );
        }
        // The Emacs keys come with the Emacs base only.
        for (input, action) in EMACS_VIEW_KEYS {
            assert_eq!(
                runs(&keymap, input, &VIEW, action),
                emacs_base,
                "{profile}: {input}"
            );
        }
        for (input, action) in [("ctrl-n", "inbox::Next"), ("ctrl-p", "inbox::Previous")] {
            assert_eq!(
                runs(&keymap, input, &MODAL, action),
                emacs_base,
                "{profile}: {input} in the modal"
            );
        }
        // The Inbox keys are the view's alone: an editor keeps its own.
        for (input, _) in VIEW_KEYS.iter().chain(EMACS_VIEW_KEYS) {
            assert!(
                !resolve(&keymap, input, &EDITOR)
                    .iter()
                    .any(|name| name.starts_with("inbox::")),
                "{profile}: {input} in an editor"
            );
        }
        for contexts in [&["Workspace", "Pane"][..], &EDITOR[..]] {
            assert_eq!(
                runs(&keymap, "ctrl-c i", contexts, "inbox::Open"),
                emacs_base,
                "{profile}: ctrl-c i in {contexts:?}"
            );
        }
    }
}

/// Loads the keymaps the app loads on Linux with the Emacs base.
fn load_app_keymap(cx: &mut gpui::App) {
    let mut all = Vec::new();
    for path in LINUX {
        let source = if path.ends_with("emacs.json") {
            settings::KeybindSource::Base
        } else {
            settings::KeybindSource::Default
        };
        all.extend(
            KeymapFile::load_asset(path, Some(source), cx)
                .unwrap_or_else(|error| panic!("{path}: {error}")),
        );
    }
    cx.bind_keys(all);
}

fn set_enabled(cx: &mut TestAppContext, enabled: bool) {
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.inbox.get_or_insert_default().enabled = Some(enabled);
            });
        });
    });
    cx.run_until_parked();
}

/// A window with an enabled inbox holding `summaries` (newest first), the app keymap with the
/// Emacs base, and an editor in front.
struct World {
    workspace: Entity<Workspace>,
    cx: VisualTestContext,
    inbox: Entity<InboxState>,
    _temp: TempDir,
}

async fn world(summaries: &[&str], cx: &mut TestAppContext) -> World {
    let app_state = init_test(cx);
    set_enabled(cx, true);
    let temp = TempDir::new().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap().join("inbox");
    cx.executor().allow_parking();
    let dirs = InboxDirs::at(root.clone());
    cx.update(|cx| {
        bongos_emacs::init(cx);
        load_app_keymap(cx);
        let fs: Arc<dyn fs::Fs> = Arc::new(fs::RealFs::new(None, cx.background_executor().clone()));
        bongos_inbox::init(dirs, fs, cx);
        bongos_inbox::init_palette(cx);
    });
    let inbox = cx.update(|cx| Inbox::global(cx).unwrap());
    wait_for_items(cx, &inbox, 0).await;
    for (index, summary) in summaries.iter().enumerate() {
        // Later summaries are older.
        let created = Utc
            .with_ymd_and_hms(2026, 11, 11, 3, 59 - index as u32, 0)
            .unwrap();
        maildir::deliver(&root, &item(summary, created)).unwrap();
    }
    inbox.update(cx, |inbox, _| inbox.refresh());
    wait_for_items(cx, &inbox, summaries.len()).await;

    app_state
        .fs
        .as_fake()
        .insert_tree(path!("/root"), json!({ "a": "text" }))
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
    let workspace = multi_workspace
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let vcx = VisualTestContext::from_window(multi_workspace.into(), cx);
    vcx.run_until_parked();
    World {
        workspace,
        cx: vcx,
        inbox,
        _temp: temp,
    }
}

fn item(summary: &str, created: chrono::DateTime<Utc>) -> Item {
    Item {
        version: 1,
        kind: "note".into(),
        created,
        source: format!("agent/{summary}"),
        group: None,
        target: None,
        summary: summary.into(),
        actions: vec!["accept".into()],
        extra: Vec::new(),
        body: format!("Body of {summary}.\n"),
        body_loaded: true,
    }
}

/// Waits, on the inbox's events, until its snapshot has `count` items.
async fn wait_for_items(cx: &mut TestAppContext, inbox: &Entity<InboxState>, count: usize) {
    let (sender, mut receiver) = mpsc::unbounded::<()>();
    let _subscriptions = cx.update(|cx| {
        let on_event = sender.clone();
        [
            cx.subscribe(inbox, move |_, _: &InboxEvent, _| {
                on_event.unbounded_send(()).ok();
            }),
            cx.observe(inbox, move |_, _| {
                sender.unbounded_send(()).ok();
            }),
        ]
    });
    let mut guard = cx.executor().timer(Duration::from_secs(30)).fuse();
    loop {
        let len = inbox.read_with(cx, |inbox, _| {
            inbox.snapshot().map(|snapshot| snapshot.len())
        });
        if len == Some(count) {
            return;
        }
        select_biased! {
            _ = receiver.next() => {}
            _ = guard => panic!("timed out waiting for {count} items"),
        }
    }
}

impl World {
    fn view(&self) -> Option<Entity<InboxView>> {
        self.workspace.read_with(&self.cx, |workspace, cx| {
            workspace.items_of_type::<InboxView>(cx).next()
        })
    }

    fn open_view(&mut self) -> Entity<InboxView> {
        self.cx.simulate_keystrokes("ctrl-c i");
        self.cx.run_until_parked();
        self.view().expect("C-c i opens the inbox view")
    }

    fn type_keys(&mut self, keys: &str) {
        self.cx.simulate_keystrokes(keys);
        self.cx.run_until_parked();
    }

    fn cursor(&self, view: &Entity<InboxView>) -> Option<String> {
        view.read_with(&self.cx, |view, _| {
            view.selected().map(|row| row.summary.clone())
        })
    }

    fn unread(&self, view: &Entity<InboxView>) -> Vec<bool> {
        view.read_with(&self.cx, |view, _| {
            view.rows().iter().map(|row| row.unread).collect()
        })
    }

    fn item_focused(&mut self, view: &Entity<InboxView>) -> bool {
        self.cx
            .update(|window, cx| view.read(cx).item_focused(window))
    }

    fn list_focused(&mut self, view: &Entity<InboxView>) -> bool {
        self.cx
            .update(|window, cx| view.focus_handle(cx).is_focused(window))
    }

    fn has_modal(&mut self) -> bool {
        self.workspace
            .update_in(&mut self.cx, |workspace, window, cx| {
                workspace.has_active_modal(window, cx)
            })
    }
}

#[gpui::test]
async fn n_typed_in_an_editor_inserts_n_and_c_c_i_opens_the_inbox(cx: &mut TestAppContext) {
    let mut world = world(&["newer", "older"], cx).await;
    let editor_text = |world: &mut World| {
        world
            .workspace
            .update_in(&mut world.cx, |workspace, _, cx| {
                let editor = workspace
                    .active_item_as::<editor::Editor>(cx)
                    .expect("the file is open in an editor");
                editor.read(cx).text(cx)
            })
    };
    world.type_keys("n");
    assert_eq!(editor_text(&mut world), "ntext");
    assert!(world.view().is_none());

    let view = world.open_view();
    assert!(world.list_focused(&view), "the view has the focus");
    // A second C-c i focuses the same view.
    world.type_keys("ctrl-c i");
    assert_eq!(
        world
            .workspace
            .read_with(&world.cx, |workspace, cx| workspace
                .items_of_type::<InboxView>(cx)
                .count()),
        1
    );
}

#[gpui::test]
async fn the_list_keys_move_focus_and_change_items(cx: &mut TestAppContext) {
    let mut world = world(&["newer", "older"], cx).await;
    let view = world.open_view();
    assert_eq!(world.cursor(&view).as_deref(), Some("newer"));
    for (down, up) in [("n", "p"), ("ctrl-n", "ctrl-p")] {
        world.type_keys(down);
        assert_eq!(world.cursor(&view).as_deref(), Some("older"), "{down}");
        world.type_keys(up);
        assert_eq!(world.cursor(&view).as_deref(), Some("newer"), "{up}");
    }
    assert_eq!(world.unread(&view), [true, true], "moving reads nothing");

    for (into, back) in [
        ("right", "left"),
        ("ctrl-f", "ctrl-b"),
        ("enter", "escape"),
        ("right", "escape"),
    ] {
        world.type_keys(into);
        assert!(world.item_focused(&view), "{into} focuses the item pane");
        world.type_keys(back);
        assert!(!world.item_focused(&view), "{back} leaves the item pane");
        assert!(world.list_focused(&view), "{back} returns to the list");
    }
    assert_eq!(world.unread(&view), [false, true], "opening reads the item");

    world.type_keys("u");
    assert_eq!(world.unread(&view), [true, true], "u marks it unread");

    // `d` archives the item under the cursor, `a` runs its first action, which archives it.
    world.type_keys("d");
    wait_for_items(cx, &world.inbox, 1).await;
    assert_eq!(world.cursor(&view).as_deref(), Some("older"));
    world.type_keys("a");
    wait_for_items(cx, &world.inbox, 0).await;
}

#[gpui::test]
async fn g_o_and_slash_are_bound_in_the_focused_view(cx: &mut TestAppContext) {
    let mut world = world(&["newer"], cx).await;
    let view = world.open_view();
    let handle = world.cx.update(|_, cx| view.focus_handle(cx));
    for (key, name) in [
        ("g", "inbox::Refresh"),
        ("o", "inbox::OpenTarget"),
        ("/", "inbox::Filter"),
    ] {
        let action = world
            .cx
            .update(|_, cx| cx.build_action(name, None).unwrap());
        let bound = world.cx.update(|window, _| {
            window
                .bindings_for_action_in(action.as_ref(), &handle)
                .iter()
                .any(|binding| {
                    binding.keystrokes().len() == 1 && binding.keystrokes()[0].key() == key
                })
        });
        assert!(bound, "{key} runs {name} in the focused view");
    }
}

#[gpui::test]
async fn n_p_and_escape_work_in_the_modal(cx: &mut TestAppContext) {
    let mut world = world(&["newer", "older"], cx).await;
    let view = world.open_view();
    view.update(&mut world.cx, |view, cx| {
        view.set_layout(Layout::Sidebar, cx)
    });
    world.cx.run_until_parked();
    assert!(!world.has_modal());

    world.type_keys("enter");
    assert!(world.has_modal(), "RET opens the modal");
    for (down, up) in [("n", "p"), ("ctrl-n", "ctrl-p")] {
        world.type_keys(down);
        assert_eq!(world.cursor(&view).as_deref(), Some("older"), "{down}");
        world.type_keys(up);
        assert_eq!(world.cursor(&view).as_deref(), Some("newer"), "{up}");
        assert!(world.has_modal());
    }
    world.type_keys("escape");
    assert!(!world.has_modal(), "Esc closes the modal");
    assert!(world.list_focused(&view), "the focus is back on the list");
}

#[gpui::test]
async fn the_palette_lists_the_inbox_commands_only_while_the_inbox_is_on(cx: &mut TestAppContext) {
    let _world = world(&[], cx).await;
    let hidden = |cx: &mut TestAppContext| -> Vec<bool> {
        cx.update(|cx| {
            let names: Vec<&'static str> = cx
                .all_action_names()
                .iter()
                .copied()
                .filter(|name| name.starts_with("inbox::"))
                .collect();
            assert!(names.len() >= bongos_inbox::action_types().len());
            let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx).unwrap();
            names
                .iter()
                .map(|name| filter.is_hidden(cx.build_action(name, None).unwrap().as_ref()))
                .collect()
        })
    };
    let shown = hidden(cx);
    assert!(shown.iter().all(|hidden| !hidden), "shown while on");
    set_enabled(cx, false);
    let gone = hidden(cx);
    assert!(gone.iter().all(|hidden| *hidden), "hidden while off");
    set_enabled(cx, true);
    assert!(hidden(cx).iter().all(|hidden| !hidden), "shown again");
}
