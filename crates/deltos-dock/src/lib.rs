//! The dock layout: where every panel of the app sits.
//!
//! A layout is a tree per window. The main window has one, and each panel window that was
//! torn off has its own, with the bounds it last had so it reopens in the same place. A
//! tree is built from two kinds of node:
//!
//! - a [`Stack`]: a tabbed group of panels with one of them showing;
//! - a [`Node::Split`]: children side by side or one above the other, each taking a
//!   fraction of the space.
//!
//! The model knows nothing about what a panel shows. Panels are opaque [`PanelId`]s, and
//! the app keeps what each one is. Every operation leaves the layout normalized: there are
//! no empty stacks, no splits with fewer than two children, no split directly inside a
//! split of the same axis, fractions are positive and add up to one, and each panel is in
//! exactly one place. [`Layout::validate`] checks this, and the tests drive long random
//! sequences of operations through it.

use serde::{Deserialize, Serialize};

/// A panel, as the app names it. Stable across restarts, so saved layouts can refer to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PanelId(pub u64);

/// A torn-off panel window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(pub u64);

/// The direction children of a split are laid out in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Left to right.
    Row,
    /// Top to bottom.
    Column,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    pub fn axis(self) -> Axis {
        match self {
            Edge::Left | Edge::Right => Axis::Row,
            Edge::Top | Edge::Bottom => Axis::Column,
        }
    }

    /// Whether a node dropped on this edge goes before the target.
    fn leading(self) -> bool {
        matches!(self, Edge::Left | Edge::Top)
    }
}

/// A tabbed group of panels. `active` is the one showing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stack {
    pub panels: Vec<PanelId>,
    #[serde(default)]
    pub active: usize,
}

impl Stack {
    pub fn new(panel: PanelId) -> Self {
        Self {
            panels: vec![panel],
            active: 0,
        }
    }

    pub fn active_panel(&self) -> Option<PanelId> {
        self.panels.get(self.active).or(self.panels.last()).copied()
    }
}

/// One child of a split and the fraction of the split it takes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Child {
    pub size: f32,
    pub node: Node,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Node {
    Split { axis: Axis, children: Vec<Child> },
    Stack(Stack),
}

impl Node {
    fn stack(panel: PanelId) -> Self {
        Node::Stack(Stack::new(panel))
    }

    /// Every panel in the node, in reading order.
    pub fn panels(&self, out: &mut Vec<PanelId>) {
        match self {
            Node::Stack(s) => out.extend_from_slice(&s.panels),
            Node::Split { children, .. } => children.iter().for_each(|c| c.node.panels(out)),
        }
    }

    /// The path to the stack holding `panel`: child indexes from this node down.
    fn path_of(&self, panel: PanelId) -> Option<Vec<usize>> {
        match self {
            Node::Stack(s) => s.panels.contains(&panel).then(Vec::new),
            Node::Split { children, .. } => children.iter().enumerate().find_map(|(i, c)| {
                c.node.path_of(panel).map(|mut p| {
                    p.insert(0, i);
                    p
                })
            }),
        }
    }

    fn at(&self, path: &[usize]) -> Option<&Node> {
        match (path.split_first(), self) {
            (None, n) => Some(n),
            (Some((i, rest)), Node::Split { children, .. }) => children.get(*i)?.node.at(rest),
            (Some(_), Node::Stack(_)) => None,
        }
    }

    fn at_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        match (path.split_first(), self) {
            (None, n) => Some(n),
            (Some((i, rest)), Node::Split { children, .. }) => {
                children.get_mut(*i)?.node.at_mut(rest)
            }
            (Some(_), Node::Stack(_)) => None,
        }
    }

    fn stack_mut(&mut self, panel: PanelId) -> Option<&mut Stack> {
        let path = self.path_of(panel)?;
        match self.at_mut(&path)? {
            Node::Stack(s) => Some(s),
            Node::Split { .. } => None,
        }
    }

    /// Removes `panel` wherever it is. The tree may be left unnormalized.
    fn remove(&mut self, panel: PanelId) -> bool {
        match self {
            Node::Stack(s) => match s.panels.iter().position(|p| *p == panel) {
                Some(i) => {
                    s.panels.remove(i);
                    if s.active > i || s.active >= s.panels.len() {
                        s.active = s.active.saturating_sub(1);
                    }
                    true
                }
                None => false,
            },
            Node::Split { children, .. } => children.iter_mut().any(|c| c.node.remove(panel)),
        }
    }

    /// Brings the node back to the normal form described in the crate docs. Returns
    /// `None` when nothing is left in it.
    fn normalize(self, seen: &mut Vec<PanelId>) -> Option<Node> {
        match self {
            Node::Stack(mut s) => {
                let active = s.active_panel();
                s.panels.retain(|p| {
                    let fresh = !seen.contains(p);
                    if fresh {
                        seen.push(*p);
                    }
                    fresh
                });
                if s.panels.is_empty() {
                    return None;
                }
                s.active = active
                    .and_then(|a| s.panels.iter().position(|p| *p == a))
                    .unwrap_or(0)
                    .min(s.panels.len() - 1);
                Some(Node::Stack(s))
            }
            Node::Split { axis, children } => {
                let mut kept: Vec<Child> = Vec::with_capacity(children.len());
                // A child that is gone leaves its space to the one before it, or to the
                // next one when it was first, so the others (such as sidebars) keep
                // their size instead of all growing a little.
                let mut freed = 0.0;
                for child in children {
                    let size = sane_size(child.size);
                    let Some(node) = child.node.normalize(seen) else {
                        match kept.last_mut() {
                            Some(last) => last.size += size,
                            None => freed += size,
                        }
                        continue;
                    };
                    let size = size + std::mem::take(&mut freed);
                    match node {
                        // A split inside a split of the same axis is merged into it, its
                        // children sharing out the space it had.
                        Node::Split {
                            axis: inner,
                            children: grandchildren,
                        } if inner == axis => {
                            let total: f32 = grandchildren.iter().map(|g| g.size).sum();
                            for g in grandchildren {
                                kept.push(Child {
                                    size: size * g.size / total,
                                    node: g.node,
                                });
                            }
                        }
                        node => kept.push(Child { size, node }),
                    }
                }
                match kept.len() {
                    0 => None,
                    1 => kept.pop().map(|c| c.node),
                    _ => {
                        let total: f32 = kept.iter().map(|c| c.size).sum();
                        for c in &mut kept {
                            c.size /= total;
                        }
                        Some(Node::Split {
                            axis,
                            children: kept,
                        })
                    }
                }
            }
        }
    }
}

/// A size read from a file or set by a drag: anything unusable becomes an even share.
fn sane_size(size: f32) -> f32 {
    if size.is_finite() && size > 0.0 {
        size.max(MIN_FRACTION / 4.0)
    } else {
        1.0
    }
}

/// The smallest fraction a resize leaves a child, so no panel can be dragged to nothing.
pub const MIN_FRACTION: f32 = 0.04;

/// The share a node dropped on the outer edge of a whole tree takes.
const ROOT_EDGE_SHARE: f32 = 0.25;

/// One window's panels. Empty when every panel has been moved out of it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Tree {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<Node>,
}

impl Tree {
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn panels(&self) -> Vec<PanelId> {
        let mut out = Vec::new();
        if let Some(r) = &self.root {
            r.panels(&mut out);
        }
        out
    }

    pub fn contains(&self, panel: PanelId) -> bool {
        self.root.as_ref().and_then(|r| r.path_of(panel)).is_some()
    }

    /// The node at `path`, for the UI to read a split it is resizing.
    pub fn node(&self, path: &[usize]) -> Option<&Node> {
        self.root.as_ref()?.at(path)
    }

    fn normalize(&mut self, seen: &mut Vec<PanelId>) {
        self.root = self.root.take().and_then(|r| r.normalize(seen));
    }
}

/// Window bounds in screen pixels, saved so a panel window reopens where it was.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FloatingWindow {
    pub id: WindowId,
    pub tree: Tree,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Rect>,
}

/// Which window's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TreeId {
    Main,
    Floating(WindowId),
}

/// Where within a target a panel lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Zone {
    /// As a new tab, at the end.
    Center,
    /// Beside the target, splitting its space.
    Edge(Edge),
    /// As a tab at this index.
    Tab(usize),
}

/// Where a dropped panel goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropTarget {
    /// Relative to the stack that holds `anchor`.
    Stack { anchor: PanelId, zone: Zone },
    /// Relative to a whole window: beside everything in it for an edge, or into it when
    /// it is empty. A center drop into a window that has panels joins its first stack.
    Root { tree: TreeId, zone: Zone },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockError {
    UnknownPanel(PanelId),
    AlreadyPlaced(PanelId),
    UnknownWindow(WindowId),
    /// A window cannot be merged into itself.
    SameWindow,
}

impl std::fmt::Display for DockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DockError::UnknownPanel(p) => write!(f, "panel {} is not in the layout", p.0),
            DockError::AlreadyPlaced(p) => write!(f, "panel {} is already in the layout", p.0),
            DockError::UnknownWindow(w) => write!(f, "window {} does not exist", w.0),
            DockError::SameWindow => f.write_str("a window cannot be docked into itself"),
        }
    }
}

impl std::error::Error for DockError {}

/// Where a panel is: the tree, the path to its stack, and its tab index there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub tree: TreeId,
    pub path: Vec<usize>,
    pub index: usize,
}

/// Every window's tree.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub main: Tree,
    pub floating: Vec<FloatingWindow>,
    /// The next free [`WindowId`].
    next_window: u64,
}

impl Layout {
    pub fn new(main: Node) -> Self {
        let mut layout = Self {
            main: Tree { root: Some(main) },
            ..Self::default()
        };
        layout.normalize();
        layout
    }

    pub fn tree(&self, id: TreeId) -> Option<&Tree> {
        match id {
            TreeId::Main => Some(&self.main),
            TreeId::Floating(w) => self.floating.iter().find(|f| f.id == w).map(|f| &f.tree),
        }
    }

    fn tree_mut(&mut self, id: TreeId) -> Option<&mut Tree> {
        match id {
            TreeId::Main => Some(&mut self.main),
            TreeId::Floating(w) => self
                .floating
                .iter_mut()
                .find(|f| f.id == w)
                .map(|f| &mut f.tree),
        }
    }

    pub fn window(&self, id: WindowId) -> Option<&FloatingWindow> {
        self.floating.iter().find(|f| f.id == id)
    }

    fn trees(&self) -> impl Iterator<Item = (TreeId, &Tree)> {
        std::iter::once((TreeId::Main, &self.main)).chain(
            self.floating
                .iter()
                .map(|f| (TreeId::Floating(f.id), &f.tree)),
        )
    }

    /// Every panel in every window.
    pub fn panels(&self) -> Vec<PanelId> {
        self.trees().flat_map(|(_, t)| t.panels()).collect()
    }

    pub fn contains(&self, panel: PanelId) -> bool {
        self.find(panel).is_some()
    }

    pub fn find(&self, panel: PanelId) -> Option<Location> {
        self.trees().find_map(|(id, t)| {
            let root = t.root.as_ref()?;
            let path = root.path_of(panel)?;
            let Node::Stack(s) = root.at(&path)? else {
                return None;
            };
            let index = s.panels.iter().position(|p| *p == panel)?;
            Some(Location {
                tree: id,
                path,
                index,
            })
        })
    }

    /// The stack holding `panel`.
    pub fn stack_of(&self, panel: PanelId) -> Option<&Stack> {
        let loc = self.find(panel)?;
        match self.tree(loc.tree)?.node(&loc.path)? {
            Node::Stack(s) => Some(s),
            Node::Split { .. } => None,
        }
    }

    fn stack_of_mut(&mut self, panel: PanelId) -> Option<&mut Stack> {
        let loc = self.find(panel)?;
        self.tree_mut(loc.tree)?.root.as_mut()?.stack_mut(panel)
    }

    /// Shows `panel` in its stack.
    pub fn activate(&mut self, panel: PanelId) -> bool {
        match self.stack_of_mut(panel) {
            Some(s) => {
                s.active = s.panels.iter().position(|p| *p == panel).unwrap_or(0);
                true
            }
            None => false,
        }
    }

    /// Puts a panel that is not in the layout yet at `target`, and shows it.
    pub fn insert(&mut self, panel: PanelId, target: DropTarget) -> Result<(), DockError> {
        if self.contains(panel) {
            return Err(DockError::AlreadyPlaced(panel));
        }
        self.insert_node(Node::stack(panel), target)?;
        self.activate(panel);
        self.normalize();
        Ok(())
    }

    /// Takes `panel` out of the layout. Returns the tree it was in. A panel window left
    /// with nothing in it is removed.
    pub fn remove(&mut self, panel: PanelId) -> Option<TreeId> {
        let loc = self.find(panel)?;
        self.tree_mut(loc.tree)?.root.as_mut()?.remove(panel);
        self.normalize();
        Some(loc.tree)
    }

    /// Moves a panel that is in the layout to `target`.
    pub fn move_panel(&mut self, panel: PanelId, target: DropTarget) -> Result<(), DockError> {
        let from = self.find(panel).ok_or(DockError::UnknownPanel(panel))?;
        let siblings = self.stack_of(panel).map_or(0, |s| s.panels.len());
        match target {
            DropTarget::Stack { anchor, zone } if anchor == panel => {
                match zone {
                    // Onto itself: nothing changes.
                    Zone::Center => {}
                    Zone::Tab(i) => {
                        if let Some(s) = self.stack_of_mut(panel) {
                            s.panels.retain(|p| *p != panel);
                            let i = i.min(s.panels.len());
                            s.panels.insert(i, panel);
                            s.active = i;
                        }
                    }
                    // Beside its own stack: it splits off, if anything is left behind.
                    Zone::Edge(edge) if siblings > 1 => {
                        let other = self
                            .stack_of(panel)
                            .and_then(|s| s.panels.iter().copied().find(|p| *p != panel))
                            .ok_or(DockError::UnknownPanel(panel))?;
                        self.remove(panel);
                        self.insert_node(
                            Node::stack(panel),
                            DropTarget::Stack {
                                anchor: other,
                                zone: Zone::Edge(edge),
                            },
                        )?;
                    }
                    Zone::Edge(_) => {}
                }
                self.normalize();
                return Ok(());
            }
            DropTarget::Stack { anchor, zone } => {
                let to = self.find(anchor).ok_or(DockError::UnknownPanel(anchor))?;
                // A tab moved within its own stack shifts the indexes after it.
                if let Zone::Tab(i) = zone
                    && to.tree == from.tree
                    && to.path == from.path
                {
                    let i = if from.index < i { i - 1 } else { i };
                    return self.move_panel(
                        panel,
                        DropTarget::Stack {
                            anchor: panel,
                            zone: Zone::Tab(i),
                        },
                    );
                }
            }
            DropTarget::Root { tree, .. } => {
                if let TreeId::Floating(w) = tree
                    && self.tree(tree).is_none()
                {
                    return Err(DockError::UnknownWindow(w));
                }
                // The last panel of a window cannot be dropped beside itself.
                if tree == from.tree && self.tree(tree).is_some_and(|t| t.panels().len() == 1) {
                    return Ok(());
                }
            }
        }
        self.remove(panel);
        let result = self.insert_node(Node::stack(panel), target);
        if result.is_err() {
            // Put it back rather than lose it.
            let _ = self.insert_node(
                Node::stack(panel),
                DropTarget::Root {
                    tree: TreeId::Main,
                    zone: Zone::Center,
                },
            );
        }
        self.activate(panel);
        self.normalize();
        result
    }

    /// Moves `panel` into a new panel window of its own and returns the window. A panel
    /// already alone in a panel window stays where it is.
    pub fn detach(&mut self, panel: PanelId, bounds: Option<Rect>) -> Result<WindowId, DockError> {
        let from = self.find(panel).ok_or(DockError::UnknownPanel(panel))?;
        if let TreeId::Floating(w) = from.tree
            && self.tree(from.tree).is_some_and(|t| t.panels().len() == 1)
        {
            if let Some(f) = self.floating.iter_mut().find(|f| f.id == w) {
                f.bounds = bounds.or(f.bounds);
            }
            return Ok(w);
        }
        self.remove(panel);
        let id = self.new_window_id();
        self.floating.push(FloatingWindow {
            id,
            tree: Tree {
                root: Some(Node::stack(panel)),
            },
            bounds,
        });
        self.normalize();
        Ok(id)
    }

    fn new_window_id(&mut self) -> WindowId {
        let used = self.floating.iter().map(|f| f.id.0 + 1).max().unwrap_or(0);
        self.next_window = self.next_window.max(used);
        let id = WindowId(self.next_window);
        self.next_window += 1;
        id
    }

    /// Moves everything in panel window `window` to `target` and removes the window. A
    /// center drop adds its panels as tabs; an edge drop keeps its arrangement.
    pub fn merge_window(&mut self, window: WindowId, target: DropTarget) -> Result<(), DockError> {
        let index = self
            .floating
            .iter()
            .position(|f| f.id == window)
            .ok_or(DockError::UnknownWindow(window))?;
        let into = match target {
            DropTarget::Stack { anchor, .. } => {
                self.find(anchor)
                    .ok_or(DockError::UnknownPanel(anchor))?
                    .tree
            }
            DropTarget::Root { tree, .. } => tree,
        };
        if into == TreeId::Floating(window) {
            return Err(DockError::SameWindow);
        }
        if self.tree(into).is_none() {
            return Err(match into {
                TreeId::Floating(w) => DockError::UnknownWindow(w),
                TreeId::Main => DockError::SameWindow,
            });
        }
        let removed = self.floating.remove(index);
        let Some(root) = removed.tree.root else {
            return Ok(());
        };
        let mut panels = Vec::new();
        root.panels(&mut panels);
        let first = panels.first().copied();
        let zone = match target {
            DropTarget::Stack { zone, .. } | DropTarget::Root { zone, .. } => zone,
        };
        match zone {
            Zone::Edge(_) => self.insert_node(root, target)?,
            Zone::Center | Zone::Tab(_) => {
                let mut target = target;
                for p in panels {
                    self.insert_node(Node::stack(p), target)?;
                    // The rest follow the first, in order.
                    target = DropTarget::Stack {
                        anchor: p,
                        zone: match zone {
                            Zone::Tab(_) => {
                                let at = self.find(p).map_or(0, |l| l.index + 1);
                                Zone::Tab(at)
                            }
                            _ => Zone::Center,
                        },
                    };
                }
            }
        }
        if let Some(p) = first {
            self.activate(p);
        }
        self.normalize();
        Ok(())
    }

    /// Records where a panel window is, so it reopens there.
    pub fn set_window_bounds(&mut self, window: WindowId, bounds: Rect) {
        if let Some(f) = self.floating.iter_mut().find(|f| f.id == window) {
            f.bounds = Some(bounds);
        }
    }

    /// Moves the divider after child `handle` of the split at `path` by `delta`, a
    /// fraction of the split's length. Neither neighbor shrinks below [`MIN_FRACTION`].
    pub fn resize(&mut self, tree: TreeId, path: &[usize], handle: usize, delta: f32) {
        let Some(Node::Split { children, .. }) = self
            .tree_mut(tree)
            .and_then(|t| t.root.as_mut())
            .and_then(|r| r.at_mut(path))
        else {
            return;
        };
        if handle + 1 >= children.len() || !delta.is_finite() {
            return;
        }
        let pair = children[handle].size + children[handle + 1].size;
        let min = MIN_FRACTION.min(pair / 2.0);
        let a = (children[handle].size + delta).clamp(min, pair - min);
        children[handle].size = a;
        children[handle + 1].size = pair - a;
    }

    /// Sets every fraction of the split at `path` at once. Ignored unless there is one
    /// size per child; the sizes are scaled to add up to one.
    pub fn set_sizes(&mut self, tree: TreeId, path: &[usize], sizes: &[f32]) {
        if let Some(Node::Split { children, .. }) = self
            .tree_mut(tree)
            .and_then(|t| t.root.as_mut())
            .and_then(|r| r.at_mut(path))
            && children.len() == sizes.len()
        {
            for (c, s) in children.iter_mut().zip(sizes) {
                c.size = sane_size(*s);
            }
        }
        self.normalize();
    }

    /// Drops every panel `keep` says no to, for panels the app cannot restore.
    pub fn retain(&mut self, mut keep: impl FnMut(PanelId) -> bool) {
        for p in self.panels() {
            if !keep(p) {
                self.remove(p);
            }
        }
        self.normalize();
    }

    /// Repairs a layout read from a file: see the crate docs for what normal means.
    pub fn normalize(&mut self) {
        let mut seen = Vec::new();
        self.main.normalize(&mut seen);
        for f in &mut self.floating {
            f.tree.normalize(&mut seen);
            if let Some(b) = &mut f.bounds
                && !(b.x.is_finite() && b.y.is_finite() && b.w > 0.0 && b.h > 0.0)
            {
                f.bounds = None;
            }
        }
        self.floating.retain(|f| !f.tree.is_empty());
        self.floating.sort_by_key(|f| f.id);
        self.floating.dedup_by_key(|f| f.id);
        let used = self.floating.iter().map(|f| f.id.0 + 1).max().unwrap_or(0);
        self.next_window = self.next_window.max(used);
    }

    /// Checks that the layout is normalized. For tests and debug assertions.
    pub fn validate(&self) -> Result<(), String> {
        fn walk(node: &Node, parent: Option<Axis>, seen: &mut Vec<PanelId>) -> Result<(), String> {
            match node {
                Node::Stack(s) => {
                    if s.panels.is_empty() {
                        return Err("empty stack".into());
                    }
                    if s.active >= s.panels.len() {
                        return Err(format!("active {} out of {}", s.active, s.panels.len()));
                    }
                    for p in &s.panels {
                        if seen.contains(p) {
                            return Err(format!("panel {} twice", p.0));
                        }
                        seen.push(*p);
                    }
                    Ok(())
                }
                Node::Split { axis, children } => {
                    if children.len() < 2 {
                        return Err(format!("split with {} children", children.len()));
                    }
                    if parent == Some(*axis) {
                        return Err("split inside a split of the same axis".into());
                    }
                    let total: f32 = children.iter().map(|c| c.size).sum();
                    if (total - 1.0).abs() > 1e-3 {
                        return Err(format!("fractions add up to {total}"));
                    }
                    for c in children {
                        if !(c.size.is_finite() && c.size > 0.0) {
                            return Err(format!("bad fraction {}", c.size));
                        }
                        walk(&c.node, Some(*axis), seen)?;
                    }
                    Ok(())
                }
            }
        }
        let mut seen = Vec::new();
        for (id, t) in self.trees() {
            match (&t.root, id) {
                (Some(r), _) => walk(r, None, &mut seen)?,
                (None, TreeId::Floating(w)) => {
                    return Err(format!("panel window {} is empty", w.0));
                }
                (None, TreeId::Main) => {}
            }
        }
        let mut ids: Vec<_> = self.floating.iter().map(|f| f.id).collect();
        ids.dedup();
        if ids.len() != self.floating.len() {
            return Err("panel window id twice".into());
        }
        Ok(())
    }

    /// Puts `node` at `target` without normalizing.
    fn insert_node(&mut self, node: Node, target: DropTarget) -> Result<(), DockError> {
        match target {
            DropTarget::Stack { anchor, zone } => {
                let loc = self.find(anchor).ok_or(DockError::UnknownPanel(anchor))?;
                let root = self
                    .tree_mut(loc.tree)
                    .and_then(|t| t.root.as_mut())
                    .ok_or(DockError::UnknownPanel(anchor))?;
                match zone {
                    Zone::Center | Zone::Tab(_) => {
                        let Some(Node::Stack(stack)) = root.at_mut(&loc.path) else {
                            return Err(DockError::UnknownPanel(anchor));
                        };
                        let mut panels = Vec::new();
                        node.panels(&mut panels);
                        let at = match zone {
                            Zone::Tab(i) => i.min(stack.panels.len()),
                            _ => stack.panels.len(),
                        };
                        let n = panels.len();
                        stack.panels.splice(at..at, panels);
                        if n > 0 {
                            stack.active = at;
                        }
                    }
                    Zone::Edge(edge) => insert_beside(root, &loc.path, node, edge),
                }
                Ok(())
            }
            DropTarget::Root { tree, zone } => {
                let t = self.tree_mut(tree).ok_or(match tree {
                    TreeId::Floating(w) => DockError::UnknownWindow(w),
                    TreeId::Main => DockError::SameWindow,
                })?;
                let Some(root) = t.root.take() else {
                    t.root = Some(node);
                    return Ok(());
                };
                t.root = Some(match zone {
                    Zone::Edge(edge) => {
                        let (new, old) = (
                            Child {
                                size: ROOT_EDGE_SHARE,
                                node,
                            },
                            Child {
                                size: 1.0 - ROOT_EDGE_SHARE,
                                node: root,
                            },
                        );
                        Node::Split {
                            axis: edge.axis(),
                            children: if edge.leading() {
                                vec![new, old]
                            } else {
                                vec![old, new]
                            },
                        }
                    }
                    Zone::Center | Zone::Tab(_) => {
                        // Into the first stack of the window.
                        let mut root = root;
                        let mut path = Vec::new();
                        while let Some(Node::Split { children, .. }) = root.at(&path) {
                            if children.is_empty() {
                                break;
                            }
                            path.push(0);
                        }
                        if let Some(Node::Stack(s)) = root.at_mut(&path) {
                            let mut panels = Vec::new();
                            node.panels(&mut panels);
                            let at = match zone {
                                Zone::Tab(i) => i.min(s.panels.len()),
                                _ => s.panels.len(),
                            };
                            s.panels.splice(at..at, panels);
                            s.active = at;
                        }
                        root
                    }
                });
                Ok(())
            }
        }
    }
}

/// Places `node` on `edge` of the node at `path`: into the parent split when it runs the
/// same way, otherwise in a new split that takes the target's place.
fn insert_beside(root: &mut Node, path: &[usize], node: Node, edge: Edge) {
    if let Some((index, parent_path)) = path.split_last()
        && let Some(Node::Split { axis, children }) = root.at_mut(parent_path)
        && *axis == edge.axis()
        && *index < children.len()
    {
        let half = children[*index].size / 2.0;
        children[*index].size = half;
        let at = if edge.leading() { *index } else { *index + 1 };
        children.insert(at, Child { size: half, node });
        return;
    }
    let Some(target) = root.at_mut(path) else {
        return;
    };
    let old = std::mem::replace(
        target,
        Node::Stack(Stack {
            panels: Vec::new(),
            active: 0,
        }),
    );
    let (a, b) = if edge.leading() {
        (node, old)
    } else {
        (old, node)
    };
    *target = Node::Split {
        axis: edge.axis(),
        children: vec![Child { size: 0.5, node: a }, Child { size: 0.5, node: b }],
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u64) -> PanelId {
        PanelId(n)
    }

    fn center(anchor: u64) -> DropTarget {
        DropTarget::Stack {
            anchor: p(anchor),
            zone: Zone::Center,
        }
    }

    fn edge(anchor: u64, e: Edge) -> DropTarget {
        DropTarget::Stack {
            anchor: p(anchor),
            zone: Zone::Edge(e),
        }
    }

    /// Sidebar | (tabs 2, 3) | inspector, as the app starts.
    fn sample() -> Layout {
        let mut l = Layout::default();
        l.insert(
            p(2),
            DropTarget::Root {
                tree: TreeId::Main,
                zone: Zone::Center,
            },
        )
        .unwrap();
        l.insert(p(3), center(2)).unwrap();
        l.insert(p(1), edge(2, Edge::Left)).unwrap();
        l.insert(p(4), edge(2, Edge::Right)).unwrap();
        l.validate().unwrap();
        l
    }

    #[test]
    fn builds_three_columns() {
        let l = sample();
        let Some(Node::Split { axis, children }) = &l.main.root else {
            panic!("expected a split");
        };
        assert_eq!(*axis, Axis::Row);
        assert_eq!(children.len(), 3);
        assert_eq!(l.main.panels(), vec![p(1), p(2), p(3), p(4)]);
        assert_eq!(l.stack_of(p(3)).unwrap().active_panel(), Some(p(3)));
    }

    #[test]
    fn removed_space_goes_to_the_neighbor() {
        let mut l = sample();
        l.set_sizes(TreeId::Main, &[], &[0.2, 0.6, 0.2]);
        let sizes = |l: &Layout| match &l.main.root {
            Some(Node::Split { children, .. }) => {
                children.iter().map(|c| c.size).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        };
        // The inspector goes: the sidebar keeps its width, the middle takes the rest.
        l.remove(p(4));
        let s = sizes(&l);
        assert!(
            (s[0] - 0.2).abs() < 1e-5 && (s[1] - 0.8).abs() < 1e-5,
            "{s:?}"
        );
        // The first child goes: the next one takes its space.
        l.remove(p(1));
        assert!(matches!(l.main.root, Some(Node::Stack(_))));
        l.validate().unwrap();
    }

    #[test]
    fn insert_twice_fails() {
        let mut l = sample();
        assert_eq!(
            l.insert(p(1), center(2)),
            Err(DockError::AlreadyPlaced(p(1)))
        );
        assert_eq!(
            l.insert(p(9), center(99)),
            Err(DockError::UnknownPanel(p(99)))
        );
        l.validate().unwrap();
    }

    #[test]
    fn edge_drop_across_axis_makes_a_nested_split() {
        let mut l = sample();
        l.move_panel(p(3), edge(2, Edge::Bottom)).unwrap();
        l.validate().unwrap();
        let loc = l.find(p(3)).unwrap();
        assert_eq!(loc.path, vec![1, 1]);
        let Some(Node::Split { axis, .. }) = l.main.node(&[1]) else {
            panic!("expected a column split");
        };
        assert_eq!(*axis, Axis::Column);
    }

    #[test]
    fn removing_collapses() {
        let mut l = sample();
        l.move_panel(p(3), edge(2, Edge::Bottom)).unwrap();
        assert_eq!(l.remove(p(3)), Some(TreeId::Main));
        l.validate().unwrap();
        assert_eq!(l.find(p(2)).unwrap().path, vec![1]);
        for n in [1, 2, 4] {
            l.remove(p(n));
        }
        assert!(l.main.is_empty());
        l.validate().unwrap();
    }

    #[test]
    fn tabs_reorder_within_a_stack() {
        let mut l = sample();
        l.insert(p(5), center(2)).unwrap();
        // [2, 3, 5]: 2 to the slot after 5.
        l.move_panel(
            p(2),
            DropTarget::Stack {
                anchor: p(3),
                zone: Zone::Tab(3),
            },
        )
        .unwrap();
        assert_eq!(l.stack_of(p(2)).unwrap().panels, vec![p(3), p(5), p(2)]);
        l.move_panel(
            p(2),
            DropTarget::Stack {
                anchor: p(2),
                zone: Zone::Tab(0),
            },
        )
        .unwrap();
        assert_eq!(l.stack_of(p(2)).unwrap().panels, vec![p(2), p(3), p(5)]);
        l.validate().unwrap();
    }

    #[test]
    fn splitting_off_own_stack() {
        let mut l = sample();
        l.move_panel(p(3), edge(3, Edge::Right)).unwrap();
        l.validate().unwrap();
        assert_ne!(l.find(p(3)).unwrap().path, l.find(p(2)).unwrap().path);
        // Alone in its stack, an edge drop on itself changes nothing.
        let before = l.clone();
        l.move_panel(p(1), edge(1, Edge::Top)).unwrap();
        assert_eq!(l, before);
    }

    #[test]
    fn detach_and_merge_back() {
        let mut l = sample();
        let w = l.detach(p(3), None).unwrap();
        l.validate().unwrap();
        assert_eq!(l.find(p(3)).unwrap().tree, TreeId::Floating(w));
        // Detaching the only panel of a window keeps the window.
        assert_eq!(l.detach(p(3), None).unwrap(), w);
        l.insert(p(7), center(3)).unwrap();
        l.move_panel(p(7), edge(3, Edge::Bottom)).unwrap();
        l.merge_window(w, edge(1, Edge::Right)).unwrap();
        l.validate().unwrap();
        assert!(l.floating.is_empty());
        assert_eq!(l.find(p(7)).unwrap().tree, TreeId::Main);
        let w = l.detach(p(4), None).unwrap();
        assert_eq!(l.merge_window(w, center(4)), Err(DockError::SameWindow));
        l.merge_window(w, center(2)).unwrap();
        assert_eq!(l.stack_of(p(4)).unwrap().active_panel(), Some(p(4)));
        l.validate().unwrap();
    }

    #[test]
    fn moving_the_last_panel_out_closes_the_window() {
        let mut l = sample();
        let w = l.detach(p(4), None).unwrap();
        l.move_panel(p(4), center(1)).unwrap();
        assert!(l.window(w).is_none());
        l.validate().unwrap();
        // New windows never reuse an id that was saved.
        let w2 = l.detach(p(4), None).unwrap();
        assert_ne!(w, w2);
    }

    #[test]
    fn root_edges_and_empty_main() {
        let mut l = sample();
        let w = l.detach(p(1), None).unwrap();
        for n in [2, 3, 4] {
            l.remove(p(n));
        }
        assert!(l.main.is_empty());
        l.move_panel(
            p(1),
            DropTarget::Root {
                tree: TreeId::Main,
                zone: Zone::Center,
            },
        )
        .unwrap();
        assert!(l.window(w).is_none());
        l.insert(
            p(2),
            DropTarget::Root {
                tree: TreeId::Main,
                zone: Zone::Edge(Edge::Bottom),
            },
        )
        .unwrap();
        l.validate().unwrap();
        assert_eq!(l.main.panels(), vec![p(1), p(2)]);
    }

    #[test]
    fn resize_keeps_neighbors_visible() {
        let mut l = sample();
        l.resize(TreeId::Main, &[], 0, 5.0);
        l.validate().unwrap();
        let Some(Node::Split { children, .. }) = &l.main.root else {
            panic!();
        };
        assert!(children[1].size >= MIN_FRACTION - 1e-6);
        l.set_sizes(TreeId::Main, &[], &[1.0, 2.0, f32::NAN]);
        l.validate().unwrap();
    }

    #[test]
    fn repairs_broken_files() {
        let text = r#"{"main":{"root":{"type":"split","axis":"row","children":[
            {"size":-1.0,"node":{"type":"stack","panels":[1,2],"active":9}},
            {"size":0.5,"node":{"type":"split","axis":"row","children":[
                {"size":1.0,"node":{"type":"stack","panels":[]}},
                {"size":1.0,"node":{"type":"stack","panels":[2,3]}}]}}]}},
            "floating":[{"id":4,"tree":{"root":{"type":"stack","panels":[3]}}},
                        {"id":5,"tree":{}}]}"#;
        let mut l: Layout = serde_json::from_str(text).unwrap();
        l.normalize();
        l.validate().unwrap();
        assert_eq!(l.panels(), vec![p(1), p(2), p(3)]);
        assert!(l.floating.is_empty());
        assert_eq!(
            serde_json::from_value::<Layout>(serde_json::json!({})).unwrap(),
            Layout::default()
        );
    }

    /// A small deterministic generator, so failures reproduce.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    fn random_zone(rng: &mut Rng) -> Zone {
        match rng.below(6) {
            0 => Zone::Center,
            1 => Zone::Tab(rng.below(4)),
            n => Zone::Edge(Edge::ALL[n - 2]),
        }
    }

    fn random_target(l: &Layout, rng: &mut Rng) -> DropTarget {
        let panels = l.panels();
        if panels.is_empty() || rng.below(5) == 0 {
            let mut trees = vec![TreeId::Main];
            trees.extend(l.floating.iter().map(|f| TreeId::Floating(f.id)));
            DropTarget::Root {
                tree: trees[rng.below(trees.len())],
                zone: random_zone(rng),
            }
        } else {
            DropTarget::Stack {
                anchor: panels[rng.below(panels.len())],
                zone: random_zone(rng),
            }
        }
    }

    #[test]
    fn random_operations_keep_the_invariants() {
        for seed in 1..200u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut l = Layout::default();
            let mut next = 0u64;
            let mut expected: Vec<PanelId> = Vec::new();
            for step in 0..120 {
                let panels = l.panels();
                let pick = |rng: &mut Rng| panels.get(rng.below(panels.len())).copied();
                match rng.below(9) {
                    0 | 1 => {
                        next += 1;
                        let target = random_target(&l, &mut rng);
                        if l.insert(PanelId(next), target).is_ok() {
                            expected.push(PanelId(next));
                        }
                    }
                    2 => {
                        if let Some(x) = pick(&mut rng) {
                            l.remove(x);
                            expected.retain(|e| *e != x);
                        }
                    }
                    3 | 4 => {
                        if let Some(x) = pick(&mut rng) {
                            let target = random_target(&l, &mut rng);
                            let _ = l.move_panel(x, target);
                        }
                    }
                    5 => {
                        if let Some(x) = pick(&mut rng) {
                            l.detach(x, None).unwrap();
                        }
                    }
                    6 => {
                        if !l.floating.is_empty() {
                            let w = l.floating[rng.below(l.floating.len())].id;
                            let target = random_target(&l, &mut rng);
                            let _ = l.merge_window(w, target);
                        }
                    }
                    7 => {
                        let path = l
                            .panels()
                            .first()
                            .and_then(|x| l.find(*x))
                            .map(|loc| loc.path)
                            .unwrap_or_default();
                        let cut = rng.below(path.len() + 1);
                        let delta = (rng.below(200) as f32 - 100.0) / 100.0;
                        l.resize(TreeId::Main, &path[..cut], rng.below(3), delta);
                        l.normalize();
                    }
                    _ => {
                        if let Some(x) = pick(&mut rng) {
                            l.activate(x);
                        }
                    }
                }
                if let Err(e) = l.validate() {
                    panic!("seed {seed} step {step}: {e}\n{l:#?}");
                }
                let mut have = l.panels();
                have.sort();
                let mut want = expected.clone();
                want.sort();
                assert_eq!(have, want, "seed {seed} step {step}");
                let back: Layout =
                    serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
                assert_eq!(back, l);
            }
        }
    }
}
