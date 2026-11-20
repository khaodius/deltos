//! Where everything sits on the plane.
//!
//! Each namespace is a region holding a grid of fixed-size cells, one object per cell, in
//! the order the connection lists them (the order of the objects panel). Regions are packed
//! onto shelves, left to right, wrapping at a width picked so the whole plane comes out
//! roughly as wide as a screen is. Because every cell has the same size, the object at a
//! point and the objects inside a rectangle come from division, not from a search.

use std::collections::HashMap;

use deltos_data::{ObjectEntry, ObjectRef};

use crate::{Anchor, Camera, Focus, Rect, SavedView};

/// Width of an object's card.
pub const NODE_W: f32 = 240.0;
/// Height of a card's title.
pub const HEADER_H: f32 = 30.0;
/// Height of one column row.
pub const ROW_H: f32 = 20.0;
/// Columns a card lists before it says how many more there are.
pub const MAX_ROWS: usize = 14;
/// Height of the "N more" line.
pub const FOOTER_H: f32 = 20.0;
/// Space under the last row.
const BOTTOM_PAD: f32 = 6.0;
/// Rows of the placeholder a card shows until its object is described.
const PLACEHOLDER_ROWS: usize = 3;

const GAP_X: f32 = 110.0;
const GAP_Y: f32 = 70.0;
/// One cell: the tallest card and the space around it.
pub const CELL_W: f32 = NODE_W + GAP_X;
pub const CELL_H: f32 = HEADER_H + MAX_ROWS as f32 * ROW_H + FOOTER_H + BOTTOM_PAD + GAP_Y;

const REGION_PAD: f32 = 56.0;
const REGION_TITLE: f32 = 64.0;
const REGION_GAP: f32 = 220.0;

/// The height of a card with `columns` columns, or of the placeholder for an object not
/// yet described.
pub fn node_height(columns: Option<usize>) -> f32 {
    match columns {
        None => HEADER_H + PLACEHOLDER_ROWS as f32 * ROW_H + BOTTOM_PAD,
        Some(n) => {
            let shown = n.clamp(1, MAX_ROWS);
            let footer = if n > MAX_ROWS { FOOTER_H } else { 0.0 };
            HEADER_H + shown as f32 * ROW_H + footer + BOTTOM_PAD
        }
    }
}

/// One namespace as the layout reads it: its path, and its objects once they are listed.
#[derive(Debug, Clone, Copy)]
pub struct Listing<'a> {
    pub path: &'a [String],
    pub objects: Option<&'a [ObjectEntry]>,
}

/// A namespace's area.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    pub path: Vec<String>,
    pub label: String,
    pub rect: Rect,
    /// The top left corner of its first cell.
    pub grid: (f32, f32),
    pub columns: usize,
    /// Its objects: `slots[first..first + len]`.
    pub first: usize,
    pub len: usize,
    /// Whether its objects have been listed.
    pub listed: bool,
}

impl Region {
    fn rows(&self) -> usize {
        self.len.div_ceil(self.columns.max(1))
    }
}

/// One object's place.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub object: ObjectRef,
    pub rows: Option<u64>,
    pub region: usize,
    /// The card's top left corner.
    pub x: f32,
    pub y: f32,
}

impl Slot {
    /// The card's area for a card of `height`.
    pub fn rect(&self, height: f32) -> Rect {
        Rect::new(self.x, self.y, NODE_W, height)
    }

    /// The whole cell the card may grow into.
    pub fn cell(&self) -> Rect {
        self.rect(CELL_H - GAP_Y)
    }
}

/// What is under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Nothing,
    Region(usize),
    Node(usize, NodeHit),
}

/// Which part of a card is under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeHit {
    Header,
    /// The column at this index of the object's columns.
    Column(usize),
    /// The "N more" line.
    More,
}

#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub regions: Vec<Region>,
    pub slots: Vec<Slot>,
    pub bounds: Rect,
    by_object: HashMap<ObjectRef, usize>,
    /// Lowercased namespace and name, for finding an object by the name a reference uses.
    by_name: HashMap<(Vec<String>, String), usize>,
}

impl Layout {
    /// Lays out the namespaces given. Only objects with rows (tables, views, collections)
    /// get a card: functions and sequences have nothing to relate.
    pub fn build<'a>(listings: impl IntoIterator<Item = Listing<'a>>) -> Layout {
        let mut regions = Vec::new();
        let mut slots: Vec<Slot> = Vec::new();
        for listing in listings {
            let first = slots.len();
            let region = regions.len();
            for entry in listing.objects.unwrap_or_default() {
                if entry.object.kind.has_rows() {
                    slots.push(Slot {
                        object: entry.object.clone(),
                        rows: entry.estimated_rows,
                        region,
                        x: 0.0,
                        y: 0.0,
                    });
                }
            }
            let len = slots.len() - first;
            // Roughly square in screen terms, a little wider than tall.
            let columns = if len == 0 {
                1
            } else {
                ((len as f32 * CELL_H / CELL_W * 1.6).sqrt().ceil() as usize).clamp(1, len)
            };
            let rows = len.div_ceil(columns).max(1);
            let w = columns as f32 * CELL_W - GAP_X + REGION_PAD * 2.0;
            let h = REGION_TITLE + rows as f32 * CELL_H - GAP_Y + REGION_PAD * 2.0;
            regions.push(Region {
                path: listing.path.to_vec(),
                label: listing.path.join("."),
                rect: Rect::new(0.0, 0.0, w, h),
                grid: (0.0, 0.0),
                columns,
                first,
                len,
                listed: listing.objects.is_some(),
            });
        }
        // Shelves: as wide as the widest region, or as the square root of the total area
        // scaled to a wide screen, whichever is more.
        let area: f32 = regions.iter().map(|r| r.rect.w * r.rect.h).sum();
        let widest = regions.iter().map(|r| r.rect.w).fold(0.0, f32::max);
        let shelf_w = widest.max(area.sqrt() * 1.5);
        let (mut x, mut y, mut tallest) = (0.0f32, 0.0f32, 0.0f32);
        let mut bounds: Option<Rect> = None;
        for r in &mut regions {
            if x > 0.0 && x + r.rect.w > shelf_w {
                x = 0.0;
                y += tallest + REGION_GAP;
                tallest = 0.0;
            }
            r.rect.x = x;
            r.rect.y = y;
            r.grid = (x + REGION_PAD, y + REGION_PAD + REGION_TITLE);
            x += r.rect.w + REGION_GAP;
            tallest = tallest.max(r.rect.h);
            bounds = Some(bounds.map_or(r.rect, |b| b.union(&r.rect)));
            for (i, s) in slots[r.first..r.first + r.len].iter_mut().enumerate() {
                s.x = r.grid.0 + (i % r.columns) as f32 * CELL_W;
                s.y = r.grid.1 + (i / r.columns) as f32 * CELL_H;
            }
        }
        let mut by_object = HashMap::with_capacity(slots.len());
        let mut by_name = HashMap::with_capacity(slots.len());
        for (i, s) in slots.iter().enumerate() {
            by_object.insert(s.object.clone(), i);
            by_name
                .entry((lower(&s.object.namespace), s.object.name.to_lowercase()))
                .or_insert(i);
        }
        Layout {
            regions,
            slots,
            bounds: bounds.unwrap_or_default(),
            by_object,
            by_name,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn slot(&self, object: &ObjectRef) -> Option<usize> {
        self.by_object.get(object).copied()
    }

    /// The object called `name` in `namespace`, ignoring case and the object's kind.
    pub fn find(&self, namespace: &[String], name: &str) -> Option<usize> {
        self.by_name
            .get(&(lower(namespace), name.to_lowercase()))
            .copied()
    }

    pub fn region(&self, path: &[String]) -> Option<usize> {
        self.regions.iter().position(|r| r.path == path)
    }

    /// The region and, inside it, the card under `p`. Card heights come from `height`,
    /// by slot index, and the part of the card from `columns`, the column count of the
    /// slot's object when known.
    pub fn hit(
        &self,
        p: (f32, f32),
        height: impl Fn(usize) -> f32,
        columns: impl Fn(usize) -> Option<usize>,
    ) -> Hit {
        let Some(ri) = self.regions.iter().position(|r| r.rect.contains(p)) else {
            return Hit::Nothing;
        };
        let r = &self.regions[ri];
        let (gx, gy) = (p.0 - r.grid.0, p.1 - r.grid.1);
        if gx < 0.0 || gy < 0.0 {
            return Hit::Region(ri);
        }
        let (col, row) = ((gx / CELL_W) as usize, (gy / CELL_H) as usize);
        if col >= r.columns {
            return Hit::Region(ri);
        }
        let i = row * r.columns + col;
        if i >= r.len {
            return Hit::Region(ri);
        }
        let slot = r.first + i;
        let s = &self.slots[slot];
        if !s.rect(height(slot)).contains(p) {
            return Hit::Region(ri);
        }
        let dy = p.1 - s.y - HEADER_H;
        if dy < 0.0 {
            return Hit::Node(slot, NodeHit::Header);
        }
        let row = (dy / ROW_H) as usize;
        match columns(slot) {
            Some(n) if row < n.min(MAX_ROWS) => Hit::Node(slot, NodeHit::Column(row)),
            Some(n) if n > MAX_ROWS && row == MAX_ROWS => Hit::Node(slot, NodeHit::More),
            _ => Hit::Node(slot, NodeHit::Header),
        }
    }

    /// The slots whose cells meet `view`, region by region and row by row; only the cells
    /// in view are visited.
    pub fn visible(&self, view: &Rect) -> Vec<usize> {
        let mut out = Vec::new();
        for r in &self.regions {
            if r.len == 0 || !r.rect.intersects(view) {
                continue;
            }
            let cell_range = |lo: f32, hi: f32, origin: f32, size: f32, n: usize| {
                let a = ((lo - origin) / size).floor().max(0.0) as usize;
                let b = (((hi - origin) / size).ceil().max(0.0) as usize).min(n);
                a..b
            };
            let cols = cell_range(view.x, view.right(), r.grid.0, CELL_W, r.columns);
            let rows = cell_range(view.y, view.bottom(), r.grid.1, CELL_H, r.rows());
            for row in rows {
                for col in cols.clone() {
                    let i = row * r.columns + col;
                    if i < r.len {
                        out.push(r.first + i);
                    }
                }
            }
        }
        out
    }

    /// The y of the middle of column row `index` of the card at `slot`, or of its title
    /// when the column is not listed on the card.
    pub fn column_y(&self, slot: usize, index: Option<usize>) -> f32 {
        let s = &self.slots[slot];
        match index {
            Some(i) if i < MAX_ROWS => s.y + HEADER_H + i as f32 * ROW_H + ROW_H * 0.5,
            _ => s.y + HEADER_H * 0.5,
        }
    }

    /// Where the camera goes to show `focus` in a viewport of `viewport` pixels.
    /// `column` is the index of the focused column on its card, when known. `None` when
    /// the focus is not on the plane (yet).
    pub fn camera_for(
        &self,
        focus: &Focus,
        viewport: (f32, f32),
        column: Option<usize>,
    ) -> Option<Camera> {
        match focus {
            Focus::Overview => {
                (!self.is_empty()).then(|| Camera::fit(&self.bounds, viewport, 40.0, 1.0))
            }
            Focus::Namespace { path } => {
                let r = &self.regions[self.region(path)?];
                Some(Camera::fit(&r.rect, viewport, 40.0, 1.0))
            }
            Focus::Object { object } => {
                let s = &self.slots[self.slot(object)?];
                // The card, with its neighbors' edges peeking in.
                let around = s.cell().inflate(GAP_X * 0.6);
                Some(Camera::fit(&around, viewport, 24.0, 1.1))
            }
            Focus::Column { object, .. } => {
                let slot = self.slot(object)?;
                let s = &self.slots[slot];
                let around = s.cell().inflate(GAP_X * 0.6);
                let fit = Camera::fit(&around, viewport, 24.0, 1.25);
                // Close enough to read the types, centered on the column.
                let zoom = fit.zoom.max(0.9);
                let y = if zoom > fit.zoom {
                    self.column_y(slot, column)
                } else {
                    fit.y
                };
                Some(Camera::new(s.x + NODE_W * 0.5, y, zoom))
            }
        }
    }

    /// `camera`, pinned to what is at its middle.
    pub fn save(&self, camera: &Camera, selected: Option<Focus>) -> SavedView {
        let center = (camera.x, camera.y);
        let near = self
            .regions
            .iter()
            .position(|r| r.rect.contains(center))
            .map(|ri| {
                let r = &self.regions[ri];
                // The cell under the middle, or the region when the middle is in a gap
                // or the margin.
                let (gx, gy) = (center.0 - r.grid.0, center.1 - r.grid.1);
                let cell = (gx >= 0.0 && gy >= 0.0)
                    .then(|| {
                        let (c, row) = ((gx / CELL_W) as usize, (gy / CELL_H) as usize);
                        (c < r.columns).then(|| row * r.columns + c)
                    })
                    .flatten()
                    .filter(|i| *i < r.len);
                match cell {
                    Some(i) => {
                        let s = &self.slots[r.first + i];
                        (
                            Anchor::Object {
                                object: s.object.clone(),
                            },
                            (s.x, s.y),
                        )
                    }
                    None => (
                        Anchor::Namespace {
                            path: r.path.clone(),
                        },
                        (r.rect.x, r.rect.y),
                    ),
                }
            });
        let (anchor, (ax, ay)) = near.unwrap_or((Anchor::Origin, (0.0, 0.0)));
        SavedView {
            anchor,
            dx: camera.x - ax,
            dy: camera.y - ay,
            zoom: camera.zoom,
            selected,
        }
    }

    /// The camera a saved view describes, or `None` while its anchor is not on the plane.
    pub fn restore(&self, view: &SavedView) -> Option<Camera> {
        let (ax, ay) = match &view.anchor {
            Anchor::Origin => (0.0, 0.0),
            Anchor::Namespace { path } => {
                let r = &self.regions[self.region(path)?];
                (r.rect.x, r.rect.y)
            }
            Anchor::Object { object } => {
                let s = &self.slots[self.slot(object)?];
                (s.x, s.y)
            }
        };
        let (dx, dy) = (finite(view.dx), finite(view.dy));
        Some(Camera::new(ax + dx, ay + dy, view.zoom))
    }
}

fn lower(path: &[String]) -> Vec<String> {
    path.iter().map(|p| p.to_lowercase()).collect()
}

fn finite(v: f32) -> f32 {
    if v.is_finite() { v } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::ObjectKind;

    pub(crate) fn entry(ns: &[&str], name: &str, kind: ObjectKind) -> ObjectEntry {
        ObjectEntry {
            object: ObjectRef {
                namespace: ns.iter().map(|s| s.to_string()).collect(),
                name: name.into(),
                kind,
            },
            estimated_rows: None,
            size_bytes: None,
            comment: None,
        }
    }

    fn sample() -> (Vec<Vec<String>>, Vec<Vec<ObjectEntry>>) {
        let public: Vec<ObjectEntry> = (0..30)
            .map(|i| entry(&["app", "public"], &format!("t{i:02}"), ObjectKind::Table))
            .chain([entry(&["app", "public"], "next_id", ObjectKind::Sequence)])
            .collect();
        let audit = vec![
            entry(&["app", "audit"], "log", ObjectKind::Table),
            entry(&["app", "audit"], "recent", ObjectKind::View),
        ];
        (
            vec![
                vec!["app".into(), "public".into()],
                vec!["app".into(), "audit".into()],
                vec!["app".into(), "empty".into()],
            ],
            vec![public, audit],
        )
    }

    fn build(paths: &[Vec<String>], lists: &[Vec<ObjectEntry>]) -> Layout {
        Layout::build(paths.iter().enumerate().map(|(i, p)| Listing {
            path: p,
            objects: lists.get(i).map(Vec::as_slice),
        }))
    }

    #[test]
    fn cards_get_cells_and_regions_do_not_overlap() {
        let (paths, lists) = sample();
        let l = build(&paths, &lists);
        assert_eq!(l.regions.len(), 3);
        // The sequence has no card.
        assert_eq!(l.slots.len(), 32);
        assert!(l.regions[2].len == 0 && !l.regions[2].listed);
        for (i, a) in l.regions.iter().enumerate() {
            for b in &l.regions[i + 1..] {
                assert!(!a.rect.intersects(&b.rect));
            }
            for s in &l.slots[a.first..a.first + a.len] {
                let cell = s.cell();
                assert!(a.rect.contains((cell.x, cell.y)));
                assert!(a.rect.contains((cell.right() - 0.1, cell.bottom() - 0.1)));
            }
        }
        for (i, a) in l.slots.iter().enumerate() {
            for b in &l.slots[i + 1..] {
                assert!(!a.cell().intersects(&b.cell()));
            }
        }
        // Found by exact reference and by name, whatever the case.
        let t07 = l.slots.iter().position(|s| s.object.name == "t07").unwrap();
        assert_eq!(l.slot(&l.slots[t07].object), Some(t07));
        assert_eq!(l.find(&["APP".into(), "Public".into()], "T07"), Some(t07));
    }

    #[test]
    fn hit_testing_matches_the_cells() {
        let (paths, lists) = sample();
        let l = build(&paths, &lists);
        let heights = |_: usize| node_height(Some(20));
        let cols = |_: usize| Some(20);
        for (i, s) in l.slots.iter().enumerate() {
            assert_eq!(
                l.hit((s.x + 5.0, s.y + 5.0), heights, cols),
                Hit::Node(i, NodeHit::Header)
            );
            assert_eq!(
                l.hit((s.x + 5.0, s.y + HEADER_H + ROW_H * 2.5), heights, cols),
                Hit::Node(i, NodeHit::Column(2))
            );
            assert_eq!(
                l.hit(
                    (s.x + 5.0, s.y + HEADER_H + ROW_H * (MAX_ROWS as f32 + 0.5)),
                    heights,
                    cols
                ),
                Hit::Node(i, NodeHit::More)
            );
            // In the gap to the right of the card.
            assert!(matches!(
                l.hit((s.x + NODE_W + 2.0, s.y + 5.0), heights, cols),
                Hit::Region(_)
            ));
        }
        // A short card: below it is the region.
        let s = &l.slots[0];
        assert_eq!(
            l.hit(
                (s.x + 5.0, s.y + node_height(Some(2)) + 2.0),
                |_| node_height(Some(2)),
                |_| Some(2)
            ),
            Hit::Region(0)
        );
        assert_eq!(l.hit((-50.0, -50.0), heights, cols), Hit::Nothing);
    }

    #[test]
    fn visible_lists_exactly_the_cells_in_view() {
        let (paths, lists) = sample();
        let l = build(&paths, &lists);
        let views = [
            l.bounds,
            Rect::new(l.slots[5].x + 10.0, l.slots[5].y + 10.0, 50.0, 50.0),
            Rect::new(l.bounds.x - 1e5, l.bounds.y - 1e5, 10.0, 10.0),
            Rect::new(
                l.regions[0].rect.x + 300.0,
                l.regions[0].rect.y + 500.0,
                900.0,
                700.0,
            ),
        ];
        for v in views {
            let got = l.visible(&v);
            // Every card that can show in the view is listed, and nothing far from it.
            for (i, s) in l.slots.iter().enumerate() {
                if s.cell().intersects(&v) {
                    assert!(got.contains(&i), "view {v:?} misses {i}");
                }
            }
            for i in &got {
                let s = &l.slots[*i];
                assert!(Rect::new(s.x, s.y, CELL_W, CELL_H).intersects(&v));
            }
        }
    }

    #[test]
    fn saved_views_follow_their_anchor_when_the_layout_moves() {
        let (paths, mut lists) = sample();
        let l = build(&paths, &lists);
        let t20 = l.slots.iter().position(|s| s.object.name == "t20").unwrap();
        let s = &l.slots[t20];
        let camera = Camera::new(s.x + 33.0, s.y + 12.0, 0.8);
        let saved = l.save(&camera, None);
        assert!(matches!(&saved.anchor, Anchor::Object { object } if object.name == "t20"));
        // Tables added in front of it move it to another cell.
        for i in 0..9 {
            lists[0].insert(
                0,
                entry(&["app", "public"], &format!("a{i}"), ObjectKind::Table),
            );
        }
        let moved = build(&paths, &lists);
        let s2 = &moved.slots[moved.slot(&s.object).unwrap()];
        assert!((s2.x, s2.y) != (s.x, s.y));
        let back = moved.restore(&saved).unwrap();
        assert!(back.same_as(&Camera::new(s2.x + 33.0, s2.y + 12.0, 0.8)));
        // Gone: nothing to restore to.
        lists[0].retain(|e| e.object.name != "t20");
        assert!(build(&paths, &lists).restore(&saved).is_none());
    }

    #[test]
    fn a_view_between_cards_pins_to_its_namespace() {
        let (paths, lists) = sample();
        let l = build(&paths, &lists);
        let r = &l.regions[1];
        let camera = Camera::new(r.rect.x + 4.0, r.rect.y + 4.0, 0.3);
        let saved = l.save(&camera, None);
        assert!(matches!(&saved.anchor, Anchor::Namespace { path } if *path == r.path));
        assert!(l.restore(&saved).unwrap().same_as(&camera));
        let off = l.save(&Camera::new(-1e6, -1e6, 0.1), None);
        assert_eq!(off.anchor, Anchor::Origin);
        assert!(
            l.restore(&off)
                .unwrap()
                .same_as(&Camera::new(-1e6, -1e6, 0.1))
        );
    }

    #[test]
    fn focus_cameras_land_on_their_target() {
        let (paths, lists) = sample();
        let l = build(&paths, &lists);
        let view = (1000.0, 700.0);
        let all = l.camera_for(&Focus::Overview, view, None).unwrap();
        let v = all.visible(view);
        assert!(v.x <= l.bounds.x && v.right() >= l.bounds.right());
        let ns = l
            .camera_for(
                &Focus::Namespace {
                    path: paths[1].clone(),
                },
                view,
                None,
            )
            .unwrap();
        assert!(ns.visible(view).contains(l.regions[1].rect.center()));
        let object = l.slots[3].object.clone();
        let c = l
            .camera_for(
                &Focus::Object {
                    object: object.clone(),
                },
                view,
                None,
            )
            .unwrap();
        assert!(c.visible(view).contains(l.slots[3].cell().center()));
        let c = l
            .camera_for(
                &Focus::Column {
                    object,
                    column: "x".into(),
                },
                view,
                Some(9),
            )
            .unwrap();
        assert!(c.zoom >= 0.9);
        assert!(
            c.visible(view)
                .contains((l.slots[3].x + 10.0, l.column_y(3, Some(9))))
        );
        let missing = ObjectRef {
            namespace: vec!["nope".into()],
            name: "x".into(),
            kind: ObjectKind::Table,
        };
        assert!(
            l.camera_for(&Focus::Object { object: missing }, view, None)
                .is_none()
        );
        assert!(
            Layout::default()
                .camera_for(&Focus::Overview, view, None)
                .is_none()
        );
    }

    #[test]
    fn many_tables_lay_out_quickly_and_cull_to_the_view() {
        let list: Vec<ObjectEntry> = (0..20_000)
            .map(|i| entry(&["big"], &format!("t{i}"), ObjectKind::Table))
            .collect();
        let path = vec!["big".to_string()];
        let l = Layout::build([Listing {
            path: &path,
            objects: Some(&list),
        }]);
        assert_eq!(l.slots.len(), 20_000);
        let c = Camera::new(l.bounds.center().0, l.bounds.center().1, 0.5);
        let shown = l.visible(&c.visible((1600.0, 1000.0)));
        assert!(!shown.is_empty() && shown.len() < 100, "{}", shown.len());
    }

    #[test]
    fn card_heights() {
        assert_eq!(node_height(Some(0)), node_height(Some(1)));
        assert!(node_height(Some(MAX_ROWS + 1)) > node_height(Some(MAX_ROWS)));
        assert_eq!(
            node_height(Some(MAX_ROWS + 1)),
            node_height(Some(MAX_ROWS + 500))
        );
        assert!(node_height(Some(5000)) <= CELL_H);
        assert!(node_height(None) > HEADER_H);
    }
}
