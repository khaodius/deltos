//! The model behind the graph view: where every namespace and object of a connection sits
//! on one plane, the camera that looks at it, and the relations drawn between objects.
//!
//! Nothing here knows about GPUI or a particular database. The app feeds it what the
//! [`deltos_data::Connection`] contract already returns (namespaces, object listings and
//! [`deltos_data::ObjectInfo`]), so any driver that implements the contract gets a graph.
//! How objects relate is the one part that differs between kinds of database, and that
//! sits behind [`RelationSource`]: declared foreign keys for SQL, references read from
//! field names for document stores, and whatever a future driver needs.
//!
//! The layout puts every object in a fixed-size cell of its namespace's grid. Positions
//! therefore never depend on how many columns an object turns out to have, so describing
//! objects as they come into view never moves anything, and finding the object under the
//! pointer or the objects in view is arithmetic on cell indexes rather than a search.

mod camera;
mod layout;
mod relations;

pub use camera::{Camera, Detail, MAX_ZOOM, MIN_ZOOM};
pub use layout::{
    CELL_H, CELL_W, FOOTER_H, HEADER_H, Hit, Layout, Listing, MAX_ROWS, NODE_W, NodeHit, ROW_H,
    Region, Slot, node_height,
};
pub use relations::{
    Catalog, DocumentReferences, ForeignKeys, Relation, RelationSource, Relations, for_kind,
};

use deltos_data::ObjectRef;
use serde::{Deserialize, Serialize};

/// A rectangle on the plane, in world units.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Rect::new(
            x,
            y,
            self.right().max(other.right()) - x,
            self.bottom().max(other.bottom()) - y,
        )
    }

    /// Grown by `by` on every side.
    pub fn inflate(&self, by: f32) -> Rect {
        Rect::new(
            self.x - by,
            self.y - by,
            self.w + by * 2.0,
            self.h + by * 2.0,
        )
    }
}

/// What the graph was asked to show: the place in the regular view the person came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Focus {
    /// The whole connection.
    Overview,
    /// A database, schema or keyspace.
    Namespace { path: Vec<String> },
    /// A table, view or collection.
    Object { object: ObjectRef },
    /// One column of a table, or one field of a collection.
    Column { object: ObjectRef, column: String },
}

impl Focus {
    pub fn object(&self) -> Option<&ObjectRef> {
        match self {
            Focus::Object { object } | Focus::Column { object, .. } => Some(object),
            _ => None,
        }
    }

    /// The namespace the focus is in, or is.
    pub fn namespace(&self) -> Option<&[String]> {
        match self {
            Focus::Namespace { path } => Some(path),
            Focus::Object { object } | Focus::Column { object, .. } => Some(&object.namespace),
            Focus::Overview => None,
        }
    }

    pub fn column(&self) -> Option<&str> {
        match self {
            Focus::Column { column, .. } => Some(column),
            _ => None,
        }
    }
}

/// What a saved view is pinned to. A view is kept relative to the object at its center
/// rather than as a raw position, so it lands in the same place after tables are added,
/// dropped or renamed elsewhere and the layout around it moves.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Anchor {
    /// The plane's origin, for a view with nothing near its center.
    Origin,
    Namespace {
        path: Vec<String>,
    },
    Object {
        object: ObjectRef,
    },
}

/// A camera position that survives restarts: the anchor, the camera's offset from the
/// anchor's corner in world units, and the zoom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedView {
    pub anchor: Anchor,
    pub dx: f32,
    pub dy: f32,
    pub zoom: f32,
    /// The object or column that was selected.
    #[serde(default)]
    pub selected: Option<Focus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rects() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert!(a.intersects(&b));
        assert!(!a.intersects(&Rect::new(10.0, 0.0, 5.0, 5.0)));
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 15.0, 15.0));
        assert!(a.contains((0.0, 9.9)));
        assert!(!a.contains((10.0, 0.0)));
        assert_eq!(a.inflate(1.0), Rect::new(-1.0, -1.0, 12.0, 12.0));
    }

    #[test]
    fn saved_views_round_trip_through_toml() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct File {
            views: Vec<SavedView>,
        }
        let object = ObjectRef {
            namespace: vec!["app".into(), "public".into()],
            name: "orders".into(),
            kind: deltos_data::ObjectKind::Table,
        };
        let file = File {
            views: vec![
                SavedView {
                    anchor: Anchor::Object {
                        object: object.clone(),
                    },
                    dx: 12.5,
                    dy: -4.0,
                    zoom: 0.8,
                    selected: Some(Focus::Column {
                        object,
                        column: "user_id".into(),
                    }),
                },
                SavedView {
                    anchor: Anchor::Origin,
                    dx: 0.0,
                    dy: 0.0,
                    zoom: 0.1,
                    selected: None,
                },
            ],
        };
        let text = toml::to_string(&file).unwrap();
        assert_eq!(toml::from_str::<File>(&text).unwrap(), file);
    }
}
