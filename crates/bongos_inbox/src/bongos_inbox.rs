//! The inbox in the app: the `inbox` settings and the global [`Inbox`], an entity that scans the
//! inbox directory (`<bongos root>/inbox`) on the background executor and watches it for
//! deliveries and moves made by other processes. It only reads: nothing here changes an item.
//!
//! While `inbox.enabled` is true the layout is created when missing, the root is watched and
//! `new/` and `cur/` are added to the watcher (the Linux watcher is not recursive). `tmp/` and
//! `archive/` are not watched, and the scan never reads `tmp/`, so a file still being written
//! never shows. An event under `new/` or `cur/` rescans; an event for `new/` or `cur/` itself
//! at the root level (someone deleted and recreated the directory) adds both to the watcher
//! again and rescans. A periodic rescan, every [`RESCAN_INTERVAL`], is the safety net for
//! events the watcher missed. Rescans never overlap: triggers that arrive while one runs are
//! merged into at most one more.

mod inbox_settings;

use bongos_inbox_model::format::Item;
use bongos_inbox_model::maildir::{self, ARCHIVE, CUR, Entry, NEW, ParseCache, ParseResult, TMP};
use bongos_inbox_model::view_model;
use fs::{Fs, PathEvent, PathEventKind, Watcher};
use futures::channel::mpsc;
use futures::stream::{BoxStream, FusedStream};
use futures::{FutureExt as _, StreamExt as _, select_biased};
use gpui::{
    App, AppContext as _, AsyncApp, BackgroundExecutor, Context, Entity, EventEmitter, Global,
    Subscription, Task, WeakEntity,
};
use parking_lot::Mutex;
use settings::{Settings as _, SettingsStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use util::ResultExt as _;

pub use inbox_settings::InboxSettings;

/// How long the watcher gathers events before delivering them.
pub const WATCH_LATENCY: Duration = Duration::from_millis(200);
/// How often the inbox is scanned again even without watcher events.
pub const RESCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Where the inbox lives: `<bongos root>/inbox` and its four subdirectories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxDirs {
    pub root: PathBuf,
    pub new: PathBuf,
    pub cur: PathBuf,
    pub tmp: PathBuf,
    pub archive: PathBuf,
}

impl InboxDirs {
    /// The inbox under a bongos root (the app passes `~/.bongos`; tests pass a temporary
    /// directory).
    pub fn new(bongos_root: &Path) -> Self {
        Self::at(bongos_root.join("inbox"))
    }

    /// The inbox rooted at `root` itself.
    pub fn at(root: PathBuf) -> Self {
        Self {
            new: root.join(NEW),
            cur: root.join(CUR),
            tmp: root.join(TMP),
            archive: root.join(ARCHIVE),
            root,
        }
    }
}

/// The items in `new/` and `cur/` as last scanned, each with its parse result. An unreadable
/// file stays in the list with an unreadable result.
#[derive(Debug, Default, PartialEq)]
pub struct Snapshot {
    items: Vec<(Entry, ParseResult)>,
}

impl Snapshot {
    /// The items sorted by path, in the shape the view model takes.
    pub fn items(&self) -> &view_model::Snapshot {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The parsed item at `index`, `None` for an unreadable file.
    pub fn item(&self, index: usize) -> Option<&Item> {
        self.items
            .get(index)
            .and_then(|(_, result)| result.as_ref().as_ref().ok())
    }

    /// The items that are read (a name with the `S` flag outside `new/`).
    pub fn read(&self) -> impl Iterator<Item = &Entry> {
        self.items
            .iter()
            .map(|(entry, _)| entry)
            .filter(|entry| entry.is_read())
    }

    /// The items that are not read.
    pub fn unread(&self) -> impl Iterator<Item = &Entry> {
        self.items
            .iter()
            .map(|(entry, _)| entry)
            .filter(|entry| !entry.is_read())
    }

    /// How many unread items the view model counts.
    pub fn unread_count(&self) -> usize {
        view_model::unread_count(&self.items)
    }
}

/// The ticks of the periodic rescan, made once per enabled period.
pub type RescanTimer = Arc<dyn Fn(&BackgroundExecutor) -> BoxStream<'static, ()> + Send + Sync>;

/// A tick every `interval` on the executor's timer.
pub fn periodic_rescan(interval: Duration) -> RescanTimer {
    Arc::new(move |executor: &BackgroundExecutor| {
        futures::stream::unfold(executor.clone(), move |executor| async move {
            executor.timer(interval).await;
            Some(((), executor))
        })
        .boxed()
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxEvent {
    /// The snapshot changed, or went away because the inbox was turned off.
    Changed,
}

/// Counters for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Watchers set up, one per enabled period.
    pub watches: usize,
    /// Scans that finished.
    pub scans: usize,
    /// Times `new/` and `cur/` were added to the watcher again after a root-level event.
    pub rewatches: usize,
}

/// The inbox state behind the global [`Inbox`]. Creates, reads and watches nothing while
/// `inbox.enabled` is false.
pub struct InboxState {
    fs: Arc<dyn Fs>,
    dirs: InboxDirs,
    rescan_timer: RescanTimer,
    enabled: bool,
    snapshot: Option<Arc<Snapshot>>,
    session: Option<Session>,
    stats: Stats,
    _settings: Subscription,
}

/// An enabled period: the task that watches and scans, and its refresh requests. Dropping it
/// cancels the task, which drops the watcher and the rescan timer.
struct Session {
    requests: mpsc::UnboundedSender<()>,
    _run: Task<()>,
}

impl EventEmitter<InboxEvent> for InboxState {}

impl InboxState {
    pub fn new(
        fs: Arc<dyn Fs>,
        dirs: InboxDirs,
        rescan_timer: RescanTimer,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = cx.observe_global::<SettingsStore>(|this, cx| this.sync_enabled(cx));
        let mut this = Self {
            fs,
            dirs,
            rescan_timer,
            enabled: false,
            snapshot: None,
            session: None,
            stats: Stats::default(),
            _settings: settings,
        };
        this.sync_enabled(cx);
        this
    }

    pub fn dirs(&self) -> &InboxDirs {
        &self.dirs
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether a watcher is set up (only while enabled).
    pub fn is_watching(&self) -> bool {
        self.session.is_some()
    }

    /// The items as last scanned; `None` while disabled or before the first scan finishes.
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.clone()
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Scans again soon, merged with any rescan already pending. Does nothing while disabled.
    pub fn refresh(&mut self) {
        if let Some(session) = &self.session {
            session.requests.unbounded_send(()).ok();
        }
    }

    fn sync_enabled(&mut self, cx: &mut Context<Self>) {
        let enabled = InboxSettings::get_global(cx).enabled;
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            let (requests, receiver) = mpsc::unbounded();
            let fs = self.fs.clone();
            let dirs = self.dirs.clone();
            let timer = self.rescan_timer.clone();
            let run =
                cx.spawn(async move |this, cx| run(this, fs, dirs, timer, receiver, cx).await);
            self.session = Some(Session {
                requests,
                _run: run,
            });
        } else {
            self.session = None;
            if self.snapshot.take().is_some() {
                cx.emit(InboxEvent::Changed);
            }
            cx.notify();
        }
    }

    fn watching(&mut self, cx: &mut Context<Self>) {
        self.stats.watches += 1;
        cx.notify();
    }

    fn finish(&mut self, scan: ScanDone, cx: &mut Context<Self>) {
        self.stats.scans += 1;
        if scan.rewatched {
            self.stats.rewatches += 1;
        }
        // The stats changed even when the snapshot did not.
        cx.notify();
        let Some(snapshot) = scan.snapshot.or_else(|| {
            // A failed first scan still says the inbox is on: it shows empty.
            self.snapshot.is_none().then(Snapshot::default)
        }) else {
            return;
        };
        if self
            .snapshot
            .as_deref()
            .is_some_and(|current| *current == snapshot)
        {
            return;
        }
        self.snapshot = Some(Arc::new(snapshot));
        cx.emit(InboxEvent::Changed);
    }
}

/// The global inbox, set by [`init`].
pub struct Inbox(pub Entity<InboxState>);

impl Global for Inbox {}

impl Inbox {
    pub fn global(cx: &App) -> Option<Entity<InboxState>> {
        cx.try_global::<Inbox>().map(|inbox| inbox.0.clone())
    }
}

/// Registers the `inbox` settings and creates the global [`Inbox`], which does nothing until
/// `inbox.enabled` is true.
pub fn init(dirs: InboxDirs, fs: Arc<dyn Fs>, cx: &mut App) {
    InboxSettings::register(cx);
    let state = cx.new(|cx| InboxState::new(fs, dirs, periodic_rescan(RESCAN_INTERVAL), cx));
    cx.set_global(Inbox(state));
}

/// What one wait for triggers asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Trigger {
    rescan: bool,
    rewatch: bool,
}

impl Trigger {
    fn add_events(&mut self, root: &Path, events: &[PathEvent]) {
        for event in events {
            let watched_dir = |path: &Path| path == root.join(NEW) || path == root.join(CUR);
            if event.kind == Some(PathEventKind::Rescan)
                || event.path == root
                || watched_dir(&event.path)
            {
                self.rewatch = true;
                self.rescan = true;
            } else if event.path.parent().is_some_and(watched_dir) {
                self.rescan = true;
            }
        }
    }

    fn any(&self) -> bool {
        self.rescan || self.rewatch
    }
}

struct ScanDone {
    /// `None` when the scan failed.
    snapshot: Option<Snapshot>,
    rewatched: bool,
}

/// The session: set up the layout and the watcher, scan, then wait for triggers and scan again,
/// one scan at a time, until the session or the state goes away.
async fn run(
    this: WeakEntity<InboxState>,
    fs: Arc<dyn Fs>,
    dirs: InboxDirs,
    timer: RescanTimer,
    mut requests: mpsc::UnboundedReceiver<()>,
    cx: &mut AsyncApp,
) {
    let background = cx.background_executor().clone();
    let (root, events, watcher) = background
        .spawn(async move {
            ensure_layout(&dirs.root);
            // Resolved once, so that event paths compare with the watched spelling.
            let root = std::fs::canonicalize(&dirs.root).unwrap_or(dirs.root);
            let (events, watcher) = fs.watch(&root, WATCH_LATENCY).await;
            for dir in [NEW, CUR] {
                watcher.add(&root.join(dir)).log_err();
            }
            (root, events, watcher)
        })
        .await;
    if this.update(cx, |state, cx| state.watching(cx)).is_err() {
        return;
    }
    let mut events = events.fuse();
    let mut ticks = timer(&background).fuse();
    let cache = Arc::new(Mutex::new(ParseCache::new()));
    let mut trigger = Trigger {
        rescan: true,
        rewatch: false,
    };
    loop {
        let done = background
            .spawn({
                let root = root.clone();
                let watcher = watcher.clone();
                let cache = cache.clone();
                async move {
                    if trigger.rewatch {
                        rewatch(&root, watcher.as_ref());
                    }
                    ScanDone {
                        snapshot: scan(&root, &mut cache.lock()),
                        rewatched: trigger.rewatch,
                    }
                }
            })
            .await;
        if this.update(cx, |state, cx| state.finish(done, cx)).is_err() {
            return;
        }
        trigger = Trigger::default();
        while !trigger.any() {
            select_biased! {
                request = requests.next() => match request {
                    Some(()) => trigger.rescan = true,
                    None => return,
                },
                batch = events.next() => match batch {
                    Some(batch) => trigger.add_events(&root, &batch),
                    None => log::error!(
                        "the inbox watcher stopped; only the periodic rescan sees changes now"
                    ),
                },
                () = ticks.select_next_some() => trigger.rescan = true,
                complete => return,
            }
            // Whatever else arrived meanwhile joins this one rescan.
            if !drain(&mut trigger, &root, &mut requests, &mut events, &mut ticks) {
                return;
            }
        }
    }
}

/// Takes every trigger that is ready without waiting. Returns false when the session ended.
fn drain(
    trigger: &mut Trigger,
    root: &Path,
    requests: &mut mpsc::UnboundedReceiver<()>,
    events: &mut (impl FusedStream<Item = Vec<PathEvent>> + Unpin),
    ticks: &mut (impl FusedStream<Item = ()> + Unpin),
) -> bool {
    loop {
        let mut took = false;
        match requests.next().now_or_never() {
            Some(Some(())) => {
                trigger.rescan = true;
                took = true;
            }
            Some(None) => return false,
            None => {}
        }
        if !events.is_terminated()
            && let Some(Some(batch)) = events.next().now_or_never()
        {
            trigger.add_events(root, &batch);
            took = true;
        }
        if !ticks.is_terminated()
            && let Some(Some(())) = ticks.next().now_or_never()
        {
            trigger.rescan = true;
            took = true;
        }
        if !took {
            return true;
        }
    }
}

fn ensure_layout(root: &Path) {
    if let Err(error) = maildir::ensure_layout(root) {
        log::error!("cannot create the inbox at {}: {error}", root.display());
    }
}

/// Creates `new/` and `cur/` again if they went away, and adds them to the watcher afresh: a
/// directory deleted and recreated is a new directory, which the old registration no longer
/// watches.
fn rewatch(root: &Path, watcher: &dyn Watcher) {
    ensure_layout(root);
    for dir in [NEW, CUR] {
        let path = root.join(dir);
        watcher.remove(&path).log_err();
        watcher.add(&path).log_err();
    }
}

/// Scans `new/` and `cur/` and parses what changed since the last scan. Errors name files only,
/// never item text.
fn scan(root: &Path, cache: &mut ParseCache) -> Option<Snapshot> {
    let entries = match maildir::scan(root) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!("cannot scan the inbox at {}: {error}", root.display());
            return None;
        }
    };
    let results = cache.parse(&entries);
    Some(Snapshot {
        items: entries.into_iter().zip(results).collect(),
    })
}

#[cfg(test)]
mod tests;
