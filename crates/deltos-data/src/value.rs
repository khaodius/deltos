//! Cell values. Kept at 24 bytes so a page of rows stays compact: anything larger than a
//! machine word lives behind a box.

use std::fmt;

use serde::{Deserialize, Serialize};

/// What a column holds, for alignment, editing and formatting in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ValueKind {
    Bool,
    Int,
    Float,
    /// Exact numbers kept as text so nothing is lost (`numeric`, `decimal`).
    Decimal,
    #[default]
    Text,
    Bytes,
    Date,
    Time,
    DateTime,
    Uuid,
    Json,
    /// MongoDB ObjectId and similar opaque identifiers.
    ObjectId,
    Array,
    /// Anything else, shown as the database formats it.
    Other,
}

impl ValueKind {
    /// Numbers are right-aligned in the grid.
    pub fn is_numeric(self) -> bool {
        matches!(self, ValueKind::Int | ValueKind::Float | ValueKind::Decimal)
    }
}

/// One cell. Dates, times, UUIDs, decimals and other values the grid only shows are kept
/// as the database's own text form in [`Value::Text`] with the column's [`ValueKind`]
/// telling them apart, which avoids a conversion per cell for what is mostly display.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(Box<str>),
    Bytes(Box<[u8]>),
    /// A nested document or array, as compact JSON text.
    Json(Box<str>),
}

impl Value {
    pub fn text(s: impl Into<Box<str>>) -> Self {
        Value::Text(s.into())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Size in bytes of the heap data behind the value, for memory budgets.
    pub fn heap_size(&self) -> usize {
        match self {
            Value::Text(s) | Value::Json(s) => s.len(),
            Value::Bytes(b) => b.len(),
            _ => 0,
        }
    }

    /// The value as a single line for a grid cell, cut to `max` characters.
    pub fn preview(&self, max: usize) -> String {
        let mut out = String::new();
        match self {
            Value::Null => out.push_str("NULL"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Float(f) => out.push_str(&f.to_string()),
            Value::Text(s) | Value::Json(s) => {
                for (n, c) in s.chars().enumerate() {
                    if n == max {
                        out.push('…');
                        break;
                    }
                    out.push(if c == '\n' || c == '\r' || c == '\t' {
                        ' '
                    } else {
                        c
                    });
                }
            }
            Value::Bytes(b) => {
                out.push_str("0x");
                for byte in b.iter().take(max / 2) {
                    out.push_str(&format!("{byte:02x}"));
                }
                if b.len() > max / 2 {
                    out.push('…');
                }
            }
        }
        out
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Text(s) | Value::Json(s) => f.write_str(s),
            Value::Bytes(b) => {
                f.write_str("0x")?;
                for byte in b.iter() {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_small() {
        assert!(std::mem::size_of::<Value>() <= 24);
    }

    #[test]
    fn previews_on_one_line() {
        assert_eq!(Value::text("a\nb").preview(10), "a b");
        assert_eq!(Value::text("abcdef").preview(3), "abc…");
        assert_eq!(Value::Bytes(vec![1, 2, 255].into()).preview(10), "0x0102ff");
        assert_eq!(Value::Null.preview(10), "NULL");
    }
}
