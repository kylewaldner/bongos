//! Saves the kill ring in the `kill_ring` namespace of the app's key-value store and restores it
//! at launch, while `emacs.save_kill_ring` is on. Secret entries and entries over the size cap are
//! left out of the saved JSON by `Ring::to_json`; they stay in memory for the session.
//!
//! Every change of the ring is sent at once to one writer task, which takes the latest pending
//! message and awaits its write before taking the next. Writes therefore never reorder, a burst of
//! kills costs one write, and a crash loses at most the write in flight.

use bongos_emacs_model::kill_ring::Ring;
use db::AppDatabase;
use db::kvp::KeyValueStore;
use editor::EditorSettings;
use editor::kill_ring::{self, KillRing};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, Global};
use settings::{Settings as _, SettingsStore};
use util::ResultExt as _;

/// The scoped namespace of the key-value store that holds the ring.
pub const NAMESPACE: &str = "kill_ring";
/// The key of the ring's JSON in [`NAMESPACE`].
pub const KEY: &str = "entries";

enum Message {
    Write(String),
    Delete,
}

/// The sending side of the writer, and what was last sent to it.
struct Saver {
    sender: mpsc::UnboundedSender<Message>,
    saving: bool,
    last_sent: u64,
}

impl Global for Saver {}

impl Saver {
    fn send(&self, message: Message) {
        if self.sender.unbounded_send(message).is_err() {
            log::error!("the kill ring writer has stopped; the ring is not saved");
        }
    }
}

/// Restores the saved ring when `emacs.save_kill_ring` is on, or deletes it when off, and starts
/// saving every change. Runs before any window opens. Does nothing without an app database (tests
/// that do not install one).
pub fn init(cx: &mut App) {
    if !cx.has_global::<AppDatabase>() {
        return;
    }
    let kvp = KeyValueStore::global(cx);
    let saving = EditorSettings::get_global(cx).save_kill_ring;
    let (sender, receiver) = mpsc::unbounded();
    cx.background_spawn(write_loop(kvp.clone(), receiver))
        .detach();
    let saver = Saver {
        sender,
        saving,
        last_sent: 0,
    };
    if saving {
        if let Some(json) = kvp.scoped(NAMESPACE).read(KEY).log_err().flatten() {
            let restored = Ring::from_json(&json);
            kill_ring::update(cx, |ring| *ring = restored);
        }
    } else {
        saver.send(Message::Delete);
    }
    cx.set_global(Saver {
        last_sent: kill_ring::generation(cx),
        ..saver
    });
    cx.observe_global::<KillRing>(ring_changed).detach();
    cx.observe_global::<SettingsStore>(settings_changed).detach();
}

fn current_json(cx: &App) -> String {
    match kill_ring::read(cx) {
        Some(ring) => ring.to_json(),
        None => Ring::default().to_json(),
    }
}

fn ring_changed(cx: &mut App) {
    let generation = kill_ring::generation(cx);
    let Some(saver) = cx.try_global::<Saver>() else {
        return;
    };
    if !saver.saving || saver.last_sent == generation {
        return;
    }
    saver.send(Message::Write(current_json(cx)));
    cx.global_mut::<Saver>().last_sent = generation;
}

fn settings_changed(cx: &mut App) {
    let saving = EditorSettings::get_global(cx).save_kill_ring;
    let Some(saver) = cx.try_global::<Saver>() else {
        return;
    };
    if saver.saving == saving {
        return;
    }
    if saving {
        saver.send(Message::Write(current_json(cx)));
    } else {
        saver.send(Message::Delete);
    }
    let generation = kill_ring::generation(cx);
    let saver = cx.global_mut::<Saver>();
    saver.saving = saving;
    saver.last_sent = generation;
}

/// Takes the next message, skips to the latest one pending, and awaits its write or delete before
/// taking the next. A failed write is logged and the loop goes on.
async fn write_loop(kvp: KeyValueStore, mut receiver: mpsc::UnboundedReceiver<Message>) {
    while let Some(mut message) = receiver.next().await {
        while let Ok(newer) = receiver.try_recv() {
            message = newer;
        }
        let store = kvp.scoped(NAMESPACE);
        match message {
            Message::Write(json) => store.write(KEY.to_string(), json).await,
            Message::Delete => store.delete(KEY.to_string()).await,
        }
        .log_err();
    }
}
