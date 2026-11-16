//! ペインの配置を表す二分木と，矩形の計算．

pub type PaneId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// 分割方向．
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// 左右に並べる（tmux の `%`）．
    Horizontal,
    /// 上下に並べる（tmux の `"`）．
    Vertical,
}

/// ペイン間の区切り線（今は枠どうしのすき間になるだけで，位置はテストでしか見ない）．
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct Separator {
    pub x: u16,
    pub y: u16,
    pub len: u16,
    /// `Horizontal` 分割の区切りは縦線になる．
    pub dir: Dir,
}

#[derive(Debug)]
pub enum Node {
    Leaf(PaneId),
    Split { dir: Dir, a: Box<Node>, b: Box<Node> },
}

impl Node {
    /// ペインを半分ずつ再帰的に分け，左右・上下を交互に並べる（4 つなら 2×2）．
    /// `ids` は空であってはならない．
    pub fn tiled(ids: &[PaneId], dir: Dir) -> Node {
        match ids {
            [] => panic!("Node::tiled: ids が空"),
            [id] => Node::Leaf(*id),
            _ => {
                let (a, b) = ids.split_at(ids.len().div_ceil(2));
                let next = match dir {
                    Dir::Horizontal => Dir::Vertical,
                    Dir::Vertical => Dir::Horizontal,
                };
                Node::Split { dir, a: Box::new(Node::tiled(a, next)), b: Box::new(Node::tiled(b, next)) }
            }
        }
    }

    /// `target` のペインを分割し，後ろ側に `new` を置く．
    pub fn split(&mut self, target: PaneId, new: PaneId, dir: Dir) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split {
                    dir,
                    a: Box::new(Node::Leaf(target)),
                    b: Box::new(Node::Leaf(new)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, new, dir) || b.split(target, new, dir),
        }
    }

    /// `target` を取り除いた木を返す．木が空になれば `None`．
    pub fn remove(self, target: PaneId) -> Option<Node> {
        match self {
            Node::Leaf(id) if id == target => None,
            leaf @ Node::Leaf(_) => Some(leaf),
            Node::Split { dir, a, b } => match (a.remove(target), b.remove(target)) {
                (Some(a), Some(b)) => Some(Node::Split {
                    dir,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(n), None) | (None, Some(n)) => Some(n),
                (None, None) => None,
            },
        }
    }

    /// 木に含まれるペイン（表示順）．
    pub fn ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        fn walk(n: &Node, out: &mut Vec<PaneId>) {
            match n {
                Node::Leaf(id) => out.push(*id),
                Node::Split { a, b, .. } => {
                    walk(a, out);
                    walk(b, out);
                }
            }
        }
        walk(self, &mut out);
        out
    }

    /// 木を `area` に割り付け，各ペインの矩形と区切り線を求める．
    pub fn layout(&self, area: Rect, panes: &mut Vec<(PaneId, Rect)>, seps: &mut Vec<Separator>) {
        match self {
            Node::Leaf(id) => panes.push((*id, area)),
            Node::Split { dir, a, b } => {
                let (ra, rb, sep) = split_rect(area, *dir);
                a.layout(ra, panes, seps);
                b.layout(rb, panes, seps);
                seps.extend(sep);
            }
        }
    }
}

/// 区切り線 1 セルを挟んで矩形を二等分する．
fn split_rect(r: Rect, dir: Dir) -> (Rect, Rect, Option<Separator>) {
    match dir {
        Dir::Horizontal => {
            let avail = r.w.saturating_sub(1);
            let aw = avail - avail / 2;
            let bw = avail / 2;
            let a = Rect { w: aw, ..r };
            let b = Rect { x: r.x + aw + 1, w: bw, ..r };
            let sep = (r.w > 0).then_some(Separator { x: r.x + aw, y: r.y, len: r.h, dir });
            (a, b, sep)
        }
        Dir::Vertical => {
            let avail = r.h.saturating_sub(1);
            let ah = avail - avail / 2;
            let bh = avail / 2;
            let a = Rect { h: ah, ..r };
            let b = Rect { y: r.y + ah + 1, h: bh, ..r };
            let sep = (r.h > 0).then_some(Separator { x: r.x, y: r.y + ah, len: r.w, dir });
            (a, b, sep)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(node: &Node, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut panes = Vec::new();
        node.layout(area, &mut panes, &mut Vec::new());
        panes
    }

    #[test]
    fn split_and_remove() {
        let area = Rect { x: 0, y: 0, w: 81, h: 24 };
        let mut root = Node::Leaf(0);
        assert!(root.split(0, 1, Dir::Horizontal));
        assert!(root.split(1, 2, Dir::Vertical));
        assert_eq!(
            rects(&root, area),
            vec![
                (0, Rect { x: 0, y: 0, w: 40, h: 24 }),
                (1, Rect { x: 41, y: 0, w: 40, h: 12 }),
                (2, Rect { x: 41, y: 13, w: 40, h: 11 }),
            ]
        );
        let root = root.remove(1).unwrap();
        assert_eq!(
            rects(&root, area),
            vec![(0, Rect { x: 0, y: 0, w: 40, h: 24 }), (2, Rect { x: 41, y: 0, w: 40, h: 24 })]
        );
        let root = root.remove(0).unwrap().remove(2);
        assert!(root.is_none());
    }

    #[test]
    fn tiled_grid() {
        let area = Rect { x: 0, y: 0, w: 81, h: 25 };
        let root = Node::tiled(&[0, 1, 2, 3], Dir::Horizontal);
        assert_eq!(
            rects(&root, area),
            vec![
                (0, Rect { x: 0, y: 0, w: 40, h: 12 }),
                (1, Rect { x: 0, y: 13, w: 40, h: 12 }),
                (2, Rect { x: 41, y: 0, w: 40, h: 12 }),
                (3, Rect { x: 41, y: 13, w: 40, h: 12 }),
            ]
        );
        // 3 つなら左に 2 つ，右に 1 つ
        let root = Node::tiled(&[0, 1, 2], Dir::Horizontal);
        assert_eq!(rects(&root, area).len(), 3);
        assert_eq!(rects(&root, area)[2].1, Rect { x: 41, y: 0, w: 40, h: 25 });
    }
}
