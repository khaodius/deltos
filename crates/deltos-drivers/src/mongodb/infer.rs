//! The field tree of a sample of documents, for [`Connection::schema`].
//!
//! Every field path seen is a node, with the types it held and how often. Embedded
//! documents add their fields as children, and so do documents inside arrays, whose
//! fields MongoDB queries by the same dotted path (`items.sku`). Arrays inside arrays
//! count their element types but are not walked further.
//!
//! The tree is bounded whatever the documents hold: past [`MAX_DEPTH`] levels, more than
//! [`MAX_CHILDREN`] fields under one parent, or [`MAX_NODES`] fields in all, fields are
//! left out and the schema says it was truncated. Documents keyed by data (a field per
//! user id) would otherwise grow it without end.
//!
//! [`Connection::schema`]: deltos_data::Connection::schema

use std::collections::HashMap;

use bson::{Bson, Document};
use deltos_data::{ObjectSchema, SchemaField, SchemaType, ValueKind};

use super::convert;

pub(crate) const MAX_DEPTH: usize = 8;
pub(crate) const MAX_CHILDREN: usize = 300;
pub(crate) const MAX_NODES: usize = 5_000;

#[derive(Default)]
struct Node {
    name: String,
    path: String,
    types: Counts,
    items: Counts,
    present: u64,
    /// Documents found at this field (itself, or as array elements): what its children
    /// are counted against.
    docs: u64,
    children: Vec<Node>,
    index: HashMap<String, usize>,
}

/// Type counts in first-seen order; a field rarely holds more than a few types.
#[derive(Default)]
struct Counts(Vec<(&'static str, ValueKind, u64)>);

impl Counts {
    fn add(&mut self, bson: &Bson) {
        let name = convert::type_name(bson);
        match self.0.iter_mut().find(|(n, _, _)| *n == name) {
            Some((_, _, count)) => *count += 1,
            None => self.0.push((name, convert::kind_of(bson), 1)),
        }
    }

    fn into_types(self) -> Vec<SchemaType> {
        let mut types: Vec<SchemaType> = self
            .0
            .into_iter()
            .map(|(name, kind, count)| SchemaType {
                name: name.to_owned(),
                kind,
                count,
            })
            .collect();
        // Stable, so ties keep first-seen order.
        types.sort_by(|a, b| b.count.cmp(&a.count));
        types
    }
}

/// Builds the tree one document at a time, so a sample never has to be held whole.
pub(crate) struct Inference {
    root: Node,
    nodes: usize,
    truncated: bool,
}

impl Default for Inference {
    fn default() -> Self {
        Self::new()
    }
}

impl Inference {
    pub(crate) fn new() -> Self {
        Self {
            root: Node::default(),
            nodes: 0,
            truncated: false,
        }
    }

    pub(crate) fn add(&mut self, doc: &Document) {
        self.root.docs += 1;
        let mut budget = Budget {
            nodes: &mut self.nodes,
            truncated: &mut self.truncated,
        };
        add_document(&mut self.root, doc, 0, &mut budget);
    }

    pub(crate) fn sampled(&self) -> u64 {
        self.root.docs
    }

    pub(crate) fn finish(self, total: Option<u64>) -> ObjectSchema {
        let sampled = self.root.docs;
        ObjectSchema {
            sampled,
            total,
            declared: false,
            truncated: self.truncated,
            fields: self
                .root
                .children
                .into_iter()
                .map(|n| field(n, sampled, true))
                .collect(),
        }
    }
}

struct Budget<'a> {
    nodes: &'a mut usize,
    truncated: &'a mut bool,
}

fn add_document(parent: &mut Node, doc: &Document, depth: usize, budget: &mut Budget) {
    for (name, value) in doc {
        let Some(child) = child(parent, name, depth, budget) else {
            continue;
        };
        child.types.add(value);
        if !matches!(value, Bson::Null | Bson::Undefined) {
            child.present += 1;
        }
        match value {
            Bson::Document(inner) => {
                child.docs += 1;
                add_document(child, inner, depth + 1, budget);
            }
            Bson::Array(items) => {
                for item in items {
                    child.items.add(item);
                    if let Bson::Document(inner) = item {
                        child.docs += 1;
                        add_document(child, inner, depth + 1, budget);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The child `name` of `parent`, made when new and the budget allows.
fn child<'a>(
    parent: &'a mut Node,
    name: &str,
    depth: usize,
    budget: &mut Budget,
) -> Option<&'a mut Node> {
    if let Some(&i) = parent.index.get(name) {
        return parent.children.get_mut(i);
    }
    if depth >= MAX_DEPTH || parent.children.len() >= MAX_CHILDREN || *budget.nodes >= MAX_NODES {
        *budget.truncated = true;
        return None;
    }
    *budget.nodes += 1;
    let path = if parent.path.is_empty() {
        name.to_owned()
    } else {
        format!("{}.{name}", parent.path)
    };
    let i = parent.children.len();
    parent.children.push(Node {
        name: name.to_owned(),
        path,
        ..Node::default()
    });
    parent.index.insert(name.to_owned(), i);
    parent.children.get_mut(i)
}

fn field(node: Node, of: u64, top: bool) -> SchemaField {
    let docs = node.docs;
    SchemaField {
        key: top && node.name == "_id",
        name: node.name,
        path: node.path,
        types: node.types.into_types(),
        items: node.items.into_types(),
        present: node.present,
        of,
        fields: node
            .children
            .into_iter()
            .map(|n| field(n, docs, false))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    fn infer(docs: &[Document]) -> ObjectSchema {
        let mut inference = Inference::new();
        for d in docs {
            inference.add(d);
        }
        inference.finish(Some(10))
    }

    #[test]
    fn nests_documents_and_arrays_of_documents() {
        let schema = infer(&[
            doc! { "_id": 1, "name": "a", "address": { "city": "x", "zip": 1 } },
            doc! { "_id": 2, "name": Bson::Null, "address": { "city": "y" }, "items": [{ "sku": "p", "qty": 1 }, { "sku": "q" }] },
            doc! { "_id": 3, "name": 5, "items": [1, "two"] },
        ]);
        assert_eq!(schema.sampled, 3);
        assert_eq!(schema.total, Some(10));
        assert!(!schema.declared && !schema.truncated);
        let top: Vec<&str> = schema.fields.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(top, ["_id", "name", "address", "items"]);
        assert!(schema.fields[0].key);
        let name = schema.field("name").unwrap();
        assert_eq!(name.present, 2);
        assert_eq!(name.of, 3);
        let types: Vec<(&str, u64)> = name
            .types
            .iter()
            .map(|t| (t.name.as_str(), t.count))
            .collect();
        assert_eq!(types, [("string", 1), ("null", 1), ("int", 1)]);
        let zip = schema.field("address.zip").unwrap();
        assert_eq!((zip.present, zip.of), (1, 2));
        assert!(!zip.key);
        let items = schema.field("items").unwrap();
        assert_eq!(items.main_type(), "array");
        let element: Vec<(&str, u64)> = items
            .items
            .iter()
            .map(|t| (t.name.as_str(), t.count))
            .collect();
        assert_eq!(element, [("object", 2), ("int", 1), ("string", 1)]);
        let sku = schema.field("items.sku").unwrap();
        assert_eq!((sku.present, sku.of), (2, 2));
        let qty = schema.field("items.qty").unwrap();
        assert_eq!(qty.share(), 0.5);
    }

    #[test]
    fn stays_bounded() {
        let mut wide = Document::new();
        for i in 0..MAX_CHILDREN + 10 {
            wide.insert(format!("k{i}"), i as i32);
        }
        let schema = infer(&[wide]);
        assert!(schema.truncated);
        assert_eq!(schema.fields.len(), MAX_CHILDREN);

        let mut deep = doc! { "leaf": 1 };
        for _ in 0..MAX_DEPTH + 5 {
            deep = doc! { "d": deep };
        }
        let schema = infer(&[deep]);
        assert!(schema.truncated);
        let depth = schema.walk().map(|f| f.path.matches('.').count() + 1).max();
        assert_eq!(depth, Some(MAX_DEPTH));

        let mut inference = Inference::new();
        for n in 0..(MAX_NODES / MAX_CHILDREN + 2) {
            let mut d = Document::new();
            for i in 0..MAX_CHILDREN {
                d.insert(format!("k{n}_{i}"), 1);
            }
            inference.add(&doc! { format!("g{n}"): d });
        }
        let schema = inference.finish(None);
        assert!(schema.truncated);
        assert!(schema.walk().count() <= MAX_NODES);
    }
}
