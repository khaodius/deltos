//! Foreign key arrows. The grid's owner says which columns form each foreign key, once,
//! when the columns are known; a cell of such a column then shows a small arrow to follow
//! the key. A row whose key has a NULL in any of its columns points nowhere and shows none.

use gpui::{App, Bounds, Pixels, TransformationMatrix, Window, point, px, size};

use crate::theme::Theme;
use crate::ui::icons::Icon;

/// The width the arrow takes at the right end of a cell, in unscaled pixels.
pub const ARROW: f32 = 16.0;

/// The foreign keys of the grid's columns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Links {
    /// Each key's columns.
    keys: Vec<Vec<usize>>,
    /// For each column, the keys it is part of.
    of_column: Vec<Vec<usize>>,
}

impl Links {
    /// `keys` lists each foreign key's columns, by index.
    pub fn new(keys: Vec<Vec<usize>>) -> Self {
        let width = keys.iter().flatten().max().map_or(0, |c| c + 1);
        let mut of_column = vec![Vec::new(); width];
        for (k, columns) in keys.iter().enumerate() {
            for &c in columns {
                if let Some(of) = of_column.get_mut(c)
                    && !of.contains(&k)
                {
                    of.push(k);
                }
            }
        }
        Self { keys, of_column }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Whether the cell in `col` of a row shows an arrow, the row read through
    /// `present`, which says whether a column holds a value that is not NULL: its column
    /// is part of a key none of whose columns is NULL in that row.
    pub fn shows_with(&self, col: usize, present: impl Fn(usize) -> bool) -> bool {
        self.of_column.get(col).is_some_and(|of| {
            of.iter().any(|&k| {
                self.keys
                    .get(k)
                    .is_some_and(|key| key.iter().all(|&c| present(c)))
            })
        })
    }
}

/// Where the arrow sits in a cell.
pub fn arrow_bounds(cell: Bounds<Pixels>) -> Bounds<Pixels> {
    let side = px(12.);
    Bounds {
        origin: point(
            cell.right() - px(ARROW) + px(1.),
            cell.top() + (cell.size.height - side) / 2.,
        ),
        size: size(side, side),
    }
}

/// Paints the arrow at the right end of a cell.
pub fn paint_arrow(cell: Bounds<Pixels>, t: &Theme, window: &mut Window, cx: &App) {
    let _ = window.paint_svg(
        arrow_bounds(cell),
        Icon::ChevronRight.path(),
        TransformationMatrix::unit(),
        t.text_faint,
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::Value;

    impl Links {
        fn shows(&self, col: usize, row: &[Value]) -> bool {
            self.shows_with(col, |c| row.get(c).is_some_and(|v| !v.is_null()))
        }
    }

    #[test]
    fn arrows_need_every_column_of_the_key() {
        // Columns 1 and 2 form one key, column 4 another; column 2 is in both.
        let links = Links::new(vec![vec![1, 2], vec![4, 2]]);
        let row = [
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Null,
            Value::Int(5),
        ];
        assert!(!links.shows(0, &row));
        assert!(links.shows(1, &row));
        assert!(links.shows(4, &row));
        assert!(!links.shows(9, &row));
        // Column 2 is NULL: neither key leads anywhere.
        let half = [
            Value::Int(1),
            Value::Int(2),
            Value::Null,
            Value::Int(4),
            Value::Int(5),
        ];
        assert!(!links.shows(1, &half));
        assert!(!links.shows(2, &half));
        assert!(!links.shows(4, &half));
        // Column 1 is NULL: the second key still does, through column 2.
        let other = [
            Value::Int(1),
            Value::Null,
            Value::Int(3),
            Value::Int(4),
            Value::Int(5),
        ];
        assert!(!links.shows(1, &other));
        assert!(links.shows(2, &other));
        assert!(links.shows(4, &other));
        assert!(Links::default().is_empty());
        assert!(!Links::default().shows(0, &row));
    }
}
