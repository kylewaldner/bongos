//! The text of a lane's wait. A feature that owns a kind of wait (the pools' owner for
//! `WaitReason::Pool`) registers a formatter; the Lanes view and `status` ask the registry before
//! falling back to the reason's own text.

use bongos_lanes_model::WaitReason;
use gpui::{App, Global};

/// A formatter: the text for a reason it knows, `None` for any other.
pub type WaitTextFn = fn(&WaitReason) -> Option<String>;

/// The registered `wait_text` formatters, asked in the order they were registered.
#[derive(Clone, Default)]
pub struct WaitTextFormatters {
    formatters: Vec<WaitTextFn>,
}

impl Global for WaitTextFormatters {}

impl WaitTextFormatters {
    /// Registers `formatter` as a global.
    pub fn register(formatter: WaitTextFn, cx: &mut App) {
        cx.default_global::<Self>().add(formatter);
    }

    /// The registry as registered in `cx`, empty when nothing registered.
    pub fn global(cx: &App) -> Self {
        cx.try_global::<Self>().cloned().unwrap_or_default()
    }

    /// Adds a formatter to this registry value (for code that holds one without an `App`).
    pub fn add(&mut self, formatter: WaitTextFn) {
        self.formatters.push(formatter);
    }

    /// The text of `reason`: the first formatter that answers, else the reason's own text.
    pub fn text(&self, reason: &WaitReason) -> String {
        self.formatters
            .iter()
            .find_map(|formatter| formatter(reason))
            .unwrap_or_else(|| reason.to_string())
    }
}
