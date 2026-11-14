//! The public notice API: code that knows a precondition a language needs (a missing tool on
//! `PATH`, say) reports it here and the status line shows it until it is cleared.

use gpui::{App, AppContext as _, Context, Entity, Global};
use settings::WorktreeId;

use crate::{LanguagesLog, state::Notice};

/// The notices reported so far; the status items observe it.
#[derive(Default)]
pub struct NoticeList {
    notices: Vec<Notice>,
}

impl NoticeList {
    pub fn notices(&self) -> &[Notice] {
        &self.notices
    }

    /// Adds or replaces the notice with this key. Returns whether anything changed.
    fn set(&mut self, notice: Notice, cx: &mut Context<Self>) -> bool {
        let existing = self.notices.iter_mut().find(|existing| {
            existing.worktree_id == notice.worktree_id
                && existing.language == notice.language
                && existing.key == notice.key
        });
        match existing {
            Some(existing) if *existing == notice => return false,
            Some(existing) => *existing = notice,
            None => self.notices.push(notice),
        }
        cx.notify();
        true
    }

    fn remove(
        &mut self,
        worktree_id: WorktreeId,
        language: &str,
        key: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let before = self.notices.len();
        self.notices.retain(|notice| {
            !(notice.worktree_id == worktree_id && notice.language == language && notice.key == key)
        });
        let changed = self.notices.len() != before;
        if changed {
            cx.notify();
        }
        changed
    }

    /// Removes a notice the user dismissed.
    pub fn dismiss(&mut self, notice: &Notice, cx: &mut Context<Self>) {
        self.remove(notice.worktree_id, &notice.language, &notice.key, cx);
    }
}

struct GlobalNotices(Entity<NoticeList>);

impl Global for GlobalNotices {}

/// Report and clear notices. Both are cheap and may be called as often as the precondition is
/// checked: reporting the same text again changes nothing and logs nothing.
pub struct LanguageNotices;

impl LanguageNotices {
    /// Shows `text` for `language` in the worktree until `clear` is called with the same
    /// arguments. A second report with the same key replaces the text.
    pub fn report(worktree_id: WorktreeId, language: &str, key: &str, text: &str, cx: &mut App) {
        let notice = Notice {
            worktree_id,
            language: language.to_string(),
            key: key.to_string(),
            text: text.to_string(),
        };
        let changed = Self::list(cx).update(cx, |list, cx| list.set(notice, cx));
        if changed {
            log_line(cx, &format!("{language}: notice: {text}"));
        }
    }

    pub fn clear(worktree_id: WorktreeId, language: &str, key: &str, cx: &mut App) {
        let changed =
            Self::list(cx).update(cx, |list, cx| list.remove(worktree_id, language, key, cx));
        if changed {
            log_line(cx, &format!("{language}: notice cleared ({key})"));
        }
    }

    /// The list the status items observe.
    pub fn list(cx: &mut App) -> Entity<NoticeList> {
        if let Some(GlobalNotices(list)) = cx.try_global::<GlobalNotices>() {
            return list.clone();
        }
        let list = cx.new(|_| NoticeList::default());
        cx.set_global(GlobalNotices(list.clone()));
        list
    }
}

fn log_line(cx: &App, text: &str) {
    if let Some(log) = cx.try_global::<LogGlobal>() {
        log.0.line(text);
    }
}

/// The log, set once by `init`.
pub(crate) struct LogGlobal(pub LanguagesLog);

impl Global for LogGlobal {}
