//! Mermaid `sequenceDiagram`: parser + pure layout (text measurement injected
//! as a closure, so tests run without gpui).
//!
//! Supported: `participant`/`actor` (with `as` aliases), the full arrow set
//! (`->>`, `-->>`, `->`, `-->`, `-x`, `--x`, `-)`, `--)`), self-messages,
//! `Note left of / right of / over`, nested `loop`/`alt`/`else`/`opt`/`par`/
//! `and`/`critical`/`break`/`rect` frames, `autonumber`, and `box` groups
//! (parsed for scoping, not drawn). `activate`/`deactivate` and `+`/`-`
//! suffixes parse and are ignored — activations are visual sugar we skip.

use super::{RectF, Vec2, label_lines};

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Participant {
    pub id: String,
    pub label: Vec<String>,
    pub actor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowLine {
    Solid,
    Dashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrowHead {
    /// `->>` / `-->>` — filled triangle.
    Filled,
    /// `->` / `-->` — plain line (mermaid draws no head).
    None,
    /// `-x` / `--x`.
    Cross,
    /// `-)` / `--)` — async open arrow.
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteSide {
    LeftOf,
    RightOf,
    Over,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Message {
        from: usize,
        to: usize,
        label: Vec<String>,
        line: ArrowLine,
        head: ArrowHead,
        number: Option<u32>,
    },
    Note {
        a: usize,
        b: usize,
        side: NoteSide,
        label: Vec<String>,
    },
    BlockOpen {
        kind: &'static str,
        label: Vec<String>,
    },
    BlockElse {
        label: Vec<String>,
    },
    BlockClose,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SequenceDiagram {
    pub participants: Vec<Participant>,
    pub items: Vec<Item>,
}

// ---------------------------------------------------------------------------
// Parse
// ---------------------------------------------------------------------------

pub fn parse(src: &str) -> Option<SequenceDiagram> {
    let mut participants: Vec<Participant> = Vec::new();
    let mut items: Vec<Item> = Vec::new();
    let mut autonumber = false;
    let mut next_number = 1u32;
    // `end` closes the innermost of: a drawn frame, or a `box` group.
    enum Scope {
        Frame,
        Box,
    }
    let mut scopes: Vec<Scope> = Vec::new();

    let ensure = |id: &str, participants: &mut Vec<Participant>| -> usize {
        let id = id.trim();
        if let Some(ix) = participants.iter().position(|p| p.id == id) {
            return ix;
        }
        participants.push(Participant {
            id: id.to_string(),
            label: label_lines(id),
            actor: false,
        });
        participants.len() - 1
    };

    let mut saw_header = false;
    for raw in src.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if !saw_header {
            if line.starts_with("sequenceDiagram") {
                saw_header = true;
                continue;
            }
            return None;
        }
        let lower = line.to_ascii_lowercase();
        let first_word = lower.split_whitespace().next().unwrap_or("");
        match first_word {
            "autonumber" => {
                autonumber = true;
                continue;
            }
            "activate" | "deactivate" | "title" | "acctitle:" | "accdescr:" | "links" | "link"
            | "properties" | "destroy" => continue,
            "box" => {
                scopes.push(Scope::Box);
                continue;
            }
            "participant" | "actor" | "create" => {
                let (is_actor, rest) = match first_word {
                    // `create participant X` / `create actor X`
                    "create" => {
                        let r = line[6..].trim_start();
                        if let Some(r2) = r.strip_prefix("participant") {
                            (false, r2)
                        } else if let Some(r2) = r.strip_prefix("actor") {
                            (true, r2)
                        } else {
                            continue;
                        }
                    }
                    "actor" => (true, &line[first_word.len()..]),
                    _ => (false, &line[first_word.len()..]),
                };
                let rest = rest.trim();
                let (id, label) = match split_as(rest) {
                    Some((id, alias)) => (id.trim(), label_lines(alias)),
                    None => {
                        let id = rest.trim_matches('"');
                        (id, label_lines(id))
                    }
                };
                if id.is_empty() {
                    continue;
                }
                let ix = ensure(id, &mut participants);
                participants[ix].label = label;
                participants[ix].actor = is_actor;
                continue;
            }
            "note" => {
                let rest = line[4..].trim_start();
                let rest_lower = rest.to_ascii_lowercase();
                let (side, after) = if rest_lower.starts_with("right of") {
                    (NoteSide::RightOf, rest[8..].trim_start())
                } else if rest_lower.starts_with("left of") {
                    (NoteSide::LeftOf, rest[7..].trim_start())
                } else if rest_lower.starts_with("over") {
                    (NoteSide::Over, rest[4..].trim_start())
                } else {
                    continue;
                };
                let Some((who, text)) = after.split_once(':') else {
                    continue;
                };
                let mut anchors = who.split(',').map(str::trim).filter(|s| !s.is_empty());
                let Some(first) = anchors.next() else {
                    continue;
                };
                let a = ensure(first, &mut participants);
                let b = anchors
                    .next()
                    .map(|s| ensure(s, &mut participants))
                    .unwrap_or(a);
                items.push(Item::Note {
                    a: a.min(b),
                    b: a.max(b),
                    side,
                    label: label_lines(text),
                });
                continue;
            }
            "loop" | "alt" | "opt" | "par" | "critical" | "break" | "rect" => {
                scopes.push(Scope::Frame);
                let kind: &'static str = match first_word {
                    "loop" => "loop",
                    "alt" => "alt",
                    "opt" => "opt",
                    "par" => "par",
                    "critical" => "critical",
                    "break" => "break",
                    _ => "rect",
                };
                let label = if kind == "rect" {
                    Vec::new()
                } else {
                    label_lines(line[first_word.len()..].trim())
                };
                items.push(Item::BlockOpen { kind, label });
                continue;
            }
            "else" | "and" | "option" => {
                if matches!(scopes.last(), Some(Scope::Frame)) {
                    items.push(Item::BlockElse {
                        label: label_lines(line[first_word.len()..].trim()),
                    });
                }
                continue;
            }
            "end" => {
                if matches!(scopes.pop(), Some(Scope::Frame)) {
                    items.push(Item::BlockClose);
                }
                continue;
            }
            _ => {}
        }

        // Message lines: `A->>+B: text`.
        const ARROWS: &[(&str, ArrowLine, ArrowHead)] = &[
            ("-->>", ArrowLine::Dashed, ArrowHead::Filled),
            ("->>", ArrowLine::Solid, ArrowHead::Filled),
            ("--x", ArrowLine::Dashed, ArrowHead::Cross),
            ("-x", ArrowLine::Solid, ArrowHead::Cross),
            ("--)", ArrowLine::Dashed, ArrowHead::Open),
            ("-)", ArrowLine::Solid, ArrowHead::Open),
            ("-->", ArrowLine::Dashed, ArrowHead::None),
            ("->", ArrowLine::Solid, ArrowHead::None),
        ];
        let Some((pos, &(op, line_kind, head))) = ARROWS
            .iter()
            .enumerate()
            .filter_map(|(i, a)| line.find(a.0).map(|p| (p, &ARROWS[i])))
            .min_by_key(|&(p, a)| (p, std::cmp::Reverse(a.0.len())))
        else {
            continue; // unknown statement — skip
        };
        let from_id = line[..pos].trim().trim_matches('"');
        let rest = &line[pos + op.len()..];
        let (to_part, text) = match rest.split_once(':') {
            Some((t, text)) => (t, text.trim()),
            None => (rest, ""),
        };
        let to_id = to_part
            .trim()
            .trim_start_matches(['+', '-'])
            .trim()
            .trim_matches('"');
        if from_id.is_empty() || to_id.is_empty() {
            continue;
        }
        let from = ensure(from_id, &mut participants);
        let to = ensure(to_id, &mut participants);
        let number = autonumber.then(|| {
            let n = next_number;
            next_number += 1;
            n
        });
        items.push(Item::Message {
            from,
            to,
            label: if text.is_empty() {
                Vec::new()
            } else {
                label_lines(text)
            },
            line: line_kind,
            head,
            number,
        });
    }
    if participants.is_empty() || participants.len() > 40 {
        return None;
    }
    Some(SequenceDiagram {
        participants,
        items,
    })
}

fn strip_comment(line: &str) -> &str {
    match line.find("%%") {
        Some(ix) => &line[..ix],
        None => line,
    }
}

/// Split `X as Label`, honoring the first unquoted ` as `.
fn split_as(rest: &str) -> Option<(&str, &str)> {
    let ix = rest.find(" as ")?;
    Some((&rest[..ix], rest[ix + 4..].trim().trim_matches('"')))
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

pub const SEQ_TEXT_LH: f32 = 16.0;
pub const MARGIN: f32 = 14.0;
const COL_GAP: f32 = 52.0;
const HEAD_PAD_X: f32 = 14.0;
const HEAD_PAD_Y: f32 = 8.0;
const FRAME_INSET: f32 = 7.0;
pub const SELF_LOOP_W: f32 = 34.0;

#[derive(Debug, Clone, PartialEq)]
pub struct MsgGeom {
    pub item: usize,
    /// Arrow line y.
    pub y: f32,
    pub x0: f32,
    pub x1: f32,
    pub self_loop: bool,
    /// Label box center.
    pub label_pos: Vec2,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrameGeom {
    pub item: usize,
    pub rect: RectF,
    /// `(y, item index)` of each `else`/`and` divider.
    pub dividers: Vec<(f32, usize)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeqLayout {
    pub size: Vec2,
    /// Participant header boxes, parallel to `participants`.
    pub headers: Vec<RectF>,
    /// Lifeline segments `(x, y0, y1)`.
    pub lifelines: Vec<(f32, f32, f32)>,
    pub messages: Vec<MsgGeom>,
    /// `(rect, item index)` per note.
    pub notes: Vec<(RectF, usize)>,
    pub frames: Vec<FrameGeom>,
}

/// `measure(line)` returns the shaped width of one label line at the diagram
/// text size.
pub fn layout(sd: &SequenceDiagram, measure: &dyn Fn(&str) -> f32) -> SeqLayout {
    let n = sd.participants.len();
    let text_w = |lines: &[String]| -> f32 { lines.iter().map(|l| measure(l)).fold(0.0, f32::max) };

    // Header boxes: uniform height, per-participant width.
    let head_h = sd
        .participants
        .iter()
        .map(|p| p.label.len() as f32 * SEQ_TEXT_LH + 2.0 * HEAD_PAD_Y)
        .fold(0.0, f32::max);
    let head_w: Vec<f32> = sd
        .participants
        .iter()
        .map(|p| text_w(&p.label) + 2.0 * HEAD_PAD_X)
        .collect();

    // Column gaps grow to fit message/note labels between the columns.
    let mut gaps = vec![COL_GAP; n.saturating_sub(1)];
    let mut trailing = 0.0f32; // extra room right of the last column
    let mut leading = 0.0f32; // extra room left of the first column
    for item in &sd.items {
        match item {
            Item::Message {
                from, to, label, ..
            } => {
                let w = text_w(label);
                if from == to {
                    let need = w + SELF_LOOP_W + 12.0;
                    if *from + 1 < n {
                        gaps[*from] = gaps[*from].max(need);
                    } else {
                        trailing = trailing.max(need);
                    }
                } else {
                    let (lo, hi) = (from.min(to), from.max(to));
                    let span_heads: f32 = (lo + 1..*hi).map(|k| head_w[k]).sum::<f32>()
                        + head_w[*lo] / 2.0
                        + head_w[*hi] / 2.0;
                    let current: f32 = span_heads + gaps[*lo..*hi].iter().sum::<f32>();
                    let deficit = w + 24.0 - current;
                    if deficit > 0.0 {
                        let per = deficit / (hi - lo) as f32;
                        for g in &mut gaps[*lo..*hi] {
                            *g += per;
                        }
                    }
                }
            }
            Item::Note { a, b, side, label } => {
                let w = text_w(label) + 20.0;
                match side {
                    NoteSide::RightOf => {
                        if *a + 1 < n {
                            gaps[*a] = gaps[*a].max(w + 20.0);
                        } else {
                            trailing = trailing.max(w + 20.0);
                        }
                    }
                    NoteSide::LeftOf => {
                        if *a > 0 {
                            gaps[*a - 1] = gaps[*a - 1].max(w + 20.0);
                        } else {
                            leading = leading.max(w + 20.0);
                        }
                    }
                    NoteSide::Over => {
                        if a == b {
                            let need = (w - head_w[*a]) / 2.0;
                            if need > 0.0 {
                                if *a > 0 {
                                    gaps[*a - 1] = gaps[*a - 1].max(COL_GAP + need);
                                }
                                if *a + 1 < n {
                                    gaps[*a] = gaps[*a].max(COL_GAP + need);
                                } else {
                                    trailing = trailing.max(need);
                                }
                                if *a == 0 {
                                    leading = leading.max(need);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Column centers.
    let mut centers = vec![0.0f32; n];
    let mut x = MARGIN + leading + head_w.first().copied().unwrap_or(0.0) / 2.0;
    for i in 0..n {
        centers[i] = x;
        if i + 1 < n {
            x += head_w[i] / 2.0 + gaps[i] + head_w[i + 1] / 2.0;
        }
    }
    let width = x + head_w.last().copied().unwrap_or(0.0) / 2.0 + trailing + MARGIN;

    // Vertical pass.
    let mut y = MARGIN + head_h + 14.0;
    let mut messages = Vec::new();
    let mut notes = Vec::new();
    let mut frames: Vec<FrameGeom> = Vec::new();
    type OpenFrame = (usize, f32, Vec<(f32, usize)>); // (item, top, dividers)
    let mut open: Vec<OpenFrame> = Vec::new();
    for (ix, item) in sd.items.iter().enumerate() {
        match item {
            Item::Message {
                from, to, label, ..
            } => {
                let label_h = label.len() as f32 * SEQ_TEXT_LH;
                if from == to {
                    let cx = centers[*from];
                    let line_y = y + label_h.max(SEQ_TEXT_LH) / 2.0;
                    messages.push(MsgGeom {
                        item: ix,
                        y: line_y,
                        x0: cx,
                        x1: cx,
                        self_loop: true,
                        // Label center, to the right of the loop arc.
                        label_pos: Vec2::new(cx + SELF_LOOP_W + 6.0 + text_w(label) / 2.0, line_y),
                    });
                    y += label_h.max(SEQ_TEXT_LH) + 26.0;
                } else {
                    let (x0, x1) = (centers[*from], centers[*to]);
                    let line_y = y + label_h + 5.0;
                    messages.push(MsgGeom {
                        item: ix,
                        y: line_y,
                        x0,
                        x1,
                        self_loop: false,
                        // Label center, above the arrow line.
                        label_pos: Vec2::new((x0 + x1) / 2.0, y + label_h / 2.0),
                    });
                    y = line_y + 22.0;
                }
            }
            Item::Note { a, b, side, label } => {
                let w = text_w(label) + 20.0;
                let h = label.len() as f32 * SEQ_TEXT_LH + 10.0;
                let rect = match side {
                    NoteSide::RightOf => RectF::new(centers[*a] + 16.0, y, w, h),
                    NoteSide::LeftOf => RectF::new(centers[*a] - 16.0 - w, y, w, h),
                    NoteSide::Over => {
                        let (ca, cb) = (centers[*a], centers[*b]);
                        let w = w.max(cb - ca + 48.0);
                        RectF::new((ca + cb) / 2.0 - w / 2.0, y, w, h)
                    }
                };
                notes.push((rect, ix));
                y += h + 10.0;
            }
            Item::BlockOpen { .. } => {
                open.push((ix, y, Vec::new()));
                y += 26.0;
            }
            Item::BlockElse { .. } => {
                if let Some(top) = open.last_mut() {
                    top.2.push((y + 4.0, ix));
                }
                y += 24.0;
            }
            Item::BlockClose => {
                if let Some((item, top, dividers)) = open.pop() {
                    let depth = open.len() as f32;
                    let inset = MARGIN / 2.0 + depth * FRAME_INSET;
                    frames.push(FrameGeom {
                        item,
                        rect: RectF::new(inset, top, width - 2.0 * inset, y - top + 4.0),
                        dividers,
                    });
                    y += 16.0;
                }
            }
        }
    }
    // Unclosed frames (streaming): close at the current cursor.
    while let Some((item, top, dividers)) = open.pop() {
        let depth = open.len() as f32;
        let inset = MARGIN / 2.0 + depth * FRAME_INSET;
        frames.push(FrameGeom {
            item,
            rect: RectF::new(inset, top, width - 2.0 * inset, y - top + 4.0),
            dividers,
        });
        y += 16.0;
    }

    let lifeline_end = y + 6.0;
    let headers: Vec<RectF> = (0..n)
        .map(|i| RectF::new(centers[i] - head_w[i] / 2.0, MARGIN, head_w[i], head_h))
        .collect();
    let lifelines: Vec<(f32, f32, f32)> = (0..n)
        .map(|i| (centers[i], MARGIN + head_h, lifeline_end))
        .collect();
    SeqLayout {
        size: Vec2::new(width, lifeline_end + MARGIN),
        headers,
        lifelines,
        messages,
        notes,
        frames,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measure(s: &str) -> f32 {
        s.chars().count() as f32 * 7.0
    }

    #[test]
    fn parses_participants_messages_and_frames() {
        let sd = parse(
            "sequenceDiagram\n  autonumber\n  participant A as Alice\n  actor B\n  A->>+B: hello\n  B-->>-A: world\n  loop every day\n    A-)B: async ping\n  end\n  Note over A,B: both\n  alt ok\n    A->B: fine\n  else bad\n    A--xB: broken\n  end",
        )
        .unwrap();
        assert_eq!(sd.participants.len(), 2);
        assert_eq!(sd.participants[0].label, vec!["Alice"]);
        assert!(sd.participants[1].actor);
        let msgs: Vec<_> = sd
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Message {
                    from,
                    to,
                    head,
                    line,
                    number,
                    ..
                } => Some((*from, *to, *head, *line, *number)),
                _ => None,
            })
            .collect();
        assert_eq!(msgs.len(), 5);
        assert_eq!(
            msgs[0],
            (0, 1, ArrowHead::Filled, ArrowLine::Solid, Some(1))
        );
        assert_eq!(msgs[1].3, ArrowLine::Dashed);
        assert_eq!(msgs[2].2, ArrowHead::Open);
        assert_eq!(msgs[3].2, ArrowHead::None);
        assert_eq!(msgs[4].2, ArrowHead::Cross);
        assert_eq!(
            sd.items
                .iter()
                .filter(|i| matches!(i, Item::BlockOpen { .. }))
                .count(),
            2
        );
        assert_eq!(
            sd.items
                .iter()
                .filter(|i| matches!(i, Item::BlockClose))
                .count(),
            2
        );
        assert!(sd.items.iter().any(|i| matches!(
            i,
            Item::Note {
                side: NoteSide::Over,
                ..
            }
        )));
    }

    #[test]
    fn box_groups_do_not_leak_frame_closes() {
        let sd = parse(
            "sequenceDiagram\n  box Purple Group\n  participant A\n  participant B\n  end\n  A->>B: hi",
        )
        .unwrap();
        assert!(!sd.items.iter().any(|i| matches!(i, Item::BlockClose)));
        assert_eq!(sd.participants.len(), 2);
    }

    #[test]
    fn implicit_participants_appear_in_order() {
        let sd = parse("sequenceDiagram\nC->>A: x\nA->>B: y").unwrap();
        let ids: Vec<_> = sd.participants.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["C", "A", "B"]);
    }

    #[test]
    fn layout_orders_columns_and_rows() {
        let sd = parse("sequenceDiagram\nA->>B: a long message label here\nB-->>A: ok").unwrap();
        let out = layout(&sd, &measure);
        assert!(out.headers[0].right() < out.headers[1].x);
        // Gap grew to fit the label between the two lifelines.
        let dist = out.lifelines[1].0 - out.lifelines[0].0;
        assert!(dist >= measure("a long message label here"));
        assert_eq!(out.messages.len(), 2);
        assert!(out.messages[0].y < out.messages[1].y);
        // Reply goes right-to-left.
        assert!(out.messages[1].x0 > out.messages[1].x1);
        assert!(out.size.x > 0.0 && out.size.y > out.messages[1].y);
    }

    #[test]
    fn self_message_and_unclosed_frame_survive_streaming() {
        let sd = parse("sequenceDiagram\nA->>A: think\nloop forever\nA->>B: spin").unwrap();
        let out = layout(&sd, &measure);
        assert!(out.messages[0].self_loop);
        assert_eq!(out.frames.len(), 1);
        assert!(out.frames[0].rect.h > 0.0);
    }

    #[test]
    fn rejects_non_sequence() {
        assert!(parse("flowchart LR\nA-->B").is_none());
        assert!(parse("sequenceDiagram").is_none()); // no participants
    }
}
