//! The columns of a set of documents.
//!
//! Documents in one collection need not share fields, while the grid needs fixed columns.
//! A [`Shape`] is the union of the fields seen so far, in the order they were first seen
//! with `_id` first, and the type each field has held. It turns each document into a row
//! with a cell per known field and NULL where the document lacks one.

use std::collections::HashMap;

use bson::{Bson, Document};
use deltos_data::{Column, Row, Value, ValueKind};

use super::convert;

/// More fields than this are left out of the grid. Documents with thousands of distinct
/// top-level keys are almost always maps keyed by data, and one column per key would make
/// a page wider than any screen and its memory unbounded.
pub(crate) const MAX_FIELDS: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldType {
    /// Only nulls so far.
    Unknown,
    One(ValueKind, &'static str),
    Mixed,
}

impl FieldType {
    fn merge(self, bson: &Bson) -> Self {
        if matches!(bson, Bson::Null | Bson::Undefined) {
            return self;
        }
        let kind = convert::kind_of(bson);
        let name = convert::type_name(bson);
        match self {
            FieldType::Unknown => FieldType::One(kind, name),
            FieldType::One(k, n) if k == kind && n == name => self,
            // Integers of both widths read as one integer column; integers and doubles
            // as one number column, which is how the shell treats them.
            FieldType::One(ValueKind::Int, _) if kind == ValueKind::Int => {
                FieldType::One(ValueKind::Int, "long")
            }
            FieldType::One(ValueKind::Int | ValueKind::Float, _)
                if matches!(kind, ValueKind::Int | ValueKind::Float) =>
            {
                FieldType::One(ValueKind::Float, "number")
            }
            _ => FieldType::Mixed,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Shape {
    names: Vec<String>,
    index: HashMap<String, usize>,
    types: Vec<FieldType>,
    /// Documents that had the field with a non-null value.
    present: Vec<u64>,
    seen: u64,
    /// `_id` is the collection's key, rather than just a field of a result.
    keyed: bool,
}

impl Shape {
    /// The shape of a collection's documents, which always have `_id`.
    pub(crate) fn keyed() -> Self {
        let mut shape = Shape {
            keyed: true,
            ..Default::default()
        };
        shape.field("_id");
        shape
    }

    /// The shape of a result. `_id` comes first when any document has it.
    pub(crate) fn for_documents(docs: &[Document]) -> Self {
        let mut shape = Shape::default();
        if docs.iter().any(|d| d.contains_key("_id")) {
            shape.field("_id");
        }
        for doc in docs {
            shape.add(doc);
        }
        shape
    }

    fn field(&mut self, name: &str) -> Option<usize> {
        if let Some(&i) = self.index.get(name) {
            return Some(i);
        }
        if self.names.len() >= MAX_FIELDS {
            return None;
        }
        let i = self.names.len();
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), i);
        self.types.push(FieldType::Unknown);
        self.present.push(0);
        Some(i)
    }

    pub(crate) fn add(&mut self, doc: &Document) {
        self.seen += 1;
        for (name, value) in doc {
            if let Some(i) = self.field(name) {
                self.types[i] = self.types[i].merge(value);
                if !matches!(value, Bson::Null | Bson::Undefined) {
                    self.present[i] += 1;
                }
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    /// The grid kind of a field, when it has held one type.
    pub(crate) fn kind(&self, name: &str) -> Option<ValueKind> {
        match self.types.get(*self.index.get(name)?)? {
            FieldType::One(kind, _) => Some(*kind),
            _ => None,
        }
    }

    pub(crate) fn columns(&self) -> Vec<Column> {
        self.names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let (kind, type_name) = match self.types[i] {
                    FieldType::Unknown => (ValueKind::Text, "null"),
                    FieldType::One(kind, type_name) => (kind, type_name),
                    FieldType::Mixed => (ValueKind::Other, "mixed"),
                };
                let mut column = Column::new(name.clone(), type_name, kind);
                let key = self.keyed && name == "_id";
                column.nullable = Some(!key && self.present[i] < self.seen);
                column.primary_key = key.then_some(0);
                column
            })
            .collect()
    }

    /// One row of the document, a cell per known field.
    pub(crate) fn row(&self, doc: Document) -> Row {
        let mut values = vec![Value::Null; self.names.len()];
        for (name, value) in doc {
            if let Some(&i) = self.index.get(&name) {
                values[i] = convert::to_value(value);
            }
        }
        values.into_boxed_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn unions_fields_with_id_first() {
        let docs = [
            doc! { "name": "a", "_id": 1 },
            doc! { "_id": 2, "age": 3, "name": Bson::Null },
            doc! { "_id": 3, "age": 4.5, "tags": ["x"] },
        ];
        let shape = Shape::for_documents(&docs);
        let columns = shape.columns();
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["_id", "name", "age", "tags"]);
        assert_eq!(columns[1].kind, ValueKind::Text);
        assert_eq!(columns[1].nullable, Some(true));
        assert_eq!(columns[2].kind, ValueKind::Float);
        assert_eq!(columns[2].type_name, "number");
        assert_eq!(columns[3].kind, ValueKind::Array);
        assert_eq!(columns[0].primary_key, None);
        let row = shape.row(docs[2].clone());
        assert_eq!(row[0], Value::Int(3));
        assert_eq!(row[1], Value::Null);
        assert_eq!(row[3], Value::Json(r#"["x"]"#.into()));
    }

    #[test]
    fn marks_mixed_fields() {
        let mut shape = Shape::keyed();
        shape.add(&doc! { "_id": 1, "v": "text" });
        shape.add(&doc! { "_id": 2, "v": true });
        let columns = shape.columns();
        assert_eq!(columns[0].primary_key, Some(0));
        assert_eq!(columns[0].nullable, Some(false));
        assert_eq!(columns[1].type_name, "mixed");
        assert_eq!(shape.kind("v"), None);
        assert_eq!(shape.kind("_id"), Some(ValueKind::Int));
    }

    #[test]
    fn caps_fields() {
        let mut doc = Document::new();
        for i in 0..MAX_FIELDS + 50 {
            doc.insert(format!("f{i}"), i as i64);
        }
        let shape = Shape::for_documents(std::slice::from_ref(&doc));
        assert_eq!(shape.len(), MAX_FIELDS);
        assert_eq!(shape.row(doc).len(), MAX_FIELDS);
    }

    #[test]
    fn results_without_id_have_no_id_column() {
        let shape = Shape::for_documents(&[doc! { "total": 5 }]);
        assert_eq!(shape.columns().len(), 1);
    }
}
