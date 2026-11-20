//! The fields of an object as a tree, for views that draw structure rather than rows.
//!
//! A table's columns are declared, so its schema is its column list. A collection's
//! documents need not share fields and nest documents and arrays inside each other, so its
//! schema is read from a sample: every field path seen, the types it held and how often,
//! with the fields of embedded documents (including documents inside arrays) as children.
//! [`Connection::schema`](crate::Connection::schema) returns one.
//!
//! Counts are against the field's parent: a top-level field is counted against the
//! sampled records, `address.city` against the records that had an `address` document,
//! and a field of documents inside an array against those array elements. So
//! [`SchemaField::share`] is "how often is this field there when it could be".

use crate::{Column, ValueKind};

/// The tree of fields of one object.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObjectSchema {
    /// Records read to build it. Zero when the schema is declared, as for a table.
    pub sampled: u64,
    /// The object's own count or estimate of its records, when known.
    pub total: Option<u64>,
    /// True when the fields are the object's declared columns rather than a sample.
    pub declared: bool,
    /// Whether some fields were left out to keep the tree bounded.
    pub truncated: bool,
    pub fields: Vec<SchemaField>,
}

/// One field and the fields nested in it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SchemaField {
    /// The field's own name, the last part of `path`.
    pub name: String,
    /// The path as queries name it: `address.city`, `items.sku` for a field of the
    /// documents inside the `items` array.
    pub path: String,
    /// The types it held, most frequent first. A declared column has one, with a count of
    /// zero.
    pub types: Vec<SchemaType>,
    /// For an array field, the types of its elements, most frequent first.
    pub items: Vec<SchemaType>,
    /// Parent occurrences that had this field with any value but null.
    pub present: u64,
    /// Parent occurrences: the sampled records for a top-level field, the occurrences of
    /// the parent document otherwise.
    pub of: u64,
    /// Part of the object's key (`_id`, a primary key column).
    pub key: bool,
    /// Fields of the embedded documents found under this one, in first-seen order.
    pub fields: Vec<SchemaField>,
}

/// A type a field held, as the database names it, with how many times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaType {
    pub name: String,
    pub kind: ValueKind,
    pub count: u64,
}

impl SchemaField {
    /// How often the field is there, from 0 to 1, against its parent's occurrences. A
    /// declared column is always there.
    pub fn share(&self) -> f32 {
        if self.of == 0 {
            1.0
        } else {
            (self.present as f64 / self.of as f64).min(1.0) as f32
        }
    }

    /// The most frequent type's name, or empty.
    pub fn main_type(&self) -> &str {
        self.types.first().map_or("", |t| t.name.as_str())
    }

    /// Every field under this one, depth first, this one included.
    pub fn walk(&self) -> impl Iterator<Item = &SchemaField> {
        let mut stack = vec![self];
        std::iter::from_fn(move || {
            let next = stack.pop()?;
            stack.extend(next.fields.iter().rev());
            Some(next)
        })
    }
}

impl ObjectSchema {
    /// The schema of declared columns: one field per column, no nesting.
    pub fn declared(columns: &[Column], total: Option<u64>) -> Self {
        Self {
            sampled: 0,
            total,
            declared: true,
            truncated: false,
            fields: columns
                .iter()
                .map(|c| SchemaField {
                    name: c.name.clone(),
                    path: c.name.clone(),
                    types: vec![SchemaType {
                        name: c.type_name.clone(),
                        kind: c.kind,
                        count: 0,
                    }],
                    items: Vec::new(),
                    present: 0,
                    of: 0,
                    key: c.primary_key.is_some(),
                    fields: Vec::new(),
                })
                .collect(),
        }
    }

    /// Every field, depth first.
    pub fn walk(&self) -> impl Iterator<Item = &SchemaField> {
        self.fields.iter().flat_map(SchemaField::walk)
    }

    /// The field at `path`, such as `address.city`.
    pub fn field(&self, path: &str) -> Option<&SchemaField> {
        self.walk().find(|f| f.path == path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_columns_are_flat_and_always_there() {
        let mut id = Column::new("id", "int4", ValueKind::Int);
        id.primary_key = Some(0);
        let schema = ObjectSchema::declared(&[id, Column::new("n", "text", ValueKind::Text)], None);
        assert!(schema.declared);
        assert_eq!(schema.fields.len(), 2);
        assert!(schema.fields[0].key);
        assert_eq!(schema.fields[1].main_type(), "text");
        assert_eq!(schema.fields[1].share(), 1.0);
    }

    #[test]
    fn walks_depth_first() {
        let leaf = |path: &str| SchemaField {
            name: path.rsplit('.').next().unwrap_or(path).into(),
            path: path.into(),
            ..Default::default()
        };
        let schema = ObjectSchema {
            fields: vec![
                SchemaField {
                    fields: vec![leaf("a.b"), leaf("a.c")],
                    ..leaf("a")
                },
                leaf("d"),
            ],
            ..Default::default()
        };
        let paths: Vec<_> = schema.walk().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a", "a.b", "a.c", "d"]);
        assert_eq!(schema.field("a.c").map(|f| f.name.as_str()), Some("c"));
        let half = SchemaField {
            present: 1,
            of: 2,
            ..leaf("x")
        };
        assert_eq!(half.share(), 0.5);
    }
}
