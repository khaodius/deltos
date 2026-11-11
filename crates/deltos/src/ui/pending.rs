//! Staged changes to one table: edited cells, deleted rows and new rows that wait in the
//! table view until the person commits them, as TablePlus does.
//!
//! Changes to existing rows are keyed by the row's primary key, never by its position,
//! so they stay with their row when the table is reloaded, sorted or filtered. The buffer
//! holds only what the person changed, so its size is bounded by what they did, not by
//! the size of the table.
//!
//! The grid asks [`PendingChanges::row`] for every row it draws. That lookup hashes the
//! key values in place and compares them against the stored key, so it allocates nothing.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use deltos_data::{Column, RowEdit, Value};

/// One changed cell: its column, the value it had when it was first changed, and the
/// value it will get.
#[derive(Debug, Clone, PartialEq)]
pub struct CellChange {
    pub col: usize,
    pub original: Value,
    pub value: Value,
}

/// The staged changes to one existing row.
#[derive(Debug, Clone, PartialEq)]
pub struct RowChange {
    /// The row's primary key values as they were read, in key order.
    key: Box<[Value]>,
    pub cells: Vec<CellChange>,
    /// The row is to be deleted. Cell changes are kept, so restoring the row brings them
    /// back, but a deleted row is only deleted on commit.
    pub deleted: bool,
}

impl RowChange {
    /// The staged change of one column, if it has one.
    pub fn cell(&self, col: usize) -> Option<&CellChange> {
        self.cells.iter().find(|c| c.col == col)
    }
}

/// A staged new row: a value per column, `None` where the database fills in its default.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRow {
    pub values: Vec<Option<Value>>,
}

/// How a row looks in the grid because of staged changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowMark {
    #[default]
    None,
    Changed,
    Deleted,
    Inserted,
}

#[derive(Debug, Default)]
pub struct PendingChanges {
    columns: Arc<[Column]>,
    /// Primary key columns, by index into `columns`, in key order.
    key_cols: Vec<usize>,
    rows: Vec<RowChange>,
    /// Positions in `rows` by the hash of their key.
    index: HashMap<u64, Vec<usize>>,
    inserts: Vec<NewRow>,
    /// Moves on every change, so views can drop what they drew from the old state.
    generation: u64,
}

fn hash_value(v: &Value, h: &mut impl Hasher) {
    std::mem::discriminant(v).hash(h);
    match v {
        Value::Null => {}
        Value::Bool(b) => b.hash(h),
        Value::Int(i) => i.hash(h),
        // 0.0 and -0.0 are equal, so they hash the same.
        Value::Float(f) => (if *f == 0.0 { 0.0f64 } else { *f }).to_bits().hash(h),
        Value::Text(s) | Value::Json(s) => s.hash(h),
        Value::Bytes(b) => b.hash(h),
    }
}

fn key_hash<'a>(values: impl Iterator<Item = &'a Value>) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for v in values {
        hash_value(v, &mut h);
    }
    h.finish()
}

impl PendingChanges {
    /// The columns of the table, as the grid shows them. When they change while
    /// something is staged (the table was opened again), staged values follow their
    /// columns by name; changes to rows whose key columns are gone are dropped.
    pub fn set_columns(&mut self, columns: &Arc<[Column]>) {
        if Arc::ptr_eq(&self.columns, columns) {
            return;
        }
        let same = self.columns.len() == columns.len()
            && self
                .columns
                .iter()
                .zip(columns.iter())
                .all(|(a, b)| a.name == b.name);
        let mut key_cols: Vec<(u16, usize)> = columns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.primary_key.map(|p| (p, i)))
            .collect();
        key_cols.sort_unstable();
        let key_cols: Vec<usize> = key_cols.into_iter().map(|(_, i)| i).collect();
        if !same && !self.is_empty() {
            let old = self.columns.clone();
            let find = |col: usize| {
                let name = &old.get(col)?.name;
                columns.iter().position(|c| &c.name == name)
            };
            let old_key: Vec<Option<&str>> = self
                .key_cols
                .iter()
                .map(|c| old.get(*c).map(|c| c.name.as_str()))
                .collect();
            let new_key: Vec<Option<&str>> = key_cols
                .iter()
                .map(|c| columns.get(*c).map(|c| c.name.as_str()))
                .collect();
            if old_key == new_key {
                for row in &mut self.rows {
                    row.cells.retain_mut(|cell| match find(cell.col) {
                        Some(col) => {
                            cell.col = col;
                            true
                        }
                        None => false,
                    });
                }
                self.rows.retain(|r| r.deleted || !r.cells.is_empty());
            } else {
                self.rows.clear();
            }
            for row in &mut self.inserts {
                let mut values = vec![None; columns.len()];
                for (col, v) in row.values.iter().enumerate() {
                    if let Some(to) = find(col) {
                        values[to] = v.clone();
                    }
                }
                row.values = values;
            }
            self.generation += 1;
        }
        self.columns = columns.clone();
        self.key_cols = key_cols;
        self.reindex();
    }

    /// Whether rows of this table can be told apart, which staging changes to them needs.
    pub fn has_key(&self) -> bool {
        !self.key_cols.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.inserts.is_empty()
    }

    /// The number of statements a commit would run: one per changed, deleted or new row.
    pub fn count(&self) -> usize {
        self.rows.len() + self.inserts.len()
    }

    /// Changed cells in rows that are not deleted.
    pub fn changed_cells(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| !r.deleted)
            .map(|r| r.cells.len())
            .sum()
    }

    pub fn deleted_rows(&self) -> usize {
        self.rows.iter().filter(|r| r.deleted).count()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn reindex(&mut self) {
        self.index.clear();
        for (i, row) in self.rows.iter().enumerate() {
            self.index
                .entry(key_hash(row.key.iter()))
                .or_default()
                .push(i);
        }
    }

    fn position(&self, values: &[Value]) -> Option<usize> {
        if self.key_cols.is_empty() || self.rows.is_empty() {
            return None;
        }
        let mut key = self.key_cols.iter().map(|c| values.get(*c));
        if key.any(|v| v.is_none()) {
            return None;
        }
        let hash = key_hash(self.key_cols.iter().filter_map(|c| values.get(*c)));
        self.index.get(&hash)?.iter().copied().find(|&i| {
            self.rows[i]
                .key
                .iter()
                .zip(&self.key_cols)
                .all(|(k, c)| values.get(*c) == Some(k))
        })
    }

    /// The staged changes of the row with these values, as read from the table.
    pub fn row(&self, values: &[Value]) -> Option<&RowChange> {
        self.position(values).map(|i| &self.rows[i])
    }

    fn entry(&mut self, values: &[Value]) -> Option<usize> {
        if let Some(i) = self.position(values) {
            return Some(i);
        }
        let key: Option<Box<[Value]>> = self
            .key_cols
            .iter()
            .map(|c| values.get(*c).cloned())
            .collect();
        let key = key.filter(|k| !k.is_empty())?;
        self.index
            .entry(key_hash(key.iter()))
            .or_default()
            .push(self.rows.len());
        self.rows.push(RowChange {
            key,
            cells: Vec::new(),
            deleted: false,
        });
        Some(self.rows.len() - 1)
    }

    fn drop_if_empty(&mut self, i: usize) {
        if !self.rows[i].deleted && self.rows[i].cells.is_empty() {
            self.rows.remove(i);
            self.reindex();
        }
    }

    /// Stages `value` for column `col` of the row with these values. Setting a cell back
    /// to the value it had removes its change. `false` when the table has no key.
    pub fn stage(&mut self, values: &[Value], col: usize, value: Value) -> bool {
        let Some(original) = values.get(col) else {
            return false;
        };
        // A row with nothing staged that gets the value it has stays as it is. Making an
        // entry for it only to drop it again would rebuild the index, once per cell of a
        // large paste or clear.
        if *original == value && self.position(values).is_none() {
            return self.has_key();
        }
        let Some(i) = self.entry(values) else {
            return false;
        };
        let row = &mut self.rows[i];
        match row.cells.iter().position(|c| c.col == col) {
            Some(c) if row.cells[c].original == value => {
                row.cells.remove(c);
            }
            Some(c) => row.cells[c].value = value,
            None if *original == value => {}
            None => row.cells.push(CellChange {
                col,
                original: original.clone(),
                value,
            }),
        }
        self.drop_if_empty(i);
        self.generation += 1;
        true
    }

    /// Takes back the staged change of one cell.
    pub fn revert_cell(&mut self, values: &[Value], col: usize) {
        if let Some(i) = self.position(values) {
            self.rows[i].cells.retain(|c| c.col != col);
            self.drop_if_empty(i);
            self.generation += 1;
        }
    }

    /// Takes back every staged change of these rows, their deletion included, rebuilding
    /// the index once.
    pub fn revert_rows<'a>(&mut self, rows: impl IntoIterator<Item = &'a [Value]>) {
        let mut any = false;
        for values in rows {
            if let Some(i) = self.position(values) {
                self.rows[i].cells.clear();
                self.rows[i].deleted = false;
                any = true;
            }
        }
        if any {
            self.sweep();
            self.generation += 1;
        }
    }

    /// Drops rows left with nothing staged.
    fn sweep(&mut self) {
        let before = self.rows.len();
        self.rows.retain(|r| r.deleted || !r.cells.is_empty());
        if self.rows.len() != before {
            self.reindex();
        }
    }

    /// Stages the row's deletion. `false` when the table has no key.
    pub fn delete(&mut self, values: &[Value]) -> bool {
        let Some(i) = self.entry(values) else {
            return false;
        };
        self.rows[i].deleted = true;
        self.generation += 1;
        true
    }

    /// Takes back the staged deletion of these rows, keeping their cell changes, and
    /// rebuilds the index once.
    pub fn restore_rows<'a>(&mut self, rows: impl IntoIterator<Item = &'a [Value]>) {
        let mut any = false;
        for values in rows {
            if let Some(i) = self.position(values) {
                self.rows[i].deleted = false;
                any = true;
            }
        }
        if any {
            self.sweep();
            self.generation += 1;
        }
    }

    pub fn inserts(&self) -> &[NewRow] {
        &self.inserts
    }

    /// Stages a new row. `values` holds one entry per column; missing ones are defaults.
    pub fn add_insert(&mut self, mut values: Vec<Option<Value>>) -> usize {
        values.resize(self.columns.len(), None);
        self.inserts.push(NewRow { values });
        self.generation += 1;
        self.inserts.len() - 1
    }

    /// Sets one value of a staged new row; `None` leaves it to the database's default.
    pub fn set_insert(&mut self, row: usize, col: usize, value: Option<Value>) {
        if let Some(slot) = self
            .inserts
            .get_mut(row)
            .and_then(|r| r.values.get_mut(col))
        {
            *slot = value;
            self.generation += 1;
        }
    }

    pub fn remove_insert(&mut self, row: usize) {
        if row < self.inserts.len() {
            self.inserts.remove(row);
            self.generation += 1;
        }
    }

    pub fn clear(&mut self) {
        if !self.is_empty() {
            self.rows.clear();
            self.index.clear();
            self.inserts.clear();
            self.generation += 1;
        }
    }

    fn key_pairs(&self, row: &RowChange) -> Vec<(String, Value)> {
        self.key_cols
            .iter()
            .zip(row.key.iter())
            .filter_map(|(c, v)| Some((self.columns.get(*c)?.name.clone(), v.clone())))
            .collect()
    }

    /// Everything staged as edits for [`deltos_data::Connection::apply_edits`]: deletions
    /// first, so a key they free can be reused, then changed rows, then new rows.
    pub fn edits(&self) -> Vec<RowEdit> {
        let mut out = Vec::with_capacity(self.count());
        for row in self.rows.iter().filter(|r| r.deleted) {
            out.push(RowEdit::Delete {
                key: self.key_pairs(row),
            });
        }
        for row in self.rows.iter().filter(|r| !r.deleted) {
            let changes = row
                .cells
                .iter()
                .filter_map(|c| Some((self.columns.get(c.col)?.name.clone(), c.value.clone())))
                .collect();
            out.push(RowEdit::Update {
                key: self.key_pairs(row),
                changes,
            });
        }
        for row in &self.inserts {
            let values = row
                .values
                .iter()
                .enumerate()
                .filter_map(|(c, v)| Some((self.columns.get(c)?.name.clone(), v.clone()?)))
                .collect();
            out.push(RowEdit::Insert { values });
        }
        out
    }

    /// Forgets the first `n` changes in the order of [`Self::edits`], after a database
    /// without transactions saved them and failed on the next.
    pub fn drop_applied(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let mut left = n;
        let deleted: Vec<usize> = (0..self.rows.len())
            .filter(|i| self.rows[*i].deleted)
            .collect();
        let changed: Vec<usize> = (0..self.rows.len())
            .filter(|i| !self.rows[*i].deleted)
            .collect();
        let mut gone = Vec::new();
        for i in deleted.into_iter().chain(changed) {
            if left == 0 {
                break;
            }
            gone.push(i);
            left -= 1;
        }
        gone.sort_unstable();
        for i in gone.into_iter().rev() {
            self.rows.remove(i);
        }
        let inserts = left.min(self.inserts.len());
        self.inserts.drain(..inserts);
        self.reindex();
        self.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::ValueKind;

    fn columns() -> Arc<[Column]> {
        let mut id = Column::new("id", "int", ValueKind::Int);
        id.primary_key = Some(0);
        let name = Column::new("name", "text", ValueKind::Text);
        let note = Column::new("note", "text", ValueKind::Text);
        Arc::from(vec![id, name, note])
    }

    fn row(id: i64, name: &str) -> Vec<Value> {
        vec![Value::Int(id), Value::text(name), Value::Null]
    }

    fn mark(p: &PendingChanges, values: &[Value]) -> RowMark {
        match p.row(values) {
            Some(r) if r.deleted => RowMark::Deleted,
            Some(_) => RowMark::Changed,
            None => RowMark::None,
        }
    }

    fn pending() -> PendingChanges {
        let mut p = PendingChanges::default();
        p.set_columns(&columns());
        p
    }

    #[test]
    fn changes_follow_the_key_not_the_position() {
        let mut p = pending();
        assert!(p.stage(&row(7, "a"), 1, Value::text("b")));
        // The same row read again, anywhere in the table, finds its change.
        let again = row(7, "a");
        let change = p.row(&again).expect("staged");
        assert_eq!(change.cell(1).map(|c| &c.value), Some(&Value::text("b")));
        assert_eq!(change.cell(1).map(|c| &c.original), Some(&Value::text("a")));
        assert!(p.row(&row(8, "a")).is_none());
        assert_eq!(mark(&p, &again), RowMark::Changed);
        assert_eq!((p.count(), p.changed_cells()), (1, 1));
    }

    #[test]
    fn editing_back_to_the_original_removes_the_change() {
        let mut p = pending();
        let r = row(1, "a");
        p.stage(&r, 1, Value::text("b"));
        p.stage(&r, 1, Value::text("c"));
        assert_eq!(
            p.row(&r).and_then(|c| c.cell(1)).map(|c| &c.original),
            Some(&Value::text("a"))
        );
        p.stage(&r, 1, Value::text("a"));
        assert!(p.is_empty());
        // Staging the value a cell already has stages nothing.
        p.stage(&r, 2, Value::Null);
        assert!(p.is_empty());
    }

    #[test]
    fn deletes_keep_cell_changes_until_restored() {
        let mut p = pending();
        let r = row(1, "a");
        p.stage(&r, 1, Value::text("b"));
        p.delete(&r);
        assert_eq!(mark(&p, &r), RowMark::Deleted);
        assert_eq!((p.deleted_rows(), p.changed_cells()), (1, 0));
        assert!(matches!(p.edits()[..], [RowEdit::Delete { .. }]));
        p.restore_rows([r.as_slice()]);
        assert_eq!(mark(&p, &r), RowMark::Changed);
        p.revert_cell(&r, 1);
        assert!(p.is_empty());
        p.delete(&r);
        p.revert_rows([r.as_slice()]);
        assert!(p.is_empty());
    }

    #[test]
    fn tables_without_a_key_stage_nothing() {
        let mut p = PendingChanges::default();
        p.set_columns(&Arc::from(vec![Column::new("x", "int", ValueKind::Int)]));
        assert!(!p.has_key());
        assert!(!p.stage(&[Value::Int(1)], 0, Value::Int(2)));
        assert!(!p.delete(&[Value::Int(1)]));
        assert!(p.is_empty());
    }

    #[test]
    fn edits_come_out_in_a_safe_order() {
        let mut p = pending();
        p.stage(&row(1, "a"), 1, Value::text("x"));
        p.delete(&row(2, "b"));
        let n = p.add_insert(vec![None, Some(Value::text("new"))]);
        p.set_insert(n, 2, Some(Value::Null));
        let edits = p.edits();
        assert_eq!(
            edits,
            vec![
                RowEdit::Delete {
                    key: vec![("id".into(), Value::Int(2))]
                },
                RowEdit::Update {
                    key: vec![("id".into(), Value::Int(1))],
                    changes: vec![("name".into(), Value::text("x"))]
                },
                RowEdit::Insert {
                    values: vec![
                        ("name".into(), Value::text("new")),
                        ("note".into(), Value::Null)
                    ]
                },
            ]
        );
        p.drop_applied(2);
        assert_eq!(p.count(), 1);
        assert_eq!(p.inserts().len(), 1);
        p.drop_applied(1);
        assert!(p.is_empty());
    }

    #[test]
    fn many_rows_are_found_by_hash() {
        let mut p = pending();
        for i in 0..2000 {
            p.stage(&row(i, "a"), 1, Value::text(format!("v{i}")));
        }
        for i in (0..2000).step_by(7) {
            let found = p
                .row(&row(i, "a"))
                .and_then(|r| r.cell(1))
                .map(|c| c.value.clone());
            assert_eq!(found, Some(Value::text(format!("v{i}"))));
        }
        p.revert_rows([row(5, "a").as_slice()]);
        assert!(p.row(&row(5, "a")).is_none());
        assert!(p.row(&row(6, "a")).is_some());
        // Many rows at once, as a selection reverts or restores them.
        let rows: Vec<Vec<Value>> = (100..1100).map(|i| row(i, "a")).collect();
        for r in &rows {
            p.delete(r);
        }
        p.restore_rows(rows.iter().map(Vec::as_slice));
        assert_eq!(p.deleted_rows(), 0);
        assert!(p.row(&rows[0]).is_some_and(|r| !r.deleted));
        p.revert_rows(rows.iter().map(Vec::as_slice));
        assert!(p.row(&rows[0]).is_none());
        assert!(p.row(&row(1100, "a")).is_some());
        assert_eq!(p.count(), 2000 - 1 - 1000);
        // A value a row already has stages nothing and leaves no entry behind.
        assert!(p.stage(&row(5000, "a"), 1, Value::text("a")));
        assert!(p.row(&row(5000, "a")).is_none());
    }

    #[test]
    fn columns_reopened_in_another_order_keep_their_changes() {
        let mut p = pending();
        p.stage(&row(1, "a"), 2, Value::text("n"));
        p.add_insert(vec![None, Some(Value::text("x")), None]);
        let cols = columns();
        let reordered: Arc<[Column]> =
            Arc::from(vec![cols[2].clone(), cols[0].clone(), cols[1].clone()]);
        p.set_columns(&reordered);
        let values = vec![Value::Null, Value::Int(1), Value::text("a")];
        assert_eq!(
            p.row(&values).and_then(|r| r.cell(0)).map(|c| &c.value),
            Some(&Value::text("n"))
        );
        assert_eq!(p.inserts()[0].values[2], Some(Value::text("x")));
    }
}
