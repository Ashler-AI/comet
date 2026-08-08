//! gpui rendering for parsed mermaid diagrams.
//!
//! Same block chrome as a code block (header strip + copy button), with the
//! diagram in a horizontal scroller. Shapes and edges are canvas paint
//! ([`PathBuilder`] strokes/fills + rounded [`quad`]s); labels are absolutely
//! positioned text divs above the canvas. Label widths come from
//! `window.text_system().shape_line`, so repeat frames hit gpui's line-layout
//! cache exactly like table cells do.

use std::rc::Rc;

use gpui::{
    AnyElement, BorderStyle, Bounds, Hsla, PathBuilder, Pixels, Point, SharedString, TextRun,
    Window, canvas, div, font, point, prelude::*, px, quad, size,
};

use crate::markdown::render::{RenderOptions, code_copy_button};
use crate::theme::Theme;

use super::flowchart::{Flowchart, LineKind, Shape, Tip, layout as flow_layout};
use super::sequence::{ArrowHead, ArrowLine, Item, SEQ_TEXT_LH, SequenceDiagram};
use super::{Diagram, RectF, Vec2};

/// Node / participant label text.
pub const TEXT_SIZE: f32 = 12.5;
pub const LINE_H: f32 = 17.0;
/// Edge labels, cluster titles, frame chips.
pub const LABEL_TEXT_SIZE: f32 = 11.5;
pub const LABEL_LINE_H: f32 = 15.0;
const NODE_PAD_X: f32 = 14.0;
const NODE_PAD_Y: f32 = 9.0;

/// Render a ```` ```mermaid ```` block as a diagram, or `None` when the
/// source isn't a diagram we can draw (caller falls back to the code block).
pub(crate) fn mermaid_block(
    code: &str,
    ix: usize,
    opts: &RenderOptions,
    theme: &Theme,
    window: &Window,
) -> Option<AnyElement> {
    let diagram = super::parse(code)?;
    let measurer = Measurer {
        window,
        font: font(theme.font_sans.clone()),
        color: theme.text,
    };
    let content = match &diagram {
        Diagram::Flowchart(fc) => flowchart_element(fc, &measurer, theme),
        Diagram::Sequence(sd) => sequence_element(sd, &measurer, theme),
    };
    let scroll_id: SharedString = format!("{}-mmd{ix}", opts.row_key).into();
    Some(
        div()
            .rounded(px(10.0))
            .bg(crate::theme::ink(0.035))
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .relative()
            .child(
                div()
                    .px(px(12.0))
                    .py(px(5.0))
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.02))
                    .text_size(px(11.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("mermaid")),
            )
            .child(
                div()
                    .id(scroll_id)
                    .overflow_x_scroll()
                    .p(px(10.0))
                    .child(content),
            )
            .children(code_copy_button(code, ix, opts, theme))
            .into_any_element(),
    )
}

/// Shapes label lines through the window text system (line-layout cached).
struct Measurer<'a> {
    window: &'a Window,
    font: gpui::Font,
    color: Hsla,
}

impl Measurer<'_> {
    fn width(&self, s: &str, text_size: f32) -> f32 {
        if s.is_empty() {
            return 0.0;
        }
        let text: SharedString = s.to_string().into();
        let run = TextRun {
            len: text.len(),
            font: self.font.clone(),
            color: self.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        f32::from(
            self.window
                .text_system()
                .shape_line(text, px(text_size), &[run], None)
                .width(),
        )
    }
    fn block(&self, lines: &[String], text_size: f32, lh: f32) -> Vec2 {
        Vec2::new(
            lines
                .iter()
                .map(|l| self.width(l, text_size))
                .fold(0.0, f32::max),
            lines.len() as f32 * lh,
        )
    }
}

// ---------------------------------------------------------------------------
// Shared paint helpers
// ---------------------------------------------------------------------------

fn at(o: Point<Pixels>, v: Vec2) -> Point<Pixels> {
    point(o.x + px(v.x), o.y + px(v.y))
}

fn rect_bounds(o: Point<Pixels>, r: RectF) -> Bounds<Pixels> {
    Bounds {
        origin: at(o, Vec2::new(r.x, r.y)),
        size: size(px(r.w), px(r.h)),
    }
}

fn norm(v: Vec2) -> Vec2 {
    let len = (v.x * v.x + v.y * v.y).sqrt();
    if len <= f32::EPSILON {
        Vec2::new(1.0, 0.0)
    } else {
        Vec2::new(v.x / len, v.y / len)
    }
}

fn stroke_line(
    window: &mut Window,
    o: Point<Pixels>,
    a: Vec2,
    b: Vec2,
    width: f32,
    dash: Option<[f32; 2]>,
    color: Hsla,
) {
    let mut pb = PathBuilder::stroke(px(width));
    if let Some([on, off]) = dash {
        pb = pb.dash_array(&[px(on), px(off)]);
    }
    pb.move_to(at(o, a));
    pb.line_to(at(o, b));
    if let Ok(path) = pb.build() {
        window.paint_path(path, color);
    }
}

fn fill_polygon(window: &mut Window, o: Point<Pixels>, pts: &[Vec2], color: Hsla) {
    let mut pb = PathBuilder::fill();
    pb.add_polygon(&pts.iter().map(|&v| at(o, v)).collect::<Vec<_>>(), true);
    if let Ok(path) = pb.build() {
        window.paint_path(path, color);
    }
}

fn stroke_polygon(window: &mut Window, o: Point<Pixels>, pts: &[Vec2], width: f32, color: Hsla) {
    let mut pb = PathBuilder::stroke(px(width));
    pb.add_polygon(&pts.iter().map(|&v| at(o, v)).collect::<Vec<_>>(), true);
    if let Ok(path) = pb.build() {
        window.paint_path(path, color);
    }
}

fn circle_path(
    o: Point<Pixels>,
    c: Vec2,
    r: f32,
    stroke: Option<f32>,
) -> Option<gpui::Path<Pixels>> {
    let mut pb = match stroke {
        Some(w) => PathBuilder::stroke(px(w)),
        None => PathBuilder::fill(),
    };
    pb.move_to(at(o, Vec2::new(c.x - r, c.y)));
    pb.arc_to(
        point(px(r), px(r)),
        px(0.0),
        false,
        true,
        at(o, Vec2::new(c.x + r, c.y)),
    );
    pb.arc_to(
        point(px(r), px(r)),
        px(0.0),
        false,
        true,
        at(o, Vec2::new(c.x - r, c.y)),
    );
    pb.close();
    pb.build().ok()
}

/// Paint an edge tip at `tip_at`, approached along unit vector `dir`.
fn paint_tip(
    window: &mut Window,
    o: Point<Pixels>,
    tip_at: Vec2,
    dir: Vec2,
    tip: Tip,
    color: Hsla,
) {
    let perp = Vec2::new(-dir.y, dir.x);
    match tip {
        Tip::None => {}
        Tip::Arrow => {
            let base = Vec2::new(tip_at.x - dir.x * 8.0, tip_at.y - dir.y * 8.0);
            fill_polygon(
                window,
                o,
                &[
                    tip_at,
                    Vec2::new(base.x + perp.x * 4.0, base.y + perp.y * 4.0),
                    Vec2::new(base.x - perp.x * 4.0, base.y - perp.y * 4.0),
                ],
                color,
            );
        }
        Tip::Circle => {
            let c = Vec2::new(tip_at.x - dir.x * 4.5, tip_at.y - dir.y * 4.5);
            if let Some(path) = circle_path(o, c, 3.5, None) {
                window.paint_path(path, color);
            }
        }
        Tip::Cross => {
            let c = Vec2::new(tip_at.x - dir.x * 5.0, tip_at.y - dir.y * 5.0);
            let a = Vec2::new((dir.x + perp.x) * 3.5, (dir.y + perp.y) * 3.5);
            let b = Vec2::new((dir.x - perp.x) * 3.5, (dir.y - perp.y) * 3.5);
            stroke_line(
                window,
                o,
                Vec2::new(c.x - a.x, c.y - a.y),
                Vec2::new(c.x + a.x, c.y + a.y),
                1.6,
                None,
                color,
            );
            stroke_line(
                window,
                o,
                Vec2::new(c.x - b.x, c.y - b.y),
                Vec2::new(c.x + b.x, c.y + b.y),
                1.6,
                None,
                color,
            );
        }
    }
}

/// Absolutely positioned centered label lines (a node body or edge chip).
fn label_div(r: RectF, lines: &[String], text_size: f32, lh: f32, color: Hsla) -> gpui::Div {
    div()
        .absolute()
        .left(px(r.x))
        .top(px(r.y))
        .w(px(r.w))
        .h(px(r.h))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .text_size(px(text_size))
        .line_height(px(lh))
        .text_color(color)
        .children(lines.iter().map(|l| {
            div()
                .whitespace_nowrap()
                .child(SharedString::from(l.clone()))
        }))
}

// ---------------------------------------------------------------------------
// Flowchart
// ---------------------------------------------------------------------------

fn node_size(shape: Shape, text: Vec2) -> Vec2 {
    let tw = text.x;
    let th = text.y.max(LINE_H);
    let (pad_x, pad_y) = (NODE_PAD_X, NODE_PAD_Y);
    match shape {
        Shape::Rect | Shape::Round => Vec2::new(tw + 2.0 * pad_x, th + 2.0 * pad_y),
        Shape::Stadium => {
            let h = th + 2.0 * pad_y;
            Vec2::new(tw + 2.0 * pad_x + h / 2.0, h)
        }
        Shape::Subroutine => Vec2::new(tw + 2.0 * pad_x + 12.0, th + 2.0 * pad_y),
        Shape::Cylinder => Vec2::new(tw + 2.0 * pad_x, th + 2.0 * pad_y + 12.0),
        Shape::Circle => {
            let d = tw.max(th) + 26.0;
            Vec2::new(d, d)
        }
        Shape::DoubleCircle => {
            let d = tw.max(th) + 36.0;
            Vec2::new(d, d)
        }
        // The rhombus band at mid-height must fit the text; mermaid itself
        // lets long labels overhang slightly rather than exploding the shape.
        Shape::Diamond => Vec2::new(tw * 1.6 + 20.0, th * 2.2 + 10.0),
        Shape::Hexagon => {
            let h = th + 2.0 * pad_y;
            Vec2::new(tw + 2.0 * pad_x + h, h)
        }
        Shape::Lean | Shape::LeanAlt | Shape::Trapezoid | Shape::TrapezoidAlt => {
            let h = th + 2.0 * pad_y;
            Vec2::new(tw + 2.0 * pad_x + h * 0.9, h)
        }
        Shape::Asymmetric => Vec2::new(tw + 2.0 * pad_x + 10.0, th + 2.0 * pad_y),
    }
}

struct FlowPaint {
    clusters: Vec<RectF>,
    nodes: Vec<(RectF, Shape)>,
    edges: Vec<(super::flowchart::EdgePath, LineKind, Tip, Tip)>,
    node_fill: Hsla,
    node_stroke: Hsla,
    cluster_fill: Hsla,
    cluster_stroke: Hsla,
    edge_color: Hsla,
}

fn flowchart_element(fc: &Flowchart, m: &Measurer, theme: &Theme) -> AnyElement {
    let node_text: Vec<Vec2> = fc
        .nodes
        .iter()
        .map(|n| m.block(&n.label, TEXT_SIZE, LINE_H))
        .collect();
    let node_sizes: Vec<Vec2> = fc
        .nodes
        .iter()
        .zip(&node_text)
        .map(|(n, &t)| node_size(n.shape, t))
        .collect();
    // Chip size includes padding so layout reserves the real footprint.
    let edge_chips: Vec<Option<Vec2>> = fc
        .edges
        .iter()
        .map(|e| {
            e.label.as_ref().map(|l| {
                let s = m.block(l, LABEL_TEXT_SIZE, LABEL_LINE_H);
                Vec2::new(s.x + 14.0, s.y + 6.0)
            })
        })
        .collect();
    let titles: Vec<Vec2> = fc
        .clusters
        .iter()
        .map(|c| m.block(&c.title, LABEL_TEXT_SIZE, LABEL_LINE_H))
        .collect();
    let l = flow_layout(fc, &node_sizes, &edge_chips, &titles);

    let paint = Rc::new(FlowPaint {
        clusters: l.clusters.clone(),
        nodes: l
            .nodes
            .iter()
            .zip(&fc.nodes)
            .map(|(&r, n)| (r, n.shape))
            .collect(),
        edges: l
            .edges
            .iter()
            .zip(&fc.edges)
            .map(|(&p, e)| (p, e.line, e.start, e.end))
            .collect(),
        node_fill: theme.accent.opacity(0.08),
        node_stroke: theme.accent.opacity(0.5),
        cluster_fill: crate::theme::ink(0.02),
        cluster_stroke: crate::theme::hairline(0.12),
        edge_color: theme.text_muted,
    });
    let canvas_el = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_flowchart(&paint, bounds, window),
    )
    .absolute()
    .size_full();

    let mut root = div()
        .relative()
        .w(px(l.size.x))
        .h(px(l.size.y))
        .flex_none()
        .child(canvas_el);
    for (ci, c) in fc.clusters.iter().enumerate() {
        if c.title.iter().all(|t| t.is_empty()) {
            continue;
        }
        let r = l.clusters[ci];
        root = root.child(
            div()
                .absolute()
                .left(px(r.x + 10.0))
                .top(px(r.y + 5.0))
                .text_size(px(LABEL_TEXT_SIZE))
                .line_height(px(LABEL_LINE_H))
                .text_color(theme.text_faint)
                .whitespace_nowrap()
                .children(c.title.iter().map(|t| SharedString::from(t.clone()))),
        );
    }
    for (ni, n) in fc.nodes.iter().enumerate() {
        root = root.child(label_div(
            l.nodes[ni],
            &n.label,
            TEXT_SIZE,
            LINE_H,
            theme.text,
        ));
    }
    for (ei, e) in fc.edges.iter().enumerate() {
        let (Some(pos), Some(chip), Some(label)) =
            (l.edges[ei].label_pos, edge_chips[ei], e.label.as_ref())
        else {
            continue;
        };
        let r = RectF::new(pos.x - chip.x / 2.0, pos.y - chip.y / 2.0, chip.x, chip.y);
        root = root.child(
            label_div(r, label, LABEL_TEXT_SIZE, LABEL_LINE_H, theme.text_muted)
                .rounded(px(5.0))
                .bg(theme.bg)
                .border_1()
                .border_color(crate::theme::hairline(0.08)),
        );
    }
    root.into_any_element()
}

fn paint_flowchart(p: &FlowPaint, bounds: Bounds<Pixels>, window: &mut Window) {
    let o = bounds.origin;
    for &r in &p.clusters {
        window.paint_quad(quad(
            rect_bounds(o, r),
            px(8.0),
            p.cluster_fill,
            px(1.0),
            p.cluster_stroke,
            BorderStyle::Solid,
        ));
    }
    for &(path, line, start, end) in &p.edges {
        // Shorten stroked ends so lines don't poke through the tips.
        let dir_end = norm(Vec2::new(path.p1.x - path.c1.x, path.p1.y - path.c1.y));
        let dir_start = norm(Vec2::new(path.p0.x - path.c0.x, path.p0.y - path.c0.y));
        let trim = |tip: Tip| match tip {
            Tip::None => 0.0,
            Tip::Arrow => 5.0,
            Tip::Circle | Tip::Cross => 8.0,
        };
        let p1 = Vec2::new(
            path.p1.x - dir_end.x * trim(end),
            path.p1.y - dir_end.y * trim(end),
        );
        let p0 = Vec2::new(
            path.p0.x - dir_start.x * trim(start),
            path.p0.y - dir_start.y * trim(start),
        );
        let width = match line {
            LineKind::Thick => 2.75,
            _ => 1.5,
        };
        let mut pb = PathBuilder::stroke(px(width));
        if line == LineKind::Dotted {
            pb = pb.dash_array(&[px(4.0), px(3.5)]);
        }
        pb.move_to(at(o, p0));
        pb.cubic_bezier_to(at(o, p1), at(o, path.c0), at(o, path.c1));
        if let Ok(built) = pb.build() {
            window.paint_path(built, p.edge_color);
        }
        paint_tip(window, o, path.p1, dir_end, end, p.edge_color);
        paint_tip(window, o, path.p0, dir_start, start, p.edge_color);
    }
    for &(r, shape) in &p.nodes {
        paint_node(window, o, r, shape, p.node_fill, p.node_stroke);
    }
}

fn paint_node(
    window: &mut Window,
    o: Point<Pixels>,
    r: RectF,
    shape: Shape,
    fill: Hsla,
    stroke: Hsla,
) {
    let rounded = |radius: f32, window: &mut Window| {
        window.paint_quad(quad(
            rect_bounds(o, r),
            px(radius),
            fill,
            px(1.0),
            stroke,
            BorderStyle::Solid,
        ));
    };
    let poly = |pts: &[Vec2], window: &mut Window| {
        fill_polygon(window, o, pts, fill);
        stroke_polygon(window, o, pts, 1.0, stroke);
    };
    let (x, y, w, h) = (r.x, r.y, r.w, r.h);
    let (cx, cy) = (r.cx(), r.cy());
    match shape {
        Shape::Rect => rounded(5.0, window),
        Shape::Round => rounded(9.0, window),
        Shape::Stadium => rounded(h / 2.0, window),
        Shape::Subroutine => {
            rounded(4.0, window);
            stroke_line(
                window,
                o,
                Vec2::new(x + 5.0, y),
                Vec2::new(x + 5.0, y + h),
                1.0,
                None,
                stroke,
            );
            stroke_line(
                window,
                o,
                Vec2::new(x + w - 5.0, y),
                Vec2::new(x + w - 5.0, y + h),
                1.0,
                None,
                stroke,
            );
        }
        Shape::Cylinder => {
            let ry = 6.0f32.min(h / 4.0);
            let build = |stroke_w: Option<f32>| {
                let mut pb = match stroke_w {
                    Some(sw) => PathBuilder::stroke(px(sw)),
                    None => PathBuilder::fill(),
                };
                pb.move_to(at(o, Vec2::new(x, y + ry)));
                // Top cap bulges up, bottom cap bulges down.
                pb.arc_to(
                    point(px(w / 2.0), px(ry)),
                    px(0.0),
                    false,
                    true,
                    at(o, Vec2::new(x + w, y + ry)),
                );
                pb.line_to(at(o, Vec2::new(x + w, y + h - ry)));
                pb.arc_to(
                    point(px(w / 2.0), px(ry)),
                    px(0.0),
                    false,
                    true,
                    at(o, Vec2::new(x, y + h - ry)),
                );
                pb.close();
                pb.build().ok()
            };
            if let Some(path) = build(None) {
                window.paint_path(path, fill);
            }
            if let Some(path) = build(Some(1.0)) {
                window.paint_path(path, stroke);
            }
            // The visible inner rim of the top cap.
            let mut rim = PathBuilder::stroke(px(1.0));
            rim.move_to(at(o, Vec2::new(x, y + ry)));
            rim.arc_to(
                point(px(w / 2.0), px(ry)),
                px(0.0),
                false,
                false,
                at(o, Vec2::new(x + w, y + ry)),
            );
            if let Ok(path) = rim.build() {
                window.paint_path(path, stroke);
            }
        }
        Shape::Circle | Shape::DoubleCircle => {
            let radius = w.min(h) / 2.0;
            let c = Vec2::new(cx, cy);
            if let Some(path) = circle_path(o, c, radius, None) {
                window.paint_path(path, fill);
            }
            if let Some(path) = circle_path(o, c, radius, Some(1.0)) {
                window.paint_path(path, stroke);
            }
            if shape == Shape::DoubleCircle
                && let Some(path) = circle_path(o, c, radius - 4.0, Some(1.0))
            {
                window.paint_path(path, stroke);
            }
        }
        Shape::Diamond => poly(
            &[
                Vec2::new(cx, y),
                Vec2::new(x + w, cy),
                Vec2::new(cx, y + h),
                Vec2::new(x, cy),
            ],
            window,
        ),
        Shape::Hexagon => {
            let inset = (h / 2.0).min(w / 4.0);
            poly(
                &[
                    Vec2::new(x + inset, y),
                    Vec2::new(x + w - inset, y),
                    Vec2::new(x + w, cy),
                    Vec2::new(x + w - inset, y + h),
                    Vec2::new(x + inset, y + h),
                    Vec2::new(x, cy),
                ],
                window,
            );
        }
        Shape::Lean => {
            let s = (h * 0.45).min(w / 3.0);
            poly(
                &[
                    Vec2::new(x + s, y),
                    Vec2::new(x + w, y),
                    Vec2::new(x + w - s, y + h),
                    Vec2::new(x, y + h),
                ],
                window,
            );
        }
        Shape::LeanAlt => {
            let s = (h * 0.45).min(w / 3.0);
            poly(
                &[
                    Vec2::new(x, y),
                    Vec2::new(x + w - s, y),
                    Vec2::new(x + w, y + h),
                    Vec2::new(x + s, y + h),
                ],
                window,
            );
        }
        Shape::Trapezoid => {
            let s = (h * 0.45).min(w / 3.0);
            poly(
                &[
                    Vec2::new(x + s, y),
                    Vec2::new(x + w - s, y),
                    Vec2::new(x + w, y + h),
                    Vec2::new(x, y + h),
                ],
                window,
            );
        }
        Shape::TrapezoidAlt => {
            let s = (h * 0.45).min(w / 3.0);
            poly(
                &[
                    Vec2::new(x, y),
                    Vec2::new(x + w, y),
                    Vec2::new(x + w - s, y + h),
                    Vec2::new(x + s, y + h),
                ],
                window,
            );
        }
        Shape::Asymmetric => poly(
            &[
                Vec2::new(x, y),
                Vec2::new(x + w, y),
                Vec2::new(x + w, y + h),
                Vec2::new(x, y + h),
                Vec2::new(x + 9.0, cy),
            ],
            window,
        ),
    }
}

// ---------------------------------------------------------------------------
// Sequence
// ---------------------------------------------------------------------------

struct SeqPaint {
    headers: Vec<RectF>,
    lifelines: Vec<(f32, f32, f32)>,
    frames: Vec<(RectF, Vec<f32>)>,
    notes: Vec<RectF>,
    /// `(y, x0, x1, line, head, self_loop)` — x's are lifeline centers.
    arrows: Vec<(f32, f32, f32, ArrowLine, ArrowHead, bool)>,
    header_fill: Hsla,
    header_stroke: Hsla,
    line_color: Hsla,
    frame_stroke: Hsla,
    note_fill: Hsla,
    note_stroke: Hsla,
}

fn sequence_element(sd: &SequenceDiagram, m: &Measurer, theme: &Theme) -> AnyElement {
    let sl = super::sequence::layout(sd, &|s| m.width(s, TEXT_SIZE));

    let paint = Rc::new(SeqPaint {
        headers: sl.headers.clone(),
        lifelines: sl.lifelines.clone(),
        frames: sl
            .frames
            .iter()
            .map(|f| (f.rect, f.dividers.iter().map(|&(y, _)| y).collect()))
            .collect(),
        notes: sl.notes.iter().map(|&(r, _)| r).collect(),
        arrows: sl
            .messages
            .iter()
            .map(|g| {
                let Item::Message { line, head, .. } = &sd.items[g.item] else {
                    return (
                        g.y,
                        g.x0,
                        g.x1,
                        ArrowLine::Solid,
                        ArrowHead::None,
                        g.self_loop,
                    );
                };
                (g.y, g.x0, g.x1, *line, *head, g.self_loop)
            })
            .collect(),
        header_fill: theme.accent.opacity(0.08),
        header_stroke: theme.accent.opacity(0.5),
        line_color: theme.text_muted,
        frame_stroke: crate::theme::hairline(0.25),
        note_fill: theme.warning.opacity(0.07),
        note_stroke: theme.warning.opacity(0.25),
    });
    let canvas_el = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_sequence(&paint, bounds, window),
    )
    .absolute()
    .size_full();

    let mut root = div()
        .relative()
        .w(px(sl.size.x))
        .h(px(sl.size.y))
        .flex_none()
        .child(canvas_el);
    // Participant names over the header boxes.
    for (pi, p) in sd.participants.iter().enumerate() {
        root = root.child(label_div(
            sl.headers[pi],
            &p.label,
            TEXT_SIZE,
            SEQ_TEXT_LH,
            theme.text,
        ));
    }
    // Message labels (+ autonumber chips).
    for g in &sl.messages {
        let Item::Message { label, number, .. } = &sd.items[g.item] else {
            continue;
        };
        if !label.is_empty() {
            let w = label
                .iter()
                .map(|l| m.width(l, LABEL_TEXT_SIZE))
                .fold(0.0, f32::max);
            let h = label.len() as f32 * LABEL_LINE_H;
            let r = RectF::new(g.label_pos.x - w / 2.0, g.label_pos.y - h / 2.0, w, h);
            root = root.child(label_div(
                r,
                label,
                LABEL_TEXT_SIZE,
                LABEL_LINE_H,
                theme.text,
            ));
        }
        if let Some(n) = number {
            let text = n.to_string();
            let w = m.width(&text, 9.5) + 8.0;
            let x = if g.x1 >= g.x0 {
                g.x0 + 4.0
            } else {
                g.x0 - 4.0 - w
            };
            root = root.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(g.y - 15.0))
                    .h(px(13.0))
                    .px(px(4.0))
                    .rounded(px(4.0))
                    .bg(theme.accent.opacity(0.2))
                    .text_size(px(9.5))
                    .line_height(px(13.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(text)),
            );
        }
    }
    // Note text.
    for &(r, item) in &sl.notes {
        let Item::Note { label, .. } = &sd.items[item] else {
            continue;
        };
        root = root.child(label_div(
            r,
            label,
            LABEL_TEXT_SIZE,
            SEQ_TEXT_LH,
            theme.text,
        ));
    }
    // Frame kind chips + condition labels + else labels.
    for f in &sl.frames {
        let Item::BlockOpen { kind, label } = &sd.items[f.item] else {
            continue;
        };
        let mut row = div()
            .absolute()
            .left(px(f.rect.x))
            .top(px(f.rect.y))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .px(px(6.0))
                    .py(px(1.0))
                    .rounded_tl(px(6.0))
                    .rounded_br(px(6.0))
                    .bg(crate::theme::ink(0.07))
                    .border_1()
                    .border_color(crate::theme::hairline(0.2))
                    .text_size(px(10.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(*kind)),
            );
        if label.iter().any(|l| !l.is_empty()) {
            row = row.child(
                div()
                    .text_size(px(LABEL_TEXT_SIZE))
                    .text_color(theme.text_faint)
                    .whitespace_nowrap()
                    .child(SharedString::from(format!("[{}]", label.join(" ")))),
            );
        }
        root = root.child(row);
        for &(y, item) in &f.dividers {
            let Item::BlockElse { label } = &sd.items[item] else {
                continue;
            };
            if label.iter().any(|l| !l.is_empty()) {
                root = root.child(
                    div()
                        .absolute()
                        .left(px(f.rect.x + 10.0))
                        .top(px(y + 2.0))
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .whitespace_nowrap()
                        .child(SharedString::from(format!("[{}]", label.join(" ")))),
                );
            }
        }
    }
    root.into_any_element()
}

fn paint_sequence(p: &SeqPaint, bounds: Bounds<Pixels>, window: &mut Window) {
    let o = bounds.origin;
    for &(rect, ref dividers) in &p.frames {
        window.paint_quad(quad(
            rect_bounds(o, rect),
            px(6.0),
            crate::theme::ink(0.012),
            px(1.0),
            p.frame_stroke,
            BorderStyle::Solid,
        ));
        for &y in dividers {
            stroke_line(
                window,
                o,
                Vec2::new(rect.x, y),
                Vec2::new(rect.right(), y),
                1.0,
                Some([4.0, 3.0]),
                p.frame_stroke,
            );
        }
    }
    for &(x, y0, y1) in &p.lifelines {
        stroke_line(
            window,
            o,
            Vec2::new(x, y0),
            Vec2::new(x, y1),
            1.0,
            Some([4.0, 3.0]),
            p.frame_stroke,
        );
    }
    for &r in &p.notes {
        window.paint_quad(quad(
            rect_bounds(o, r),
            px(4.0),
            p.note_fill,
            px(1.0),
            p.note_stroke,
            BorderStyle::Solid,
        ));
    }
    for &(y, x0, x1, line, head, self_loop) in &p.arrows {
        let dash = match line {
            ArrowLine::Solid => None,
            ArrowLine::Dashed => Some([4.0, 3.0]),
        };
        if self_loop {
            // Out to the right, loop down, back to the lifeline.
            let out = x0 + super::sequence::SELF_LOOP_W;
            let (top, bottom) = (y - 8.0, y + 8.0);
            let mut pb = PathBuilder::stroke(px(1.5));
            if let Some([on, off]) = dash {
                pb = pb.dash_array(&[px(on), px(off)]);
            }
            pb.move_to(at(o, Vec2::new(x0, top)));
            pb.cubic_bezier_to(
                at(o, Vec2::new(x0 + 6.0, bottom)),
                at(o, Vec2::new(out, top - 2.0)),
                at(o, Vec2::new(out, bottom + 2.0)),
            );
            if let Ok(path) = pb.build() {
                window.paint_path(path, p.line_color);
            }
            paint_seq_head(
                window,
                o,
                Vec2::new(x0 + 6.0, bottom),
                Vec2::new(-1.0, 0.15),
                head,
                p.line_color,
            );
            continue;
        }
        let dir = if x1 >= x0 { 1.0 } else { -1.0 };
        let trim = match head {
            ArrowHead::Filled => 5.0,
            ArrowHead::Cross => 8.0,
            _ => 0.0,
        };
        stroke_line(
            window,
            o,
            Vec2::new(x0, y),
            Vec2::new(x1 - dir * trim, y),
            1.5,
            dash,
            p.line_color,
        );
        paint_seq_head(
            window,
            o,
            Vec2::new(x1, y),
            Vec2::new(dir, 0.0),
            head,
            p.line_color,
        );
    }
    for &r in &p.headers {
        window.paint_quad(quad(
            rect_bounds(o, r),
            px(6.0),
            p.header_fill,
            px(1.0),
            p.header_stroke,
            BorderStyle::Solid,
        ));
    }
}

fn paint_seq_head(
    window: &mut Window,
    o: Point<Pixels>,
    tip_at: Vec2,
    dir: Vec2,
    head: ArrowHead,
    color: Hsla,
) {
    let dir = norm(dir);
    match head {
        ArrowHead::Filled => paint_tip(window, o, tip_at, dir, Tip::Arrow, color),
        ArrowHead::Cross => paint_tip(window, o, tip_at, dir, Tip::Cross, color),
        ArrowHead::None => {}
        ArrowHead::Open => {
            // Chevron: two strokes meeting at the tip.
            let perp = Vec2::new(-dir.y, dir.x);
            for sign in [1.0f32, -1.0] {
                stroke_line(
                    window,
                    o,
                    tip_at,
                    Vec2::new(
                        tip_at.x - dir.x * 8.0 + perp.x * 4.5 * sign,
                        tip_at.y - dir.y * 8.0 + perp.y * 4.5 * sign,
                    ),
                    1.5,
                    None,
                    color,
                );
            }
        }
    }
}
