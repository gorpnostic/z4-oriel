//! The split tree for one tab: every node is either a pane or a split of two children, like tmux.

use ratatui::layout::Rect;

pub type PaneId = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Right, // children side by side
    Down,  // children stacked
}

#[derive(Debug, Clone)]
pub enum Node {
    Leaf(PaneId),
    Split { dir: Dir, ratio: f32, a: Box<Node>, b: Box<Node> },
}

impl Node {
    pub fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(x) => *x == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    /// Replace leaf `target` with a split of (target, new).
    pub fn split(&mut self, target: PaneId, new: PaneId, dir: Dir) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split { dir, ratio: 0.5, a: Box::new(Node::Leaf(target)), b: Box::new(Node::Leaf(new)) };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, new, dir) || b.split(target, new, dir),
        }
    }

    /// Remove leaf `target`; its sibling takes the parent's place. Returns false if target is the root leaf.
    pub fn remove(&mut self, target: PaneId) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => {
                if matches!(**a, Node::Leaf(id) if id == target) {
                    *self = (**b).clone();
                    return true;
                }
                if matches!(**b, Node::Leaf(id) if id == target) {
                    *self = (**a).clone();
                    return true;
                }
                a.remove(target) || b.remove(target)
            }
        }
    }

    pub fn replace(&mut self, target: PaneId, new: PaneId) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *id = new;
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.replace(target, new) || b.replace(target, new),
        }
    }

    /// Pane rectangles. Splits leave no gap: each pane draws its own rounded frame.
    pub fn rects(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, area)),
            Node::Split { dir, ratio, a, b } => {
                let (ra, rb) = split_rect(area, *dir, *ratio);
                a.rects(ra, out);
                b.rects(rb, out);
            }
        }
    }

    /// Split borders, for mouse dragging: (rect of the whole split, dir, path-to-node).
    pub fn borders(&self, area: Rect, path: &mut Vec<bool>, out: &mut Vec<(Rect, Dir, Vec<bool>)>) {
        if let Node::Split { dir, ratio, a, b } = self {
            out.push((area, *dir, path.clone()));
            let (ra, rb) = split_rect(area, *dir, *ratio);
            path.push(false);
            a.borders(ra, path, out);
            path.pop();
            path.push(true);
            b.borders(rb, path, out);
            path.pop();
        }
    }

    pub fn node_at(&mut self, path: &[bool]) -> Option<&mut Node> {
        if path.is_empty() {
            return Some(self);
        }
        match self {
            Node::Leaf(_) => None,
            Node::Split { a, b, .. } => if path[0] { b } else { a }.node_at(&path[1..]),
        }
    }

    /// Grow/shrink the nearest split of `dir` that contains `id`. `delta` > 0 moves the divider right/down.
    pub fn resize(&mut self, id: PaneId, dir: Dir, delta: f32) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { dir: d, ratio, a, b } => {
                let in_a = a.contains(id);
                let in_b = b.contains(id);
                if !in_a && !in_b {
                    return false;
                }
                // deepest matching split wins
                let child = if in_a { a } else { b };
                if child.resize(id, dir, delta) {
                    return true;
                }
                if *d == dir {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    return true;
                }
                false
            }
        }
    }
}

pub fn split_rect(area: Rect, dir: Dir, ratio: f32) -> (Rect, Rect) {
    match dir {
        Dir::Right => {
            let w = ((area.width as f32) * ratio).round() as u16;
            let w = w.clamp(1.min(area.width), area.width.saturating_sub(1));
            (Rect { width: w, ..area }, Rect { x: area.x + w, width: area.width - w, ..area })
        }
        Dir::Down => {
            let h = ((area.height as f32) * ratio).round() as u16;
            let h = h.clamp(1.min(area.height), area.height.saturating_sub(1));
            (Rect { height: h, ..area }, Rect { y: area.y + h, height: area.height - h, ..area })
        }
    }
}

/// The pane whose rect is nearest in a direction from `from` (for Alt+arrow focus).
pub fn neighbor(rects: &[(PaneId, Rect)], from: PaneId, dx: i32, dy: i32) -> Option<PaneId> {
    let (_, r) = rects.iter().find(|(id, _)| *id == from)?;
    let (cx, cy) = (r.x as i32 + r.width as i32 / 2, r.y as i32 + r.height as i32 / 2);
    rects
        .iter()
        .filter(|(id, _)| *id != from)
        .filter(|(_, o)| {
            let (ox, oy) = (o.x as i32, o.y as i32);
            let (ow, oh) = (o.width as i32, o.height as i32);
            match (dx, dy) {
                (1, _) => ox >= r.x as i32 + r.width as i32 - 1 && oy < r.y as i32 + r.height as i32 && oy + oh > r.y as i32,
                (-1, _) => ox + ow <= r.x as i32 + 1 && oy < r.y as i32 + r.height as i32 && oy + oh > r.y as i32,
                (_, 1) => oy >= r.y as i32 + r.height as i32 - 1 && ox < r.x as i32 + r.width as i32 && ox + ow > r.x as i32,
                _ => oy + oh <= r.y as i32 + 1 && ox < r.x as i32 + r.width as i32 && ox + ow > r.x as i32,
            }
        })
        .min_by_key(|(_, o)| {
            let (ox, oy) = (o.x as i32 + o.width as i32 / 2, o.y as i32 + o.height as i32 / 2);
            (ox - cx).abs() + (oy - cy).abs()
        })
        .map(|(id, _)| *id)
}
