//! The flags each find in files history entry was run with. The history ring holds the input;
//! the flags are kept beside it in the `navigation_find_flags` namespace of the key-value
//! store, keyed by the input, for as many inputs as the ring keeps.

use bongos_minibuffer_model::HistoryRing;
use bongos_navigation_model::flags::Flags;
use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _};
use util::ResultExt as _;

/// The scoped namespace of the key-value store that holds the flags.
pub const NAMESPACE: &str = "navigation_find_flags";
/// The key of the inputs that have flags, newest first. No input is empty, so no input is it.
const INDEX_KEY: &str = "";

/// The flags `input` was run with, if they were kept.
pub fn read(kvp: &KeyValueStore, input: &str) -> Option<Flags> {
    let json = kvp.scoped(NAMESPACE).read(input).log_err().flatten()?;
    serde_json::from_str(&json).log_err()
}

/// Keeps `flags` for `input` in the background, and forgets the flags of the input the ring of
/// [`bongos_minibuffer_model::HISTORY_CAPACITY`] inputs drops for it.
pub fn write(kvp: &KeyValueStore, input: &str, flags: &Flags, cx: &App) {
    if input.is_empty() {
        return;
    }
    let kvp = kvp.clone();
    let input = input.to_string();
    let Some(json) = serde_json::to_string(flags).log_err() else {
        return;
    };
    cx.background_spawn(async move {
        let scoped = kvp.scoped(NAMESPACE);
        let mut index = scoped
            .read(INDEX_KEY)
            .log_err()
            .flatten()
            .map(|json| HistoryRing::from_json(&json))
            .unwrap_or_default();
        let before = index.entries().to_vec();
        index.push(input.clone());
        for gone in before.iter().filter(|old| !index.entries().contains(old)) {
            scoped.delete(gone.clone()).await.log_err();
        }
        scoped.write(input, json).await.log_err();
        scoped
            .write(INDEX_KEY.to_string(), index.to_json())
            .await
            .log_err();
    })
    .detach();
}
