//! BlockTree → gpui elements.
//!
//! Numbers drive layout (font sizes, line heights, paddings — all constants
//! here); colors are paint. Code blocks render per-line so their height is
//! exactly `lines × line_height`, and syntax highlighting arrives later as
//! recolored `TextRun`s on the identical mono font — layout never changes
//! (mugen's "highlight is pure paint"). Streaming fade-in is a per-appended-
//! chunk opacity veil over the text runs (see [`super::veil`]) — opacity only,
//! zero translate, applied after layout-relevant properties are fixed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    AnyElement, BorderStyle, Bounds, FontStyle, FontWeight, Hsla, InteractiveText, ObjectFit,
    SharedString, StyledText, TextRun, UnderlineStyle, Window, canvas, div, font, img, point,
    prelude::*, px, quad, size,
};

use crate::theme::Theme;

use super::highlight::{Token, TokenClass};
use super::parser::{Block, BlockTree, InlineRun, TableAlign};
use super::veil::{RowVeil, apply_veil, slice_spans};

/// Gap between markdown blocks inside one message (comet mdBlockGap).
pub const MD_BLOCK_GAP: f32 = 12.0;
/// Body text size / line height (comet: 14px / 22px).
pub const MD_TEXT_SIZE: f32 = 14.0;
pub const MD_LINE_HEIGHT: f32 = 22.0;
/// Code block metrics — height is `lines × CODE_LINE_HEIGHT + padding + header`.
pub const CODE_TEXT_SIZE: f32 = 12.5;
pub const CODE_LINE_HEIGHT: f32 = 18.0;
pub const CODE_PADDING_X: f32 = 12.0;
pub const CODE_PADDING_Y: f32 = 10.0;

// Table metrics — a port of mugen-markdown 0.6.2's `TableBlock` under comet's
// resolved md theme. The design is frameless ("flat hairline"): 1px horizontal
// rules under the header and between rows are the only chrome — no outer box,
// no header fill, no corner radius (theme: headerBackground transparent,
// radius 0). Cells use the body scale (14/22) with a uniform 12px padding;
// the header row is weight-700 per `table.headerWeight`.
/// Uniform cell padding in px (comet `table.cellPadding`).
pub const TABLE_CELL_PADDING: f32 = 12.0;
/// Hairline between rows in px (comet `table.gap`).
pub const TABLE_DIVIDER: f32 = 1.0;
/// Header row font weight (comet `table.headerWeight` = 700).
pub const TABLE_HEADER_WEIGHT: FontWeight = FontWeight::BOLD;
/// Floor for a column's max-content share, so a short column ("1k") beside a
/// prose column keeps a readable width (mugen `MIN_COLUMN_CONTENT`).
pub const TABLE_MIN_COLUMN_CONTENT: f32 = 48.0;
/// Minimum rendered column width in px, padding included (comet
/// `table.minColumnWidth`). Naturally narrower columns keep their content
/// width; wider ones wrap down to this floor, then the table scrolls.
pub const TABLE_MIN_COLUMN_WIDTH: f32 = 96.0;
/// Hairline tone (comet md theme `table.borderColor`: rgba(255,255,255,0.1)).
pub fn table_hairline() -> Hsla {
    crate::theme::hairline(0.10)
}

/// Options for one rendered tree (a transcript row or a whole live message).
pub struct RenderOptions {
    /// Stable row key — prefixes element ids (scroll state, animations).
    pub row_key: SharedString,
    /// Streaming veil state for a live row: newly appended text fades in via
    /// paint-only run opacity, keyed per (element, chunk offset) so each chunk
    /// fades exactly once. `None` renders without fades (completed rows).
    pub veil: Option<Rc<RefCell<RowVeil>>>,
    /// Flatten/shape input cache (see [`RenderCache`]): settled blocks reuse
    /// their flat text + runs across frames instead of rebuilding them — the
    /// per-frame cost of a fading live row stays O(tail block), flat in the
    /// total reply length. `None` rebuilds every pass.
    pub cache: Option<Rc<RefCell<RenderCache>>>,
    /// Frame timestamp driving veil opacities (one clock per render pass).
    pub now: Instant,
    /// Code-block copy-button plumbing (round 9): `None` renders no button
    /// (previews outside the transcript).
    pub copy: Option<CopyUi>,
    /// Inline-image plumbing: `None` renders `![alt](url)` as a plain link.
    pub image: Option<ImageUi>,
    /// Chat working directory used to resolve relative file links. `None`
    /// leaves relative destinations untouched (markdown previews have no
    /// owning workspace).
    pub link_cwd: Option<SharedString>,
}

/// Inline `![alt](url)` plumbing: the transcript resolves URLs against the
/// cross-device attachment cache (claiming loads in its own pre-pass) and owns
/// the full-size preview overlay. `None` (previews outside the transcript)
/// renders image runs as plain links to the same URL.
#[derive(Clone)]
pub struct ImageUi {
    /// URL → current cache snapshot; `None` means "not renderable inline"
    /// (remote/unsupported source) and the run stays in the text flow.
    pub resolve: Rc<ImageResolver>,
    /// Open the clicked, already-decoded image in the preview overlay.
    pub open: Rc<ImageOpener>,
}

/// URL → current cache snapshot (see [`ImageUi::resolve`]).
pub type ImageResolver = dyn Fn(&str) -> Option<crate::attachments::AttachmentSnapshot>;
/// Open a decoded image in the host's preview overlay.
pub type ImageOpener =
    dyn Fn(crate::attachments::CachedAttachmentImage, &mut Window, &mut gpui::App);

pub type CopyHandler = dyn Fn(usize, SharedString, &mut Window, &mut gpui::App);

/// Copy-button wiring for one row's code blocks: the handler writes the code
/// to the clipboard and flips a transient per-row "Copied" state owned by the
/// transcript entity; `copied_ix` is the block currently showing feedback.
#[derive(Clone)]
pub struct CopyUi {
    pub handler: Rc<CopyHandler>,
    pub copied_ix: Option<usize>,
}

impl RenderOptions {
    /// Options for a completed (non-streaming) row — no veil, no cache.
    pub fn settled(row_key: SharedString) -> Self {
        Self {
            row_key,
            veil: None,
            cache: None,
            now: Instant::now(),
            copy: None,
            image: None,
            link_cwd: None,
        }
    }
}

/// Cross-frame cache of flatten results, keyed by
/// `(row key, top-level block ix, element discriminator)`.
///
/// During a streaming fade the live row re-renders every frame; without the
/// cache each frame re-derives every block's flat `String` + `TextRun`s —
/// O(reply length) per frame, growing through long replies. The incremental
/// parser only ever touches a suffix of the top-level blocks
/// ([`super::parser::IncrementalParser::stable_prefix_blocks`]), so everything
/// below that boundary is byte-identical and its flatten result (and, via
/// gpui's line-layout cache keyed on identical text+runs, its shaping) can be
/// reused as-is. `SharedString`/`Rc` make the reuse O(1) per block.
/// Cached runs carry a resolved [`gpui::Hsla`] per span, so an entry is only
/// valid for the palette that produced it — content-only keys silently serve
/// dark-mode text onto a light background after an appearance switch.
/// [`RenderCache::sync_palette`] drops everything when the palette moves.
#[derive(Default)]
pub struct RenderCache {
    // Group by row so invalidating a streaming tail never scans settled history.
    flats: HashMap<SharedString, HashMap<(usize, usize), Rc<FlatText>>>,
    code: HashMap<SharedString, HashMap<(usize, usize), Rc<CachedCode>>>,
    /// The [`crate::theme::theme_generation`] these entries were shaped under.
    generation: u32,
}

/// Cached per-line code runs (validity: code length + highlight identity).
pub struct CachedCode {
    /// Retain the copy payload once, rather than cloning all source bytes per frame.
    code_text: SharedString,
    /// Slice-pointer identity + len of the highlight Arc that produced this.
    hl_key: (usize, usize),
    lines: Vec<(SharedString, Vec<TextRun>)>,
    content_width: f32,
}

impl RenderCache {
    /// Drop every cached entry for `row`.
    pub fn invalidate_row(&mut self, row: &str) {
        self.flats.remove(row);
        self.code.remove(row);
    }

    pub fn clear(&mut self) {
        self.flats.clear();
        self.code.clear();
    }

    /// Drop every entry if the palette changed since they were shaped. Cheap
    /// enough (one relaxed atomic load) to call on every cache access.
    fn sync_palette(&mut self) {
        let generation = crate::theme::theme_generation();
        if self.generation != generation {
            self.clear();
            self.generation = generation;
        }
    }
}

/// Per-line highlight tokens for a code block, or `None` while pending.
pub type CodeHighlight<'a> = Option<&'a [Vec<Token>]>;

/// Render a whole tree stacked with the md block gap. `highlight` resolves
/// tokens for a top-level block index (code blocks only).
pub fn render_tree(
    tree: &BlockTree,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
    highlight: &dyn Fn(usize) -> Option<std::sync::Arc<Vec<Vec<Token>>>>,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(MD_BLOCK_GAP))
        .children(tree.blocks.iter().enumerate().map(|(ix, top)| {
            let lines = highlight(ix);
            render_block(
                &top.block,
                ix,
                ix,
                opts,
                theme,
                window,
                lines.as_deref().map(|l| &l[..]),
            )
        }))
        .into_any_element()
}

/// Render one block (top-level or nested). `top_ix` is the enclosing top-level
/// block index (cache invalidation scope); `ix` the per-element discriminator.
#[allow(clippy::too_many_arguments)]
pub fn render_block(
    block: &Block,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
    highlight: CodeHighlight,
) -> AnyElement {
    match block {
        Block::Paragraph { runs } => {
            // Inline images lift out of the text flow into real pixels when
            // the host wired an [`ImageUi`] and the source resolves; anything
            // else (previews, remote URLs) stays a link-styled run.
            let has_images = opts.image.as_ref().is_some_and(|ui| {
                runs.iter().any(|run| {
                    run.style
                        .image
                        .as_deref()
                        .is_some_and(|url| (ui.resolve)(url).is_some())
                })
            });
            if has_images {
                paragraph_with_images(runs, top_ix, ix, opts, theme)
            } else {
                text_element(
                    runs,
                    MD_TEXT_SIZE,
                    MD_LINE_HEIGHT,
                    false,
                    top_ix,
                    ix,
                    opts,
                    theme,
                )
            }
        }
        Block::Heading { level, runs } => {
            let (size, line) = heading_metrics(*level);
            text_element(runs, size, line, true, top_ix, ix, opts, theme)
        }
        Block::DisplayMath { runs } => div()
            .w_full()
            .flex()
            .justify_center()
            .py(px(2.0))
            .child(text_element(
                runs, 16.0, 26.0, false, top_ix, ix, opts, theme,
            ))
            .into_any_element(),
        Block::CodeBlock { language, code } => {
            // ```mermaid``` upgrades to a native diagram when the source
            // parses (streaming-tolerant); otherwise it stays a code block.
            let mermaid = language
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case("mermaid"))
                .then(|| super::mermaid::render::mermaid_block(code, ix, opts, theme, window))
                .flatten();
            match mermaid {
                Some(el) => el,
                None => render_code_block(
                    language.as_deref(),
                    code,
                    top_ix,
                    ix,
                    opts,
                    theme,
                    window,
                    highlight,
                ),
            }
        }
        Block::BlockQuote { children } => div()
            // Accent-tinted quote: indigo rail + a whisper of the same hue
            // behind it (the inline-code treatment, dialed down).
            .border_l_2()
            .border_color(theme.accent.opacity(0.6))
            .bg(theme.accent.opacity(0.05))
            .rounded_tr(px(6.0))
            .rounded_br(px(6.0))
            .pl(px(12.0))
            .pr(px(10.0))
            .py(px(6.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .text_color(theme.text_muted)
            .children(children.iter().enumerate().map(|(ci, child)| {
                render_block(child, top_ix, ix * 100 + ci, opts, theme, window, None)
            }))
            .into_any_element(),
        Block::List {
            ordered_start,
            items,
        } => div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(items.iter().enumerate().map(|(item_ix, item)| {
                // Accent markers (the inline-code hue): ordered numbers as
                // tinted text, unordered as a REAL 5px disc — the glyph "•"
                // reads too small at 14px.
                let marker: gpui::AnyElement = match ordered_start {
                    Some(start) => div()
                        .flex_none()
                        .min_w(px(18.0))
                        .text_size(px(MD_TEXT_SIZE))
                        .line_height(px(MD_LINE_HEIGHT))
                        .text_color(theme.accent.opacity(0.85))
                        .child(SharedString::from(format!("{}.", start + item_ix as u64)))
                        .into_any_element(),
                    None => div()
                        .flex_none()
                        .min_w(px(18.0))
                        // Center the disc on the first text line's cap band.
                        .h(px(MD_LINE_HEIGHT))
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .ml(px(1.0))
                                .w(px(5.0))
                                .h(px(5.0))
                                .rounded_full()
                                .bg(theme.accent.opacity(0.85)),
                        )
                        .into_any_element(),
                };
                div().flex().flex_row().gap(px(8.0)).child(marker).child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .children(item.iter().enumerate().map(|(ci, child)| {
                            render_block(
                                child,
                                top_ix,
                                ix * 100 + item_ix * 10 + ci,
                                opts,
                                theme,
                                window,
                                None,
                            )
                        })),
                )
            }))
            .into_any_element(),
        Block::Table {
            header,
            rows,
            align,
        } => render_table(header, rows, align, top_ix, ix, opts, theme, window),
        Block::Rule => div()
            .h(px(1.0))
            .w_full()
            .bg(theme.border)
            .into_any_element(),
    }
}

/// A paragraph whose image runs resolve to pixels: text segments and images
/// stack in document order. Segment element ids extend the paragraph's own
/// discriminator (the nested-block convention), so flatten caches stay unique
/// per (row, top block, element).
fn paragraph_with_images(
    runs: &[super::parser::InlineRun],
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> AnyElement {
    let ui = opts.image.as_ref().expect("checked by caller");
    let mut parts: Vec<AnyElement> = Vec::new();
    let mut seg: Vec<super::parser::InlineRun> = Vec::new();
    let mut child_ix = ix * 100;
    let flush = |seg: &mut Vec<super::parser::InlineRun>,
                 parts: &mut Vec<AnyElement>,
                 child_ix: &mut usize| {
        if seg.is_empty() {
            return;
        }
        *child_ix += 1;
        parts.push(text_element(
            seg,
            MD_TEXT_SIZE,
            MD_LINE_HEIGHT,
            false,
            top_ix,
            *child_ix,
            opts,
            theme,
        ));
        seg.clear();
    };
    for run in runs {
        let resolved = run.style.image.as_deref().and_then(|url| (ui.resolve)(url));
        match resolved {
            Some(snapshot) => {
                flush(&mut seg, &mut parts, &mut child_ix);
                child_ix += 1;
                parts.push(inline_image_element(
                    &run.text, snapshot, child_ix, ui, opts, theme,
                ));
            }
            None => seg.push(run.clone()),
        }
    }
    flush(&mut seg, &mut parts, &mut child_ix);
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .children(parts)
        .into_any_element()
}

/// One resolved inline image: the decoded picture at natural size (capped),
/// a static skeleton while loading, or a dashed "unavailable" chip after the
/// cache gave up. Clicking a loaded image opens the transcript's preview.
fn inline_image_element(
    alt: &str,
    snapshot: crate::attachments::AttachmentSnapshot,
    elem_ix: usize,
    ui: &ImageUi,
    opts: &RenderOptions,
    theme: &Theme,
) -> AnyElement {
    use crate::attachments::AttachmentSnapshot;
    match snapshot {
        AttachmentSnapshot::Loaded(image) => {
            let open = ui.open.clone();
            let preview = image.clone();
            div()
                .flex()
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "{}-img-{elem_ix}",
                            opts.row_key
                        )))
                        .max_w_full()
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .cursor_pointer()
                        .on_click(move |_, window, cx| (open)(preview.clone(), window, cx))
                        .child(
                            img(image.image.clone())
                                .object_fit(ObjectFit::Contain)
                                .max_w_full()
                                .max_h(px(360.0)),
                        ),
                )
                .into_any_element()
        }
        // Loading: a fixed-footprint skeleton (the image's own size settles on
        // decode) — static on purpose, this path has no animation clock.
        AttachmentSnapshot::Loading => div()
            .w(px(300.0))
            .h(px(160.0))
            .rounded(px(10.0))
            .border_1()
            .border_color(crate::theme::hairline(0.08))
            .bg(crate::theme::ink(0.055))
            .into_any_element(),
        AttachmentSnapshot::Error { .. } => div()
            .flex()
            .child(
                div()
                    .rounded(px(10.0))
                    .border_1()
                    .border_dashed()
                    .border_color(crate::theme::hairline(0.14))
                    .bg(crate::theme::ink(0.025))
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(px(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!("{alt} — image unavailable"))),
            )
            .into_any_element(),
    }
}

/// Tight monochrome heading scale (comet: h2 ≈ 16px semibold; headings step
/// down quickly toward body size).
fn heading_metrics(level: u8) -> (f32, f32) {
    match level {
        1 => (19.0, 27.0),
        2 => (16.0, 24.0),
        3 => (15.0, 22.0),
        _ => (14.0, 22.0),
    }
}

/// Shared per-column table geometry (port of mugen `tableColumns`).
pub struct TableColumns {
    /// Per-column max-content width, padding included.
    pub naturals: Vec<f32>,
    /// Per-column minimum width, padding included = `min(natural, minColumnWidth)`.
    pub minimums: Vec<f32>,
    /// Σ minimums — the width below which the table stops shrinking and scrolls.
    pub min_table_width: f32,
}

/// Resolve column geometry from measured per-column max-content widths
/// (content only — padding is added here, as the source adds `2 * cellPadding`).
pub fn table_columns(content_widths: &[f32]) -> TableColumns {
    let naturals: Vec<f32> = content_widths
        .iter()
        .map(|w| w.max(TABLE_MIN_COLUMN_CONTENT) + 2.0 * TABLE_CELL_PADDING)
        .collect();
    let minimums: Vec<f32> = naturals
        .iter()
        .map(|n| n.min(TABLE_MIN_COLUMN_WIDTH))
        .collect();
    let min_table_width = minimums.iter().sum();
    TableColumns {
        naturals,
        minimums,
        min_table_width,
    }
}

/// Element/cache discriminator for a table cell (row-major under the block ix).
fn table_cell_ix(ix: usize, r: usize, c: usize) -> usize {
    ix * 100_000 + r * 100 + c
}

/// A GFM table — a port of mugen-markdown's `TableBlock` under comet's md
/// theme (see the `TABLE_*` constants).
///
/// Column widths resolve exactly the way the source's CSS does: each cell is
/// `flex: <max-content> <max-content> 0; min-width: min(max-content, 96px)`,
/// so widths are content-proportional with a readable per-column floor.
/// Naturals come from shaping each cell's runs unwrapped (gpui's line-layout
/// cache makes repeat frames cheap); the flex resolution itself is Taffy's —
/// the same algorithm as the web's. When even the floors no longer fit, the
/// rows overflow the viewport and the table scrolls horizontally instead of
/// crushing every column into per-character wrapping.
#[allow(clippy::too_many_arguments)]
fn render_table(
    header: &[Vec<InlineRun>],
    rows: &[Vec<Vec<InlineRun>>],
    align: &[TableAlign],
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
) -> AnyElement {
    // Header row first, mirroring the source's `rows` shape (rows may be ragged).
    let all: Vec<&[Vec<InlineRun>]> = std::iter::once(header)
        .filter(|h| !h.is_empty())
        .map(|h| h as &[Vec<InlineRun>])
        .chain(rows.iter().map(|r| r.as_slice()))
        .collect();
    let cols = all.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return gpui::Empty.into_any_element();
    }
    let has_header = !header.is_empty();

    // Flatten every cell (cache-aware) and take per-column max-content widths.
    let text_system = window.text_system();
    let mut flats: Vec<Vec<Option<Rc<FlatText>>>> = Vec::with_capacity(all.len());
    let mut content = vec![0.0f32; cols];
    for (r, row) in all.iter().enumerate() {
        let weight = if has_header && r == 0 {
            TABLE_HEADER_WEIGHT
        } else {
            FontWeight::NORMAL
        };
        let mut out: Vec<Option<Rc<FlatText>>> = Vec::with_capacity(cols);
        for (c, natural) in content.iter_mut().enumerate() {
            let Some(runs) = row.get(c) else {
                out.push(None);
                continue;
            };
            let flat = flatten_cached(runs, weight, top_ix, table_cell_ix(ix, r, c), opts, theme);
            if !flat.text.is_empty() {
                // Cell sources are single-line; guard anyway (same byte count,
                // so the runs still cover the text exactly).
                let line: SharedString = if flat.text.contains('\n') {
                    flat.text.replace('\n', " ").into()
                } else {
                    flat.text.clone()
                };
                let width = f32::from(
                    text_system
                        .shape_line(line, px(MD_TEXT_SIZE), &flat.runs, None)
                        .width(),
                );
                if width > *natural {
                    *natural = width;
                }
            }
            out.push(Some(flat));
        }
        flats.push(out);
    }
    let geo = table_columns(&content);

    // Frameless flat-hairline chrome: 1px rules under the header and between
    // rows are the only paint (`table.gap` = 1, borderColor white@10%); the
    // theme's headerBackground is transparent and its radius 0, so there is no
    // header fill, outer box, or rounding.
    let hairline = table_hairline();
    let mut inner = div()
        .flex()
        .flex_col()
        .w_full()
        .min_w(px(geo.min_table_width));
    for (r, row) in flats.iter().enumerate() {
        if r > 0 {
            inner = inner.child(div().flex_none().h(px(TABLE_DIVIDER)).w_full().bg(hairline));
        }
        let mut row_el = div().flex().flex_row();
        for (c, cell_flat) in row.iter().enumerate() {
            let mut cell = div()
                .flex_grow(geo.naturals[c])
                .flex_shrink(geo.naturals[c])
                .flex_basis(px(0.0))
                .min_w(px(geo.minimums[c]))
                .p(px(TABLE_CELL_PADDING))
                .text_size(px(MD_TEXT_SIZE))
                .line_height(px(MD_LINE_HEIGHT));
            cell = match align.get(c).copied().unwrap_or_default() {
                TableAlign::Left => cell,
                TableAlign::Center => cell.text_center(),
                TableAlign::Right => cell.text_right(),
            };
            if let Some(flat) = cell_flat {
                cell = cell.child(flat_text_element(
                    flat,
                    table_cell_ix(ix, r, c),
                    opts,
                    theme,
                ));
            }
            row_el = row_el.child(cell);
        }
        inner = inner.child(row_el);
    }

    // The horizontal scroller — when the floors exceed the viewport the inner
    // block keeps `min_table_width` and this viewport scrolls it.
    let scroll_id: SharedString = format!("{}-table{ix}", opts.row_key).into();
    div()
        .id(scroll_id)
        .w_full()
        .overflow_x_scroll()
        .child(inner)
        .into_any_element()
}

/// Flattened inline runs: one string + gpui `TextRun`s + clickable link ranges
/// + inline-code ranges (their rounded washes are painted by a canvas UNDER
///   the text — `TextRun::background_color` can only paint square boxes).
///   `text` is a `SharedString` so cached reuse across frames is an Arc clone.
pub struct FlatText {
    pub text: SharedString,
    pub runs: Vec<TextRun>,
    pub links: Vec<(Range<usize>, String)>,
    pub code_ranges: Vec<Range<usize>>,
}

/// Inline-code tint (round 9): the original is neutral (chat-view.tsx mdTheme
/// `inlineCode: #f0f0f0 on white/8%`), but the user asked for "a nice purple"
/// — violet-300 text over a violet-400 wash, readable on the #060606 panel.
pub fn inline_code_text(theme: &Theme) -> Hsla {
    theme.code_text // violet-300
}
pub fn inline_code_wash(theme: &Theme) -> Hsla {
    theme.code_wash // violet-400/12
}
/// Rounded-wash geometry: small radius on a slightly inset box (paint-only —
/// x extends 2px past the glyphs, y insets 2px from the 22px line box).
pub const INLINE_CODE_RADIUS: f32 = 4.5;
pub const INLINE_CODE_PAD_X: f32 = 2.0;
pub const INLINE_CODE_INSET_Y: f32 = 2.0;

/// Render the TeX subset emitted in ordinary agent prose as readable Unicode.
/// Pulldown-cmark supplies math boundaries; this keeps Comet dependency-free
/// while covering operators, Greek symbols, scripts, fractions, and roots.
fn math_to_text(source: &str) -> String {
    MathText::new(source.trim()).render_until(None)
}

struct MathText<'a> {
    source: &'a str,
    pos: usize,
}

impl<'a> MathText<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, pos: 0 }
    }

    fn render_until(&mut self, end: Option<char>) -> String {
        let mut out = String::with_capacity(self.source.len());
        while let Some(ch) = self.peek() {
            if Some(ch) == end {
                self.next();
                break;
            }
            match ch {
                '\\' => {
                    self.next();
                    self.render_command(&mut out);
                }
                '{' => {
                    self.next();
                    out.push_str(&self.render_until(Some('}')));
                }
                '}' => {
                    self.next();
                    out.push('}');
                }
                '^' | '_' => {
                    self.next();
                    self.render_script(ch, &mut out);
                }
                '&' => {
                    self.next();
                    Self::push_space(&mut out);
                }
                c if c.is_whitespace() => {
                    self.next();
                    Self::push_space(&mut out);
                }
                _ => {
                    out.push(self.next().expect("peeked character"));
                }
            }
        }
        out
    }

    fn render_command(&mut self, out: &mut String) {
        let Some(next) = self.peek() else {
            out.push('\\');
            return;
        };
        if !next.is_ascii_alphabetic() {
            self.next();
            match next {
                ',' => out.push('\u{2009}'),
                ':' | ';' => Self::push_space(out),
                '!' => {}
                '\\' => out.push('\n'),
                '{' | '}' | '$' | '%' | '_' | '#' | '&' => out.push(next),
                _ => {
                    out.push('\\');
                    out.push(next);
                }
            }
            return;
        }

        let start = self.pos;
        while self.peek().is_some_and(|ch| ch.is_ascii_alphabetic()) {
            self.next();
        }
        let command = &self.source[start..self.pos];
        match command {
            "frac" => {
                let checkpoint = self.pos;
                if let (Some(numerator), Some(denominator)) = (self.group(), self.group()) {
                    Self::push_fraction(out, &numerator, &denominator);
                } else {
                    self.pos = checkpoint;
                    out.push_str("\\frac");
                }
            }
            "sqrt" => {
                if let Some(value) = self.group() {
                    out.push('√');
                    if value.chars().count() == 1 {
                        out.push_str(&value);
                    } else {
                        out.push('(');
                        out.push_str(&value);
                        out.push(')');
                    }
                } else {
                    out.push('√');
                }
            }
            "text" | "textrm" | "mathrm" | "mathbf" | "mathit" | "mathsf" | "mathtt"
            | "operatorname" => {
                if let Some(value) = self.group() {
                    out.push_str(&value);
                }
            }
            "begin" | "end" => {
                let _ = self.group();
            }
            "left" | "right" => {}
            "quad" => out.push_str("  "),
            "qquad" => out.push_str("    "),
            _ => {
                if let Some(symbol) = math_symbol(command) {
                    out.push_str(symbol);
                } else {
                    out.push('\\');
                    out.push_str(command);
                }
            }
        }
    }

    fn render_script(&mut self, marker: char, out: &mut String) {
        let value = self.group().or_else(|| self.next().map(String::from));
        let Some(value) = value else {
            out.push(marker);
            return;
        };
        let mapped: Option<String> = value.chars().map(|ch| script_char(marker, ch)).collect();
        if let Some(mapped) = mapped {
            out.push_str(&mapped);
        } else {
            out.push(marker);
            if value.chars().count() > 1 {
                out.push('(');
                out.push_str(&value);
                out.push(')');
            } else {
                out.push_str(&value);
            }
        }
    }

    fn group(&mut self) -> Option<String> {
        let checkpoint = self.pos;
        while self.peek().is_some_and(char::is_whitespace) {
            self.next();
        }
        if self.peek() != Some('{') {
            self.pos = checkpoint;
            return None;
        }
        self.next();
        Some(self.render_until(Some('}')))
    }

    fn push_fraction(out: &mut String, numerator: &str, denominator: &str) {
        let grouped = |value: &str, out: &mut String| {
            if value.chars().count() == 1 {
                out.push_str(value);
            } else {
                out.push('(');
                out.push_str(value);
                out.push(')');
            }
        };
        grouped(numerator, out);
        out.push('⁄');
        grouped(denominator, out);
    }

    fn push_space(out: &mut String) {
        if !out.ends_with([' ', '\n', '\u{2009}']) {
            out.push(' ');
        }
    }

    fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn next(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }
}

fn math_symbol(command: &str) -> Option<&'static str> {
    Some(match command {
        "times" => "×",
        "cdot" => "·",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "infty" => "∞",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "partial" => "∂",
        "nabla" => "∇",
        "to" | "rightarrow" => "→",
        "leftarrow" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" => "⇔",
        "in" => "∈",
        "notin" => "∉",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "neg" => "¬",
        "forall" => "∀",
        "exists" => "∃",
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" | "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" | "vartheta" => "θ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "omicron" => "ο",
        "pi" | "varpi" => "π",
        "rho" | "varrho" => "ρ",
        "sigma" | "varsigma" => "σ",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" | "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        _ => return None,
    })
}

fn script_char(marker: char, ch: char) -> Option<char> {
    match marker {
        '^' => Some(match ch {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '+' => '⁺',
            '-' => '⁻',
            '=' => '⁼',
            '(' => '⁽',
            ')' => '⁾',
            'i' => 'ⁱ',
            'n' => 'ⁿ',
            _ => return None,
        }),
        '_' => Some(match ch {
            '0' => '₀',
            '1' => '₁',
            '2' => '₂',
            '3' => '₃',
            '4' => '₄',
            '5' => '₅',
            '6' => '₆',
            '7' => '₇',
            '8' => '₈',
            '9' => '₉',
            '+' => '₊',
            '-' => '₋',
            '=' => '₌',
            '(' => '₍',
            ')' => '₎',
            'a' => 'ₐ',
            'e' => 'ₑ',
            'h' => 'ₕ',
            'i' => 'ᵢ',
            'j' => 'ⱼ',
            'k' => 'ₖ',
            'l' => 'ₗ',
            'm' => 'ₘ',
            'n' => 'ₙ',
            'o' => 'ₒ',
            'p' => 'ₚ',
            'r' => 'ᵣ',
            's' => 'ₛ',
            't' => 'ₜ',
            'u' => 'ᵤ',
            'v' => 'ᵥ',
            'x' => 'ₓ',
            _ => return None,
        }),
        _ => None,
    }
}

/// Flatten inline runs into shaped-text inputs. Pure given a theme.
pub fn flatten_runs(runs: &[InlineRun], theme: &Theme, bold_default: bool) -> FlatText {
    flatten_runs_weighted(
        runs,
        theme,
        if bold_default {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        },
    )
}

/// [`flatten_runs`] with an explicit base weight (table headers are 700 per
/// comet's `table.headerWeight`; strong runs never drop below semibold).
fn flatten_runs_weighted(runs: &[InlineRun], theme: &Theme, base_weight: FontWeight) -> FlatText {
    let mut text = String::new();
    let mut out: Vec<TextRun> = Vec::with_capacity(runs.len());
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    let mut code_ranges: Vec<Range<usize>> = Vec::new();
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        let rendered_math = run.style.math.map(|_| math_to_text(&run.text));
        let run_text = rendered_math.as_deref().unwrap_or(&run.text);
        let start = text.len();
        text.push_str(run_text);
        let mut f = if run.style.code {
            font(theme.font_mono.clone())
        } else {
            font(theme.font_sans.clone())
        };
        f.weight = if run.style.bold && base_weight.0 < FontWeight::SEMIBOLD.0 {
            FontWeight::SEMIBOLD
        } else {
            base_weight
        };
        f.style = if run.style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        // Links stay monochrome — foreground with an underline (comet's md
        // theme underlines in the text color; indigo is reserved for primary
        // actions).
        let is_link = run.style.link.is_some();
        // Inline code reads violet (see `inline_code_text`); everything else
        // stays the monochrome foreground.
        let color = if run.style.code {
            inline_code_text(theme)
        } else {
            theme.text
        };
        if run.style.code {
            // Merge adjacent code runs into one wash box (like links below).
            match code_ranges.last_mut() {
                Some(range) if range.end == start => range.end = text.len(),
                _ => code_ranges.push(start..text.len()),
            }
        }
        if let Some(url) = &run.style.link {
            // A still-streaming link (mend.rs sentinel) keeps link styling —
            // so the URL's completion changes nothing visually — but is not
            // clickable until the real destination exists.
            if url != super::mend::PENDING_LINK_URL {
                // Merge adjacent runs of the same link into one clickable range.
                match links.last_mut() {
                    Some((range, last_url)) if range.end == start && last_url == url => {
                        range.end = text.len();
                    }
                    _ => links.push((start..text.len(), url.clone())),
                }
            }
        }
        out.push(TextRun {
            len: run_text.len(),
            font: f,
            color,
            // Inline code's wash is painted as ROUNDED quads by the canvas
            // underlay (`code_wash_underlay`) — a run background here could
            // only be a square box.
            background_color: None,
            underline: is_link.then_some(UnderlineStyle {
                color: Some(theme.text_muted),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: run.style.strikethrough.then_some(gpui::StrikethroughStyle {
                thickness: px(1.0),
                color: Some(theme.text_muted),
            }),
        });
    }
    FlatText {
        text: text.into(),
        runs: out,
        links,
        code_ranges,
    }
}

/// Flatten through the cross-frame cache when one is wired: settled blocks
/// reuse text + runs untouched (O(1) per block per frame); only blocks the
/// incremental parser invalidated rebuild.
fn flatten_cached(
    runs: &[InlineRun],
    base_weight: FontWeight,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> Rc<FlatText> {
    match &opts.cache {
        Some(cache) => {
            let mut cache = cache.borrow_mut();
            cache.sync_palette();
            cache
                .flats
                .entry(opts.row_key.clone())
                .or_default()
                .entry((top_ix, ix))
                .or_insert_with(|| Rc::new(flatten_runs_weighted(runs, theme, base_weight)))
                .clone()
        }
        None => Rc::new(flatten_runs_weighted(runs, theme, base_weight)),
    }
}

/// Turn a markdown destination into an OS-openable target. Web/custom URLs
/// pass through. Absolute paths and paths relative to the chat workspace
/// become encoded `file://` URLs; handing GPUI a bare relative path makes
/// macOS construct a relative `NSURL`, which Finder rejects with error -50.
fn link_target(url: &str, cwd: Option<&str>) -> String {
    let trimmed = url.trim();
    if trimmed.starts_with("file://") {
        return trimmed.to_string();
    }
    let Some(path) = crate::attachments::inline_image_path(trimmed, cwd) else {
        return url.to_string();
    };
    file_url(&path)
}

fn file_url(path: &str) -> String {
    #[cfg(target_os = "windows")]
    let path = path.replace('\\', "/");
    #[cfg(not(target_os = "windows"))]
    let path = path.to_string();

    #[cfg(target_os = "windows")]
    let mut url = if path.starts_with("//") {
        String::from("file:")
    } else {
        String::from("file:///")
    };
    #[cfg(not(target_os = "windows"))]
    let mut url = String::from("file://");

    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/')
            || cfg!(target_os = "windows") && byte == b':'
        {
            url.push(char::from(byte));
        } else {
            url.push('%');
            url.push(char::from(HEX[(byte >> 4) as usize]));
            url.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    url
}

/// Veiled, clickable text for a flattened block (no sizing wrapper).
fn flat_text_element(
    flat: &FlatText,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> AnyElement {
    // Streaming veil: opacity-only recolor of the runs covering newly appended
    // chunks. Same text, same fonts, same lengths — layout is untouched.
    // Settled elements return no spans and reuse the cached runs unsplit.
    let text_runs = match &opts.veil {
        Some(veil) => {
            let spans = veil.borrow_mut().advance(ix, &flat.text, opts.now);
            apply_veil(flat.runs.clone(), &spans)
        }
        None => flat.runs.clone(),
    };
    let styled = StyledText::new(flat.text.clone()).with_runs(text_runs);
    let layout = styled.layout().clone();
    let text_el: AnyElement = if flat.links.is_empty() {
        styled.into_any_element()
    } else {
        let (ranges, urls): (Vec<_>, Vec<_>) = flat
            .links
            .iter()
            .map(|(range, url)| (range.clone(), link_target(url, opts.link_cwd.as_deref())))
            .unzip();
        let id: SharedString = format!("{}-t{ix}", opts.row_key).into();
        InteractiveText::new(id, styled)
            .on_click(ranges, move |clicked_ix, _window, cx| {
                if let Some(url) = urls.get(clicked_ix) {
                    cx.open_url(url);
                }
            })
            .into_any_element()
    };
    selectable_layout_element(
        format!("{}:{ix}", opts.row_key).into(),
        flat.text.clone(),
        layout,
        text_el,
        flat.code_ranges.clone(),
        Some(inline_code_wash(theme)),
        theme,
    )
}

/// Make an already-styled plain text element participate in transcript-wide
/// selection. Callers keep their own non-selection underlays, such as mention
/// chip washes, around the returned element.
pub(crate) fn selectable_styled_text(
    selection_key: SharedString,
    text: SharedString,
    styled: StyledText,
    theme: &Theme,
) -> AnyElement {
    let layout = styled.layout().clone();
    selectable_layout_element(
        selection_key.as_ref().into(),
        text,
        layout,
        styled.into_any_element(),
        Vec::new(),
        None,
        theme,
    )
}

fn selectable_layout_element(
    sel_key: std::sync::Arc<str>,
    flat_text: SharedString,
    layout: gpui::TextLayout,
    text_el: AnyElement,
    code_ranges: Vec<Range<usize>>,
    code_wash: Option<Hsla>,
    theme: &Theme,
) -> AnyElement {
    // Underlay canvas: inline-code washes + the selection wash, painted
    // BEFORE the text (earlier sibling ⇒ underneath), reading glyph geometry
    // from the text's own layout handle. Pure paint — never in layout.
    let sel_wash = selection_wash(theme);
    let underlay = canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            if let Some(wash) = code_wash {
                for range in &code_ranges {
                    for rect in range_rects(
                        &layout,
                        &flat_text,
                        range,
                        INLINE_CODE_PAD_X,
                        INLINE_CODE_INSET_Y,
                    ) {
                        window.paint_quad(quad(
                            rect,
                            px(INLINE_CODE_RADIUS),
                            wash,
                            px(0.0),
                            gpui::transparent_black(),
                            BorderStyle::default(),
                        ));
                    }
                }
            }
            if let Some(range) = super::selection::wash_range(&sel_key) {
                for rect in range_rects(&layout, &flat_text, &range, 0.0, 0.0) {
                    window.paint_quad(quad(
                        rect,
                        px(0.0),
                        sel_wash,
                        px(0.0),
                        gpui::transparent_black(),
                        BorderStyle::default(),
                    ));
                }
            }
            // Paint order is document order. The transcript root owns the
            // pointer handlers and resolves drags against this registry.
            REGISTRY.with(|registry| {
                registry.borrow_mut().push(RegEntry {
                    key: sel_key.clone(),
                    text: flat_text.clone(),
                    layout: layout.clone(),
                })
            });
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        .child(underlay)
        .child(text_el)
        .into_any_element()
}

/// Selection tint: the accent hue under the glyphs, dark-panel strength.
fn selection_wash(theme: &Theme) -> Hsla {
    theme.accent.opacity(0.35) // indigo-400
}

/// One painted text element, registered per frame in document order — the
/// continuity model that lets a drag span paragraphs/list items (Zed gets
/// this for free from its single-element markdown; our tree rebuilds it).
struct RegEntry {
    key: std::sync::Arc<str>,
    text: SharedString,
    layout: gpui::TextLayout,
}

thread_local! {
    static REGISTRY: RefCell<Vec<RegEntry>> = const { RefCell::new(Vec::new()) };
    static PENDING_SELECTION_HEAD: RefCell<Option<gpui::Point<gpui::Pixels>>> =
        const { RefCell::new(None) };
}

/// A zero-size canvas that clears the selection registry — paint it FIRST in
/// the transcript root (before any markdown), so each frame's registry holds
/// exactly that frame's visible text elements in paint order.
pub fn selection_frame_reset() -> impl IntoElement {
    canvas(
        |_, _, _| (),
        |_, _, _, _| REGISTRY.with(|r| r.borrow_mut().clear()),
    )
    .absolute()
    .w(px(0.0))
    .h(px(0.0))
}

/// A zero-size canvas painted after the transcript list. Wheel scrolling can
/// move content under a stationary drag pointer without emitting MouseMove;
/// this resolves that pending head only after the new visible rows registered.
pub fn selection_frame_finalize() -> impl IntoElement {
    canvas(
        |_, _, _| (),
        |_, _, window, _| {
            let position = PENDING_SELECTION_HEAD.with(|head| head.borrow_mut().take());
            if position.is_some_and(selection_mouse_move) {
                window.refresh();
            }
        },
    )
    .absolute()
    .w(px(0.0))
    .h(px(0.0))
}

/// `(element index, byte offset)` for a window position: the registered
/// element whose vertical band contains it, else the nearest by vertical
/// distance (a drag past the gutter or between blocks clamps sensibly).
fn registry_point(position: gpui::Point<gpui::Pixels>) -> Option<(usize, usize)> {
    REGISTRY.with(|r| {
        let reg = r.borrow();
        let mut best: Option<(usize, f32)> = None;
        for (ei, entry) in reg.iter().enumerate() {
            let b = entry.layout.bounds();
            let dy = if position.y < b.top() {
                f32::from(b.top() - position.y)
            } else if position.y > b.bottom() {
                f32::from(position.y - b.bottom())
            } else {
                0.0
            };
            if best.is_none_or(|(_, d)| dy < d) {
                best = Some((ei, dy));
            }
            if dy == 0.0 {
                break;
            }
        }
        let (ei, _) = best?;
        let ix = match reg[ei].layout.index_for_position(position) {
            Ok(ix) | Err(ix) => ix,
        };
        Some((ei, ix))
    })
}

/// Resolve the active anchor + head over the frame's visible registry. The
/// selection state bridges overlapping virtualized slices when the anchor is
/// no longer painted.
fn resolve_drag(head: (usize, usize)) -> bool {
    REGISTRY.with(|r| {
        let reg = r.borrow();
        let elements: Vec<(&str, &str)> = reg
            .iter()
            .map(|entry| (entry.key.as_ref(), entry.text.as_ref()))
            .collect();
        super::selection::extend_to(&elements, head)
    })
}

/// Start, extend, or clear transcript selection from a root-level pointer down.
pub(crate) fn selection_mouse_down(
    position: gpui::Point<gpui::Pixels>,
    click_count: usize,
    shift: bool,
) -> bool {
    PENDING_SELECTION_HEAD.with(|head| head.borrow_mut().take());
    // Native Shift-click extends to the cursor's nearest text position, not
    // only when the pointer lands directly inside a glyph layout. Row padding,
    // list gutters, and block gaps must therefore keep the existing anchor.
    if shift && super::selection::resume_drag() {
        if let Some(head) = registry_point(position) {
            let _ = resolve_drag(head);
        }
        return true;
    }

    let hit = REGISTRY.with(|registry| {
        registry.borrow().iter().find_map(|entry| {
            entry.layout.bounds().contains(&position).then(|| {
                let ix = match entry.layout.index_for_position(position) {
                    Ok(ix) | Err(ix) => ix,
                };
                (entry.key.clone(), entry.text.clone(), ix)
            })
        })
    });
    if let Some((key, text, ix)) = hit {
        match click_count {
            2 => {
                let range = super::selection::word_range(&text, ix);
                super::selection::begin_with_span(&key, &text, range);
            }
            n if n >= 3 => {
                super::selection::begin_with_span(&key, &text, 0..text.len());
            }
            _ => super::selection::begin(&key, ix),
        }
        true
    } else {
        super::selection::clear()
    }
}

/// Extend the active transcript selection to the nearest painted text point.
pub(crate) fn selection_mouse_move(position: gpui::Point<gpui::Pixels>) -> bool {
    if super::selection::drag_anchor().is_none() {
        return false;
    }
    let Some(head) = registry_point(position) else {
        return false;
    };
    resolve_drag(head)
}

/// Settle the active transcript selection.
pub(crate) fn selection_mouse_up(position: gpui::Point<gpui::Pixels>) -> Option<String> {
    PENDING_SELECTION_HEAD.with(|head| head.borrow_mut().take());
    let (anchor_key, _) = super::selection::drag_anchor()?;
    super::selection::end_drag(
        &anchor_key,
        Some((f32::from(position.x), f32::from(position.y))),
    )
}

/// Re-resolve a stationary drag after a wheel/touch scroll paints a new slice.
pub(crate) fn selection_scroll_to(position: gpui::Point<gpui::Pixels>) -> bool {
    if super::selection::drag_anchor().is_none() {
        return false;
    }
    PENDING_SELECTION_HEAD.with(|head| *head.borrow_mut() = Some(position));
    true
}

/// The wash boxes for one byte range: one box per visual line the range
/// covers (soft wraps split it), in window coordinates from the laid-out
/// text's own geometry. `text` is the layout's source string — it tells hard
/// `\n` breaks from soft wraps at row boundaries. `pad_x` overhangs the box
/// horizontally (inline code); `inset_y` shrinks it vertically — both 0 for
/// a selection wash, which wants full-line-height boxes that tile seamlessly
/// across wrapped rows.
pub(crate) fn range_rects(
    layout: &gpui::TextLayout,
    text: &str,
    range: &Range<usize>,
    pad_x: f32,
    inset_y: f32,
) -> Vec<Bounds<gpui::Pixels>> {
    let mut rects = Vec::new();
    let line_height = layout.line_height();
    let left_edge = layout.bounds().origin.x;
    let mut cur = range.start;
    // gpui positions a soft-wrap boundary index at the END of the row before
    // it, so a row opened by one can't take its origin from
    // `position_for_index(cur)` — that would drop the row's first character
    // from the wash. It takes x from the element's left edge and y from the
    // first following index that is unambiguously on the row instead.
    let mut wrapped = false;
    // Walk the range one visual row at a time: find the furthest index that
    // still sits on the current row (binary search over glyph positions).
    let mut guard = 0;
    while cur < range.end && guard < 256 {
        guard += 1;
        let p1 = if wrapped {
            let mut probe = cur + 1;
            while probe < range.end && !text.is_char_boundary(probe) {
                probe += 1;
            }
            let Some(p) = layout.position_for_index(probe) else {
                break;
            };
            point(left_edge, p.y)
        } else {
            let Some(p) = layout.position_for_index(cur) else {
                break;
            };
            p
        };
        // `seg_end` closes the wash on this row: the largest index whose
        // position still reports this row's y (a trailing soft-wrap boundary
        // index does — at the row's right edge, exactly what the box needs).
        let seg_end = match layout.position_for_index(range.end) {
            Some(pe) if pe.y == p1.y => range.end,
            _ => {
                // Largest ix on this row (probes stay on char boundaries only
                // at the ends; intermediate probes just need a y).
                let (mut lo, mut hi) = (cur, range.end);
                while hi - lo > 1 {
                    let mid = lo + (hi - lo) / 2;
                    match layout.position_for_index(mid) {
                        Some(pm) if pm.y == p1.y => lo = mid,
                        _ => hi = mid,
                    }
                }
                lo
            }
        };
        if let Some(p2) = layout.position_for_index(seg_end)
            && p2.y == p1.y
            && p2.x > p1.x
        {
            rects.push(Bounds::new(
                point(p1.x - px(pad_x), p1.y + px(inset_y)),
                size(
                    p2.x - p1.x + px(2.0 * pad_x),
                    line_height - px(2.0 * inset_y),
                ),
            ));
        }
        if seg_end >= range.end {
            break;
        }
        if text.as_bytes().get(seg_end) == Some(&b'\n') {
            // Hard break: skip the newline; the next index positions cleanly
            // at the following row's start.
            cur = seg_end + 1;
            wrapped = false;
        } else if seg_end > cur || !wrapped {
            // Soft wrap: the boundary character itself opens the next row.
            cur = seg_end;
            wrapped = true;
        } else {
            break;
        }
    }
    rects
}

#[allow(clippy::too_many_arguments)]
fn text_element(
    runs: &[InlineRun],
    size: f32,
    line_height: f32,
    bold_default: bool,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> AnyElement {
    let weight = if bold_default {
        FontWeight::SEMIBOLD
    } else {
        FontWeight::NORMAL
    };
    let flat = flatten_cached(runs, weight, top_ix, ix, opts, theme);
    let inner = flat_text_element(&flat, ix, opts, theme);
    div()
        .text_size(px(size))
        .line_height(px(line_height))
        .child(inner)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn render_code_block(
    language: Option<&str>,
    code: &str,
    top_ix: usize,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
    highlight: CodeHighlight,
) -> AnyElement {
    let mono = font(theme.font_mono.clone());
    // Per-line strings + runs through the cross-frame cache (validity: code
    // length + highlight slice identity — a fresh highlight Arc re-derives).
    let hl_key = highlight.map_or((0, 0), |h| (h.as_ptr() as usize, h.len()));
    let build = || {
        let lines: Vec<(SharedString, Vec<TextRun>)> = code
            .split('\n')
            .enumerate()
            .map(|(li, line)| {
                let tokens = highlight
                    .and_then(|h| h.get(li))
                    .map(|t| &t[..])
                    .unwrap_or(&[]);
                (
                    SharedString::from(line.to_string()),
                    runs_for_code_line(line, tokens, &mono, theme),
                )
            })
            .collect();
        let content_width = lines
            .iter()
            .map(|(line, runs)| {
                f32::from(
                    window
                        .text_system()
                        .shape_line(line.clone(), px(CODE_TEXT_SIZE), runs, None)
                        .width(),
                )
            })
            .fold(0.0, f32::max);
        Rc::new(CachedCode {
            code_text: code.to_string().into(),
            hl_key,
            lines,
            content_width,
        })
    };
    let cached: Rc<CachedCode> = match &opts.cache {
        Some(cache) => {
            let mut cache = cache.borrow_mut();
            cache.sync_palette();
            let entry = cache
                .code
                .entry(opts.row_key.clone())
                .or_default()
                .entry((top_ix, ix))
                .or_insert_with(&build);
            if entry.code_text.len() != code.len() || entry.hl_key != hl_key {
                *entry = build();
            }
            entry.clone()
        }
        None => build(),
    };
    // The scroll viewport must contain a child wider than itself. A nowrap
    // StyledText can paint past a stretched line div without contributing that
    // width to GPUI's overflow extent, so resolve the block's max-content width
    // from the same shaped runs used for paint. GPUI caches identical shapes.
    let content_width = cached.content_width;
    // Streaming veil over appended code, tracked on the whole code text and
    // sliced per line below (paint-only run recolor — heights stay exact).
    let veil_spans = match &opts.veil {
        Some(veil) => veil.borrow_mut().advance(ix, code, opts.now),
        None => Vec::new(),
    };
    let scroll_id: SharedString = format!("{}-code{ix}", opts.row_key).into();
    let copy_button = code_copy_button(|| cached.code_text.clone(), ix, opts, theme);
    div()
        .rounded(px(10.0))
        // Faint white wash over the near-black panel ≈ #101010 (comet's code
        // surface), with the hairline border.
        .bg(crate::theme::ink(0.035))
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .relative()
        .when_some(language, |el, lang| {
            el.child(
                div()
                    .px(px(CODE_PADDING_X))
                    .py(px(5.0))
                    .border_b_1()
                    .border_color(theme.border)
                    // A whisper of tone separation between header and body.
                    .bg(crate::theme::ink(0.02))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(lang.to_string())),
            )
        })
        .child(
            div()
                .id(scroll_id)
                .debug_selector(|| "CODE_SCROLL_VIEWPORT".into())
                .overflow_x_scroll()
                .px(px(CODE_PADDING_X))
                .py(px(CODE_PADDING_Y))
                .child(
                    div()
                        .w(px(content_width))
                        .font_family(theme.font_mono.clone())
                        .text_size(px(CODE_TEXT_SIZE))
                        .line_height(px(CODE_LINE_HEIGHT))
                        .whitespace_nowrap()
                        .flex()
                        .flex_col()
                        .items_start()
                        .children((0..cached.lines.len()).scan(0usize, move |off, li| {
                            let (line, runs) = &cached.lines[li];
                            let start = *off;
                            *off = start + line.len() + 1; // +1 for the '\n'
                            let local = slice_spans(&veil_spans, start, start + line.len());
                            let runs = apply_veil(runs.clone(), &local);
                            let styled = StyledText::new(line.clone()).with_runs(runs);
                            let selection_key: SharedString =
                                format!("{}:code{ix}.{li}", opts.row_key).into();
                            Some(
                                div()
                                    .debug_selector(move || format!("CODE_SCROLL_LINE_{li}"))
                                    .h(px(CODE_LINE_HEIGHT))
                                    .flex_none()
                                    .child(selectable_styled_text(
                                        selection_key,
                                        line.clone(),
                                        styled,
                                        theme,
                                    )),
                            )
                        })),
                ),
        )
        .children(copy_button)
        .into_any_element()
}

/// Copy affordance shared by code blocks and mermaid diagrams (round 9; no
/// source counterpart — the original block is header + body only): a small
/// ghost button in the block's top-right, absolutely overlaid so clicking /
/// the "Copied" flash never shifts layout. Sits centered in the header when
/// there is one, floats over the first code line otherwise.
pub(crate) fn code_copy_button(
    code: impl FnOnce() -> SharedString,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
) -> Option<gpui::Stateful<gpui::Div>> {
    opts.copy.clone().map(|copy| {
        let copied = copy.copied_ix == Some(ix);
        let code_text = code();
        let handler = copy.handler.clone();
        let fade_key = format!("{}-copy{ix}", opts.row_key);
        div()
            .id(SharedString::from(fade_key.clone()))
            .absolute()
            .top(px(3.0))
            .right(px(5.0))
            .h(px(20.0))
            .px(px(6.0))
            .rounded(px(5.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            // Ghost-button hover wash fades over transition-colors like every
            // other interactive chrome (crate::motion hover fades).
            .bg(crate::motion::hover_blend(
                &fade_key,
                gpui::transparent_black(),
                crate::theme::ink(0.08),
            ))
            .on_hover(crate::motion::hover_listener(fade_key))
            .text_size(px(10.5))
            .text_color(theme.text_muted)
            .on_click(move |_, window, cx| handler(ix, code_text.clone(), window, cx))
            .child(
                crate::icons::icon(if copied {
                    crate::icons::CHECK
                } else {
                    crate::icons::COPY
                })
                .size(px(12.0))
                .text_color(theme.text_muted),
            )
            .when(copied, |el| el.child(SharedString::from("Copied")))
    })
}

/// Paint color for a token class — the soft syntax palette (round 9: the
/// original's mdTheme code blocks are monochrome `#e7e7e7`, but the user
/// asked for color; these are the diff pane's hues, now shared by both).
pub fn token_color(class: TokenClass, theme: &Theme) -> Hsla {
    match class {
        TokenClass::Keyword => theme.syntax_keyword, // soft rose
        TokenClass::StringLit => theme.syntax_string, // soft green
        TokenClass::Number => theme.syntax_number,   // soft amber
        TokenClass::Comment => theme.text_faint,
    }
}

/// Build the exact-cover `TextRun` list for one code line from its tokens.
/// Same font everywhere — recoloring can never change layout.
pub fn runs_for_code_line(
    line: &str,
    tokens: &[Token],
    mono: &gpui::Font,
    theme: &Theme,
) -> Vec<TextRun> {
    runs_with_palette(line, tokens, mono, theme.text, |class| {
        token_color(class, theme)
    })
}

/// [`runs_for_code_line`] with a caller-supplied palette (the diff pane keys
/// its plain color differently; the hues are shared via [`token_color`]).
pub fn runs_with_palette(
    line: &str,
    tokens: &[Token],
    mono: &gpui::Font,
    plain_color: Hsla,
    color_for: impl Fn(TokenClass) -> Hsla,
) -> Vec<TextRun> {
    let plain = |len: usize| TextRun {
        len,
        font: mono.clone(),
        color: plain_color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::new();
    let mut at = 0usize;
    for token in tokens {
        if token.range.start > at {
            runs.push(plain(token.range.start - at));
        }
        let mut run = plain(token.range.len());
        run.color = color_for(token.class);
        runs.push(run);
        at = token.range.end;
    }
    if at < line.len() {
        runs.push(plain(line.len() - at));
    }
    runs.retain(|r| r.len > 0);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::highlight::{Lang, tokenize_line};
    use crate::markdown::parser::{InlineStyle, MathStyle};

    #[test]
    fn code_line_runs_cover_exactly() {
        let theme = Theme::dark();
        let mono = font(theme.font_mono.clone());
        let line = r#"let x = "hi"; // done"#;
        let (tokens, _) = tokenize_line(Lang::Rust, line, Default::default());
        let runs = runs_for_code_line(line, &tokens, &mono, &theme);
        let total: usize = runs.iter().map(|r| r.len).sum();
        assert_eq!(total, line.len());
        assert!(
            runs.iter().all(|r| r.font == mono),
            "highlight must not change fonts"
        );
        // At least one non-plain color made it through.
        assert!(runs.iter().any(|r| r.color != theme.text));
    }

    #[test]
    fn code_line_runs_with_no_tokens_are_one_plain_run() {
        let theme = Theme::dark();
        let mono = font(theme.font_mono.clone());
        let runs = runs_for_code_line("plain text", &[], &mono, &theme);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 10);
    }

    #[test]
    fn flatten_collects_and_merges_inline_code_ranges() {
        let theme = Theme::dark();
        let code = |text: &str| InlineRun {
            text: text.into(),
            style: InlineStyle {
                code: true,
                ..Default::default()
            },
        };
        let plain = |text: &str| InlineRun {
            text: text.into(),
            style: InlineStyle::default(),
        };
        let flat = flatten_runs(
            &[
                plain("use "),
                code("foo"),
                code("()"),
                plain(" and "),
                code("bar"),
            ],
            &theme,
            false,
        );
        // Adjacent code runs merge into ONE wash box; separated ones don't.
        assert_eq!(flat.code_ranges, vec![4..9, 14..17]);
        // Code text is the violet tint; the square run background is gone
        // (the rounded wash is painted by the canvas underlay instead).
        assert_eq!(flat.runs[1].color, inline_code_text(&theme));
        assert_eq!(flat.runs[1].background_color, None);
        assert_eq!(flat.runs[0].color, theme.text);
    }

    #[test]
    fn math_runs_render_common_tex_without_delimiters() {
        let theme = Theme::dark();
        let display = InlineRun {
            text: "6 \\times 7 + 6 \\times 5 = 72".into(),
            style: InlineStyle {
                math: Some(MathStyle::Display),
                ..Default::default()
            },
        };
        let flat = flatten_runs(&[display], &theme, false);
        assert_eq!(flat.text, "6 × 7 + 6 × 5 = 72");
        assert_eq!(
            flat.runs.iter().map(|run| run.len).sum::<usize>(),
            flat.text.len()
        );
    }

    #[test]
    fn math_runs_render_scripts_fractions_roots_and_greek() {
        assert_eq!(
            math_to_text("x^2 + a_{10} + \\frac{1}{2} + \\sqrt{x} + \\alpha"),
            "x² + a₁₀ + 1⁄2 + √x + α"
        );
    }

    #[test]
    fn code_palette_is_colored_and_shared() {
        // Round 9: transcript code blocks paint the soft hues (rose keyword,
        // green string, amber number); comments stay faint neutral.
        let theme = Theme::dark();
        assert_ne!(token_color(TokenClass::Keyword, &theme), theme.text);
        assert_ne!(
            token_color(TokenClass::StringLit, &theme),
            token_color(TokenClass::Keyword, &theme)
        );
        assert_eq!(token_color(TokenClass::Comment, &theme), theme.text_faint);
    }

    #[test]
    fn flatten_runs_maps_links_and_styles() {
        let theme = Theme::dark();
        let runs = vec![
            InlineRun {
                text: "go ".into(),
                style: InlineStyle::default(),
            },
            InlineRun {
                text: "here".into(),
                style: InlineStyle {
                    link: Some("https://x.dev".into()),
                    ..Default::default()
                },
            },
            InlineRun {
                text: " now".into(),
                style: InlineStyle {
                    bold: true,
                    ..Default::default()
                },
            },
        ];
        let flat = flatten_runs(&runs, &theme, false);
        assert_eq!(flat.text, "go here now");
        assert_eq!(flat.links, vec![(3..7, "https://x.dev".to_string())]);
        let total: usize = flat.runs.iter().map(|r| r.len).sum();
        assert_eq!(total, flat.text.len());
        // Links stay monochrome (foreground + underline), never accent-tinted.
        assert_eq!(flat.runs[1].color, theme.text);
        assert!(flat.runs[1].underline.is_some());
        assert_eq!(flat.runs[2].font.weight, FontWeight::SEMIBOLD);
    }

    #[test]
    fn link_targets_resolve_csv_and_generic_files_against_chat_cwd() {
        assert_eq!(
            link_target("drawing-intelligence-original-textbooks.csv", Some("/repo")),
            "file:///repo/drawing-intelligence-original-textbooks.csv"
        );
        assert_eq!(
            link_target("reports/final draft #1.pdf", Some("/repo with spaces")),
            "file:///repo%20with%20spaces/reports/final%20draft%20%231.pdf"
        );
        assert_eq!(
            link_target("/tmp/naïve data.json", Some("/ignored")),
            "file:///tmp/na%C3%AFve%20data.json"
        );
    }

    #[test]
    fn link_targets_leave_urls_and_unrooted_relative_links_unchanged() {
        assert_eq!(
            link_target("https://x.dev/a?b=1#c", Some("/repo")),
            "https://x.dev/a?b=1#c"
        );
        assert_eq!(
            link_target("mailto:user@example.com", Some("/repo")),
            "mailto:user@example.com"
        );
        assert_eq!(
            link_target("file:///tmp/a%20b.csv", Some("/repo")),
            "file:///tmp/a%20b.csv"
        );
        assert_eq!(link_target("output.csv", None), "output.csv");
        assert_eq!(link_target("~/output.csv", Some("/repo")), "~/output.csv");
    }

    #[test]
    fn table_columns_floor_and_padding() {
        // A short column keeps its content width (floored at MIN_COLUMN_CONTENT
        // + padding); a wide one may wrap but no narrower than minColumnWidth.
        let geo = table_columns(&[10.0, 200.0]);
        assert_eq!(geo.naturals, vec![72.0, 224.0]); // 48+24, 200+24
        assert_eq!(geo.minimums, vec![72.0, 96.0]);
        assert_eq!(geo.min_table_width, 168.0);
    }

    #[test]
    fn table_columns_are_content_proportional_not_equal() {
        let geo = table_columns(&[300.0, 60.0, 60.0]);
        // Flex grow factors are the naturals — a prose column gets a larger
        // share than short ones (not equal thirds).
        assert!(geo.naturals[0] > 3.0 * geo.naturals[1] * 0.9);
        assert_eq!(geo.naturals[1], geo.naturals[2]);
    }

    #[test]
    fn table_header_flattens_at_weight_700() {
        let theme = Theme::dark();
        let runs = vec![InlineRun {
            text: "Header".into(),
            style: InlineStyle::default(),
        }];
        let flat = flatten_runs_weighted(&runs, &theme, TABLE_HEADER_WEIGHT);
        assert_eq!(flat.runs[0].font.weight, FontWeight::BOLD);
        // Strong runs inside a 700 header stay 700 (never drop to semibold).
        let bold_runs = vec![InlineRun {
            text: "Strong".into(),
            style: InlineStyle {
                bold: true,
                ..Default::default()
            },
        }];
        let flat = flatten_runs_weighted(&bold_runs, &theme, TABLE_HEADER_WEIGHT);
        assert_eq!(flat.runs[0].font.weight, FontWeight::BOLD);
    }

    #[test]
    fn adjacent_same_link_runs_merge_into_one_range() {
        let theme = Theme::dark();
        let style = InlineStyle {
            link: Some("https://x.dev".into()),
            ..Default::default()
        };
        let runs = vec![
            InlineRun {
                text: "bold".into(),
                style: InlineStyle {
                    bold: true,
                    ..style.clone()
                },
            },
            InlineRun {
                text: " tail".into(),
                style,
            },
        ];
        let flat = flatten_runs(&runs, &theme, false);
        assert_eq!(flat.links, vec![(0..9, "https://x.dev".to_string())]);
    }

    #[gpui::test]
    fn clicking_relative_file_link_opens_resolved_file_url(cx: &mut gpui::TestAppContext) {
        struct LinkProbe;
        impl gpui::Render for LinkProbe {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let runs = vec![InlineRun {
                    text: "report.csv".into(),
                    style: InlineStyle {
                        link: Some("report.csv".into()),
                        ..Default::default()
                    },
                }];
                div().w(px(240.0)).h(px(80.0)).child(
                    div()
                        .debug_selector(|| "FILE_LINK".into())
                        .child(flat_text_element(
                            &flatten_runs(&runs, &Theme::dark(), false),
                            0,
                            &RenderOptions {
                                link_cwd: Some("/tmp/comet link proof".into()),
                                ..RenderOptions::settled("row".into())
                            },
                            &Theme::dark(),
                        )),
                )
            }
        }

        cx.update(|cx| Theme::install(crate::theme::Appearance::Dark, cx));
        let window = cx.open_window(gpui::size(px(320.0), px(120.0)), |_, _| LinkProbe);
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let bounds = visual
            .debug_bounds("FILE_LINK")
            .expect("file link rendered");
        visual.simulate_click(
            gpui::point(bounds.origin.x + px(8.0), bounds.origin.y + px(11.0)),
            gpui::Modifiers::default(),
        );
        assert_eq!(
            visual.opened_url().as_deref(),
            Some("file:///tmp/comet%20link%20proof/report.csv")
        );
    }

    #[gpui::test]
    fn wrapped_selection_wash_rows_start_at_the_left_edge(cx: &mut gpui::TestAppContext) {
        // Regression: `position_for_index` reports a soft-wrap boundary index
        // at the END of the previous row, so the row walk used to resume each
        // continuation row one character in — its first glyph went unwashed.
        use std::cell::RefCell;
        use std::rc::Rc;

        const TEXT: &str = "alpha beta gamma delta epsilon zeta";
        const LINE_HEIGHT: f32 = 20.0;

        struct WrapProbe {
            layout: Rc<RefCell<Option<gpui::TextLayout>>>,
        }
        impl gpui::Render for WrapProbe {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let styled = StyledText::new(TEXT);
                *self.layout.borrow_mut() = Some(styled.layout().clone());
                // The test text system advances 0.6em per char: 6px at 10px —
                // a 100px column wraps the sentence across several rows.
                div()
                    .w(px(100.0))
                    .text_size(px(10.0))
                    .line_height(px(LINE_HEIGHT))
                    .child(styled)
            }
        }

        let layout = Rc::new(RefCell::new(None));
        let window = cx.open_window(gpui::size(px(400.0), px(300.0)), {
            let layout = layout.clone();
            move |_, _| WrapProbe { layout }
        });
        cx.run_until_parked();
        let _visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let layout = layout.borrow().clone().expect("probe rendered its text");

        let rects = range_rects(&layout, TEXT, &(0..TEXT.len()), 0.0, 0.0);
        assert!(
            rects.len() >= 2,
            "the probe text must soft-wrap; got {} row(s)",
            rects.len()
        );
        let left = layout.bounds().origin.x;
        assert_eq!(rects[0].origin.x, left);
        for rect in &rects[1..] {
            // Continuation rows wash flush from the element's left edge —
            // including the wrap-boundary character that opens the row.
            assert_eq!(rect.origin.x, left);
        }
        for pair in rects.windows(2) {
            assert_eq!(pair[1].origin.y - pair[0].origin.y, px(LINE_HEIGHT));
        }
    }

    #[gpui::test]
    fn long_code_lines_scroll_horizontally(cx: &mut gpui::TestAppContext) {
        struct CodeScrollProbe;
        impl gpui::Render for CodeScrollProbe {
            fn render(
                &mut self,
                window: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let block = Block::CodeBlock {
                    language: Some("text".into()),
                    code: "/tmp/ashler-state-code-corpus-national-uncapped/state-code-corpus-candidates.pre-browser-validation.json".into(),
                };
                div().w(px(240.0)).child(render_block(
                    &block,
                    0,
                    0,
                    &RenderOptions::settled("scroll-probe".into()),
                    &Theme::dark(),
                    window,
                    None,
                ))
            }
        }

        cx.update(|cx| Theme::install(crate::theme::Appearance::Dark, cx));
        let window = cx.open_window(gpui::size(px(320.0), px(160.0)), |_, _| CodeScrollProbe);
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let viewport = visual
            .debug_bounds("CODE_SCROLL_VIEWPORT")
            .expect("code scroll viewport rendered");
        let line_before = visual
            .debug_bounds("CODE_SCROLL_LINE_0")
            .expect("code line rendered");
        assert!(
            line_before.size.width > viewport.size.width,
            "long code line must create horizontal overflow: line={}, viewport={}",
            line_before.size.width,
            viewport.size.width,
        );

        visual.simulate_event(gpui::ScrollWheelEvent {
            position: gpui::point(viewport.center().x, viewport.center().y),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(-120.0), px(0.0))),
            ..Default::default()
        });
        let line_after = visual
            .debug_bounds("CODE_SCROLL_LINE_0")
            .expect("code line rendered after scroll");
        assert!(
            line_after.origin.x < line_before.origin.x,
            "horizontal scroll must reveal later code: before={}, after={}",
            line_before.origin.x,
            line_after.origin.x,
        );
    }

    #[gpui::test]
    fn shift_click_extends_to_nearest_text_through_row_padding(cx: &mut gpui::TestAppContext) {
        struct SelectionProbe;
        impl gpui::Render for SelectionProbe {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                _: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let theme = Theme::dark();
                let first = StyledText::new("alpha");
                let second = StyledText::new("second");
                div()
                    .w(px(300.0))
                    .on_mouse_down(gpui::MouseButton::Left, |event, window, _| {
                        if selection_mouse_down(
                            event.position,
                            event.click_count,
                            event.modifiers.shift,
                        ) {
                            window.refresh();
                        }
                    })
                    .on_mouse_up(gpui::MouseButton::Left, |event, window, _| {
                        let _ = selection_mouse_up(event.position);
                        window.refresh();
                    })
                    .child(selection_frame_reset())
                    .child(div().h(px(30.0)).child(selectable_styled_text(
                        "first".into(),
                        "alpha".into(),
                        first,
                        &theme,
                    )))
                    .child(
                        div()
                            .debug_selector(|| "SHIFT_TARGET_ROW".into())
                            .w(px(260.0))
                            .h(px(30.0))
                            .child(selectable_styled_text(
                                "second".into(),
                                "second".into(),
                                second,
                                &theme,
                            )),
                    )
                    .child(selection_frame_finalize())
            }
        }

        super::super::selection::clear();
        cx.update(|cx| Theme::install(crate::theme::Appearance::Dark, cx));
        let window = cx.open_window(gpui::size(px(340.0), px(100.0)), |_, _| SelectionProbe);
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        super::super::selection::begin_with_span("first", "alpha", 0..5);
        assert!(super::super::selection::end_drag("first", None).is_some());

        let target = visual
            .debug_bounds("SHIFT_TARGET_ROW")
            .expect("target row rendered");
        visual.simulate_click(
            gpui::point(target.right() - px(4.0), target.center().y),
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
        );
        assert_eq!(
            super::super::selection::selected_text().as_deref(),
            Some("alpha\nsecond")
        );
        super::super::selection::clear();
    }
}
