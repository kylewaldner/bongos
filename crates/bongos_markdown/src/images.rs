//! The image layer (design D13): an image link whose url is a local file gets a block below the
//! link's last row that shows the image, scaled down to the text width and at most
//! [`MAX_IMAGE_ROWS`] rows tall. The image's size is read on the background executor and cached
//! per path and modification time; once its block is placed the url goes into the
//! [`LoadedImages`] set shared with the conceal layer, which then hides the link while point is
//! off it. A missing, unreadable or remote image gets no block, so its link stays as text.
//!
//! Point never rests in a block: the editor's line moves clip a row inside a `Below` block forward
//! (down) or back (up), so `C-n` and `C-p` cross the image in one press.

use std::cell::RefCell;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

use bongos_markdown_model::ElementKind;
use collections::{HashMap, HashSet};
use editor::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId, RenderBlock,
};
use editor::{Anchor, Editor, MultiBufferOffset, ToOffset as _};
use gpui::{
    AppContext as _, Context, IntoElement as _, ParentElement as _, Pixels, Size, Styled as _,
    Task, WeakEntity, div, img, px, size,
};

use crate::conceals::LoadedImages;
use crate::layers::{LiveContext, LiveLayer};

/// The most rows an image block takes; a taller image is scaled down to fit.
pub const MAX_IMAGE_ROWS: u32 = 20;

/// The text width, in pixels, before the editor's first layout.
const FALLBACK_TEXT_WIDTH: f32 = 400.0;

/// The local file an image url points to: a path relative to `base` (the buffer file's
/// directory), an absolute path, or a `file://` url. Any other scheme (`https:`, `data:`) and a
/// relative path without a base give `None`.
pub fn resolve_image_path(url: &str, base: Option<&Path>) -> Option<PathBuf> {
    let url = url.trim();
    let path = match url.strip_prefix("file://") {
        Some(rest) => rest,
        None if has_scheme(url) => return None,
        None => url,
    };
    if path.is_empty() {
        return None;
    }
    let path = Path::new(path);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        base.map(|base| base.join(path))
    }
}

/// Whether `url` starts with a scheme (`name:`), the name two characters or longer so a Windows
/// drive letter is not taken for one.
fn has_scheme(url: &str) -> bool {
    let Some((name, _)) = url.split_once(':') else {
        return false;
    };
    name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// An image's size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

/// How an image of `image` pixels is shown in a text area `text_width` wide with rows
/// `line_height` tall: scaled down (never up) to the width and to [`MAX_IMAGE_ROWS`] rows, and
/// the block's height in rows.
pub fn image_rows(image: ImageSize, text_width: f32, line_height: f32) -> (u32, Size<Pixels>) {
    let width = image.width.max(1) as f32;
    let height = image.height.max(1) as f32;
    let line_height = line_height.max(1.0);
    let scale = 1f32
        .min(text_width.max(1.0) / width)
        .min(MAX_IMAGE_ROWS as f32 * line_height / height);
    let shown = size(px(width * scale), px(height * scale));
    let rows = ((height * scale) / line_height).ceil().max(1.0) as u32;
    (rows.min(MAX_IMAGE_ROWS), shown)
}

/// Reads the size of the image at `path`, reusing `cached` when the file's modification time is
/// the cached one.
fn read_size(
    path: &Path,
    cached: Option<(SystemTime, ImageSize)>,
) -> Option<(SystemTime, ImageSize)> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if let Some((time, size)) = cached
        && time == modified
    {
        return Some((time, size));
    }
    let (width, height) = image::image_dimensions(path).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((modified, ImageSize { width, height }))
}

/// The state of an image file's size read.
enum Read {
    /// The read's task; dropping it cancels the read.
    Pending {
        _task: Task<()>,
    },
    Failed,
    Loaded(ImageSize),
}

/// What the layer and its read tasks share.
#[derive(Default)]
struct Shared {
    /// The reads of the paths in the window. A path that leaves the window is dropped (a pending
    /// read with it) and read again when it comes back.
    reads: HashMap<PathBuf, Read>,
    /// Sizes by path, with the modification time they were read at.
    sizes: HashMap<PathBuf, (SystemTime, ImageSize)>,
    /// Bumped when a read finishes.
    finished: u64,
}

/// A block the layer put on the editor.
struct Placed {
    url: String,
    path: PathBuf,
    /// The end of the image link; the block sits below its row.
    anchor: Anchor,
    block: CustomBlockId,
    rows: u32,
    shown: Size<Pixels>,
}

/// An image the window wants a block for.
struct Wanted {
    url: String,
    path: PathBuf,
    end: usize,
    size: ImageSize,
}

/// What the last refresh saw: the index generation, buffer version, window, text width, line
/// height (as bits) and finished read count.
type Key = (u64, clock::Global, Range<usize>, u32, u32, u64);

/// The image layer of one editor.
pub struct ImageLayer {
    images: LoadedImages,
    shared: Rc<RefCell<Shared>>,
    placed: Vec<Placed>,
    last: Option<Key>,
    /// The text width the editor last rendered a block at, as `f32` bits (0 before the first).
    seen_width: Arc<AtomicU32>,
}

impl Default for ImageLayer {
    fn default() -> Self {
        Self::new(LoadedImages::default())
    }
}

impl ImageLayer {
    /// A layer that records the urls of placed images in `images`.
    pub fn new(images: LoadedImages) -> Self {
        Self {
            images,
            shared: Rc::default(),
            placed: Vec::new(),
            last: None,
            seen_width: Arc::default(),
        }
    }

    /// Starts reading the size of `path`; when the read finishes the editor's layers refresh.
    fn start_read(&self, path: PathBuf, cx: &mut Context<Editor>) -> Task<()> {
        let cached = self.shared.borrow().sizes.get(&path).copied();
        let read_path = path.clone();
        let read = cx.background_spawn(async move { read_size(&read_path, cached) });
        let shared = self.shared.clone();
        cx.spawn(async move |editor, cx| {
            let result = read.await;
            {
                let mut shared = shared.borrow_mut();
                let outcome = match result {
                    Some((time, size)) => {
                        shared.sizes.insert(path.clone(), (time, size));
                        Read::Loaded(size)
                    }
                    None => Read::Failed,
                };
                shared.reads.insert(path, outcome);
                shared.finished += 1;
            }
            editor
                .update(cx, |editor, cx| {
                    crate::mode::refresh_layers(editor, false, cx)
                })
                .ok();
        })
    }

    /// Removes every block and every url this layer recorded.
    fn remove_all(&mut self, editor: &mut Editor, cx: &mut Context<Editor>) {
        let blocks: HashSet<CustomBlockId> =
            self.placed.iter().map(|placed| placed.block).collect();
        for placed in self.placed.drain(..) {
            self.images.remove(&placed.url);
        }
        if !blocks.is_empty() {
            editor.remove_blocks(blocks, None, cx);
        }
    }
}

/// The directory of the editor's buffer file, when it is a local file.
fn base_dir(editor: &Editor, cx: &mut Context<Editor>) -> Option<PathBuf> {
    let buffer = editor.buffer().read(cx).as_singleton()?;
    let buffer = buffer.read(cx);
    let file = buffer.file()?.as_local()?;
    file.abs_path(cx).parent().map(Path::to_path_buf)
}

/// What every block render of one layer shares: the text width the editor last laid the blocks
/// out at (as `f32` bits, 0 before the first), and the editor, refreshed when that width changes.
#[derive(Clone)]
struct Draw {
    seen_width: Arc<AtomicU32>,
    editor: WeakEntity<Editor>,
}

impl Draw {
    fn render(&self, path: Arc<Path>, shown: Size<Pixels>) -> RenderBlock {
        let draw = self.clone();
        Arc::new(move |cx: &mut BlockContext| {
            // A width change (a resize, a split) refreshes the layers, which resize the blocks.
            let width = f32::from(cx.max_width).max(1.0).to_bits();
            if draw.seen_width.swap(width, Ordering::Relaxed) != width {
                let editor = draw.editor.clone();
                cx.app.defer(move |cx| {
                    editor
                        .update(cx, |editor, cx| {
                            crate::mode::refresh_layers(editor, false, cx)
                        })
                        .ok();
                });
            }
            div()
                .w_full()
                .h(cx.line_height * cx.height as f32)
                .overflow_hidden()
                .pl(cx.margins.gutter.full_width())
                .child(img(path.clone()).w(shown.width).h(shown.height))
                .into_any_element()
        })
    }
}

impl LiveLayer for ImageLayer {
    fn refresh(&mut self, live: &LiveContext<'_>, editor: &mut Editor, cx: &mut Context<Editor>) {
        // The width the blocks were last laid out at (the text area, or the longest line when
        // that is wider), never more than the editor's own width.
        let bounds = editor
            .last_bounds()
            .map_or(FALLBACK_TEXT_WIDTH, |bounds| f32::from(bounds.size.width));
        let seen = f32::from_bits(self.seen_width.load(Ordering::Relaxed));
        let text_width = if seen > 0.0 { seen.min(bounds) } else { bounds };
        let draw = Draw {
            seen_width: self.seen_width.clone(),
            editor: cx.weak_entity(),
        };
        let line_height = f32::from(crate::mode::live_line_height(cx));
        let key = (
            live.generation,
            live.snapshot.version().clone(),
            live.window.clone(),
            text_width.to_bits(),
            line_height.to_bits(),
            self.shared.borrow().finished,
        );
        if !live.force && self.last.as_ref() == Some(&key) {
            return;
        }
        self.last = Some(key);

        let base = base_dir(editor, cx);
        let mut wanted = Vec::new();
        let mut in_window = HashSet::default();
        let mut to_read = Vec::new();
        {
            let shared = self.shared.borrow();
            for element in live.index.elements_in(live.window.clone()) {
                let ElementKind::Image { url, .. } = &element.kind else {
                    continue;
                };
                let Some(path) = resolve_image_path(url, base.as_deref()) else {
                    continue;
                };
                match shared.reads.get(&path) {
                    Some(Read::Loaded(size)) => wanted.push(Wanted {
                        url: url.clone(),
                        path: path.clone(),
                        end: element.range.end,
                        size: *size,
                    }),
                    Some(Read::Pending { .. } | Read::Failed) => {}
                    None => {
                        if !in_window.contains(&path) {
                            to_read.push(path.clone());
                        }
                    }
                }
                in_window.insert(path);
            }
        }
        self.shared
            .borrow_mut()
            .reads
            .retain(|path, _| in_window.contains(path));
        for path in to_read {
            let task = self.start_read(path.clone(), cx);
            self.shared
                .borrow_mut()
                .reads
                .insert(path, Read::Pending { _task: task });
        }

        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let before: HashSet<String> = self.placed.iter().map(|p| p.url.clone()).collect();
        let mut kept = vec![false; wanted.len()];
        let mut removed = HashSet::default();
        let mut resized = HashMap::default();
        let mut redrawn = HashMap::default();
        self.placed.retain_mut(|placed| {
            let at = placed.anchor.to_offset(&snapshot).0;
            let found = (0..wanted.len()).position(|ix| {
                let want = &wanted[ix];
                !kept[ix] && want.end == at && want.url == placed.url && want.path == placed.path
            });
            let Some(ix) = found else {
                removed.insert(placed.block);
                return false;
            };
            kept[ix] = true;
            let (rows, shown) = image_rows(wanted[ix].size, text_width, line_height);
            if rows != placed.rows {
                resized.insert(placed.block, rows);
                placed.rows = rows;
            }
            if shown != placed.shown {
                redrawn.insert(
                    placed.block,
                    draw.render(placed.path.as_path().into(), shown),
                );
                placed.shown = shown;
            }
            true
        });

        let mut new = Vec::new();
        let mut properties = Vec::new();
        for (want, _) in wanted.into_iter().zip(kept).filter(|(_, kept)| !kept) {
            let (rows, shown) = image_rows(want.size, text_width, line_height);
            let anchor = snapshot.anchor_before(MultiBufferOffset(want.end));
            properties.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(rows),
                style: BlockStyle::Flex,
                render: draw.render(want.path.as_path().into(), shown),
                priority: 0,
            });
            new.push((want, anchor, rows, shown));
        }

        if !removed.is_empty() {
            editor.remove_blocks(removed, None, cx);
        }
        if !resized.is_empty() {
            editor.resize_blocks(resized, None, cx);
        }
        if !redrawn.is_empty() {
            editor.replace_blocks(redrawn, None, cx);
        }
        if !properties.is_empty() {
            let blocks = editor.insert_blocks(properties, None, cx);
            for ((want, anchor, rows, shown), block) in new.into_iter().zip(blocks) {
                self.placed.push(Placed {
                    url: want.url,
                    path: want.path,
                    anchor,
                    block,
                    rows,
                    shown,
                });
            }
        }

        let after: HashSet<String> = self.placed.iter().map(|p| p.url.clone()).collect();
        for url in before.difference(&after) {
            self.images.remove(url);
        }
        for url in after {
            self.images.insert(url);
        }
    }

    fn clear(&mut self, editor: &mut Editor, cx: &mut Context<Editor>) {
        self.remove_all(editor, cx);
        self.shared.borrow_mut().reads.clear();
        self.last = None;
    }
}
