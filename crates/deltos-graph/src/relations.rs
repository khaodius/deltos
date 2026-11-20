//! How objects relate, which is the part of the graph that depends on the kind of
//! database.
//!
//! A [`RelationSource`] reads one described object and returns the lines to draw from or
//! to it. SQL databases declare their relations as foreign keys, which [`ForeignKeys`]
//! reads. Document stores declare nothing, so [`DocumentReferences`] reads the convention
//! most schemas follow instead: a field named after another collection, holding its ids
//! (`user_id`, `authorId`, `tag_ids`). A new kind of database plugs in by returning its
//! own source from [`for_kind`]: a wide-column store's lookup tables, a graph store's
//! edges, and so on.

use std::collections::HashSet;

use deltos_data::{DriverKind, ObjectInfo, ObjectRef, ValueKind};

use crate::Layout;

/// A line between two objects.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Relation {
    /// The object holding the reference, and its columns that hold it.
    pub from: ObjectRef,
    pub from_columns: Vec<String>,
    /// The object referred to, and the columns the reference matches.
    pub to: ObjectRef,
    pub to_columns: Vec<String>,
    /// Read from names rather than declared by the database.
    pub inferred: bool,
}

/// The objects on the plane, for sources that resolve a name to an object.
pub trait Catalog {
    /// The object called `name` in `namespace`, ignoring case.
    fn find(&self, namespace: &[String], name: &str) -> Option<&ObjectRef>;
}

impl Catalog for Layout {
    fn find(&self, namespace: &[String], name: &str) -> Option<&ObjectRef> {
        Layout::find(self, namespace, name).map(|i| &self.slots[i].object)
    }
}

/// Reads the relations of one described object.
pub trait RelationSource: Send + Sync {
    fn relations(
        &self,
        object: &ObjectRef,
        info: &ObjectInfo,
        catalog: &dyn Catalog,
    ) -> Vec<Relation>;
}

/// Declared foreign keys, both those the object holds and, where the database reports
/// them, those that point at it. Targets are resolved through the catalog so a key
/// reported with another object kind or another case still meets its card.
#[derive(Debug, Clone, Copy, Default)]
pub struct ForeignKeys;

impl RelationSource for ForeignKeys {
    fn relations(
        &self,
        object: &ObjectRef,
        info: &ObjectInfo,
        catalog: &dyn Catalog,
    ) -> Vec<Relation> {
        let resolve = |r: &ObjectRef| {
            catalog
                .find(&r.namespace, &r.name)
                .cloned()
                .unwrap_or_else(|| r.clone())
        };
        let outgoing = info.foreign_keys.iter().map(|fk| Relation {
            from: object.clone(),
            from_columns: fk.columns.clone(),
            to: resolve(&fk.target),
            to_columns: fk.target_columns.clone(),
            inferred: false,
        });
        // Seen from this side: `columns` are ours, `target` refers to us.
        let incoming = info.referenced_by.iter().map(|fk| Relation {
            from: resolve(&fk.target),
            from_columns: fk.target_columns.clone(),
            to: object.clone(),
            to_columns: fk.columns.clone(),
            inferred: false,
        });
        outgoing.chain(incoming).collect()
    }
}

/// References read from field names, for databases that do not declare them.
#[derive(Debug, Clone, Copy, Default)]
pub struct DocumentReferences;

impl DocumentReferences {
    /// The name a reference field is built on: `user` for `user_id`, `userId`, `userID`,
    /// `user_ids` and `userIds`. A nested field counts by its last part.
    fn base(field: &str) -> Option<&str> {
        let last = field.rsplit('.').next().unwrap_or(field);
        for suffix in ["_ids", "Ids", "IDs", "_id", "Id", "ID"] {
            if let Some(base) = last.strip_suffix(suffix)
                && !base.is_empty()
                && base != "_"
            {
                return Some(base.trim_end_matches('_'));
            }
        }
        None
    }

    /// Collection names a base may refer to: itself, its plural forms, and the snake
    /// case of a camel case base.
    fn candidates(base: &str) -> Vec<String> {
        let mut forms = vec![base.to_owned()];
        let snake = to_snake(base);
        if snake != base {
            forms.push(snake);
        }
        let mut out = Vec::new();
        for f in forms {
            out.push(format!("{f}s"));
            out.push(format!("{f}es"));
            if let Some(stem) = f.strip_suffix('y') {
                out.push(format!("{stem}ies"));
            }
            out.push(f);
        }
        out
    }
}

impl RelationSource for DocumentReferences {
    fn relations(
        &self,
        object: &ObjectRef,
        info: &ObjectInfo,
        catalog: &dyn Catalog,
    ) -> Vec<Relation> {
        let mut out = Vec::new();
        for c in &info.columns {
            if c.name == "_id" || !may_hold_ids(c.kind) {
                continue;
            }
            let Some(base) = Self::base(&c.name) else {
                continue;
            };
            let target = Self::candidates(base)
                .iter()
                .find_map(|name| catalog.find(&object.namespace, name));
            if let Some(target) = target
                && target != object
            {
                out.push(Relation {
                    from: object.clone(),
                    from_columns: vec![c.name.clone()],
                    to: target.clone(),
                    to_columns: vec!["_id".into()],
                    inferred: true,
                });
            }
        }
        out
    }
}

fn may_hold_ids(kind: ValueKind) -> bool {
    matches!(
        kind,
        ValueKind::ObjectId
            | ValueKind::Uuid
            | ValueKind::Int
            | ValueKind::Text
            | ValueKind::Array
            | ValueKind::Other
    )
}

fn to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.char_indices() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Several sources in turn.
#[derive(Default)]
pub struct Relations(pub Vec<Box<dyn RelationSource>>);

impl RelationSource for Relations {
    fn relations(
        &self,
        object: &ObjectRef,
        info: &ObjectInfo,
        catalog: &dyn Catalog,
    ) -> Vec<Relation> {
        let mut seen = HashSet::new();
        self.0
            .iter()
            .flat_map(|s| s.relations(object, info, catalog))
            .filter(|r| seen.insert(r.clone()))
            .collect()
    }
}

/// The relation sources for a kind of database.
pub fn for_kind(kind: DriverKind) -> Box<dyn RelationSource> {
    if kind.is_document() {
        // Declared keys are empty there today, but a driver may report them later.
        Box::new(Relations(vec![
            Box::new(ForeignKeys),
            Box::new(DocumentReferences),
        ]))
    } else {
        Box::new(ForeignKeys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Listing;
    use deltos_data::{Column, ForeignKey, ObjectEntry, ObjectKind};

    fn obj(ns: &[&str], name: &str, kind: ObjectKind) -> ObjectRef {
        ObjectRef {
            namespace: ns.iter().map(|s| s.to_string()).collect(),
            name: name.into(),
            kind,
        }
    }

    fn layout(ns: &[&str], names: &[&str], kind: ObjectKind) -> Layout {
        let entries: Vec<ObjectEntry> = names
            .iter()
            .map(|n| ObjectEntry {
                object: obj(ns, n, kind),
                estimated_rows: None,
                size_bytes: None,
                comment: None,
            })
            .collect();
        let path: Vec<String> = ns.iter().map(|s| s.to_string()).collect();
        Layout::build([Listing {
            path: &path,
            objects: Some(&entries),
        }])
    }

    fn fk(columns: &[&str], target: ObjectRef, target_columns: &[&str]) -> ForeignKey {
        ForeignKey {
            name: "fk".into(),
            columns: columns.iter().map(|s| s.to_string()).collect(),
            target,
            target_columns: target_columns.iter().map(|s| s.to_string()).collect(),
            on_delete: Default::default(),
            on_update: Default::default(),
        }
    }

    #[test]
    fn foreign_keys_both_ways() {
        let l = layout(
            &["db", "public"],
            &["orders", "users", "items"],
            ObjectKind::Table,
        );
        let orders = obj(&["db", "public"], "orders", ObjectKind::Table);
        // Reported as a view and in another case: resolved to the card.
        let users_as_reported = obj(&["db", "public"], "Users", ObjectKind::View);
        let info = ObjectInfo {
            foreign_keys: vec![fk(&["user_id"], users_as_reported, &["id"])],
            referenced_by: vec![fk(
                &["id"],
                obj(&["db", "public"], "items", ObjectKind::Table),
                &["order_id"],
            )],
            ..Default::default()
        };
        let rels = ForeignKeys.relations(&orders, &info, &l);
        assert_eq!(rels.len(), 2);
        assert_eq!(rels[0].from, orders);
        assert_eq!(
            rels[0].to,
            obj(&["db", "public"], "users", ObjectKind::Table)
        );
        assert_eq!(rels[0].from_columns, ["user_id"]);
        assert_eq!(rels[1].from.name, "items");
        assert_eq!(rels[1].from_columns, ["order_id"]);
        assert_eq!(rels[1].to, orders);
        assert_eq!(rels[1].to_columns, ["id"]);
        assert!(rels.iter().all(|r| !r.inferred));
    }

    #[test]
    fn document_references_from_field_names() {
        let ns = ["shop"];
        let l = layout(
            &ns,
            &[
                "users",
                "categories",
                "boxes",
                "order_lines",
                "orders",
                "tag",
            ],
            ObjectKind::Collection,
        );
        let orders = obj(&ns, "orders", ObjectKind::Collection);
        let col = |name: &str, kind| Column::new(name, "x", kind);
        let info = ObjectInfo {
            columns: vec![
                col("_id", ValueKind::ObjectId),
                col("user_id", ValueKind::ObjectId),
                col("categoryId", ValueKind::ObjectId),
                col("box_ids", ValueKind::Array),
                col("orderLineIds", ValueKind::Array),
                col("meta.tagId", ValueKind::Text),
                // Not references: unknown targets, numbers that are not ids, itself.
                col("planet_id", ValueKind::ObjectId),
                col("price_id", ValueKind::Float),
                col("order_id", ValueKind::ObjectId),
                col("id", ValueKind::Int),
            ],
            ..Default::default()
        };
        let rels = DocumentReferences.relations(&orders, &info, &l);
        let found: Vec<(&str, &str)> = rels
            .iter()
            .map(|r| (r.from_columns[0].as_str(), r.to.name.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("user_id", "users"),
                ("categoryId", "categories"),
                ("box_ids", "boxes"),
                ("orderLineIds", "order_lines"),
                ("meta.tagId", "tag"),
            ]
        );
        assert!(rels.iter().all(|r| r.inferred && r.to_columns == ["_id"]));
    }

    #[test]
    fn sources_by_kind_and_without_duplicates() {
        let l = layout(&["shop"], &["users", "orders"], ObjectKind::Collection);
        let orders = obj(&["shop"], "orders", ObjectKind::Collection);
        let users = obj(&["shop"], "users", ObjectKind::Collection);
        let info = ObjectInfo {
            columns: vec![Column::new("user_id", "objectId", ValueKind::ObjectId)],
            ..Default::default()
        };
        assert_eq!(
            for_kind(DriverKind::Mongodb)
                .relations(&orders, &info, &l)
                .len(),
            1
        );
        assert!(
            for_kind(DriverKind::Postgres)
                .relations(&orders, &info, &l)
                .is_empty()
        );
        let twice = Relations(vec![
            Box::new(DocumentReferences),
            Box::new(DocumentReferences),
        ]);
        let rels = twice.relations(&orders, &info, &l);
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].to, users);
    }

    #[test]
    fn bases() {
        assert_eq!(DocumentReferences::base("user_id"), Some("user"));
        assert_eq!(DocumentReferences::base("userID"), Some("user"));
        assert_eq!(DocumentReferences::base("a.b.ownerIds"), Some("owner"));
        assert_eq!(DocumentReferences::base("_id"), None);
        assert_eq!(DocumentReferences::base("Id"), None);
        assert_eq!(DocumentReferences::base("name"), None);
        assert_eq!(to_snake("orderLine"), "order_line");
    }
}
