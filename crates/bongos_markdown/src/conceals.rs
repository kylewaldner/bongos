//! The conceal layer: hidden markup with reveal at point. Over the window it hides the markup the
//! selections do not reveal, and draws list markers, task boxes, quote markers, rules, fence lines
//! and table delimiter rows as glyphs. Conceals are display-only folds tagged [`MarkdownConceal`]:
//! the buffer's text never changes, and every refresh replaces all of the editor's conceals in one
//! [`Editor::set_conceals`] call, so a conceal removed by something else comes back at the next
//! refresh.

use std::any::TypeId;
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bongos_markdown_model::{Conceal, ConcealKind, RevealSelection, plan, revealed};
use collections::HashSet;
use editor::display_map::{Crease, FoldId, FoldPlaceholder};
use editor::{Anchor, Editor, MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use gpui::{
    AnyElement, App, Context, Hsla, IntoElement as _, ParentElement as _, Pixels, SharedString,
    Styled as _, div, px,
};
use theme::ActiveTheme as _;

use crate::layers::{LiveContext, LiveLayer};

/// The fold type tag of the markdown conceals.
pub enum MarkdownConceal {}

/// The type tag of the markdown conceals, as [`editor::register_conceal_tag`] takes it.
pub fn conceal_tag() -> TypeId {
    TypeId::of::<MarkdownConceal>()
}

/// The image urls whose images have loaded, shared by an editor's layers. An image is hidden
/// whole only once its url is here.
#[derive(Clone, Default)]
pub struct LoadedImages(Rc<RefCell<HashSet<String>>>);

impl LoadedImages {
    pub fn contains(&self, url: &str) -> bool {
        self.0.borrow().contains(url)
    }

    /// Adds `url`; true when it was not there.
    pub fn insert(&self, url: impl Into<String>) -> bool {
        self.0.borrow_mut().insert(url.into())
    }
}

const BULLET: &str = "•";
const UNCHECKED: &str = "☐";
const CHECKED: &str = "☑";
const QUOTE_BAR_WIDTH: f32 = 3.0;
/// The room after a bullet, a checkbox or a quote bar, before the item's text.
const GLYPH_GAP: f32 = 6.0;
/// The rule width before the editor's first layout.
const FALLBACK_RULE_WIDTH: f32 = 400.0;

/// The theme colors the glyphs are drawn in.
#[derive(Clone, Copy, PartialEq)]
struct Look {
    text: Hsla,
    muted: Hsla,
    border: Hsla,
    border_variant: Hsla,
}

impl Look {
    fn current(cx: &App) -> Self {
        let colors = cx.theme().colors();
        Self {
            text: colors.text,
            muted: colors.text_muted,
            border: colors.border,
            border_variant: colors.border_variant,
        }
    }
}

/// The conceal layer of one editor.
pub struct ConcealLayer {
    images: LoadedImages,
    /// The buffer text at a version, kept so a selection change does not copy it again.
    text: Option<(clock::Global, Arc<str>)>,
    /// The conceals put on the editor, as anchors.
    applied: Vec<(Range<Anchor>, ConcealKind)>,
    look: Option<Look>,
    /// The text area width in pixels (as `f32` bits), read by rules when they draw.
    width: Arc<AtomicU32>,
}

impl Default for ConcealLayer {
    fn default() -> Self {
        Self::new(LoadedImages::default())
    }
}

impl ConcealLayer {
    pub fn new(images: LoadedImages) -> Self {
        Self {
            images,
            text: None,
            applied: Vec::new(),
            look: None,
            width: Arc::new(AtomicU32::new(FALLBACK_RULE_WIDTH.to_bits())),
        }
    }

    /// The image urls this layer treats as loaded.
    pub fn images(&self) -> &LoadedImages {
        &self.images
    }

    fn text(&mut self, snapshot: &language::BufferSnapshot) -> Arc<str> {
        match &self.text {
            Some((version, text)) if version == snapshot.version() => text.clone(),
            _ => {
                let text: Arc<str> = snapshot.text().into();
                self.text = Some((snapshot.version().clone(), text.clone()));
                text
            }
        }
    }
}

fn whole(snapshot: &MultiBufferSnapshot) -> Range<Anchor> {
    snapshot.anchor_before(MultiBufferOffset(0))..snapshot.anchor_after(snapshot.len())
}

fn offsets(snapshot: &MultiBufferSnapshot, range: &Range<Anchor>) -> Range<usize> {
    range.start.to_offset(snapshot).0..range.end.to_offset(snapshot).0
}

/// The editor's conceal folds, as buffer offsets, in order.
fn present(editor: &mut Editor, cx: &mut Context<Editor>) -> Vec<Range<usize>> {
    let display = editor.display_snapshot(cx);
    let buffer = display.buffer_snapshot();
    let tag = Some(conceal_tag());
    display
        .folds_in_range(MultiBufferOffset(0)..buffer.len())
        .filter(|fold| fold.placeholder.type_tag == tag)
        .map(|fold| offsets(buffer, &(fold.range.start..fold.range.end)))
        .collect()
}

type Render = Arc<dyn Send + Sync + Fn(FoldId, Range<Anchor>, &mut App) -> AnyElement>;

fn placeholder(kind: ConcealKind, look: Look, width: &Arc<AtomicU32>) -> FoldPlaceholder {
    let width = width.clone();
    let draw: Render = match kind {
        ConcealKind::Hide | ConcealKind::FenceLine => render(|| gpui::Empty.into_any_element()),
        ConcealKind::Bullet => glyph(BULLET, look.text),
        ConcealKind::Checkbox(false) => glyph(UNCHECKED, look.text),
        ConcealKind::Checkbox(true) => glyph(CHECKED, look.muted),
        ConcealKind::QuoteBar => render(move || {
            div()
                .h_full()
                .pr(px(GLYPH_GAP))
                .child(div().h_full().w(px(QUOTE_BAR_WIDTH)).bg(look.border))
                .into_any_element()
        }),
        ConcealKind::Rule | ConcealKind::DelimiterRow => {
            let color = if kind == ConcealKind::Rule {
                look.border
            } else {
                look.border_variant
            };
            render(move || {
                let width = Pixels::from(f32::from_bits(width.load(Ordering::Relaxed)));
                div()
                    .h_full()
                    .w(width)
                    .flex()
                    .items_center()
                    .child(div().w_full().h(px(1.0)).bg(color))
                    .into_any_element()
            })
        }
    };
    FoldPlaceholder {
        render: draw,
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(conceal_tag()),
        collapsed_text: None,
    }
}

/// A placeholder render that ignores the fold and its range.
fn render(draw: impl Fn() -> AnyElement + Send + Sync + 'static) -> Render {
    Arc::new(move |_: FoldId, _: Range<Anchor>, _: &mut App| draw())
}

fn glyph(text: &'static str, color: Hsla) -> Render {
    render(move || {
        div()
            .pr(px(GLYPH_GAP))
            .text_color(color)
            .child(SharedString::new_static(text))
            .into_any_element()
    })
}

impl LiveLayer for ConcealLayer {
    fn refresh(&mut self, live: &LiveContext<'_>, editor: &mut Editor, cx: &mut Context<Editor>) {
        if let Some(bounds) = editor.last_bounds() {
            let width = f32::from(bounds.size.width).max(1.0);
            self.width.store(width.to_bits(), Ordering::Relaxed);
        }
        let text = self.text(live.snapshot);
        let display = editor.display_snapshot(cx);
        let selections: Vec<RevealSelection> = editor
            .selections
            .all::<MultiBufferOffset>(&display)
            .iter()
            .map(|selection| {
                RevealSelection::new(selection.start.0..selection.end.0, selection.head().0)
            })
            .collect();
        drop(display);
        let shown = revealed(live.index, &text, &selections);
        let images = self.images.clone();
        let wanted = plan(live.index, &text, live.window.clone(), &shown, &|url| {
            images.contains(url)
        });

        let look = Look::current(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let applied: Vec<Conceal> = self
            .applied
            .iter()
            .map(|(range, kind)| Conceal {
                range: offsets(&snapshot, range),
                kind: *kind,
            })
            .collect();
        let unchanged = !live.force
            && self.look == Some(look)
            && applied == wanted
            && present(editor, cx)
                == applied
                    .iter()
                    .map(|conceal| conceal.range.clone())
                    .collect::<Vec<_>>();
        if unchanged {
            return;
        }

        let mut creases = Vec::with_capacity(wanted.len());
        let mut kept = Vec::with_capacity(wanted.len());
        for conceal in &wanted {
            let range = snapshot.anchor_after(MultiBufferOffset(conceal.range.start))
                ..snapshot.anchor_before(MultiBufferOffset(conceal.range.end));
            creases.push(Crease::simple(
                range.clone(),
                placeholder(conceal.kind, look, &self.width),
            ));
            kept.push((range, conceal.kind));
        }
        editor.set_conceals(conceal_tag(), whole(&snapshot), creases, cx);
        self.applied = kept;
        self.look = Some(look);
    }

    fn clear(&mut self, editor: &mut Editor, cx: &mut Context<Editor>) {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        editor.set_conceals(conceal_tag(), whole(&snapshot), Vec::new(), cx);
        self.applied.clear();
        self.look = None;
        self.text = None;
    }
}
