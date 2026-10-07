use unicode_width::UnicodeWidthChar;

const SOURCE_LIMIT: usize = 16 * 1024;
const NODE_LIMIT: usize = 20;
const EDGE_LIMIT: usize = 28;
const LABEL_LIMIT: usize = 48;
const CELL_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Node,
    Edge,
    Label,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DiagramSpan {
    pub text: String,
    pub role: Role,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DiagramError {
    Unsupported,
    Limit,
    TooWide,
}

impl DiagramError {
    pub fn notice(self) -> &'static str {
        match self {
            Self::Unsupported => "Mermaid source · unsupported syntax",
            Self::Limit => "Mermaid source · diagram exceeds display limits",
            Self::TooWide => "Mermaid source · diagram does not fit this width",
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Cell {
    ch: Option<char>,
    role: Option<Role>,
    strokes: u8,
    owner: usize,
}

struct Canvas {
    cells: Vec<Cell>,
    width: usize,
    height: usize,
}

impl Canvas {
    fn new(width: usize, height: usize, available: usize) -> Result<Self, DiagramError> {
        if width > available {
            return Err(DiagramError::TooWide);
        }
        if width.saturating_mul(height) > CELL_LIMIT {
            return Err(DiagramError::Limit);
        }
        Ok(Self {
            cells: vec![Cell::default(); width * height],
            width,
            height,
        })
    }

    fn put(&mut self, x: usize, y: usize, ch: char, role: Role) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = Cell {
                ch: Some(ch),
                role: Some(role),
                ..Cell::default()
            };
        }
    }

    fn text(&mut self, mut x: usize, y: usize, text: &str, role: Role) {
        for ch in text.chars() {
            self.put(x, y, ch, role);
            let width = ch.width().unwrap_or(1);
            if width == 2 {
                self.put(x + 1, y, '\0', role);
            }
            x += width;
        }
    }

    fn segment(&mut self, from: (usize, usize), to: (usize, usize), owner: usize, dashed: bool) {
        let (x1, y1) = from;
        let (x2, y2) = to;
        let horizontal = y1 == y2;
        for y in y1.min(y2)..=y1.max(y2) {
            for x in x1.min(x2)..=x1.max(x2) {
                if x >= self.width || y >= self.height {
                    continue;
                }
                let cell = &mut self.cells[y * self.width + x];
                if matches!(cell.ch, Some('▲' | '▼' | '▶' | '◀')) {
                    continue;
                }
                if cell.strokes != 0 && cell.owner != owner {
                    cell.ch = Some('╪');
                } else {
                    let mask = if horizontal {
                        (u8::from(x > x1.min(x2)) * 4) | (u8::from(x < x1.max(x2)) * 8)
                    } else {
                        u8::from(y > y1.min(y2)) | (u8::from(y < y1.max(y2)) * 2)
                    };
                    cell.strokes |= mask;
                    cell.ch = Some(match cell.strokes {
                        1..=3 => {
                            if dashed {
                                '┆'
                            } else {
                                '│'
                            }
                        }
                        4 | 8 | 12 => {
                            if dashed {
                                '┄'
                            } else {
                                '─'
                            }
                        }
                        10 => '┌',
                        6 => '┐',
                        9 => '└',
                        5 => '┘',
                        11 => '├',
                        7 => '┤',
                        14 => '┬',
                        13 => '┴',
                        _ => '┼',
                    });
                    cell.owner = owner;
                }
                cell.role = Some(Role::Edge);
            }
        }
    }

    fn node_box(
        &mut self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        node: &Node,
    ) -> Result<(), DiagramError> {
        let (tl, tr, bl, br) = if matches!(node.shape, Shape::Round | Shape::Cylinder) {
            ('╭', '╮', '╰', '╯')
        } else {
            ('┌', '┐', '└', '┘')
        };
        self.put(x, y, tl, Role::Node);
        self.put(x + width - 1, y, tr, Role::Node);
        self.put(x, y + height - 1, bl, Role::Node);
        self.put(x + width - 1, y + height - 1, br, Role::Node);
        for col in 1..width - 1 {
            self.put(x + col, y, '─', Role::Node);
            self.put(x + col, y + height - 1, '─', Role::Node);
        }
        for row in 1..height - 1 {
            self.put(x, y + row, '│', Role::Node);
            self.put(x + width - 1, y + row, '│', Role::Node);
        }
        let text = if node.shape == Shape::Decision {
            format!("◇ {}", node.text)
        } else {
            node.text.clone()
        };
        let top = if node.shape == Shape::Cylinder {
            self.put(x, y + 1, '├', Role::Node);
            self.put(x + width - 1, y + 1, '┤', Role::Node);
            for col in 1..width - 1 {
                self.put(x + col, y + 1, '─', Role::Node);
            }
            2
        } else {
            1
        };
        let rows = wrap_label(&text, width.saturating_sub(2));
        if height < rows.len() + top + 1 {
            return Err(DiagramError::Limit);
        }
        let start = top + (height - top - 1 - rows.len()) / 2;
        for (row, text) in rows.iter().enumerate() {
            let text_width = unicode_width::UnicodeWidthStr::width(text.as_str());
            self.text(
                x + (width - text_width) / 2,
                y + start + row,
                text,
                Role::Label,
            );
        }
        Ok(())
    }

    fn finish(self) -> Vec<Vec<DiagramSpan>> {
        (0..self.height)
            .map(|y| {
                let row = &self.cells[y * self.width..(y + 1) * self.width];
                let end = row
                    .iter()
                    .rposition(|cell| cell.ch.is_some())
                    .map_or(0, |x| x + 1);
                let mut spans: Vec<DiagramSpan> = Vec::new();
                for cell in &row[..end] {
                    let ch = cell.ch.unwrap_or(' ');
                    if ch == '\0' {
                        continue;
                    }
                    let role = cell.role.unwrap_or(Role::Label);
                    if let Some(span) = spans.last_mut().filter(|span| span.role == role) {
                        span.text.push(ch);
                    } else {
                        spans.push(DiagramSpan {
                            text: ch.to_string(),
                            role,
                        });
                    }
                }
                spans
            })
            .collect()
    }
}

fn label_width(text: &str) -> Result<usize, DiagramError> {
    let mut width = 0usize;
    for ch in text.chars() {
        let cells = ch.width().ok_or(DiagramError::Unsupported)?;
        if cells == 0 || ch.is_control() || matches!(ch, '<' | '>') {
            return Err(DiagramError::Unsupported);
        }
        width += cells;
    }
    if width != unicode_width::UnicodeWidthStr::width(text) {
        return Err(DiagramError::Unsupported);
    }
    if width > LABEL_LIMIT {
        Err(DiagramError::Limit)
    } else {
        Ok(width)
    }
}

fn label(text: &str) -> Result<String, DiagramError> {
    let trimmed = text.trim();
    let text = if trimmed.starts_with('"') {
        trimmed
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .ok_or(DiagramError::Unsupported)?
    } else {
        if trimmed.contains('"') {
            return Err(DiagramError::Unsupported);
        }
        trimmed
    };
    if text.contains(['\\', '&', '`', '"']) {
        return Err(DiagramError::Unsupported);
    }
    label_width(text)?;
    Ok(text.to_string())
}

fn statements(source: &str) -> Result<Vec<String>, DiagramError> {
    if source.len() > SOURCE_LIMIT {
        return Err(DiagramError::Limit);
    }
    let mut statements = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("%%{") {
            return Err(DiagramError::Unsupported);
        }
        if line.starts_with("%%") {
            continue;
        }
        let mut quote = false;
        let mut depth = 0usize;
        let mut start = 0usize;
        for (i, ch) in line.char_indices() {
            if ch == '"' {
                quote = !quote;
            } else if !quote {
                if statements.len() > 128 {
                    return Err(DiagramError::Limit);
                }
                match ch {
                    '[' | '{' | '(' => depth += 1,
                    ']' | '}' | ')' => {
                        depth = depth.checked_sub(1).ok_or(DiagramError::Unsupported)?
                    }
                    ';' if depth == 0 => {
                        statements.push(line[start..i].trim().to_string());
                        start = i + 1;
                    }
                    _ => {}
                }
            }
        }
        if quote || depth != 0 {
            return Err(DiagramError::Unsupported);
        }
        if !line[start..].trim().is_empty() {
            statements.push(line[start..].trim().to_string());
        }
        if statements.len() > 128 {
            return Err(DiagramError::Limit);
        }
    }
    Ok(statements)
}

#[derive(Clone, Copy, Default)]
enum Direction {
    #[default]
    Down,
    Up,
    Right,
    Left,
}

impl Direction {
    fn parse(text: &str) -> Result<Self, DiagramError> {
        match text {
            "TD" | "TB" | "" => Ok(Self::Down),
            "BT" => Ok(Self::Up),
            "LR" => Ok(Self::Right),
            "RL" => Ok(Self::Left),
            _ => Err(DiagramError::Unsupported),
        }
    }

    fn horizontal(self) -> bool {
        matches!(self, Self::Right | Self::Left)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Shape {
    #[default]
    Box,
    Round,
    Decision,
    Cylinder,
}

struct Node {
    id: String,
    text: String,
    shape: Shape,
    declared: bool,
}

struct Edge {
    from: usize,
    to: usize,
    text: String,
    directed: bool,
    both: bool,
    dashed: bool,
}

#[derive(Default)]
struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    direction: Direction,
}

fn identifier(text: &str) -> Result<(&str, &str), DiagramError> {
    let end = text
        .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .unwrap_or(text.len());
    let id = &text[..end];
    if id.is_empty() || !id.as_bytes()[0].is_ascii_alphabetic() || id.len() > 40 {
        return Err(DiagramError::Unsupported);
    }
    Ok((id, text[end..].trim_start()))
}

impl Graph {
    fn node<'a>(&mut self, text: &'a str) -> Result<(usize, &'a str), DiagramError> {
        let (id, mut rest) = identifier(text.trim_start())?;
        let mut declaration = None;
        for (open, close, shape) in [
            ("([", "])", Shape::Round),
            ("[(", ")]", Shape::Cylinder),
            ("[", "]", Shape::Box),
            ("{", "}", Shape::Decision),
            ("(", ")", Shape::Round),
        ] {
            if let Some(body) = rest.strip_prefix(open) {
                let mut quoted = false;
                let mut end = None;
                for (i, ch) in body.char_indices() {
                    if ch == '"' {
                        quoted = !quoted;
                    }
                    if !quoted && body[i..].starts_with(close) {
                        end = Some(i);
                        break;
                    }
                }
                let end = end.ok_or(DiagramError::Unsupported)?;
                declaration = Some((label(&body[..end])?, shape));
                rest = body[end + close.len()..].trim_start();
                break;
            }
        }
        let index = if let Some(index) = self.nodes.iter().position(|node| node.id == id) {
            index
        } else {
            if self.nodes.len() == NODE_LIMIT {
                return Err(DiagramError::Limit);
            }
            self.nodes.push(Node {
                id: id.to_string(),
                text: id.to_string(),
                shape: Shape::Box,
                declared: false,
            });
            self.nodes.len() - 1
        };
        if let Some((text, shape)) = declaration {
            let node = &mut self.nodes[index];
            if node.declared && (node.text != text || node.shape != shape) {
                return Err(DiagramError::Unsupported);
            }
            node.text = text;
            node.shape = shape;
            node.declared = true;
        }
        Ok((index, rest))
    }

    fn groups<'a>(&mut self, text: &'a str) -> Result<(Vec<usize>, &'a str), DiagramError> {
        let (node, mut rest) = self.node(text)?;
        let mut nodes = vec![node];
        while let Some(next) = rest.strip_prefix('&') {
            let (node, tail) = self.node(next.trim_start())?;
            nodes.push(node);
            rest = tail;
            if nodes.len() > NODE_LIMIT {
                return Err(DiagramError::Limit);
            }
        }
        Ok((nodes, rest))
    }

    fn state_node(&mut self, id: &str, initial: bool) -> Result<usize, DiagramError> {
        if id != "[*]" {
            let (parsed_id, rest) = identifier(id)?;
            if !rest.is_empty() {
                return Err(DiagramError::Unsupported);
            }
            let (index, _) = self.node(parsed_id)?;
            return Ok(index);
        }
        let marker = if initial { "[initial]" } else { "[final]" };
        if let Some(index) = self.nodes.iter().position(|node| node.id == marker) {
            return Ok(index);
        }
        if self.nodes.len() == NODE_LIMIT {
            return Err(DiagramError::Limit);
        }
        self.nodes.push(Node {
            id: marker.into(),
            text: if initial { "● Initial" } else { "◎ Final" }.into(),
            shape: Shape::Round,
            declared: true,
        });
        Ok(self.nodes.len() - 1)
    }

    fn statement(&mut self, text: &str) -> Result<(), DiagramError> {
        let (mut sources, mut rest) = self.groups(text)?;
        while !rest.is_empty() {
            let mut edge_label = String::new();
            let mut arrow = None;
            for (token, directed, both, dashed) in [
                ("<-.->", true, true, true),
                ("<-->", true, true, false),
                ("-.->", true, false, true),
                ("-->", true, false, false),
                ("---", false, false, false),
                ("-.-", false, false, true),
            ] {
                if let Some(tail) = rest.strip_prefix(token) {
                    arrow = Some((tail.trim_start(), directed, both, dashed));
                    break;
                }
            }
            if arrow.is_none() {
                if let Some(body) = rest
                    .strip_prefix("-- ")
                    .and_then(|body| body.split_once(" -->"))
                {
                    edge_label = label(body.0)?;
                    arrow = Some((body.1.trim_start(), true, false, false));
                } else if let Some(body) = rest
                    .strip_prefix("-. ")
                    .and_then(|body| body.split_once(" .->"))
                {
                    edge_label = label(body.0)?;
                    arrow = Some((body.1.trim_start(), true, false, true));
                }
            }
            let (mut tail, directed, both, dashed) = arrow.ok_or(DiagramError::Unsupported)?;
            if let Some(body) = tail.strip_prefix('|') {
                if !edge_label.is_empty() {
                    return Err(DiagramError::Unsupported);
                }
                let end = body.find('|').ok_or(DiagramError::Unsupported)?;
                edge_label = label(&body[..end])?;
                tail = body[end + 1..].trim_start();
            }
            let (targets, next) = self.groups(tail)?;
            for &from in &sources {
                for &to in &targets {
                    if self.edges.len() == EDGE_LIMIT {
                        return Err(DiagramError::Limit);
                    }
                    self.edges.push(Edge {
                        from,
                        to,
                        text: edge_label.clone(),
                        directed,
                        both,
                        dashed,
                    });
                }
            }
            sources = targets;
            rest = next;
        }
        Ok(())
    }

    fn render_layered(
        &self,
        available: usize,
    ) -> Result<Option<Vec<Vec<DiagramSpan>>>, DiagramError> {
        let count = self.nodes.len();
        if count == 0 {
            return Err(DiagramError::Unsupported);
        }
        let mut indegree = vec![0; count];
        let mut outgoing = vec![Vec::new(); count];
        let mut incoming = vec![Vec::new(); count];
        for (index, edge) in self.edges.iter().enumerate() {
            indegree[edge.to] += 1;
            outgoing[edge.from].push(index);
            incoming[edge.to].push(index);
        }
        let mut ready =
            std::collections::VecDeque::from_iter((0..count).filter(|&i| indegree[i] == 0));
        let mut rank = vec![0usize; count];
        let mut visited = 0;
        while let Some(node) = ready.pop_front() {
            visited += 1;
            for &index in &outgoing[node] {
                let to = self.edges[index].to;
                rank[to] = rank[to].max(rank[node] + 1);
                indegree[to] -= 1;
                if indegree[to] == 0 {
                    ready.push_back(to);
                }
            }
        }
        // Cycles retain the explicit, edge-owned routing layout rather than losing a back edge.
        if visited != count {
            return Ok(None);
        }
        let levels = rank.iter().copied().max().unwrap_or(0) + 1;
        let mut layers = vec![Vec::new(); levels];
        for (node, &level) in rank.iter().enumerate() {
            layers[level].push(node);
        }
        let mut order = vec![0; count];
        for layer in &layers {
            for (slot, &node) in layer.iter().enumerate() {
                order[node] = slot;
            }
        }
        for layer in layers.iter_mut().skip(1) {
            layer.sort_by_key(|&node| {
                let parents = &incoming[node];
                let center = parents
                    .iter()
                    .map(|&edge| order[self.edges[edge].from] * 1024)
                    .sum::<usize>()
                    / parents.len().max(1);
                (center, node)
            });
            for (slot, &node) in layer.iter().enumerate() {
                order[node] = slot;
            }
        }
        let horizontal = self.direction.horizontal();
        let reverse = matches!(self.direction, Direction::Up | Direction::Left);
        let ports = outgoing
            .iter()
            .chain(&incoming)
            .map(Vec::len)
            .max()
            .unwrap_or(0);
        let node_width = self
            .nodes
            .iter()
            .map(|node| {
                label_width(&node.text)
                    .map(|w| w + if node.shape == Shape::Decision { 6 } else { 4 })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(5)
            .max(if horizontal { 5 } else { ports * 2 + 3 });
        let minimum_height = if self.nodes.iter().any(|node| node.shape == Shape::Cylinder) {
            4
        } else {
            3
        };
        let node_height = if horizontal {
            (ports * 2 + 1).max(minimum_height)
        } else {
            minimum_height
        };
        let max_label = self
            .edges
            .iter()
            .map(|edge| label_width(&edge.text))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        let cross_gap = if horizontal {
            3
        } else {
            (max_label + 4).max(6)
        };
        let breadth = if horizontal { node_height } else { node_width };
        let depth = if horizontal { node_width } else { node_height };
        let widest = layers.iter().map(Vec::len).max().unwrap_or(1);
        let cross_size = widest * breadth + widest.saturating_sub(1) * cross_gap;
        let mut starts = vec![0; levels];
        let mut gap_edges = vec![Vec::new(); levels];
        let mut bypass = Vec::new();
        for (index, edge) in self.edges.iter().enumerate() {
            if rank[edge.to] == rank[edge.from] + 1 {
                gap_edges[rank[edge.from]].push(index);
            } else {
                bypass.push(index);
            }
        }
        for level in 1..levels {
            let gap = if horizontal {
                if gap_edges[level - 1].len() == 1 {
                    max_label.max(3) + 2
                } else {
                    max_label + gap_edges[level - 1].len() * 2 + 3
                }
            } else {
                if gap_edges[level - 1].len() == 1 {
                    2 + usize::from(max_label > 0)
                } else {
                    gap_edges[level - 1].len() * 2 + 1
                }
            };
            starts[level] = starts[level - 1] + depth + gap;
        }
        let main_size = starts[levels - 1] + depth;
        let mut positions = vec![(0, 0); count];
        for (level, layer) in layers.iter().enumerate() {
            let used = layer.len() * breadth + layer.len().saturating_sub(1) * cross_gap;
            let offset = (cross_size - used) / 2;
            for (slot, &node) in layer.iter().enumerate() {
                let main = if reverse {
                    main_size - starts[level] - depth
                } else {
                    starts[level]
                };
                let cross = offset + slot * (breadth + cross_gap);
                positions[node] = if horizontal {
                    (main, cross)
                } else {
                    (cross, main)
                };
            }
        }
        // Skip-level edges use the ranked lane layout below.
        if !bypass.is_empty() {
            return Ok(None);
        }
        let canvas_width = if horizontal {
            main_size
        } else {
            cross_size + max_label.saturating_sub(node_width / 2).saturating_add(2)
        };
        let canvas_height = if horizontal { cross_size } else { main_size };
        if canvas_width > available {
            return Ok(None);
        }
        let mut canvas = Canvas::new(canvas_width, canvas_height, available)?;
        for (node, &(x, y)) in self.nodes.iter().zip(&positions) {
            canvas.node_box(x, y, node_width, node_height, node)?;
        }
        let port = |edges: &[usize], index: usize, span: usize| {
            let slot = edges.iter().position(|&i| i == index).unwrap_or(0);
            (slot + 1) * (span - 1) / (edges.len() + 1)
        };
        let mut labels = Vec::new();
        for (index, edge) in self.edges.iter().enumerate() {
            let (fx, fy) = positions[edge.from];
            let (tx, ty) = positions[edge.to];
            let lane = gap_edges[rank[edge.from]]
                .iter()
                .position(|&i| i == index)
                .unwrap_or(0);
            let (from, to, elbow) = if horizontal {
                let from = (
                    if reverse { fx - 1 } else { fx + node_width },
                    fy + port(&outgoing[edge.from], index, node_height),
                );
                let to = (
                    if reverse { tx + node_width } else { tx - 1 },
                    ty + port(&incoming[edge.to], index, node_height),
                );
                let bend = if reverse {
                    from.0 - 2 - lane * 2
                } else {
                    from.0 + 2 + lane * 2
                };
                (from, to, (bend, from.1))
            } else {
                let from = (
                    fx + port(&outgoing[edge.from], index, node_width),
                    if reverse { fy - 1 } else { fy + node_height },
                );
                let to = (
                    tx + port(&incoming[edge.to], index, node_width),
                    if reverse { ty + node_height } else { ty - 1 },
                );
                let bend = if reverse {
                    from.1 - 1 - lane * 2
                } else {
                    from.1 + 1 + lane * 2
                };
                (from, to, (from.0, bend))
            };
            let other = if horizontal {
                (elbow.0, to.1)
            } else {
                (to.0, elbow.1)
            };
            if (horizontal && from.1 == to.1) || (!horizontal && from.0 == to.0) {
                canvas.segment(from, to, index + 1, edge.dashed);
            } else {
                canvas.segment(from, elbow, index + 1, edge.dashed);
                canvas.segment(elbow, other, index + 1, edge.dashed);
                canvas.segment(other, to, index + 1, edge.dashed);
            }
            let target_tip = match (horizontal, reverse) {
                (true, false) => '▶',
                (true, true) => '◀',
                (false, false) => '▼',
                (false, true) => '▲',
            };
            let source_tip = match target_tip {
                '▶' => '◀',
                '◀' => '▶',
                '▼' => '▲',
                _ => '▼',
            };
            if edge.directed {
                canvas.put(to.0, to.1, target_tip, Role::Edge);
            }
            if edge.both {
                canvas.put(from.0, from.1, source_tip, Role::Edge);
            }
            if !edge.text.is_empty() {
                let mut candidates = Vec::new();
                let cells = label_width(&edge.text)?;
                if horizontal {
                    let segments = if from.1 == to.1 {
                        vec![(from, to)]
                    } else {
                        vec![(from, elbow), (other, to)]
                    };
                    for (a, b) in segments {
                        let left = a.0.min(b.0) + 1;
                        if left + cells <= a.0.max(b.0) {
                            candidates.push((left, a.1.saturating_sub(1)));
                            candidates.push((left, a.1 + 1));
                        }
                    }
                } else {
                    candidates.push((from.0.min(to.0) + 2, elbow.1.saturating_sub(1)));
                    candidates.push((from.0.max(to.0) + 2, elbow.1));
                }
                labels.push((candidates, &edge.text));
            }
        }
        for (candidates, text) in labels {
            let cells = label_width(text)?;
            let Some((x, y)) = candidates.into_iter().find(|&(x, y)| {
                x + cells <= canvas.width
                    && y < canvas.height
                    && canvas.cells[y * canvas.width + x..y * canvas.width + x + cells]
                        .iter()
                        .all(|cell| cell.ch.is_none())
            }) else {
                return Ok(None);
            };
            canvas.text(x, y, text, Role::Label);
        }
        Ok(Some(canvas.finish()))
    }

    fn render(mut self, width: usize) -> Result<Vec<Vec<DiagramSpan>>, DiagramError> {
        match self.render_fixed(width) {
            Err(DiagramError::TooWide) if self.direction.horizontal() => {
                let direction = if matches!(self.direction, Direction::Right) {
                    self.direction = Direction::Down;
                    "LR"
                } else {
                    self.direction = Direction::Up;
                    "RL"
                };
                let mut diagram = self.render_fixed(width)?;
                let notice = format!("Diagram · vertical reflow ({direction})");
                let mut rows = wrap_label(&notice, width)
                    .into_iter()
                    .map(|text| {
                        vec![DiagramSpan {
                            text,
                            role: Role::Label,
                        }]
                    })
                    .collect::<Vec<_>>();
                rows.append(&mut diagram);
                Ok(rows)
            }
            result => result,
        }
    }

    fn render_fixed(&self, width: usize) -> Result<Vec<Vec<DiagramSpan>>, DiagramError> {
        if let Some(diagram) = self.render_layered(width)? {
            return Ok(diagram);
        }
        if let Some(diagram) = self.render_ranked(width)? {
            return Ok(diagram);
        }
        self.render_routed(width)
    }

    fn render_ranked(
        &self,
        available: usize,
    ) -> Result<Option<Vec<Vec<DiagramSpan>>>, DiagramError> {
        if self.direction.horizontal() || self.nodes.is_empty() {
            return Ok(None);
        }
        let count = self.nodes.len();
        let mut outgoing = vec![Vec::new(); count];
        let mut incoming = vec![Vec::new(); count];
        for (index, edge) in self.edges.iter().enumerate() {
            outgoing[edge.from].push(index);
            incoming[edge.to].push(index);
        }
        fn visit(
            node: usize,
            graph: &Graph,
            outgoing: &[Vec<usize>],
            color: &mut [u8],
            back: &mut [bool],
        ) {
            color[node] = 1;
            for &index in &outgoing[node] {
                let to = graph.edges[index].to;
                if color[to] == 1 {
                    back[index] = true;
                } else if color[to] == 0 {
                    visit(to, graph, outgoing, color, back);
                }
            }
            color[node] = 2;
        }
        // Back edges are excluded only from ranking; every edge is routed below.
        let mut color = vec![0; count];
        let mut back = vec![false; self.edges.len()];
        for node in 0..count {
            if color[node] == 0 {
                visit(node, self, &outgoing, &mut color, &mut back);
            }
        }
        let mut indegree = vec![0; count];
        for (index, edge) in self.edges.iter().enumerate() {
            if !back[index] {
                indegree[edge.to] += 1;
            }
        }
        let mut ready =
            std::collections::VecDeque::from_iter((0..count).filter(|&node| indegree[node] == 0));
        let mut rank = vec![0usize; count];
        while let Some(node) = ready.pop_front() {
            for &index in &outgoing[node] {
                if !back[index] {
                    let to = self.edges[index].to;
                    rank[to] = rank[to].max(rank[node] + 1);
                    indegree[to] -= 1;
                    if indegree[to] == 0 {
                        ready.push_back(to);
                    }
                }
            }
        }
        let levels = rank.iter().copied().max().unwrap_or(0) + 1;
        let mut layers = vec![Vec::new(); levels];
        for (node, &level) in rank.iter().enumerate() {
            layers[level].push(node);
        }
        let mut order = vec![0usize; count];
        for layer in &mut layers {
            layer.sort_by_key(|&node| {
                let parents = incoming[node]
                    .iter()
                    .filter(|&&index| !back[index])
                    .collect::<Vec<_>>();
                let center = parents
                    .iter()
                    .map(|&&index| order[self.edges[index].from] * 1024)
                    .sum::<usize>()
                    / parents.len().max(1);
                (center, node)
            });
            for (slot, &node) in layer.iter().enumerate() {
                order[node] = slot;
            }
        }
        let adjacent = self
            .edges
            .iter()
            .map(|edge| rank[edge.to] == rank[edge.from] + 1)
            .collect::<Vec<_>>();
        let mut lane_ends = Vec::new();
        let mut edge_lanes = vec![0; self.edges.len()];
        let mut indices = (0..self.edges.len())
            .filter(|&index| !adjacent[index])
            .collect::<Vec<_>>();
        indices.sort_by_key(|&index| {
            let edge = &self.edges[index];
            (
                rank[edge.from].min(rank[edge.to]),
                rank[edge.from].max(rank[edge.to]),
                index,
            )
        });
        for index in indices {
            let edge = &self.edges[index];
            let start = rank[edge.from].min(rank[edge.to]);
            let end = rank[edge.from].max(rank[edge.to]);
            // Neighboring ranks share a connector gap, so their lanes cannot be reused.
            let lane = if let Some(lane) = lane_ends.iter().position(|&last| last + 1 < start) {
                lane_ends[lane] = end;
                lane
            } else {
                lane_ends.push(end);
                lane_ends.len() - 1
            };
            edge_lanes[index] = lane;
        }
        let widest = layers.iter().map(Vec::len).max().unwrap_or(1);
        let reserve = lane_ends.len() * 2 + 3;
        let cross_gap = 4;
        let budget = available.saturating_sub(reserve + (widest - 1) * cross_gap) / widest;
        let ports = outgoing
            .iter()
            .chain(&incoming)
            .map(Vec::len)
            .max()
            .unwrap_or(0);
        let desired = self
            .nodes
            .iter()
            .map(|node| {
                label_width(&node.text)
                    .map(|width| width + if node.shape == Shape::Decision { 6 } else { 4 })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(5);
        let node_width = desired.max(7).min(budget);
        if node_width < (ports + 2).max(7) {
            return Ok(None);
        }
        let node_height = self
            .nodes
            .iter()
            .map(|node| {
                let text = if node.shape == Shape::Decision {
                    format!("◇ {}", node.text)
                } else {
                    node.text.clone()
                };
                wrap_label(&text, node_width - 2).len()
                    + if node.shape == Shape::Cylinder { 3 } else { 2 }
            })
            .max()
            .unwrap_or(3);
        let cross_size = widest * node_width + (widest - 1) * cross_gap;
        let canvas_width = cross_size + reserve;
        let label_size = (node_width - 2).min(24);
        let wrapped = self
            .edges
            .iter()
            .map(|edge| {
                if edge.text.is_empty() {
                    Vec::new()
                } else {
                    wrap_label(&edge.text, label_size)
                }
            })
            .collect::<Vec<_>>();
        let mut gaps = vec![Vec::new(); levels + 1];
        for (index, edge) in self.edges.iter().enumerate() {
            gaps[rank[edge.from] + 1].push((index, true));
            if !adjacent[index] {
                gaps[rank[edge.to]].push((index, false));
            }
        }
        let mut starts = vec![0; levels];
        let mut source_rows = vec![0; self.edges.len()];
        let mut target_rows = vec![0; self.edges.len()];
        let mut label_rows = vec![0; self.edges.len()];
        let mut cursor = 0;
        for (level, gap) in gaps.iter().enumerate() {
            for &(index, source) in gap {
                if source {
                    source_rows[index] = cursor;
                    label_rows[index] = cursor + 1;
                    cursor += wrapped[index].len().max(1) + 1;
                } else {
                    target_rows[index] = cursor;
                    cursor += 2;
                }
            }
            if level < levels {
                starts[level] = cursor;
                cursor += node_height;
            }
        }
        let height = cursor;
        let reverse = matches!(self.direction, Direction::Up);
        let point = |x, y| (x, if reverse { height - y - 1 } else { y });
        let mut positions = vec![(0, 0); count];
        for (level, layer) in layers.iter().enumerate() {
            let used = layer.len() * node_width + (layer.len() - 1) * cross_gap;
            let offset = (cross_size - used) / 2;
            for (slot, &node) in layer.iter().enumerate() {
                positions[node] = (offset + slot * (node_width + cross_gap), starts[level]);
            }
        }
        let mut canvas = Canvas::new(canvas_width, height, available)?;
        let port = |edges: &[usize], index: usize| {
            let slot = edges.iter().position(|&edge| edge == index).unwrap_or(0);
            (slot + 1) * (node_width - 1) / (edges.len() + 1)
        };
        let mut labels = Vec::new();
        for (index, edge) in self.edges.iter().enumerate() {
            let (fx, fy) = positions[edge.from];
            let (tx, ty) = positions[edge.to];
            let from = (fx + port(&outgoing[edge.from], index), fy + node_height);
            let to = (tx + port(&incoming[edge.to], index), ty - 1);
            let lane = cross_size + 2 + edge_lanes[index] * 2;
            let path = if adjacent[index] {
                vec![
                    from,
                    (from.0, source_rows[index]),
                    (to.0, source_rows[index]),
                    to,
                ]
            } else {
                vec![
                    from,
                    (from.0, source_rows[index]),
                    (lane, source_rows[index]),
                    (lane, target_rows[index]),
                    (to.0, target_rows[index]),
                    to,
                ]
            };
            for pair in path.windows(2) {
                canvas.segment(
                    point(pair[0].0, pair[0].1),
                    point(pair[1].0, pair[1].1),
                    index + 1,
                    edge.dashed,
                );
            }
            if edge.directed {
                let (x, y) = point(to.0, to.1);
                canvas.put(x, y, if reverse { '▲' } else { '▼' }, Role::Edge);
            }
            if edge.both {
                let (x, y) = point(from.0, from.1);
                canvas.put(x, y, if reverse { '▼' } else { '▲' }, Role::Edge);
            }
            labels.push((from.0, label_rows[index], &wrapped[index]));
        }
        for (node, &(x, y)) in self.nodes.iter().zip(&positions) {
            canvas.node_box(
                x,
                if reverse { height - y - node_height } else { y },
                node_width,
                node_height,
                node,
            )?;
        }
        for (anchor, row, lines) in labels {
            if lines.is_empty() {
                continue;
            }
            let y = if reverse {
                height - row - lines.len()
            } else {
                row
            };
            let cells = lines
                .iter()
                .map(|line| unicode_width::UnicodeWidthStr::width(line.as_str()))
                .max()
                .unwrap_or(0);
            let preferred = (anchor + 2).min(canvas_width.saturating_sub(cells));
            let mut candidates = (0..=canvas_width.saturating_sub(cells)).collect::<Vec<_>>();
            candidates.sort_by_key(|&x| x.abs_diff(preferred));
            let Some(x) = candidates.into_iter().find(|&x| {
                (y..y + lines.len()).all(|row| {
                    canvas.cells[row * canvas_width + x..row * canvas_width + x + cells]
                        .iter()
                        .all(|cell| cell.ch.is_none())
                })
            }) else {
                return Ok(None);
            };
            for (offset, text) in lines.iter().enumerate() {
                canvas.text(x, y + offset, text, Role::Label);
            }
        }
        Ok(Some(canvas.finish()))
    }

    fn render_routed(&self, width: usize) -> Result<Vec<Vec<DiagramSpan>>, DiagramError> {
        if self.nodes.is_empty() {
            return Err(DiagramError::Unsupported);
        }
        let horizontal = self.direction.horizontal();
        let reverse = matches!(self.direction, Direction::Up | Direction::Left);
        let count = self.nodes.len();
        let mut incident = vec![0usize; count];
        let mut outgoing = vec![0usize; count];
        let mut incoming = vec![0usize; count];
        for edge in &self.edges {
            outgoing[edge.from] += 1;
            incoming[edge.to] += 1;
        }
        let straight = self
            .edges
            .iter()
            .map(|edge| {
                edge.to == edge.from + 1 && outgoing[edge.from] == 1 && incoming[edge.to] == 1
            })
            .collect::<Vec<_>>();
        let mut ports = Vec::new();
        for (edge, &direct) in self.edges.iter().zip(&straight) {
            if direct {
                ports.push((0, 0));
            } else {
                incident[edge.from] += 1;
                let from = incident[edge.from];
                incident[edge.to] += 1;
                ports.push((from, incident[edge.to]));
            }
        }
        let max_ports = incident.iter().copied().max().unwrap_or(0);
        let box_width = self
            .nodes
            .iter()
            .map(|node| {
                label_width(&node.text)
                    .map(|w| w + if node.shape == Shape::Decision { 6 } else { 4 })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(4)
            .max(if horizontal { max_ports + 2 } else { 5 });
        let minimum_height = if self.nodes.iter().any(|node| node.shape == Shape::Cylinder) {
            4
        } else {
            3
        };
        let box_height = if horizontal {
            minimum_height
        } else {
            (max_ports + 2).max(minimum_height)
        };
        let max_label = self
            .edges
            .iter()
            .map(|edge| label_width(&edge.text))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        let gap = if horizontal {
            (max_label + 4).max(5)
        } else {
            4
        };
        let positions = (0..count)
            .map(|index| {
                let slot = if reverse { count - index - 1 } else { index };
                if horizontal {
                    (slot * (box_width + gap), 0)
                } else {
                    (0, slot * (box_height + gap))
                }
            })
            .collect::<Vec<_>>();
        let lanes = straight.iter().filter(|direct| !**direct).count();
        let base_width = if horizontal {
            count * box_width + (count - 1) * gap
        } else {
            box_width
        };
        let base_height = if horizontal {
            box_height
        } else {
            count * box_height + (count - 1) * gap
        };
        let canvas_width = if horizontal {
            base_width
                + if lanes > 0 && max_label > 0 {
                    max_label + 2
                } else {
                    0
                }
        } else {
            base_width.max(box_width / 2 + 2 + max_label)
                + if lanes > 0 {
                    lanes * 3 + max_label + 4
                } else {
                    0
                }
        };
        let canvas_height = base_height
            + if horizontal && lanes > 0 {
                lanes * 3 + 2
            } else {
                0
            };
        let mut canvas = Canvas::new(canvas_width, canvas_height, width)?;
        let mut lane = 0usize;
        for (i, edge) in self.edges.iter().enumerate() {
            let (fx, fy) = positions[edge.from];
            let (tx, ty) = positions[edge.to];
            if !straight[i] {
                let (from_port, to_port) = ports[i];
                if horizontal {
                    let y = base_height + 2 + lane * 3;
                    let from = (fx + from_port, box_height);
                    let to = (tx + to_port, box_height);
                    canvas.segment(from, (from.0, y), i + 1, edge.dashed);
                    canvas.segment((from.0, y), (to.0, y), i + 1, edge.dashed);
                    canvas.segment((to.0, y), to, i + 1, edge.dashed);
                    if edge.directed {
                        canvas.put(to.0, to.1, '▲', Role::Edge);
                    }
                    if edge.both {
                        canvas.put(from.0, from.1, '▲', Role::Edge);
                    }
                    canvas.text(base_width + 2, y, &edge.text, Role::Label);
                } else {
                    let x = box_width + 2 + lane * 3;
                    let from = (box_width, fy + from_port);
                    let to = (box_width, ty + to_port);
                    canvas.segment(from, (x, from.1), i + 1, edge.dashed);
                    canvas.segment((x, from.1), (x, to.1), i + 1, edge.dashed);
                    canvas.segment((x, to.1), to, i + 1, edge.dashed);
                    if edge.directed {
                        canvas.put(to.0, to.1, '◀', Role::Edge);
                    }
                    if edge.both {
                        canvas.put(from.0, from.1, '◀', Role::Edge);
                    }
                    canvas.text(box_width + lanes * 3 + 3, from.1, &edge.text, Role::Label);
                }
                lane += 1;
            } else if horizontal {
                let rightward = tx > fx;
                let from = (if rightward { fx + box_width } else { fx - 1 }, 1);
                let to = (if rightward { tx - 1 } else { tx + box_width }, 1);
                canvas.segment(from, to, i + 1, edge.dashed);
                if edge.directed {
                    canvas.put(to.0, to.1, if rightward { '▶' } else { '◀' }, Role::Edge);
                }
                if edge.both {
                    canvas.put(
                        from.0,
                        from.1,
                        if rightward { '◀' } else { '▶' },
                        Role::Edge,
                    );
                }
                canvas.text(from.0.min(to.0), 0, &edge.text, Role::Label);
            } else {
                let down = ty > fy;
                let from = (box_width / 2, if down { fy + box_height } else { fy - 1 });
                let to = (box_width / 2, if down { ty - 1 } else { ty + box_height });
                canvas.segment(from, to, i + 1, edge.dashed);
                if edge.directed {
                    canvas.put(to.0, to.1, if down { '▼' } else { '▲' }, Role::Edge);
                }
                if edge.both {
                    canvas.put(from.0, from.1, if down { '▲' } else { '▼' }, Role::Edge);
                }
                canvas.text(
                    if lanes > 0 {
                        box_width + lanes * 3 + 3
                    } else {
                        box_width / 2 + 2
                    },
                    (from.1 + to.1) / 2,
                    &edge.text,
                    Role::Label,
                );
            }
        }
        for (node, &(x, y)) in self.nodes.iter().zip(&positions) {
            canvas.node_box(x, y, box_width, box_height, node)?;
        }
        Ok(canvas.finish())
    }
}

fn wrap_label(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut cells = 0;
        let mut end = 0;
        let mut boundary = 0;
        for (offset, ch) in rest.char_indices() {
            let size = ch.width().unwrap_or(1);
            if cells + size > width {
                break;
            }
            cells += size;
            end = offset + ch.len_utf8();
            if ch.is_whitespace() {
                boundary = end;
            }
        }
        if end == 0 {
            end = rest.chars().next().map_or(0, char::len_utf8);
        } else if end < rest.len() && boundary > 0 {
            end = boundary;
        }
        lines.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn render_sequence(body: &[String], width: usize) -> Result<Vec<Vec<DiagramSpan>>, DiagramError> {
    let mut graph = Graph::default();
    let mut events = Vec::new();
    for statement in body {
        if let Some(declaration) = statement
            .strip_prefix("participant ")
            .or_else(|| statement.strip_prefix("actor "))
        {
            let (id, alias) = identifier(declaration)?;
            let (index, _) = graph.node(id)?;
            let text = if alias.is_empty() {
                id.to_string()
            } else {
                label(alias.strip_prefix("as ").ok_or(DiagramError::Unsupported)?)?
            };
            if graph.nodes[index].declared && graph.nodes[index].text != text {
                return Err(DiagramError::Unsupported);
            }
            graph.nodes[index].text = text;
            graph.nodes[index].declared = true;
            continue;
        }
        if events.len() >= 48 || graph.nodes.len() > 8 {
            return Err(DiagramError::Limit);
        }
        let (from_id, rest) = identifier(statement)?;
        let mut arrow = None;
        for (token, dashed, head) in [
            ("-->>", true, '▶'),
            ("->>", false, '▶'),
            ("--x", true, '×'),
            ("-x", false, '×'),
            ("-->", true, '─'),
            ("->", false, '─'),
        ] {
            if let Some(tail) = rest.strip_prefix(token) {
                arrow = Some((tail.trim_start(), dashed, head));
                break;
            }
        }
        let (tail, dashed, head) = arrow.ok_or(DiagramError::Unsupported)?;
        let (to_id, rest) = identifier(tail)?;
        let text = label(rest.strip_prefix(':').ok_or(DiagramError::Unsupported)?)?;
        let from = graph.node(from_id)?.0;
        let to = graph.node(to_id)?.0;
        if graph.nodes.len() > 8 {
            return Err(DiagramError::Limit);
        }
        events.push((from, to, text, dashed, head));
    }
    if graph.nodes.is_empty() {
        return Err(DiagramError::Unsupported);
    }
    if graph.nodes.len() > 8 {
        return Err(DiagramError::Limit);
    }
    let box_width = graph
        .nodes
        .iter()
        .map(|node| label_width(&node.text).map(|width| width + 4))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(5)
        .max(7);
    if events.is_empty() {
        let mut canvas = Canvas::new(graph.nodes.len() * (box_width + 4) - 4, 5, width)?;
        for (index, node) in graph.nodes.iter().enumerate() {
            let x = index * (box_width + 4);
            canvas.node_box(x, 0, box_width, 3, node)?;
            canvas.segment(
                (x + box_width / 2, 3),
                (x + box_width / 2, 4),
                usize::MAX,
                true,
            );
        }
        return Ok(canvas.finish());
    }
    let label_gap = events
        .iter()
        .map(|(_, _, text, _, _)| label_width(text))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0)
        + 4;
    let columns = graph.nodes.len();
    let last_self = events
        .iter()
        .any(|(from, to, _, _, _)| *from == columns - 1 && from == to);
    let minimum_spacing = box_width + 4;
    let spacing = if columns > 1 {
        let mut budget = width.saturating_sub(box_width) / (columns - 1);
        if last_self {
            budget = budget.min(width.saturating_sub(box_width / 2) / columns);
        }
        if budget < minimum_spacing {
            return Err(DiagramError::TooWide);
        }
        (box_width + 4).max(label_gap + 2).min(budget)
    } else {
        (box_width + 4)
            .max(label_gap + 2)
            .min(width.saturating_sub(box_width / 2))
    };
    if spacing < 7 {
        return Err(DiagramError::TooWide);
    }
    let label_width = spacing - 4;
    let wrapped = events
        .iter()
        .map(|(_, _, text, _, _)| wrap_label(text, label_width))
        .collect::<Vec<_>>();
    let self_extent = if last_self { spacing - 2 } else { 0 };
    let canvas_width = (columns - 1) * spacing + box_width.max(box_width / 2 + self_extent + 1);
    let height = 6 + wrapped
        .iter()
        .zip(&events)
        .map(|(lines, (from, to, _, _, _))| lines.len() + if from == to { 4 } else { 2 })
        .sum::<usize>();
    let mut canvas = Canvas::new(canvas_width, height, width)?;
    for (i, node) in graph.nodes.iter().enumerate() {
        let x = i * spacing;
        canvas.node_box(x, 0, box_width, 3, node)?;
        let center = x + box_width / 2;
        canvas.segment((center, 3), (center, height - 1), usize::MAX, true);
    }
    let mut row = 4;
    for (i, (from, to, _, dashed, head)) in events.iter().enumerate() {
        let start = from * spacing + box_width / 2;
        let end = to * spacing + box_width / 2;
        for (line, label) in wrapped[i].iter().enumerate() {
            canvas.text(
                start.min(end) + if from == to { 2 } else { 1 },
                row + line,
                label,
                Role::Label,
            );
        }
        let y = row + wrapped[i].len();
        row = y + if from == to { 4 } else { 2 };
        if from == to {
            let side = start + spacing - 2;
            canvas.segment((start, y), (side, y), i + 1, *dashed);
            canvas.segment((side, y), (side, y + 2), i + 1, *dashed);
            canvas.segment((side, y + 2), (start, y + 2), i + 1, *dashed);
            canvas.put(
                start,
                y + 2,
                match head {
                    '▶' => '◀',
                    other => *other,
                },
                Role::Edge,
            );
        } else {
            canvas.segment((start, y), (end, y), i + 1, *dashed);
            canvas.put(
                end,
                y,
                if start > end && *head == '▶' {
                    '◀'
                } else {
                    *head
                },
                Role::Edge,
            );
        }
    }
    Ok(canvas.finish())
}

fn render_states(body: &[String], width: usize) -> Result<Vec<Vec<DiagramSpan>>, DiagramError> {
    let mut graph = Graph::default();
    for statement in body {
        if let Some(direction) = statement.strip_prefix("direction ") {
            graph.direction = Direction::parse(direction)?;
        } else if let Some(declaration) = statement.strip_prefix("state ") {
            let (text, id) = declaration
                .rsplit_once(" as ")
                .ok_or(DiagramError::Unsupported)?;
            let index = graph.state_node(id.trim(), false)?;
            let text = label(text)?;
            if graph.nodes[index].declared && graph.nodes[index].text != text {
                return Err(DiagramError::Unsupported);
            }
            graph.nodes[index].text = text;
            graph.nodes[index].declared = true;
        } else if let Some((from, rest)) = statement.split_once("-->") {
            let (to, text) = rest
                .split_once(':')
                .map_or((rest, ""), |(to, text)| (to, text));
            let from = graph.state_node(from.trim(), true)?;
            let to = graph.state_node(to.trim(), false)?;
            if graph.edges.len() == EDGE_LIMIT {
                return Err(DiagramError::Limit);
            }
            graph.edges.push(Edge {
                from,
                to,
                text: label(text)?,
                directed: true,
                both: false,
                dashed: false,
            });
        } else if let Some((id, text)) = statement.split_once(':') {
            let index = graph.state_node(id.trim(), false)?;
            let description = label(text)?;
            graph.nodes[index].text = format!("{}: {description}", graph.nodes[index].text);
            graph.nodes[index].declared = true;
            label_width(&graph.nodes[index].text)?;
        } else {
            if matches!(statement.as_str(), "end" | "fork" | "join") {
                return Err(DiagramError::Unsupported);
            }
            graph.state_node(statement, false)?;
        }
    }
    graph.render(width)
}

pub(super) fn render(source: &str, width: usize) -> Result<Vec<Vec<DiagramSpan>>, DiagramError> {
    let statements = statements(source)?;
    let (header, body) = statements.split_first().ok_or(DiagramError::Unsupported)?;
    match header.as_str() {
        "sequenceDiagram" => return render_sequence(body, width),
        "stateDiagram" | "stateDiagram-v2" => return render_states(body, width),
        _ => {}
    }
    let direction = header
        .strip_prefix("flowchart")
        .or_else(|| header.strip_prefix("graph"))
        .ok_or(DiagramError::Unsupported)?;
    if !direction.is_empty() && !direction.starts_with(char::is_whitespace) {
        return Err(DiagramError::Unsupported);
    }
    let mut graph = Graph {
        direction: Direction::parse(direction.trim())?,
        ..Graph::default()
    };
    for statement in body {
        graph.statement(statement)?;
    }
    graph.render(width)
}

#[cfg(test)]
pub(super) const COMPONENTS_FIXTURE: &str = r#"flowchart TD
  U["You (terminal)"] --> CLI["cli.py REPL/say/view"]
  CLI -->|"acquire"| LK["unix socket lock"]
  CLI -->|"chat.send(text)"| CHAT["chat.py Chat.turn"]
  CLI -->|"start(COMPACT)"| COMP["compactor.py pump"]
  AG["AGENTS.md"] --> PR["prompts.py"]
  PR -->|"MASTER+VIEW_DOC"| CHAT
  PR -->|"COMPACT+SCALE"| COMP
  CHAT -->|"settle/render/log"| MEM["memory.py view+tree"]
  CHAT -->|"zoom, date"| MEM
  CHAT -->|"sh tool"| SH["subprocess shell"]
  CHAT -->|"stream_call"| CL["client.py"]
  COMP -->|"complete"| CL
  CL -->|"HTTPS + SSE"| API["Messages API"]
  COMP -->|"node + fit()"| MEM
  MEM -->|"append_message"| ST["store.py"]
  COMP -->|"append_node"| ST
  ST -->|"write + fsync"| DIR[("~/.optchat")]
  LK --- DIR
  CHAT -->|"git commit/turn"| DIR"#;

#[cfg(test)]
pub(super) const DEPLOYMENT_FIXTURE: &str = r#"flowchart LR
  TERM["your terminal"] -.->|"ssh+tmux advised"| OC["optchat process"]
  OC -->|"fsync per line"| DIR[("~/.optchat")]
  DIR -->|"commit per turn"| GIT[("local git")]
  OC -->|"master stream"| API["Messages API"]
  OC -->|"compactor calls"| API
  GIT -.->|"push: not built"| BK[("off-box backup")]
  OC -.->|"spec 9: not built"| SUB["subagents"]
  OC -.->|"spec 8: not built"| OAI["OpenAI Responses"]"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(source: &str, width: usize) -> String {
        render(source, width)
            .unwrap()
            .into_iter()
            .map(|line| line.into_iter().map(|span| span.text).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn review_supplied_architectures_render_all_nodes_and_edges() {
        for (source, arrows) in [(COMPONENTS_FIXTURE, 18), (DEPLOYMENT_FIXTURE, 8)] {
            for width in [80, 100, 160, 240] {
                let diagram = plain(source, width);
                if width == 160 {
                    eprintln!(
                        "{}: {} rows, {} cells",
                        if source == COMPONENTS_FIXTURE {
                            "Components"
                        } else {
                            "Deployment"
                        },
                        diagram.lines().count(),
                        diagram
                            .lines()
                            .map(unicode_width::UnicodeWidthStr::width)
                            .max()
                            .unwrap()
                    );
                }
                assert_eq!(
                    diagram.matches(['▶', '◀', '▲', '▼']).count(),
                    arrows,
                    "{diagram}"
                );
                assert!(
                    diagram
                        .lines()
                        .all(|line| unicode_width::UnicodeWidthStr::width(line) <= width)
                );
                let lines = statements(source).unwrap();
                let mut graph = Graph::default();
                for statement in &lines[1..] {
                    graph.statement(statement).unwrap();
                }
                for label in graph
                    .nodes
                    .iter()
                    .map(|node| &node.text)
                    .chain(graph.edges.iter().map(|edge| &edge.text))
                {
                    assert!(
                        (5..=LABEL_LIMIT).any(|cells| wrap_label(label, cells)
                            .iter()
                            .all(|chunk| diagram.contains(chunk))),
                        "lost label {label:?}: {diagram}"
                    );
                }
                assert!(diagram.contains("~/.optchat"));
                assert!(diagram.contains('├'), "cylinder shape lost: {diagram}");
                if source == COMPONENTS_FIXTURE && width >= 100 {
                    assert!(
                        diagram
                            .lines()
                            .any(|line| line.contains("chat.py") && line.contains("compactor.py")),
                        "branches collapsed: {diagram}"
                    );
                    assert!(
                        diagram.lines().count() < 130,
                        "unnecessarily tall: {diagram}"
                    );
                }
                if source == DEPLOYMENT_FIXTURE {
                    let labels = render(source, width)
                        .unwrap()
                        .into_iter()
                        .flatten()
                        .filter(|span| span.role == Role::Label)
                        .map(|span| span.text)
                        .collect::<String>();
                    let words = labels.split_whitespace().collect::<Vec<_>>().join(" ");
                    assert!(words.contains("ssh+tmux advised"), "{diagram}");
                    assert!(words.contains("push: not built"), "{diagram}");
                    assert!(diagram.contains('┆') || diagram.contains('┄'));
                }
            }
        }
    }

    #[test]
    fn review_cyclic_and_skipping_graphs_keep_topology_in_multiple_columns() {
        for source in [
            "graph TD\nA[Request] --> B{Ready?}\nB -->|yes| C[Execute]\nB -->|no| D[Review]\nC --> E[Result]\nD --> E\nE -->|retry| B",
            "graph TD\nA --> B & C\nB --> D\nC --> D\nA -->|skip| D",
        ] {
            let diagram = plain(source, 100);
            assert!(
                diagram
                    .lines()
                    .any(|line| (line.contains("Execute") && line.contains("Review"))
                        || (line.contains('B') && line.contains('C'))),
                "{diagram}"
            );
        }
    }

    #[test]
    fn review_cylinder_shapes_and_wrapped_labels_are_lossless() {
        for shape in ["[(\"database\")]", "[(database)]"] {
            for direction in ["TD", "BT", "LR", "RL"] {
                let diagram = plain(&format!("graph {direction}\nDB{shape} --> END[Done]"), 100);
                assert!(diagram.contains("database"));
                assert!(diagram.contains('├') && diagram.contains('┤'), "{diagram}");
                assert_eq!(diagram.matches(['▶', '◀', '▲', '▼']).count(), 1);
            }
        }
        for source in [
            "graph TD\nA[(\"bad\"] --> B",
            "graph TD\nA[(good)]\nA[good]",
            "graph TD\nA[[unsupported]] --> B",
        ] {
            assert_eq!(render(source, 100), Err(DiagramError::Unsupported));
        }
        let node = Node {
            id: "DB".into(),
            text: "研究  two   gaps".into(),
            shape: Shape::Cylinder,
            declared: true,
        };
        let mut canvas = Canvas::new(10, 7, 10).unwrap();
        canvas.node_box(0, 0, 10, 7, &node).unwrap();
        let rows = canvas
            .finish()
            .into_iter()
            .map(|row| row.into_iter().map(|span| span.text).collect::<String>())
            .collect::<Vec<_>>();
        for chunk in wrap_label(&node.text, 8) {
            assert!(
                rows.iter().any(|row| row.contains(&chunk)),
                "missing {chunk:?}: {rows:?}"
            );
        }
    }

    #[test]
    fn review_dense_graphs_are_bounded_and_preserve_label_tokens() {
        for direction in ["TD", "BT", "LR", "RL"] {
            for width in [0, 1, 20, 60, 100, 160, 240] {
                let mut source = format!("graph {direction}\n");
                for from in 0..5 {
                    for to in 0..5 {
                        source.push_str(&format!(
                            "N{from}[node{from}] -->|edge{from}{to}| N{to}[node{to}]\n"
                        ));
                    }
                }
                match render(&source, width) {
                    Ok(rows) => {
                        assert!(
                            rows.len() * width <= CELL_LIMIT
                                || rows
                                    .iter()
                                    .map(|row| row
                                        .iter()
                                        .map(|span| unicode_width::UnicodeWidthStr::width(
                                            span.text.as_str()
                                        ))
                                        .sum::<usize>())
                                    .max()
                                    .unwrap_or(0)
                                    * rows.len()
                                    <= CELL_LIMIT
                        );
                        let plain = rows
                            .into_iter()
                            .map(|row| row.into_iter().map(|span| span.text).collect::<String>())
                            .collect::<Vec<_>>()
                            .join("\n");
                        assert_eq!(plain.matches(['▶', '◀', '▲', '▼']).count(), 25, "{plain}");
                        for from in 0..5 {
                            assert!(plain.contains(&format!("node{from}")), "{plain}");
                            for to in 0..5 {
                                assert!(plain.contains(&format!("edge{from}{to}")), "{plain}");
                            }
                        }
                        assert!(
                            plain
                                .lines()
                                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= width)
                        );
                    }
                    Err(error) => {
                        assert!(matches!(error, DiagramError::TooWide | DiagramError::Limit))
                    }
                }
            }
        }
    }

    #[test]
    fn review_label_wrapping_is_lossless_and_cell_bounded() {
        for source in [
            "keep  two   gaps",
            "研究  空白   維持",
            "averylongword mixed  text",
            " spaced   ",
        ] {
            for width in 2..=20 {
                let rows = wrap_label(source, width);
                assert_eq!(rows.concat(), source);
                assert!(
                    rows.iter()
                        .all(|row| unicode_width::UnicodeWidthStr::width(row.as_str()) <= width)
                );
            }
        }
        assert_eq!(
            render("sequenceDiagram\nparticipant A", 6),
            Err(DiagramError::TooWide)
        );
        assert!(render("sequenceDiagram\nparticipant A\nparticipant B", 18).is_ok());
    }

    #[test]
    fn review_sequence_participants_without_messages_need_only_their_boxes() {
        let diagram = plain("sequenceDiagram\nparticipant A", 7);
        assert!(diagram.contains('A'));
    }

    #[test]
    fn review_sequence_wrapping_preserves_label_whitespace() {
        let diagram = plain("sequenceDiagram\nA->>B: keep  two   gaps", 100);
        assert!(diagram.contains("keep  two   gaps"), "{diagram}");
    }

    #[test]
    fn quest_zoom_chain_reflows_without_discarding_nodes_or_edges() {
        let source = "flowchart LR\nV[\"view line 0+64\"] -->|\"zoom(0,64)\"| A[\"0+32, 32+32\"]\nA -->|\"zoom(32,32)\"| B[\"32+16, 48+16\"]\nB -->|\"3 more zooms\"| C[\"40+2, 42+2\"]\nC -->|\"zoom(40,2)\"| D[\"40+1, 41+1\"]\nD -->|\"zoom(41,1)\"| E[\"msg 41, whole\"]";
        for direction in ["LR", "RL"] {
            let source = source.replacen("flowchart LR", &format!("flowchart {direction}"), 1);
            for width in [30, 60, 80, 100, 160, 240] {
                let diagram = plain(&source, width);
                assert_eq!(
                    diagram.matches(['▶', '◀', '▲', '▼']).count(),
                    5,
                    "{diagram}"
                );
                for label in [
                    "view line 0+64",
                    "0+32, 32+32",
                    "32+16, 48+16",
                    "40+2, 42+2",
                    "40+1, 41+1",
                    "msg 41, whole",
                    "zoom(0,64)",
                    "zoom(32,32)",
                    "3 more zooms",
                    "zoom(40,2)",
                    "zoom(41,1)",
                ] {
                    assert!(diagram.contains(label), "missing {label}: {diagram}");
                }
                assert!(
                    diagram
                        .lines()
                        .all(|line| unicode_width::UnicodeWidthStr::width(line) <= width)
                );
                if width < 180 {
                    assert!(diagram.contains("vertical reflow"), "{diagram}");
                    assert_eq!(
                        diagram
                            .matches(if direction == "LR" { '▼' } else { '▲' })
                            .count(),
                        5
                    );
                } else {
                    assert!(!diagram.contains("vertical reflow"));
                }
            }
        }
        assert_eq!(render(source, 2), Err(DiagramError::TooWide));
        assert_eq!(
            render("flowchart LR\nA[one]\nA[two]", 20),
            Err(DiagramError::Unsupported)
        );
    }

    #[test]
    fn quest_sequence_fits_normal_terminal_with_wrapped_messages() {
        let source = "sequenceDiagram\nactor U as You\nparticipant C as chat.Chat\nparticipant M as memory\nparticipant S as store\nparticipant L as LLM endpoint\nU->>C: one message\nC->>M: view_text() — before the message is logged\nC->>S: append kind=user (1 write + fsync)\nC->>L: tools · system · view pieces · message\nL-->>C: talk / tool_call\nC->>M: zoom(id+n) when a line is too vague\nM-->>C: two children … down to the original message\nC->>S: append tool / echo (CAP 30k, head+tail)\nL-->>C: final talk\nC->>S: append talk · git commit";
        for width in [100, 160] {
            let diagram = plain(source, width);
            assert_eq!(diagram.matches(['▶', '◀']).count(), 10, "{diagram}");
            assert!(
                diagram
                    .lines()
                    .all(|row| unicode_width::UnicodeWidthStr::width(row) <= width)
            );
            assert!(diagram.contains("view_text()"));
            eprintln!("Quest sequence, {width} columns:\n{diagram}");
        }
    }

    #[test]
    fn review_sequence_labels_and_self_routes_do_not_overwrite_lifelines() {
        let source = "sequenceDiagram\nparticipant A\nparticipant B\nparticipant C\nA->>A: a sufficiently long self message\nA->>C: a sufficiently long cross message";
        let rows = render(source, 240).unwrap();
        let diagram = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let centers = diagram[1]
            .chars()
            .enumerate()
            .filter_map(|(i, ch)| matches!(ch, 'A' | 'B' | 'C').then_some(i))
            .collect::<Vec<_>>();
        assert_eq!(centers.len(), 3);
        assert_eq!(
            diagram[6].chars().nth(centers[1]),
            Some('┆'),
            "{}",
            diagram.join("\n")
        );
        assert_eq!(
            diagram[9].chars().nth(centers[1]),
            Some('┆'),
            "{}",
            diagram.join("\n")
        );
    }

    #[test]
    fn layered_branch_merge_uses_horizontal_space() {
        for direction in ["TD", "LR"] {
            let source = format!(
                "flowchart {direction}\nA[Request] --> B{{Ready?}}\nB -->|yes| C[Execute]\nB -->|no| D[Review]\nC --> E[Result]\nD --> E"
            );
            let diagram = plain(&source, 100);
            eprintln!("\n{direction}\n{diagram}");
            assert_eq!(diagram.matches(['▶', '◀', '▲', '▼']).count(), 5);
            if direction == "TD" {
                assert!(
                    diagram
                        .lines()
                        .any(|line| line.contains("Execute") && line.contains("Review")),
                    "{diagram}"
                );
            } else {
                assert!(
                    diagram.lines().count() <= 13,
                    "LR unexpectedly fell back: {diagram}"
                );
            }
        }
    }

    #[test]
    fn flowchart_layout_preserves_labels_and_direction() {
        for direction in ["TD", "TB", "BT", "LR", "RL"] {
            let source =
                format!("flowchart {direction}\nA([Start]) --> B{{Ready?}}\nB --> C[Done]");
            let rendered = plain(&source, 200);
            assert!(rendered.contains("Start"));
            assert!(rendered.contains("◇ Ready?"));
            assert!(rendered.contains("Done"));
            assert!(rendered.contains('╭'));
            assert!(rendered.contains(match direction {
                "BT" => '▲',
                "LR" => '▶',
                "RL" => '◀',
                _ => '▼',
            }));
        }
    }

    #[test]
    fn branches_cycles_and_fanout_have_independent_routes() {
        let rendered = plain("graph TD; A --> B & C; B -.-> C; C --> A; A --> A", 120);
        assert!(rendered.contains('╪'));
        assert!(rendered.contains('┆') || rendered.contains('┄'));
        assert_eq!(rendered.matches(['◀', '▼']).count(), 5, "{rendered}");
    }

    #[test]
    fn quoted_labels_unicode_and_comments_survive() {
        let rendered = plain(
            "%% title\nflowchart LR\nA[\"研究; notes\"] -- reviewed --> B[Done]",
            100,
        );
        assert!(rendered.contains("研究; notes"));
        assert!(rendered.contains("reviewed"));
    }

    #[test]
    fn unsupported_input_never_returns_a_partial_diagram() {
        for source in [
            "pie\nDogs: 4",
            "flowchart TD\nA --> B\nclick B \"https://example.com\"",
            "flowchart TD\nsubgraph cluster\nA --> B\nend",
            "flowchart TD\nA[<script>]",
            "flowchart TD\nA[bad\u{1b}label]",
            "flowchart TD\nA[combining e\u{301}]",
            "flowchart TD\nA -->",
            "flowchart TD\nA[one]\nA[two]",
        ] {
            assert_eq!(
                render(source, 120),
                Err(DiagramError::Unsupported),
                "{source}"
            );
        }
    }

    #[test]
    fn sequence_and_flat_states_preserve_chronology_and_labels() {
        let sequence = plain(
            "sequenceDiagram\nparticipant A as Client\nparticipant B as Service\nA->>B: request\nB-->>A: response\nA->>A: retry",
            120,
        );
        assert!(sequence.contains("Client") && sequence.contains("Service"));
        assert!(sequence.find("request") < sequence.find("response"));
        assert!(sequence.contains("retry"));
        assert_eq!(sequence.matches(['▶', '◀']).count(), 3, "{sequence}");
        let states = plain(
            "stateDiagram-v2\n[*] --> Idle\nIdle --> Running: start\nRunning --> [*]: stop",
            120,
        );
        assert!(states.contains("Initial") && states.contains("Final"));
        assert!(states.contains("start") && states.contains("stop"));
        assert_eq!(
            render("sequenceDiagram\nloop repeat\nA->>B: request\nend", 120),
            Err(DiagramError::Unsupported)
        );
    }

    #[test]
    fn state_markers_do_not_alias_user_states_and_conflicts_fail_closed() {
        let diagram = plain(
            "stateDiagram-v2\n[*] --> Initial\nInitial --> Final\nFinal --> [*]",
            120,
        );
        assert!(diagram.contains("● Initial") && diagram.contains("◎ Final"));
        assert_eq!(diagram.matches('╭').count(), 2);
        assert_eq!(diagram.matches('┌').count(), 2);
        assert_eq!(
            render(
                "stateDiagram-v2\nstate \"One\" as A\nstate \"Two\" as A",
                120
            ),
            Err(DiagramError::Unsupported)
        );
    }

    #[test]
    fn directives_conflicting_labels_and_flow_shapes_in_states_fail_closed() {
        for source in [
            "%%{init: {}}%%\ngraph LR\nA --> B",
            "graph LR\nA -- first -->|second| B",
            "stateDiagram-v2\nstate \"Label\" as A[discarded]",
            "stateDiagram-v2\nA[flowchart shape] --> B",
            "stateDiagram-v2\nA: description\nstate \"Replacement\" as A",
            "stateDiagram-v2\nend",
            "graph LR\nA[\"bad\"quotes\"]",
        ] {
            assert_eq!(
                render(source, 120),
                Err(DiagramError::Unsupported),
                "{source}"
            );
        }
    }

    #[test]
    fn vertical_edge_labels_never_overwrite_routed_lanes() {
        let diagram = render("graph TD\nA -->|a very long label| B\nC --> A", 120).unwrap();
        let row = diagram
            .iter()
            .find(|row| {
                row.iter()
                    .any(|span| span.text.contains("a very long label"))
            })
            .unwrap();
        let label = row
            .iter()
            .position(|span| span.text.contains("a very long label"))
            .unwrap();
        let edges_before = row[..label]
            .iter()
            .filter(|span| span.role == Role::Edge)
            .map(|span| span.text.matches('│').count())
            .sum::<usize>();
        assert!(edges_before >= 1, "{row:?}");
    }

    #[test]
    fn every_small_directed_graph_preserves_all_arrowheads() {
        for direction in ["TD", "BT", "LR", "RL"] {
            for mask in 0..512usize {
                let mut source = format!("graph {direction}\nA\nB\nC\n");
                for from in 0..3 {
                    for to in 0..3 {
                        if mask & (1 << (from * 3 + to)) != 0 {
                            source.push_str(&format!(
                                "{} --> {}\n",
                                (b'A' + from as u8) as char,
                                (b'A' + to as u8) as char
                            ));
                        }
                    }
                }
                let diagram = plain(&source, 240);
                assert_eq!(
                    diagram.matches(['▶', '◀', '▲', '▼']).count(),
                    mask.count_ones() as usize,
                    "{source}\n{diagram}"
                );
                assert!(
                    diagram
                        .lines()
                        .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 240)
                );
            }
        }
    }

    #[test]
    fn width_source_and_graph_limits_are_explicit() {
        assert_eq!(render("graph LR\nA --> B", 2), Err(DiagramError::TooWide));
        assert_eq!(
            render(&"x".repeat(SOURCE_LIMIT + 1), 120),
            Err(DiagramError::Limit)
        );
        let source = format!(
            "graph TD\n{}",
            (0..=NODE_LIMIT)
                .map(|i| format!("N{i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert_eq!(render(&source, 120), Err(DiagramError::Limit));
    }
}
