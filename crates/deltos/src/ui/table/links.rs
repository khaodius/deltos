//! Foreign keys from a table. Which grid columns form each key is worked out when the
//! columns or the structure load, not per cell, and handed to the grid for its arrows.
//! Following a key opens the referenced row in the inspector; the cell menu also opens
//! the referenced table, or the rows of other tables that refer to this row, filtered.

use std::sync::Arc;

use deltos_data::{Column, FilterOp, ForeignKey, ObjectRef, Value};
use deltos_settings::PanelKind;
use gpui::{App, Context, Entity};

use super::memory::{self, SavedCondition, SavedView};
use super::{Mode, TablePanel};
use crate::state::AppState;
use crate::ui::MenuItem;
use crate::ui::grid::{GridView, Links};
use crate::ui::references::{self, Hop, positions, shortcut_label, values_at};

/// Most keys of other tables the cell menu lists.
const MAX_INCOMING: usize = 8;

/// The table's own foreign keys, with where their columns are in the grid.
#[derive(Default)]
pub(super) struct Resolved {
    /// The grid columns these were worked out for, and whether the structure was known.
    stamp: Option<(Arc<[Column]>, bool)>,
    keys: Vec<(ForeignKey, Vec<usize>)>,
}

/// Opens the table panel of `object`, showing only the rows where each column equals its
/// value.
pub fn open_filtered(
    state: &Entity<AppState>,
    connection: &str,
    object: &ObjectRef,
    filters: &[(String, Value)],
    cx: &mut App,
) {
    let conditions = filters
        .iter()
        .map(|(column, value)| SavedCondition::new(column.clone(), FilterOp::Eq, value.to_string()))
        .collect();
    memory::request(cx, connection, object, conditions);
    let kind = PanelKind::Table {
        connection: connection.to_owned(),
        object: object.clone(),
    };
    state.update(cx, |s, cx| s.open(kind, true, cx));
}

/// Pairs a key's columns on the far side with the values on this one.
fn pairs(columns: &[String], values: Vec<Value>) -> Vec<(String, Value)> {
    columns.iter().cloned().zip(values).collect()
}

impl TablePanel {
    /// Works the keys out again when the grid's columns or the structure changed.
    pub(super) fn sync_links(&mut self, cx: &mut Context<Self>) {
        // The cache's columns, which the grid takes up only as it draws.
        let grid = self.grid.read(cx);
        let columns = grid
            .cache()
            .and_then(|c| c.columns())
            .unwrap_or_else(|| grid.columns().clone());
        let known = matches!(self.info, Some(Ok(_)));
        if self
            .links
            .stamp
            .as_ref()
            .is_some_and(|(c, k)| Arc::ptr_eq(c, &columns) && *k == known)
        {
            return;
        }
        let keys: Vec<(ForeignKey, Vec<usize>)> = match &self.info {
            Some(Ok(info)) => info
                .foreign_keys
                .iter()
                .filter(|k| k.columns.len() == k.target_columns.len())
                .filter_map(|k| Some((k.clone(), positions(&columns, &k.columns)?)))
                .collect(),
            _ => Vec::new(),
        };
        let links = Links::new(keys.iter().map(|(_, at)| at.clone()).collect());
        self.links = Resolved {
            stamp: Some((columns, known)),
            keys,
        };
        self.grid.update(cx, |g, cx| g.set_links(links, cx));
    }

    /// The keys `col` is part of that have a value in every column of the row.
    fn hops_at(&self, row: &[Value], col: usize) -> Vec<Hop> {
        self.links
            .keys
            .iter()
            .filter(|(_, at)| at.contains(&col))
            .filter_map(|(key, at)| {
                Some(Hop {
                    connection: self.connection.clone(),
                    key: key.clone(),
                    values: values_at(row, at)?,
                })
            })
            .collect()
    }

    /// Shows the row `hop` leads to from `row` in the inspector.
    fn follow_hop(&mut self, row: u64, hop: Hop, cx: &mut Context<Self>) {
        // The inspector shows the row the key starts from, then where it leads.
        self.inspect_row(row, cx);
        let Some(from) = self.state.read(cx).inspected.clone() else {
            return;
        };
        references::follow(&self.state, &from, hop, cx);
    }

    /// The arrow of a cell was clicked.
    pub(super) fn follow_link(&mut self, row: u64, col: usize, cx: &mut Context<Self>) {
        let Some(values) = self.grid.read(cx).row_values(row) else {
            return;
        };
        if let Some(hop) = self.hops_at(&values, col).into_iter().next() {
            self.follow_hop(row, hop, cx);
        }
    }

    /// Ctrl+Enter: follows the key of the selected cell.
    pub(super) fn follow_selected(&mut self, cx: &mut Context<Self>) {
        let Some((row, col)) = self.grid.read(cx).selection.map(|s| s.head) else {
            return;
        };
        let values = self.grid.read(cx).row_values(row).unwrap_or_default();
        match self.hops_at(&values, col).into_iter().next() {
            Some(hop) => self.follow_hop(row, hop, cx),
            None if self.links.keys.is_empty() => {
                self.notify_user("This table has no foreign keys.", false, cx)
            }
            None => self.notify_user("This cell does not refer to another row.", false, cx),
        }
    }

    /// Puts filters asked for this table in place, keeping the sort. True when there
    /// were some; the caller reloads.
    pub(super) fn take_requested(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(conditions) = memory::take_request(cx, &self.connection, &self.object) else {
            return false;
        };
        let view = SavedView {
            sort: self.sort.clone(),
            conditions,
            open: true,
            ..SavedView::default()
        };
        self.apply_view(view, cx);
        self.mode = Mode::Data;
        true
    }

    /// The cell menu's foreign key items: the row a key of the cell leads to, its table
    /// filtered to it, and the rows of other tables that refer to this row.
    pub(super) fn link_items(
        &self,
        row: u64,
        col: usize,
        grid: &GridView,
        this: &Entity<Self>,
        cx: &App,
    ) -> Vec<MenuItem> {
        let mut items = Vec::new();
        let Some(values) = grid.row_values(row) else {
            return items;
        };
        let mut shortcut = self
            .state
            .read(cx)
            .settings
            .keybinds
            .get("follow-reference")
            .map(shortcut_label)
            .filter(|s| !s.is_empty());
        for hop in self.hops_at(&values, col) {
            let target = hop.key.target.name.clone();
            let weak = this.downgrade();
            let follow = hop.clone();
            let mut item =
                MenuItem::new(format!("Open referenced row in {target}"), move |_, cx| {
                    let hop = follow.clone();
                    weak.update(cx, |t, cx| t.follow_hop(row, hop, cx)).ok();
                });
            if let Some(keys) = shortcut.take() {
                item = item.detail(keys);
            }
            items.push(item);
            let state = self.state.clone();
            items.push(MenuItem::new(
                format!("Open {target} filtered to this row"),
                move |_, cx| {
                    let filters = pairs(&hop.key.target_columns, hop.values.clone());
                    open_filtered(&state, &hop.connection, &hop.key.target, &filters, cx);
                },
            ));
        }
        if let Some(Ok(info)) = &self.info {
            let incoming = &info.referenced_by;
            for key in incoming
                .iter()
                .filter(|k| k.columns.len() == k.target_columns.len())
                .take(MAX_INCOMING)
            {
                let Some(found) =
                    positions(grid.columns(), &key.columns).and_then(|at| values_at(&values, &at))
                else {
                    continue;
                };
                let label = if incoming.iter().filter(|k| k.target == key.target).count() > 1 {
                    format!(
                        "Rows in {} referencing this by {}",
                        key.target.name,
                        key.target_columns.join(", ")
                    )
                } else {
                    format!("Rows in {} referencing this", key.target.name)
                };
                let (state, connection) = (self.state.clone(), self.connection.clone());
                let (target, filters) = (key.target.clone(), pairs(&key.target_columns, found));
                items.push(MenuItem::new(label, move |_, cx| {
                    open_filtered(&state, &connection, &target, &filters, cx)
                }));
            }
        }
        if !items.is_empty() {
            items.insert(0, MenuItem::separator());
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn far_columns_pair_with_near_values() {
        let columns = ["country".to_string(), "code".to_string()];
        assert_eq!(
            pairs(&columns, vec![Value::text("DE"), Value::Int(7)]),
            [
                ("country".to_string(), Value::text("DE")),
                ("code".to_string(), Value::Int(7))
            ]
        );
    }
}
