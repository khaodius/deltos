//! What each table showed last in this session: its sort and its filter bar, so a table
//! closed and opened again comes back as it was left. Kept in memory only, by connection
//! and object.

use std::collections::HashMap;

use deltos_data::{FilterOp, ObjectRef, Sort};
use gpui::{App, Global};

/// Past this many remembered tables the memory starts over; a session rarely gets near.
const MAX_TABLES: usize = 1024;

/// One condition of the filter bar as typed.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedCondition {
    pub column: String,
    pub op: FilterOp,
    pub value: String,
    /// The upper end of a `BETWEEN`.
    pub high: String,
}

impl SavedCondition {
    pub fn new(column: impl Into<String>, op: FilterOp, value: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            op,
            value: value.into(),
            high: String::new(),
        }
    }
}

/// A table view's sort and filter bar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SavedView {
    pub sort: Vec<Sort>,
    pub conditions: Vec<SavedCondition>,
    pub match_any: bool,
    /// The raw condition is what filters, instead of the conditions.
    pub raw_mode: bool,
    pub raw: String,
    /// Whether the filter bar was showing.
    pub open: bool,
}

type Key = (String, ObjectRef);

#[derive(Default)]
struct TableMemory {
    views: HashMap<Key, SavedView>,
}

impl Global for TableMemory {}

fn memory(cx: &mut App) -> &mut TableMemory {
    cx.default_global::<TableMemory>()
}

/// Remembers what the table shows now.
pub fn remember(cx: &mut App, connection: &str, object: &ObjectRef, view: SavedView) {
    let views = &mut memory(cx).views;
    if views.len() >= MAX_TABLES {
        views.clear();
    }
    views.insert((connection.to_owned(), object.clone()), view);
}

/// What the table showed last, if it was open earlier in the session.
pub fn recall(cx: &App, connection: &str, object: &ObjectRef) -> Option<SavedView> {
    cx.try_global::<TableMemory>()?
        .views
        .get(&(connection.to_owned(), object.clone()))
        .cloned()
}
