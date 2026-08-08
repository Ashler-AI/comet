//! Mermaid `flowchart` / `graph`: a forgiving line-based parser into a small
//! graph model, plus a Sugiyama-lite layered layout. Layout is pure — node /
//! label sizes are injected — so everything here unit-tests without gpui.
//!
//! Parsing philosophy: agents stream these diagrams token by token, so the
//! parser NEVER errors on a line it does not understand — unknown statements
//! are skipped and the diagram renders from what did parse. The caller only
//! falls back to a code block when no nodes materialize at all.

use super::{RectF, Vec2, label_lines};

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Td,
    Bt,
    Lr,
    Rl,
}

impl Dir {
    pub fn horizontal(self) -> bool {
        matches!(self, Dir::Lr | Dir::Rl)
    }
    /// Flow runs against the coordinate axis (mirrored at the end of layout).
    fn reversed(self) -> bool {
        matches!(self, Dir::Rl | Dir::Bt)
    }
    fn parse(s: &str) -> Option<Dir> {
        match s {
            "TD" | "TB" | "v" => Some(Dir::Td),
            "BT" | "^" => Some(Dir::Bt),
            "LR" | ">" => Some(Dir::Lr),
            "RL" | "<" => Some(Dir::Rl),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Rect,
    Round,
    Stadium,
    Subroutine,
    Cylinder,
    Circle,
    DoubleCircle,
    Diamond,
    Hexagon,
    /// `[/text/]` — parallelogram leaning right.
    Lean,
    /// `[\text\]` — parallelogram leaning left.
    LeanAlt,
    /// `[/text\]`.
    Trapezoid,
    /// `[\text/]`.
    TrapezoidAlt,
    /// `>text]` — flag / asymmetric.
    Asymmetric,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Solid,
    Dotted,
    Thick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tip {
    None,
    Arrow,
    Circle,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: String,
    pub label: Vec<String>,
    pub shape: Shape,
    /// Owning subgraph (first subgraph body that mentions the node).
    pub cluster: Option<usize>,
}

/// An edge endpoint — subgraph ids are legal endpoints in mermaid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndRef {
    Node(usize),
    Cluster(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub from: EndRef,
    pub to: EndRef,
    pub label: Option<Vec<String>>,
    pub line: LineKind,
    pub start: Tip,
    pub end: Tip,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    pub id: String,
    pub title: Vec<String>,
    pub parent: Option<usize>,
    pub dir: Option<Dir>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flowchart {
    pub dir: Dir,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub clusters: Vec<Cluster>,
}

// ---------------------------------------------------------------------------
// Parse
// ---------------------------------------------------------------------------

struct Mention {
    id: String,
    label: Option<String>,
    shape: Option<Shape>,
    ctx: Option<usize>,
}

struct RawEdge {
    from: String,
    to: String,
    label: Option<String>,
    line: LineKind,
    start: Tip,
    end: Tip,
}
pub fn parse(src: &str) -> Option<Flowchart> {
    let mut dir = None;
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut mentions: Vec<Mention> = Vec::new();
    let mut raw_edges: Vec<RawEdge> = Vec::new();

    for line in src.lines() {
        for stmt in split_statements(strip_comment(line)) {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            let (kw, rest) = split_keyword(stmt);
            match kw {
                "flowchart" | "graph" if dir.is_none() => {
                    dir = Some(
                        rest.split_whitespace()
                            .next()
                            .and_then(Dir::parse)
                            .unwrap_or(Dir::Td),
                    );
                    continue;
                }
                // Styling / interaction statements we deliberately ignore.
                "classDef" | "class" | "style" | "linkStyle" | "click" | "accTitle"
                | "accDescr" | "title" => continue,
                "direction" => {
                    if let Some(d) = rest.split_whitespace().next().and_then(Dir::parse) {
                        match stack.last() {
                            Some(&c) => clusters[c].dir = Some(d),
                            None => dir = Some(d),
                        }
                    }
                    continue;
                }
                "subgraph" => {
                    let (id, title) = match rest.find('[') {
                        Some(open) => {
                            let close = rest.rfind(']').unwrap_or(rest.len());
                            let inner = &rest[open + 1..close.max(open + 1)];
                            (rest[..open].trim().to_string(), label_lines(inner))
                        }
                        None => {
                            let id = rest.trim().trim_matches('"').to_string();
                            (id.clone(), label_lines(&id))
                        }
                    };
                    clusters.push(Cluster {
                        id,
                        title,
                        parent: stack.last().copied(),
                        dir: None,
                    });
                    stack.push(clusters.len() - 1);
                    continue;
                }
                "end" if rest.is_empty() => {
                    stack.pop();
                    continue;
                }
                _ => {}
            }
            if dir.is_none() {
                // Statements before the `flowchart`/`graph` header: not ours.
                return None;
            }
            parse_chain(stmt, stack.last().copied(), &mut mentions, &mut raw_edges);
        }
    }
    let dir = dir?;

    // Resolve mentions → nodes. A mention whose id names a subgraph and that
    // carries no shape/label of its own is a cluster reference, not a node.
    let mut nodes: Vec<Node> = Vec::new();
    let mut by_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let cluster_ix = |id: &str, clusters: &[Cluster]| clusters.iter().position(|c| c.id == id);
    for m in &mentions {
        if m.label.is_none() && m.shape.is_none() && cluster_ix(&m.id, &clusters).is_some() {
            continue;
        }
        match by_id.get(&m.id) {
            Some(&ix) => {
                let node = &mut nodes[ix];
                if let Some(raw) = &m.label {
                    node.label = label_lines(raw);
                }
                if let Some(shape) = m.shape {
                    node.shape = shape;
                }
                if node.cluster.is_none() {
                    node.cluster = m.ctx;
                }
            }
            None => {
                by_id.insert(m.id.clone(), nodes.len());
                nodes.push(Node {
                    id: m.id.clone(),
                    label: m
                        .label
                        .as_deref()
                        .map(label_lines)
                        .unwrap_or_else(|| vec![m.id.clone()]),
                    shape: m.shape.unwrap_or(Shape::Rect),
                    cluster: m.ctx,
                });
            }
        }
    }
    if nodes.is_empty() || nodes.len() > 400 {
        // Empty → nothing to draw; absurdly large → protect the UI thread and
        // fall back to the scrollable code block.
        return None;
    }

    let resolve = |id: &str| -> Option<EndRef> {
        by_id
            .get(id)
            .map(|&ix| EndRef::Node(ix))
            .or_else(|| cluster_ix(id, &clusters).map(EndRef::Cluster))
    };
    let edges = raw_edges
        .iter()
        .filter_map(|e| {
            let from = resolve(&e.from)?;
            let to = resolve(&e.to)?;
            Some(Edge {
                from,
                to,
                label: e.label.as_deref().map(label_lines),
                line: e.line,
                start: e.start,
                end: e.end,
            })
        })
        .collect();
    Some(Flowchart {
        dir,
        nodes,
        edges,
        clusters,
    })
}

/// Truncate at a `%%` comment start outside quotes.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_quote = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => in_quote = !in_quote,
            b'%' if !in_quote && i + 1 < bytes.len() && bytes[i + 1] == b'%' => {
                return &line[..i];
            }
            _ => {}
        }
        i += 1;
    }
    line
}

/// Split a physical line into `;`-separated statements, outside quotes.
fn split_statements(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quote = false;
    let mut start = 0;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            ';' if !in_quote => {
                out.push(&line[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&line[start..]);
    out
}

/// Leading identifier-ish keyword + the rest (for statement dispatch).
fn split_keyword(stmt: &str) -> (&str, &str) {
    let end = stmt
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(stmt.len());
    (&stmt[..end], stmt[end..].trim_start())
}

struct Scan {
    cs: Vec<char>,
    i: usize,
}

impl Scan {
    fn new(s: &str) -> Self {
        Self {
            cs: s.chars().collect(),
            i: 0,
        }
    }
    fn peek(&self, k: usize) -> Option<char> {
        self.cs.get(self.i + k).copied()
    }
    fn at_end(&self) -> bool {
        self.i >= self.cs.len()
    }
    fn ws(&mut self) {
        while self.peek(0).is_some_and(char::is_whitespace) {
            self.i += 1;
        }
    }
    fn starts(&self, pat: &str) -> bool {
        pat.chars()
            .enumerate()
            .all(|(k, c)| self.peek(k) == Some(c))
    }
    fn eat(&mut self, pat: &str) -> bool {
        if self.starts(pat) {
            self.i += pat.chars().count();
            true
        } else {
            false
        }
    }
    /// Scan forward (quote-aware) until one of `closers` matches; returns the
    /// consumed text and the closer's index, with the closer consumed too.
    fn until_closer(&mut self, closers: &[&str]) -> Option<(String, usize)> {
        let mut text = String::new();
        let mut in_quote = false;
        while let Some(c) = self.peek(0) {
            if c == '"' {
                in_quote = !in_quote;
                text.push(c);
                self.i += 1;
                continue;
            }
            if !in_quote && let Some(which) = closers.iter().position(|cl| self.starts(cl)) {
                self.eat(closers[which]);
                return Some((text, which));
            }
            text.push(c);
            self.i += 1;
        }
        None
    }
}

fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.')
}

/// `id` + optional shape-delimited label + optional `:::class` suffix.
fn parse_node(scan: &mut Scan) -> Option<(String, Option<String>, Option<Shape>)> {
    scan.ws();
    let start = scan.i;
    while scan.peek(0).is_some_and(is_id_char) {
        scan.i += 1;
    }
    if scan.i == start {
        return None;
    }
    let id: String = scan.cs[start..scan.i].iter().collect();

    // Openers longest-first; each maps to its closer set. `[/`+`[\` resolve
    // the exact shape by which closer terminated the label.
    const OPENERS: &[(&str, &[&str], &[Shape])] = &[
        ("(((", &[")))"], &[Shape::DoubleCircle]),
        ("([", &["])"], &[Shape::Stadium]),
        ("((", &["))"], &[Shape::Circle]),
        ("[[", &["]]"], &[Shape::Subroutine]),
        ("[(", &[")]"], &[Shape::Cylinder]),
        ("[/", &["/]", "\\]"], &[Shape::Lean, Shape::Trapezoid]),
        (
            "[\\",
            &["\\]", "/]"],
            &[Shape::LeanAlt, Shape::TrapezoidAlt],
        ),
        ("{{", &["}}"], &[Shape::Hexagon]),
        ("(", &[")"], &[Shape::Round]),
        ("[", &["]"], &[Shape::Rect]),
        ("{", &["}"], &[Shape::Diamond]),
        (">", &["]"], &[Shape::Asymmetric]),
    ];
    let mut label = None;
    let mut shape = None;
    for (open, closers, shapes) in OPENERS {
        if scan.eat(open) {
            let (text, which) = scan.until_closer(closers)?;
            label = Some(text);
            shape = Some(shapes[which.min(shapes.len() - 1)]);
            break;
        }
    }
    if scan.eat(":::") {
        while scan.peek(0).is_some_and(|c| is_id_char(c) || c == '-') {
            scan.i += 1;
        }
    }
    Some((id, label, shape))
}

struct Link {
    line: LineKind,
    start: Tip,
    end: Tip,
    label: Option<String>,
}

fn parse_link(scan: &mut Scan) -> Option<Link> {
    scan.ws();
    let save = scan.i;
    let mut start_tip = Tip::None;
    if let Some(c) = scan.peek(0)
        && matches!(scan.peek(1), Some('-' | '=' | '.'))
    {
        match c {
            '<' => start_tip = Tip::Arrow,
            'x' => start_tip = Tip::Cross,
            'o' => start_tip = Tip::Circle,
            _ => {}
        }
        if start_tip != Tip::None {
            scan.i += 1;
        }
    }
    let body_start = scan.i;
    while matches!(scan.peek(0), Some('-' | '=' | '.')) {
        scan.i += 1;
    }
    let body: String = scan.cs[body_start..scan.i].iter().collect();
    if body.chars().count() < 2 {
        scan.i = save;
        return None;
    }
    let mut line = if body.contains('.') {
        LineKind::Dotted
    } else if body.starts_with('=') {
        LineKind::Thick
    } else {
        LineKind::Solid
    };
    let mut end_tip = Tip::None;
    match scan.peek(0) {
        Some('>') => {
            end_tip = Tip::Arrow;
            scan.i += 1;
        }
        // `x`/`o` only close the link when not starting an identifier
        // (`A --oval` must read as edge into node `oval`).
        Some('x') if !scan.peek(1).is_some_and(is_id_char) => {
            end_tip = Tip::Cross;
            scan.i += 1;
        }
        Some('o') if !scan.peek(1).is_some_and(is_id_char) => {
            end_tip = Tip::Circle;
            scan.i += 1;
        }
        _ => {}
    }

    scan.ws();
    let mut label = None;
    if scan.eat("|") {
        let (text, _) = scan.until_closer(&["|"])?;
        label = Some(text);
    } else if end_tip == Tip::None && body.chars().count() == 2 {
        // `A-- text -->B` / `A-. text .->B` / `A== text ==>B`: an open
        // two-char link start means the label sits inline until the closer.
        const CLOSERS: &[&str] = &[
            "-->", "---", "--x", "--o", ".->", ".-x", ".-o", ".-", "==>", "===", "==x", "==o", "==",
        ];
        let Some((text, which)) = scan.until_closer(CLOSERS) else {
            scan.i = save;
            return None;
        };
        let closer = CLOSERS[which];
        line = if closer.contains('.') {
            LineKind::Dotted
        } else if closer.contains('=') {
            LineKind::Thick
        } else {
            line
        };
        end_tip = match closer.chars().last() {
            Some('>') => Tip::Arrow,
            Some('x') => Tip::Cross,
            Some('o') => Tip::Circle,
            _ => Tip::None,
        };
        let text = text.trim();
        if !text.is_empty() {
            label = Some(text.to_string());
        }
    }
    Some(Link {
        line,
        start: start_tip,
        end: end_tip,
        label,
    })
}

/// `group (link group)*` where group = `node (& node)*`.
fn parse_chain(
    stmt: &str,
    ctx: Option<usize>,
    mentions: &mut Vec<Mention>,
    edges: &mut Vec<RawEdge>,
) {
    let mut scan = Scan::new(stmt);
    let parse_group = |scan: &mut Scan, mentions: &mut Vec<Mention>| -> Option<Vec<String>> {
        let mut ids = Vec::new();
        loop {
            let (id, label, shape) = parse_node(scan)?;
            mentions.push(Mention {
                id: id.clone(),
                label,
                shape,
                ctx,
            });
            ids.push(id);
            scan.ws();
            if !scan.eat("&") {
                return Some(ids);
            }
        }
    };
    let Some(mut group) = parse_group(&mut scan, mentions) else {
        return;
    };
    loop {
        scan.ws();
        if scan.at_end() {
            return;
        }
        let Some(link) = parse_link(&mut scan) else {
            return; // unknown tail — keep what we have
        };
        let Some(next) = parse_group(&mut scan, mentions) else {
            return;
        };
        for a in &group {
            for b in &next {
                edges.push(RawEdge {
                    from: a.clone(),
                    to: b.clone(),
                    label: link.label.clone(),
                    line: link.line,
                    start: link.start,
                    end: link.end,
                });
            }
        }
        group = next;
    }
}

// ---------------------------------------------------------------------------
// Layout — Sugiyama-lite per cluster, clusters composed as meta-nodes
// ---------------------------------------------------------------------------

pub const NODE_GAP: f32 = 18.0;
pub const RANK_GAP: f32 = 44.0;
pub const MARGIN: f32 = 14.0;
pub const CLUSTER_PAD: f32 = 12.0;
/// Extra main-axis room a self-loop needs beyond its node.
const LOOP_EXTENT: f32 = 40.0;

/// One routed edge: a cubic bézier plus the label chip center (if labeled).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgePath {
    pub p0: Vec2,
    pub c0: Vec2,
    pub c1: Vec2,
    pub p1: Vec2,
    pub label_pos: Option<Vec2>,
}

impl EdgePath {
    pub fn midpoint(&self) -> Vec2 {
        Vec2::new(
            (self.p0.x + 3.0 * self.c0.x + 3.0 * self.c1.x + self.p1.x) / 8.0,
            (self.p0.y + 3.0 * self.c0.y + 3.0 * self.c1.y + self.p1.y) / 8.0,
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub size: Vec2,
    /// Parallel to `Flowchart::nodes`.
    pub nodes: Vec<RectF>,
    /// Parallel to `Flowchart::clusters`; includes the title strip.
    pub clusters: Vec<RectF>,
    /// Parallel to `Flowchart::edges`.
    pub edges: Vec<EdgePath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Entity {
    N(usize),
    C(usize),
}

/// Compute the diagram layout. `node_sizes` / `edge_label_sizes` /
/// `cluster_title_sizes` are measured by the caller (gpui or a test stub).
pub fn layout(
    fc: &Flowchart,
    node_sizes: &[Vec2],
    edge_label_sizes: &[Option<Vec2>],
    cluster_title_sizes: &[Vec2],
) -> Layout {
    let n_clusters = fc.clusters.len();
    // Deepest-first cluster order so child sizes exist before parents lay out.
    let depth = |mut c: usize| {
        let mut d = 0;
        while let Some(p) = fc.clusters[c].parent {
            d += 1;
            c = p;
        }
        d
    };
    let mut ctx_order: Vec<Option<usize>> = (0..n_clusters).map(Some).collect();
    ctx_order.sort_by_key(|c| std::cmp::Reverse(depth(c.unwrap())));
    ctx_order.push(None);

    let effective_dir = |ctx: Option<usize>| -> Dir {
        let mut cur = ctx;
        while let Some(c) = cur {
            if let Some(d) = fc.clusters[c].dir {
                return d;
            }
            cur = fc.clusters[c].parent;
        }
        fc.dir
    };

    // Context chain (list of enclosing clusters, innermost first) per entity.
    let chain_of = |r: EndRef| -> Vec<Option<usize>> {
        let mut ctx = match r {
            EndRef::Node(n) => fc.nodes[n].cluster,
            EndRef::Cluster(c) => fc.clusters[c].parent,
        };
        let mut chain = Vec::new();
        while let Some(c) = ctx {
            chain.push(Some(c));
            ctx = fc.clusters[c].parent;
        }
        chain.push(None);
        chain
    };
    // Lowest common ancestor context of an edge, and the endpoint entities
    // lifted to direct children of that context.
    let lift = |r: EndRef, lca: Option<usize>| -> Entity {
        let own = match r {
            EndRef::Node(n) => (Entity::N(n), fc.nodes[n].cluster),
            EndRef::Cluster(c) => (Entity::C(c), fc.clusters[c].parent),
        };
        let (mut e, mut ctx) = own;
        while ctx != lca {
            let c = ctx.expect("lca is an ancestor");
            e = Entity::C(c);
            ctx = fc.clusters[c].parent;
        }
        e
    };
    let edge_lca = |e: &Edge| -> Option<usize> {
        let a = chain_of(e.from);
        let b = chain_of(e.to);
        a.iter().find(|c| b.contains(c)).copied().unwrap_or(None)
    };

    // ---- per-context local layouts -------------------------------------
    let mut cluster_sizes: Vec<Vec2> = vec![Vec2::default(); n_clusters];
    let mut cluster_title_h: Vec<f32> = vec![0.0; n_clusters];
    let mut local_rects: std::collections::HashMap<Entity, RectF> =
        std::collections::HashMap::new();
    let mut content_sizes: std::collections::HashMap<Option<usize>, Vec2> =
        std::collections::HashMap::new();

    for &ctx in &ctx_order {
        let dir = effective_dir(ctx);
        let mut entities: Vec<Entity> = fc
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.cluster == ctx)
            .map(|(i, _)| Entity::N(i))
            .collect();
        entities.extend(
            fc.clusters
                .iter()
                .enumerate()
                .filter(|(_, c)| c.parent == ctx)
                .map(|(i, _)| Entity::C(i)),
        );
        let size_of = |e: Entity| -> Vec2 {
            match e {
                Entity::N(n) => node_sizes[n],
                Entity::C(c) => cluster_sizes[c],
            }
        };
        // Local edges: lifted to this context's direct children.
        let mut local_edges: Vec<(Entity, Entity, f32)> = Vec::new();
        for (ei, e) in fc.edges.iter().enumerate() {
            if edge_lca(e) != ctx {
                continue;
            }
            let a = lift(e.from, ctx);
            let b = lift(e.to, ctx);
            if a == b {
                continue; // self loop or degenerate boundary edge
            }
            let label_main =
                edge_label_sizes[ei].map_or(0.0, |s| if dir.horizontal() { s.x } else { s.y });
            local_edges.push((a, b, label_main));
        }

        let extent = layout_local(&entities, &local_edges, dir, &size_of, &mut local_rects);
        content_sizes.insert(ctx, extent);
        if let Some(c) = ctx {
            let title = cluster_title_sizes[c];
            let title_h = if fc.clusters[c].title.iter().any(|l| !l.is_empty()) {
                title.y + 8.0
            } else {
                6.0
            };
            cluster_title_h[c] = title_h;
            cluster_sizes[c] = Vec2::new(
                (extent.x + 2.0 * CLUSTER_PAD).max(title.x + 2.0 * CLUSTER_PAD),
                extent.y + 2.0 * CLUSTER_PAD + title_h,
            );
        }
    }

    // ---- compose absolute rects -----------------------------------------
    let mut abs_nodes: Vec<RectF> = vec![RectF::default(); fc.nodes.len()];
    let mut abs_clusters: Vec<RectF> = vec![RectF::default(); n_clusters];
    // Iterative DFS from the root context.
    let mut stack: Vec<(Option<usize>, Vec2)> = vec![(None, Vec2::new(MARGIN, MARGIN))];
    while let Some((ctx, origin)) = stack.pop() {
        for (i, n) in fc.nodes.iter().enumerate() {
            if n.cluster == ctx {
                let r = local_rects[&Entity::N(i)];
                abs_nodes[i] = RectF::new(origin.x + r.x, origin.y + r.y, r.w, r.h);
            }
        }
        for (i, c) in fc.clusters.iter().enumerate() {
            if c.parent == ctx {
                let r = local_rects[&Entity::C(i)];
                let rect = RectF::new(origin.x + r.x, origin.y + r.y, r.w, r.h);
                abs_clusters[i] = rect;
                stack.push((
                    Some(i),
                    Vec2::new(
                        rect.x + CLUSTER_PAD,
                        rect.y + CLUSTER_PAD + cluster_title_h[i],
                    ),
                ));
            }
        }
    }

    // ---- route edges ------------------------------------------------------
    let rect_of = |r: EndRef| -> RectF {
        match r {
            EndRef::Node(n) => abs_nodes[n],
            EndRef::Cluster(c) => abs_clusters[c],
        }
    };
    // Spread parallel edges between the same pair so they don't coincide.
    let pair_key = |e: &Edge| {
        let a = match e.from {
            EndRef::Node(n) => (0usize, n),
            EndRef::Cluster(c) => (1usize, c),
        };
        let b = match e.to {
            EndRef::Node(n) => (0usize, n),
            EndRef::Cluster(c) => (1usize, c),
        };
        if a <= b { (a, b) } else { (b, a) }
    };
    let mut pair_counts: std::collections::HashMap<_, (usize, usize)> =
        std::collections::HashMap::new();
    for e in &fc.edges {
        pair_counts.entry(pair_key(e)).or_insert((0, 0)).0 += 1;
    }
    // Backward edges (cycles) swing around the far side of everything —
    // their midpoint label then sits in clear space instead of on top of
    // forward-edge chips between the same ranks.
    let lane = {
        let mut max_bottom = 0.0f32;
        let mut max_right = 0.0f32;
        for r in abs_nodes.iter().chain(abs_clusters.iter()) {
            max_bottom = max_bottom.max(r.bottom());
            max_right = max_right.max(r.right());
        }
        Vec2::new(max_right + 36.0, max_bottom + 36.0)
    };
    let mut edge_paths: Vec<EdgePath> = Vec::with_capacity(fc.edges.len());
    for (ei, e) in fc.edges.iter().enumerate() {
        let horizontal = effective_dir(edge_lca(e)).horizontal();
        let s = rect_of(e.from);
        let t = rect_of(e.to);
        let entry = pair_counts.get_mut(&pair_key(e)).unwrap();
        let (count, seen) = *entry;
        entry.1 += 1;
        let spread = (seen as f32 - (count as f32 - 1.0) / 2.0) * 10.0;
        let mut path = route_edge(s, t, horizontal, spread, lane);
        if edge_label_sizes[ei].is_some() {
            path.label_pos = Some(path.midpoint());
        }
        edge_paths.push(path);
    }

    // ---- final bounds (labels / loops can spill past node extents) --------
    let mut max = Vec2::new(0.0, 0.0);
    let mut min = Vec2::new(f32::MAX, f32::MAX);
    let mut include = |x: f32, y: f32| {
        max.x = max.x.max(x);
        max.y = max.y.max(y);
        min.x = min.x.min(x);
        min.y = min.y.min(y);
    };
    for r in abs_nodes.iter().chain(abs_clusters.iter()) {
        include(r.x, r.y);
        include(r.right(), r.bottom());
    }
    for (ei, p) in edge_paths.iter().enumerate() {
        for v in [p.p0, p.c0, p.c1, p.p1] {
            include(v.x, v.y);
        }
        if let (Some(pos), Some(size)) = (p.label_pos, edge_label_sizes[ei]) {
            include(pos.x - size.x / 2.0 - 8.0, pos.y - size.y / 2.0 - 4.0);
            include(pos.x + size.x / 2.0 + 8.0, pos.y + size.y / 2.0 + 4.0);
        }
    }
    if min.x > max.x {
        min = Vec2::default();
    }
    let shift = Vec2::new(MARGIN - min.x.min(MARGIN), MARGIN - min.y.min(MARGIN));
    if shift.x != 0.0 || shift.y != 0.0 {
        for r in abs_nodes.iter_mut().chain(abs_clusters.iter_mut()) {
            r.x += shift.x;
            r.y += shift.y;
        }
        for p in edge_paths.iter_mut() {
            for v in [&mut p.p0, &mut p.c0, &mut p.c1, &mut p.p1] {
                v.x += shift.x;
                v.y += shift.y;
            }
            if let Some(l) = &mut p.label_pos {
                l.x += shift.x;
                l.y += shift.y;
            }
        }
    }
    Layout {
        size: Vec2::new(max.x + shift.x + MARGIN, max.y + shift.y + MARGIN),
        nodes: abs_nodes,
        clusters: abs_clusters,
        edges: edge_paths,
    }
}

/// Layered layout of one context's entities; writes local rects, returns the
/// content extent.
fn layout_local(
    entities: &[Entity],
    edges: &[(Entity, Entity, f32)],
    dir: Dir,
    size_of: &dyn Fn(Entity) -> Vec2,
    out: &mut std::collections::HashMap<Entity, RectF>,
) -> Vec2 {
    if entities.is_empty() {
        return Vec2::new(48.0, 24.0);
    }
    let ix_of: std::collections::HashMap<Entity, usize> =
        entities.iter().enumerate().map(|(i, &e)| (e, i)).collect();
    let n = entities.len();
    let adj: Vec<(usize, usize, f32)> = edges
        .iter()
        .filter_map(|&(a, b, l)| Some((*ix_of.get(&a)?, *ix_of.get(&b)?, l)))
        .collect();

    // Cycle breaking: orient every edge forward along a DFS reverse-postorder
    // (a topological order for DAGs; cycles get exactly their back edges
    // flipped, layout-only — arrowheads still draw at the true target).
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b, _) in &adj {
        succ[a].push(b);
    }
    let mut visited = vec![false; n];
    let mut postorder: Vec<usize> = Vec::with_capacity(n);
    let dfs = |root: usize, visited: &mut Vec<bool>, postorder: &mut Vec<usize>| {
        if visited[root] {
            return;
        }
        // Iterative DFS: (vertex, next child index) so deep chains can't
        // overflow the stack on adversarial input.
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        visited[root] = true;
        while let Some(&mut (v, ref mut child)) = stack.last_mut() {
            if let Some(&w) = succ[v].get(*child) {
                *child += 1;
                if !visited[w] {
                    visited[w] = true;
                    stack.push((w, 0));
                }
            } else {
                postorder.push(v);
                stack.pop();
            }
        }
    };
    // Roots: entities with no incoming edges first, then any unvisited.
    let mut has_in = vec![false; n];
    for &(_, b, _) in &adj {
        has_in[b] = true;
    }
    for v in (0..n).filter(|&v| !has_in[v]) {
        dfs(v, &mut visited, &mut postorder);
    }
    for v in 0..n {
        dfs(v, &mut visited, &mut postorder);
    }
    let topo_pos: Vec<usize> = {
        let mut pos = vec![0; n];
        for (i, &v) in postorder.iter().rev().enumerate() {
            pos[v] = i;
        }
        pos
    };
    let mut acyclic: Vec<(usize, usize, f32)> = Vec::with_capacity(adj.len());
    for &(a, b, l) in &adj {
        if a == b {
            continue;
        }
        if topo_pos[a] <= topo_pos[b] {
            acyclic.push((a, b, l));
        } else {
            acyclic.push((b, a, l));
        }
    }

    // Ranks: longest path, one relaxation pass in topological order.
    let order_seed: Vec<usize> = {
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&v| topo_pos[v]);
        order
    };
    let mut out_edges: Vec<Vec<(usize, f32)>> = vec![Vec::new(); n];
    for &(a, b, l) in &acyclic {
        out_edges[a].push((b, l));
    }
    let mut rank = vec![0usize; n];
    for &v in &order_seed {
        for &(b, _) in &out_edges[v] {
            rank[b] = rank[b].max(rank[v] + 1);
        }
    }
    let n_ranks = rank.iter().copied().max().unwrap_or(0) + 1;
    let mut ranks: Vec<Vec<usize>> = vec![Vec::new(); n_ranks];
    for v in order_seed {
        ranks[rank[v]].push(v);
    }

    // Ordering: barycenter sweeps over undirected adjacency.
    let mut neighbors: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b, _) in &acyclic {
        neighbors[a].push(b);
        neighbors[b].push(a);
    }
    let mut pos_in_rank = vec![0f32; n];
    let reindex = |ranks: &[Vec<usize>], pos: &mut [f32]| {
        for row in ranks {
            for (i, &v) in row.iter().enumerate() {
                pos[v] = i as f32;
            }
        }
    };
    reindex(&ranks, &mut pos_in_rank);
    for sweep in 0..4 {
        let down = sweep % 2 == 0;
        let range: Vec<usize> = if down {
            (0..n_ranks).collect()
        } else {
            (0..n_ranks).rev().collect()
        };
        for &r in &range {
            let row = &mut ranks[r];
            let mut keyed: Vec<(f32, usize)> = row
                .iter()
                .map(|&v| {
                    let adjacent: Vec<f32> = neighbors[v]
                        .iter()
                        .filter(|&&w| if down { rank[w] < r } else { rank[w] > r })
                        .map(|&w| pos_in_rank[w])
                        .collect();
                    let key = if adjacent.is_empty() {
                        pos_in_rank[v]
                    } else {
                        adjacent.iter().sum::<f32>() / adjacent.len() as f32
                    };
                    (key, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            *row = keyed.into_iter().map(|(_, v)| v).collect();
            reindex(&ranks, &mut pos_in_rank);
        }
    }

    // Geometry. Main axis: flow direction; cross axis: the other.
    let main_size = |v: usize| {
        let s = size_of(entities[v]);
        if dir.horizontal() { s.x } else { s.y }
    };
    let cross_size = |v: usize| {
        let s = size_of(entities[v]);
        if dir.horizontal() { s.y } else { s.x }
    };
    // Per-rank main extents and gaps (labels widen the gap they live in).
    let rank_main: Vec<f32> = ranks
        .iter()
        .map(|row| row.iter().map(|&v| main_size(v)).fold(0.0, f32::max))
        .collect();
    let mut gaps = vec![RANK_GAP; n_ranks.saturating_sub(1)];
    for &(a, b, label_main) in &acyclic {
        if label_main > 0.0 {
            let lo = rank[a].min(rank[b]);
            if lo < gaps.len() {
                gaps[lo] = gaps[lo].max(label_main + 22.0);
            }
        }
    }
    let mut rank_start = vec![0f32; n_ranks];
    for r in 1..n_ranks {
        rank_start[r] = rank_start[r - 1] + rank_main[r - 1] + gaps[r - 1];
    }

    // Cross positions: stack, center ranks, then median-straighten.
    let mut cross = vec![0f32; n];
    let rank_cross_extent = |row: &[usize], cross_size: &dyn Fn(usize) -> f32| -> f32 {
        row.iter().map(|&v| cross_size(v)).sum::<f32>() + NODE_GAP * (row.len() as f32 - 1.0)
    };
    let max_extent = ranks
        .iter()
        .map(|row| rank_cross_extent(row, &cross_size))
        .fold(0.0, f32::max);
    for row in &ranks {
        let extent = rank_cross_extent(row, &cross_size);
        let mut cursor = (max_extent - extent) / 2.0;
        for &v in row {
            cross[v] = cursor + cross_size(v) / 2.0;
            cursor += cross_size(v) + NODE_GAP;
        }
    }
    // One down + one up median pass with order-preserving separation.
    for (pass, down) in [(0, true), (1, false)] {
        let _ = pass;
        let range: Vec<usize> = if down {
            (1..n_ranks).collect()
        } else {
            (0..n_ranks.saturating_sub(1)).rev().collect()
        };
        for r in range {
            let row = ranks[r].clone();
            let mut desired: Vec<f32> = row
                .iter()
                .map(|&v| {
                    let adjacent: Vec<f32> = neighbors[v]
                        .iter()
                        .filter(|&&w| if down { rank[w] < r } else { rank[w] > r })
                        .map(|&w| cross[w])
                        .collect();
                    if adjacent.is_empty() {
                        cross[v]
                    } else {
                        adjacent.iter().sum::<f32>() / adjacent.len() as f32
                    }
                })
                .collect();
            for i in 1..row.len() {
                let min_pos = desired[i - 1]
                    + cross_size(row[i - 1]) / 2.0
                    + cross_size(row[i]) / 2.0
                    + NODE_GAP;
                if desired[i] < min_pos {
                    desired[i] = min_pos;
                }
            }
            for (i, &v) in row.iter().enumerate() {
                cross[v] = desired[i];
            }
        }
    }
    let min_cross = (0..n)
        .map(|v| cross[v] - cross_size(v) / 2.0)
        .fold(f32::MAX, f32::min);
    let main_extent = rank_start[n_ranks - 1] + rank_main[n_ranks - 1];
    let cross_extent = (0..n)
        .map(|v| cross[v] + cross_size(v) / 2.0 - min_cross)
        .fold(0.0, f32::max);

    for v in 0..n {
        let s = size_of(entities[v]);
        // Center within the rank band on the main axis.
        let main_c = rank_start[rank[v]] + rank_main[rank[v]] / 2.0;
        let (mut m, c) = (main_c, cross[v] - min_cross);
        if dir.reversed() {
            m = main_extent - m;
        }
        let rect = if dir.horizontal() {
            RectF::new(m - s.x / 2.0, c - s.y / 2.0, s.x, s.y)
        } else {
            RectF::new(c - s.x / 2.0, m - s.y / 2.0, s.x, s.y)
        };
        out.insert(entities[v], rect);
    }
    if dir.horizontal() {
        Vec2::new(main_extent, cross_extent)
    } else {
        Vec2::new(cross_extent, main_extent)
    }
}

/// Bézier between two rects along the layout axis. `spread` offsets parallel
/// edges; self-edges loop out of the far side; backward edges arc through
/// `lane` — a cross-axis lane past every node (`x` = right of all content,
/// `y` = below it), so cycle labels land in clear space.
fn route_edge(s: RectF, t: RectF, horizontal: bool, spread: f32, lane: Vec2) -> EdgePath {
    let same = (s.x - t.x).abs() < 0.5
        && (s.y - t.y).abs() < 0.5
        && (s.w - t.w).abs() < 0.5
        && (s.h - t.h).abs() < 0.5;
    if same {
        // Self loop on the main-axis far side.
        return if horizontal {
            let dy = (s.h * 0.25).min(10.0);
            let x = s.right();
            EdgePath {
                p0: Vec2::new(x, s.cy() - dy),
                c0: Vec2::new(x + LOOP_EXTENT, s.cy() - dy - 8.0),
                c1: Vec2::new(x + LOOP_EXTENT, s.cy() + dy + 8.0),
                p1: Vec2::new(x, s.cy() + dy),
                label_pos: None,
            }
        } else {
            let dx = (s.w * 0.25).min(10.0);
            let y = s.bottom();
            EdgePath {
                p0: Vec2::new(s.cx() - dx, y),
                c0: Vec2::new(s.cx() - dx - 8.0, y + LOOP_EXTENT),
                c1: Vec2::new(s.cx() + dx + 8.0, y + LOOP_EXTENT),
                p1: Vec2::new(s.cx() + dx, y),
                label_pos: None,
            }
        };
    }
    if horizontal {
        let (p0, p1) = if t.x >= s.right() + 4.0 {
            (
                Vec2::new(s.right(), s.cy() + spread),
                Vec2::new(t.x, t.cy() + spread),
            )
        } else if s.x >= t.right() + 4.0 {
            // Backward: under everything and back up into the target.
            let y = lane.y + spread;
            return EdgePath {
                p0: Vec2::new(s.cx(), s.bottom()),
                c0: Vec2::new(s.cx(), y),
                c1: Vec2::new(t.cx(), y),
                p1: Vec2::new(t.cx(), t.bottom()),
                label_pos: None,
            };
        } else {
            // Same rank band: connect vertically.
            let (p0, p1) = if t.cy() >= s.cy() {
                (
                    Vec2::new(s.cx() + spread, s.bottom()),
                    Vec2::new(t.cx() + spread, t.y),
                )
            } else {
                (
                    Vec2::new(s.cx() + spread, s.y),
                    Vec2::new(t.cx() + spread, t.bottom()),
                )
            };
            let dy = (p1.y - p0.y) * 0.45;
            return EdgePath {
                p0,
                c0: Vec2::new(p0.x, p0.y + dy),
                c1: Vec2::new(p1.x, p1.y - dy),
                p1,
                label_pos: None,
            };
        };
        let dx = (p1.x - p0.x) * 0.45;
        EdgePath {
            p0,
            c0: Vec2::new(p0.x + dx, p0.y),
            c1: Vec2::new(p1.x - dx, p1.y),
            p1,
            label_pos: None,
        }
    } else {
        let (p0, p1) = if t.y >= s.bottom() + 4.0 {
            (
                Vec2::new(s.cx() + spread, s.bottom()),
                Vec2::new(t.cx() + spread, t.y),
            )
        } else if s.y >= t.bottom() + 4.0 {
            // Backward: around the right of everything and back into the target.
            let x = lane.x + spread;
            return EdgePath {
                p0: Vec2::new(s.right(), s.cy()),
                c0: Vec2::new(x, s.cy()),
                c1: Vec2::new(x, t.cy()),
                p1: Vec2::new(t.right(), t.cy()),
                label_pos: None,
            };
        } else {
            let (p0, p1) = if t.cx() >= s.cx() {
                (
                    Vec2::new(s.right(), s.cy() + spread),
                    Vec2::new(t.x, t.cy() + spread),
                )
            } else {
                (
                    Vec2::new(s.x, s.cy() + spread),
                    Vec2::new(t.right(), t.cy() + spread),
                )
            };
            let dx = (p1.x - p0.x) * 0.45;
            return EdgePath {
                p0,
                c0: Vec2::new(p0.x + dx, p0.y),
                c1: Vec2::new(p1.x - dx, p1.y),
                p1,
                label_pos: None,
            };
        };
        let dy = (p1.y - p0.y) * 0.45;
        EdgePath {
            p0,
            c0: Vec2::new(p0.x, p0.y + dy),
            c1: Vec2::new(p1.x, p1.y - dy),
            p1,
            label_pos: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_sizes(fc: &Flowchart) -> (Vec<Vec2>, Vec<Option<Vec2>>, Vec<Vec2>) {
        let nodes = fc
            .nodes
            .iter()
            .map(|n| {
                let w = n.label.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f32 * 7.0
                    + 24.0;
                Vec2::new(w, n.label.len() as f32 * 17.0 + 18.0)
            })
            .collect();
        let labels = fc
            .edges
            .iter()
            .map(|e| {
                e.label.as_ref().map(|l| {
                    let w =
                        l.iter().map(|s| s.chars().count()).max().unwrap_or(0) as f32 * 6.0 + 10.0;
                    Vec2::new(w, l.len() as f32 * 15.0)
                })
            })
            .collect();
        let titles = fc
            .clusters
            .iter()
            .map(|c| {
                let w = c.title.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f32 * 6.5;
                Vec2::new(w, 15.0)
            })
            .collect();
        (nodes, labels, titles)
    }

    #[test]
    fn parses_screenshot_flowchart() {
        let fc = parse(
            "flowchart LR\n    H[harness adapters<br/>omp / claude / codex] -->|\"ToolResult { output }\"| J[run journal<br/>jsonl]\n    J -->|\"ToolCallDetail RPC<br/>(relay-forwardable)\"| T[transcript chip pane]\n    J -.->|\"render_parts still strips\"| D[synced doc<br/>unchanged]",
        )
        .unwrap();
        assert_eq!(fc.dir, Dir::Lr);
        assert_eq!(fc.nodes.len(), 4);
        assert_eq!(fc.edges.len(), 3);
        assert_eq!(
            fc.nodes[0].label,
            vec!["harness adapters", "omp / claude / codex"]
        );
        assert_eq!(
            fc.edges[0].label.as_ref().unwrap(),
            &vec!["ToolResult { output }"]
        );
        assert_eq!(
            fc.edges[1].label.as_ref().unwrap(),
            &vec!["ToolCallDetail RPC", "(relay-forwardable)"]
        );
        assert_eq!(fc.edges[2].line, LineKind::Dotted);
        assert_eq!(fc.edges[2].end, Tip::Arrow);
    }

    #[test]
    fn parses_shapes() {
        let fc = parse(
            "graph TD\nA[rect]\nB(round)\nC([stadium])\nD[[sub]]\nE[(db)]\nF((circle))\nG{decision}\nH{{hex}}\nI[/lean/]\nJ[/trap\\]\nK>flag]",
        )
        .unwrap();
        let shape = |id: &str| fc.nodes.iter().find(|n| n.id == id).unwrap().shape;
        assert_eq!(shape("A"), Shape::Rect);
        assert_eq!(shape("B"), Shape::Round);
        assert_eq!(shape("C"), Shape::Stadium);
        assert_eq!(shape("D"), Shape::Subroutine);
        assert_eq!(shape("E"), Shape::Cylinder);
        assert_eq!(shape("F"), Shape::Circle);
        assert_eq!(shape("G"), Shape::Diamond);
        assert_eq!(shape("H"), Shape::Hexagon);
        assert_eq!(shape("I"), Shape::Lean);
        assert_eq!(shape("J"), Shape::Trapezoid);
        assert_eq!(shape("K"), Shape::Asymmetric);
    }

    #[test]
    fn parses_chains_groups_and_link_kinds() {
        let fc = parse(
            "graph LR\nA --> B & C --> D\nB === E\nC -.- F\nA -- label --> G\nH --o I\nJ --x K",
        )
        .unwrap();
        // A→B, A→C, B→D, C→D (the group chains through) + 5 singles
        assert_eq!(fc.edges.len(), 9);
        let edge = |from: &str, to: &str| {
            fc.edges
                .iter()
                .find(|e| {
                    matches!(e.from, EndRef::Node(a) if fc.nodes[a].id == from)
                        && matches!(e.to, EndRef::Node(b) if fc.nodes[b].id == to)
                })
                .unwrap()
        };
        assert_eq!(edge("B", "D").end, Tip::Arrow);
        assert_eq!(edge("B", "E").line, LineKind::Thick);
        assert_eq!(edge("B", "E").end, Tip::None);
        assert_eq!(edge("C", "F").line, LineKind::Dotted);
        assert_eq!(edge("A", "G").label.as_ref().unwrap(), &vec!["label"]);
        assert_eq!(edge("H", "I").end, Tip::Circle);
        assert_eq!(edge("J", "K").end, Tip::Cross);
    }

    #[test]
    fn dangling_open_link_keeps_left_nodes() {
        // `--` followed by an identifier is invalid mermaid (open links need
        // `---`); the forgiving parser keeps the left node and drops the rest
        // of the statement instead of erroring the whole diagram.
        let fc = parse("graph LR\nA --oval").unwrap();
        assert_eq!(fc.nodes.len(), 1);
        assert!(fc.edges.is_empty());
        // The real spellings both work.
        let fc = parse("graph LR\nA --- oval\nB --> oval").unwrap();
        assert_eq!(fc.nodes.len(), 3);
        assert_eq!(fc.edges[0].end, Tip::None);
        assert_eq!(fc.edges[1].end, Tip::Arrow);
    }

    #[test]
    fn subgraph_membership_and_edges() {
        let fc = parse(
            "flowchart TD\nsubgraph API [API Layer]\n  A --> B\nend\nsubgraph DB\n  C[(store)]\nend\nB --> C\nA --> DB",
        )
        .unwrap();
        assert_eq!(fc.clusters.len(), 2);
        assert_eq!(fc.clusters[0].title, vec!["API Layer"]);
        let node = |id: &str| fc.nodes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(node("A").cluster, Some(0));
        assert_eq!(node("C").cluster, Some(1));
        // `A --> DB` binds to the cluster, not a phantom node.
        assert!(fc.edges.iter().any(|e| matches!(e.to, EndRef::Cluster(1))));
        assert!(!fc.nodes.iter().any(|n| n.id == "DB"));
    }

    #[test]
    fn skips_style_noise_and_comments() {
        let fc = parse(
            "graph LR\n%% a comment\nA --> B %% trailing\nclassDef hot fill:#f00\nclass A hot\nstyle B fill:#00f\nlinkStyle 0 stroke:red\nclick A callback",
        )
        .unwrap();
        assert_eq!(fc.nodes.len(), 2);
        assert_eq!(fc.edges.len(), 1);
    }

    #[test]
    fn cycle_does_not_hang_and_lays_out() {
        let fc = parse("graph TD\nA --> B\nB --> C\nC --> A").unwrap();
        let (ns, ls, ts) = stub_sizes(&fc);
        let layout = layout(&fc, &ns, &ls, &ts);
        assert!(layout.size.x > 0.0 && layout.size.y > 0.0);
        assert_eq!(layout.nodes.len(), 3);
    }

    #[test]
    fn lr_layout_ranks_left_to_right_without_overlap() {
        let fc = parse("flowchart LR\nA --> B\nA --> C\nB --> D\nC --> D").unwrap();
        let (ns, ls, ts) = stub_sizes(&fc);
        let out = layout(&fc, &ns, &ls, &ts);
        let r = |id: &str| out.nodes[fc.nodes.iter().position(|n| n.id == id).unwrap()];
        assert!(r("A").right() < r("B").x);
        assert!(r("A").right() < r("C").x);
        assert!(r("B").right() < r("D").x);
        // B and C share a rank; they must not overlap vertically.
        let (b, c) = (r("B"), r("C"));
        assert!(b.bottom() <= c.y || c.bottom() <= b.y);
    }

    #[test]
    fn labeled_edges_widen_the_gap() {
        let fc = parse("flowchart LR\nA -->|a very long edge label indeed| B").unwrap();
        let (ns, ls, ts) = stub_sizes(&fc);
        let out = layout(&fc, &ns, &ls, &ts);
        let a = out.nodes[0];
        let b = out.nodes[1];
        let label_w = ls[0].unwrap().x;
        assert!(b.x - a.right() >= label_w);
        assert!(out.edges[0].label_pos.is_some());
    }

    #[test]
    fn nested_subgraphs_compose() {
        let fc =
            parse("flowchart LR\nsubgraph outer\n subgraph inner\n  A\n end\n B\nend\nA --> B")
                .unwrap();
        let (ns, ls, ts) = stub_sizes(&fc);
        let out = layout(&fc, &ns, &ls, &ts);
        let inner = out.clusters[1];
        let outer = out.clusters[0];
        let a = out.nodes[0];
        assert!(a.x >= inner.x && a.right() <= inner.right() + 0.5);
        assert!(inner.x >= outer.x && inner.right() <= outer.right() + 0.5);
    }

    #[test]
    fn self_loop_routes_outside_the_node() {
        let fc = parse("graph LR\nA --> A").unwrap();
        let (ns, ls, ts) = stub_sizes(&fc);
        let out = layout(&fc, &ns, &ls, &ts);
        let a = out.nodes[0];
        assert!(out.edges[0].c0.x > a.right());
    }

    #[test]
    fn backward_edge_labels_route_around_content() {
        let fc = parse("flowchart LR\nA --> B\nB --> C\nC -->|back| A").unwrap();
        let (ns, ls, ts) = stub_sizes(&fc);
        let out = layout(&fc, &ns, &ls, &ts);
        let max_bottom = out.nodes.iter().map(|r| r.bottom()).fold(0.0, f32::max);
        // The cycle label sits in the clear lane below every node.
        let label = out.edges[2].label_pos.unwrap();
        assert!(label.y > max_bottom);
    }

    #[test]
    fn no_nodes_means_none() {
        assert!(parse("flowchart LR\n%% nothing here").is_none());
        assert!(parse("classDiagram\nA --> B").is_none());
    }
}
