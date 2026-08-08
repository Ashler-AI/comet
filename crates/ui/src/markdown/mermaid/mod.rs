//! Native mermaid rendering — no JS engine, no webview, no network.
//!
//! The two diagram families agents actually emit — `flowchart`/`graph` and
//! `sequenceDiagram` — parse into small models here and lay out with pure
//! math (sizes injected, so parse + layout unit-test without gpui). The gpui
//! side ([`render`]) measures labels through the window text system, paints
//! shapes/edges on a canvas with [`gpui::PathBuilder`], and overlays plain
//! text divs. Anything else (`gantt`, `pie`, typos…) returns `None` and the
//! caller falls back to the ordinary highlighted code block — mermaid source
//! is never lost, only upgraded when we understand it.

pub mod flowchart;
pub mod render;
pub mod sequence;

/// A point / extent in diagram-local logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rect in diagram-local logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
}

/// A parsed mermaid diagram we know how to draw.
#[derive(Debug, Clone, PartialEq)]
pub enum Diagram {
    Flowchart(flowchart::Flowchart),
    Sequence(sequence::SequenceDiagram),
}

/// Parse mermaid source into a supported diagram, or `None` (→ code block).
pub fn parse(source: &str) -> Option<Diagram> {
    let body = strip_preamble(source);
    let header = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("%%"))?;
    let keyword = header.split_whitespace().next()?;
    match keyword {
        "flowchart" | "graph" => flowchart::parse(&body).map(Diagram::Flowchart),
        "sequenceDiagram" => sequence::parse(&body).map(Diagram::Sequence),
        _ => None,
    }
}

/// Drop YAML frontmatter (`---\ntitle: x\n---`) and `%%{init: …}%%`
/// directives — both are config we deliberately ignore.
fn strip_preamble(source: &str) -> String {
    let mut lines: Vec<&str> = source.lines().collect();
    if lines.first().is_some_and(|l| l.trim() == "---")
        && let Some(close) = lines.iter().skip(1).position(|l| l.trim() == "---")
    {
        lines.drain(..close + 2);
    }
    let joined = lines.join("\n");
    // `%%{ … }%%` directives can span lines; excise every occurrence.
    let mut out = String::with_capacity(joined.len());
    let mut rest = joined.as_str();
    while let Some(start) = rest.find("%%{") {
        out.push_str(&rest[..start]);
        match rest[start..].find("}%%") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Split a label on `<br>` variants, strip remaining tags, decode entities.
/// Every diagram family shares mermaid's label conventions.
pub(crate) fn label_lines(raw: &str) -> Vec<String> {
    let mut text = raw.trim().to_string();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text = text[1..text.len() - 1].to_string();
    }
    // Normalize <br>, <br/>, <br /> (any case) into '\n'.
    let mut normalized = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(open) = rest.find('<') {
        let (before, after) = rest.split_at(open);
        normalized.push_str(before);
        match after.find('>') {
            Some(close) => {
                let tag = &after[1..close];
                let t = tag.trim().trim_end_matches('/').trim().to_ascii_lowercase();
                if t == "br" {
                    normalized.push('\n');
                }
                // Other tags (<b>, <i>, spans…) drop; we render plain text.
                rest = &after[close + 1..];
            }
            None => {
                normalized.push_str(after);
                rest = "";
            }
        }
    }
    normalized.push_str(rest);
    normalized
        .split('\n')
        .map(|l| decode_entities(l.trim()))
        .collect()
}

/// Decode the HTML entities mermaid labels lean on (`&amp;` and the mermaid
/// `#quot;`-style spellings). Unknown entities pass through verbatim.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '&' && c != '#' {
            out.push(c);
            continue;
        }
        let rest = &s[i + 1..];
        let Some(semi) = rest.find(';').filter(|&n| n <= 8) else {
            out.push(c);
            continue;
        };
        let name = &rest[..semi];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => name
                .strip_prefix('#')
                .or(if c == '#' { Some(name) } else { None })
                .and_then(|d| d.parse::<u32>().ok())
                .and_then(char::from_u32),
        };
        match decoded {
            Some(d) => {
                out.push(d);
                for _ in 0..=semi {
                    chars.next();
                }
            }
            None => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_by_header() {
        assert!(matches!(
            parse("flowchart LR\nA --> B"),
            Some(Diagram::Flowchart(_))
        ));
        assert!(matches!(
            parse("graph TD\nA --> B"),
            Some(Diagram::Flowchart(_))
        ));
        assert!(matches!(
            parse("sequenceDiagram\nA->>B: hi"),
            Some(Diagram::Sequence(_))
        ));
        assert!(parse("pie\n\"a\": 1").is_none());
        assert!(parse("not mermaid at all").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn preamble_is_stripped() {
        let src = "---\ntitle: X\n---\n%%{init: {'theme':'dark'}}%%\nflowchart LR\nA --> B";
        assert!(matches!(parse(src), Some(Diagram::Flowchart(_))));
    }

    #[test]
    fn labels_split_and_decode() {
        assert_eq!(label_lines("a<br/>b"), vec!["a", "b"]);
        assert_eq!(label_lines("a<br >b<BR/>c"), vec!["a", "b", "c"]);
        assert_eq!(
            label_lines("\"quoted &amp; #quot;x#quot;\""),
            vec!["quoted & \"x\""]
        );
        assert_eq!(label_lines("<b>bold</b> text"), vec!["bold text"]);
    }
}
