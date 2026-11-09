//! The input history of the completing reads, one ring per session name, persisted as JSON in the
//! `minibuffer_history` namespace of the key-value store. Entries are newest first, a repeated
//! entry moves to the front, and a read is written when it is confirmed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use bongos_minibuffer_model::HistoryRing;
use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Global};
use util::ResultExt as _;

/// The scoped namespace of the key-value store that holds the rings.
pub const HISTORY_NAMESPACE: &str = "minibuffer_history";

/// The rings of every session name over one database. Clones share their rings. Reads are served
/// from memory once a ring has been loaded or written, so two confirms in a row never lose the
/// first while its write is still in flight.
#[derive(Clone)]
pub struct HistoryStore {
    kvp: KeyValueStore,
    rings: Rc<RefCell<HashMap<String, HistoryRing>>>,
}

impl Global for HistoryStore {}

impl HistoryStore {
    /// A store over `kvp`. Another store over the same database reads what this one wrote.
    pub fn new(kvp: KeyValueStore) -> Self {
        Self {
            kvp,
            rings: Rc::default(),
        }
    }

    /// The store the sessions use: the one installed with [`install`](Self::install), else one
    /// over the app database, which is then installed.
    pub fn global(cx: &mut App) -> Self {
        if let Some(store) = cx.try_global::<Self>() {
            return store.clone();
        }
        let store = Self::new(KeyValueStore::global(cx));
        cx.set_global(store.clone());
        store
    }

    /// Makes this the store of every session opened from now on.
    pub fn install(self, cx: &mut App) {
        cx.set_global(self);
    }

    /// The ring of `name`, empty when nothing was stored or the stored data is malformed.
    pub fn ring(&self, name: &str) -> HistoryRing {
        if let Some(ring) = self.rings.borrow().get(name) {
            return ring.clone();
        }
        let ring = self
            .kvp
            .scoped(HISTORY_NAMESPACE)
            .read(name)
            .log_err()
            .flatten()
            .map(|json| HistoryRing::from_json(&json))
            .unwrap_or_default();
        self.rings
            .borrow_mut()
            .insert(name.to_string(), ring.clone());
        ring
    }

    /// Records `entry` as the newest of `name` and writes the ring in the background.
    pub fn push(&self, name: &str, entry: &str, cx: &App) {
        let mut ring = self.ring(name);
        ring.push(entry);
        self.rings
            .borrow_mut()
            .insert(name.to_string(), ring.clone());
        let kvp = self.kvp.clone();
        let name = name.to_string();
        let json = ring.to_json();
        cx.background_spawn(async move {
            kvp.scoped(HISTORY_NAMESPACE)
                .write(name, json)
                .await
                .log_err();
        })
        .detach();
    }
}
