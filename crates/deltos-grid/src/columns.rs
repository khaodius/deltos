//! The merged column list of a cache, and how a page's rows are laid out against it.
//!
//! Tables have one column list for every page. Document stores do not: each page carries
//! the union of the fields its documents had, so a later page can bring columns no earlier
//! page knew. The cache keeps one merged list that only ever grows at the end, by name, so
//! a column keeps its position for as long as the cache lives and the grid's header never
//! reshuffles under the person.
//!
//! Rows are stored in merged order. When a page arrives, its rows are laid out once
//! ([`ColumnMap::align`]): a page whose columns already match the merged list in order
//! is moved in as it is; any other is permuted, which moves 24-byte values and never
//! copies their heap data. A row stored before the list grew is a prefix of the current
//! order, so it is widened with nulls the first time it is read and then read in place
//! from then on. Reads therefore hand out the stored slice directly.

use std::collections::HashMap;
use std::sync::Arc;

use deltos_data::{Column, Row, Value};

/// The merged column list. Column `i` of every stored row is `columns[i]`.
#[derive(Debug, Default)]
pub(crate) struct ColumnSet {
    columns: Vec<Column>,
    /// Merged positions of each name, in the order the duplicates appeared. A statement
    /// like `SELECT a, a` has two columns of one name and both keep a slot.
    by_name: HashMap<String, Vec<usize>>,
    shared: Option<Arc<[Column]>>,
}

impl ColumnSet {
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn clear(&mut self) {
        self.columns.clear();
        self.by_name.clear();
        self.shared = None;
    }

    /// The merged list for the UI. Built once per change and then handed out by `Arc`.
    pub fn shared(&mut self) -> Option<Arc<[Column]>> {
        if self.columns.is_empty() {
            return None;
        }
        Some(
            self.shared
                .get_or_insert_with(|| Arc::from(self.columns.as_slice()))
                .clone(),
        )
    }

    /// Adds the names `columns` brings that are not known yet and returns how a row in
    /// that column order maps onto the merged order.
    pub fn merge(&mut self, columns: &[Column]) -> ColumnMap {
        let mut map = Vec::with_capacity(columns.len());
        let mut seen: HashMap<&str, usize> = HashMap::new();
        let mut grew = false;
        for column in columns {
            let occurrence = seen.entry(column.name.as_str()).or_insert(0);
            let slots = self.by_name.entry(column.name.clone()).or_default();
            let slot = match slots.get(*occurrence) {
                Some(&slot) => slot,
                None => {
                    let slot = self.columns.len();
                    self.columns.push(column.clone());
                    slots.push(slot);
                    grew = true;
                    slot
                }
            };
            *occurrence += 1;
            map.push(slot);
        }
        if grew {
            self.shared = None;
        }
        ColumnMap::new(map)
    }
}

/// Where each value of a page's row goes in the merged order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ColumnMap {
    /// The page's columns are the merged list, or a prefix of it, in the same order.
    Identity,
    Permute(Box<[usize]>),
}

impl ColumnMap {
    fn new(map: Vec<usize>) -> Self {
        if map.iter().enumerate().all(|(i, &slot)| i == slot) {
            ColumnMap::Identity
        } else {
            ColumnMap::Permute(map.into_boxed_slice())
        }
    }

    /// Lays `row` out in merged order at `width` columns. Values move; nothing is cloned.
    pub fn align(&self, row: Row, width: usize) -> Row {
        match self {
            ColumnMap::Identity if row.len() == width => row,
            ColumnMap::Identity => widen(row, width),
            ColumnMap::Permute(map) => {
                let mut out = vec![Value::Null; width];
                for (i, value) in row.into_vec().into_iter().enumerate() {
                    if let Some(&slot) = map.get(i)
                        && let Some(cell) = out.get_mut(slot)
                    {
                        *cell = value;
                    }
                }
                out.into_boxed_slice()
            }
        }
    }
}

/// Pads a row stored before the merged list grew with nulls, or trims one a malformed
/// page made too long. Values move; nothing is cloned.
pub(crate) fn widen(row: Row, width: usize) -> Row {
    if row.len() == width {
        return row;
    }
    let mut out = row.into_vec();
    out.resize_with(width, || Value::Null);
    out.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::ValueKind;

    fn cols(names: &[&str]) -> Vec<Column> {
        names
            .iter()
            .map(|n| Column::new(*n, "text", ValueKind::Text))
            .collect()
    }

    fn row(values: &[i64]) -> Row {
        values.iter().map(|v| Value::Int(*v)).collect()
    }

    #[test]
    fn same_columns_are_identity() {
        let mut set = ColumnSet::default();
        assert_eq!(set.merge(&cols(&["a", "b"])), ColumnMap::Identity);
        assert_eq!(set.merge(&cols(&["a", "b"])), ColumnMap::Identity);
        assert_eq!(set.merge(&cols(&["a"])), ColumnMap::Identity);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn growth_appends_by_name_and_permutes() {
        let mut set = ColumnSet::default();
        set.merge(&cols(&["a", "b"]));
        let map = set.merge(&cols(&["c", "b"]));
        assert_eq!(map, ColumnMap::Permute(vec![2, 1].into_boxed_slice()));
        let names: Vec<&str> = set.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        let aligned = map.align(row(&[30, 20]), set.len());
        assert_eq!(
            aligned.as_ref(),
            [Value::Null, Value::Int(20), Value::Int(30)]
        );
        // A row from before the growth is widened on read.
        let old = ColumnMap::Identity.align(row(&[1, 2]), 2);
        assert_eq!(
            widen(old, 3).as_ref(),
            [Value::Int(1), Value::Int(2), Value::Null]
        );
    }

    #[test]
    fn duplicate_names_keep_separate_slots() {
        let mut set = ColumnSet::default();
        assert_eq!(set.merge(&cols(&["a", "a"])), ColumnMap::Identity);
        assert_eq!(set.len(), 2);
        assert_eq!(set.merge(&cols(&["a"])), ColumnMap::Identity);
        assert_eq!(set.len(), 2);
        assert_eq!(
            set.merge(&cols(&["a", "a", "a"])),
            ColumnMap::Identity,
            "a third copy appends a third slot"
        );
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn shared_list_is_rebuilt_only_on_growth() {
        let mut set = ColumnSet::default();
        assert!(set.shared().is_none());
        set.merge(&cols(&["a"]));
        let first = set.shared().unwrap();
        set.merge(&cols(&["a"]));
        assert!(Arc::ptr_eq(&first, &set.shared().unwrap()));
        set.merge(&cols(&["b"]));
        assert_eq!(set.shared().unwrap().len(), 2);
    }
}
