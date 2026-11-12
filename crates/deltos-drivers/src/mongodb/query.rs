//! Browse requests as MongoDB filters, sorts and seek conditions, and shell operations as
//! server commands. Everything here is pure, so it is tested without a server.
//!
//! # Seeking
//!
//! Pages are ordered by the sort fields, then `_id`, which is unique and so makes the
//! order total. When the only order is `_id` (the default when browsing), the next page
//! seeks past the last `_id` instead of skipping documents, so reading far into a large
//! collection stays as fast as reading its start.
//!
//! The seek condition has to agree with MongoDB's sort order, which ranks types before
//! values (numbers before strings before ObjectIds before dates, and so on), while a
//! query's `$gt` only matches values of the same type. So "after `v`" is written as
//! "`$gt: v`, or of a type ranked above `v`'s". `_id` can never be an array, which is what
//! makes this exact.
//!
//! Sorting by any other field reads pages with `skip`. Such a field can hold an array in
//! some documents, and MongoDB sorts an array by its smallest (or largest) element while
//! a query compares it element by element; no seek condition matches that order exactly,
//! and an inexact one would skip or repeat documents. `last_key` is `None` then.

use bson::{Bson, Document, doc};
use deltos_data::{BrowseRequest, Error, FilterOp, Result, SortDirection, Value, ValueKind};

use super::convert::{self, Target};
use super::shell::{self, Find};

/// The filter of a browse request: the filter bar's conditions and the typed filter,
/// all of which must hold. With `match_any` the filter bar's conditions are one `$or`.
/// `kind_of` gives a field's grid kind, so text typed into the filter bar is compared as
/// the field's type (an ObjectId, a date, a number).
pub(crate) fn filter(
    request: &BrowseRequest,
    kind_of: impl Fn(&str) -> Option<ValueKind>,
) -> Result<Vec<Document>> {
    let mut conditions = Vec::with_capacity(request.filters.len());
    for f in &request.filters {
        let target = kind_of(&f.column).map_or(Target::Any, Target::for_kind);
        let value = || convert::to_bson(&f.value, target);
        let list = || {
            Bson::Array(
                f.values
                    .iter()
                    .map(|v| convert::to_bson(v, target))
                    .collect(),
            )
        };
        let empty = || {
            Bson::Array(vec![
                Bson::Null,
                Bson::String(String::new()),
                Bson::Array(Vec::new()),
            ])
        };
        let condition = match f.op {
            FilterOp::Eq => value(),
            FilterOp::NotEq => Bson::Document(doc! { "$ne": value() }),
            FilterOp::Lt => Bson::Document(doc! { "$lt": value() }),
            FilterOp::LtEq => Bson::Document(doc! { "$lte": value() }),
            FilterOp::Gt => Bson::Document(doc! { "$gt": value() }),
            FilterOp::GtEq => Bson::Document(doc! { "$gte": value() }),
            FilterOp::Contains => regex(&escaped(&f.value, "", ""), "i"),
            FilterOp::StartsWith => regex(&escaped(&f.value, "^", ""), "i"),
            FilterOp::EndsWith => regex(&escaped(&f.value, "", "$"), "i"),
            // `$not` over a `$regex` document needs MongoDB 4.0.7 or newer.
            FilterOp::NotContains => {
                Bson::Document(doc! { "$not": regex(&escaped(&f.value, "", ""), "i") })
            }
            // `s`: `%` runs across line breaks, as it does in SQL.
            FilterOp::Like => regex(&like_pattern(&f.value.to_string()), "s"),
            FilterOp::In => Bson::Document(doc! { "$in": list() }),
            FilterOp::NotIn => Bson::Document(doc! { "$nin": list() }),
            FilterOp::Between => {
                let [low, high] = f.values.as_slice() else {
                    return Err(Error::Query(format!(
                        "Between on {} needs two values, the low and the high end.",
                        f.column
                    )));
                };
                Bson::Document(doc! {
                    "$gte": convert::to_bson(low, target),
                    "$lte": convert::to_bson(high, target),
                })
            }
            FilterOp::IsNull => Bson::Null,
            FilterOp::IsNotNull => Bson::Document(doc! { "$ne": Bson::Null }),
            FilterOp::IsEmpty => Bson::Document(doc! { "$in": empty() }),
            FilterOp::IsNotEmpty => Bson::Document(doc! { "$nin": empty() }),
        };
        let mut one = Document::new();
        one.insert(f.column.clone(), condition);
        conditions.push(one);
    }
    let mut all = if request.match_any && conditions.len() > 1 {
        vec![doc! { "$or": conditions }]
    } else {
        conditions
    };
    if let Some(raw) = request.raw_where.as_deref().map(str::trim)
        && !raw.is_empty()
    {
        let typed = shell::parse_document(raw)
            .map_err(|e| Error::Query(format!("The filter is not valid: {}", e.describe(raw))))?;
        if !typed.is_empty() {
            all.push(typed);
        }
    }
    Ok(all)
}

/// Conditions joined into one filter document.
pub(crate) fn all_of(mut conditions: Vec<Document>) -> Document {
    match conditions.len() {
        0 => Document::new(),
        1 => conditions.pop().unwrap_or_default(),
        _ => doc! { "$and": conditions },
    }
}

/// A `$regex` condition. Text matches ignore case (`i`), as the SQL drivers' `ILIKE` does.
fn regex(pattern: &str, options: &str) -> Bson {
    Bson::Document(doc! { "$regex": pattern, "$options": options })
}

/// The value's text as a regular expression that matches it literally, between `before`
/// and `after`.
fn escaped(value: &Value, before: &str, after: &str) -> String {
    let text = match value {
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let mut pattern = String::with_capacity(text.len() + 8);
    pattern.push_str(before);
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push_str(after);
    pattern
}

/// A SQL `LIKE` pattern as an anchored regular expression: `%` is any run of characters,
/// `_` one character, and a backslash takes the next character literally.
fn like_pattern(like: &str) -> String {
    let mut pattern = String::with_capacity(like.len() + 8);
    pattern.push('^');
    let mut chars = like.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => pattern.push_str(".*"),
            '_' => pattern.push('.'),
            '\\' => {
                if let Some(next) = chars.next() {
                    if next.is_ascii_punctuation() {
                        pattern.push('\\');
                    }
                    pattern.push(next);
                }
            }
            c => {
                if c.is_ascii_punctuation() {
                    pattern.push('\\');
                }
                pattern.push(c);
            }
        }
    }
    pattern.push('$');
    pattern
}

/// The sort document: the requested fields, then `_id`. The flag says whether the order
/// is by `_id` alone and in which direction, which is when pages can seek.
pub(crate) fn sort(request: &BrowseRequest) -> (Document, Option<bool>) {
    let mut sort = Document::new();
    for s in &request.sort {
        if sort.contains_key(&s.column) {
            continue;
        }
        let direction = match s.direction {
            SortDirection::Ascending => 1,
            SortDirection::Descending => -1,
        };
        sort.insert(s.column.clone(), direction);
    }
    if !sort.contains_key("_id") {
        sort.insert("_id", 1);
    }
    let seek = match (sort.len(), sort.get("_id")) {
        (1, Some(Bson::Int32(direction))) => Some(*direction < 0),
        _ => None,
    };
    (sort, seek)
}

/// The `_id` a `seek_after` key names, as BSON of its own type.
pub(crate) fn seek_value(after: &[(String, Value)]) -> Option<Bson> {
    let (_, value) = after.iter().find(|(name, _)| name == "_id")?;
    match value {
        Value::Null => None,
        Value::Json(text) => convert::parse_json(text),
        Value::Text(text) => {
            let target = if bson::oid::ObjectId::parse_str(text.trim()).is_ok() {
                Target::ObjectId
            } else {
                Target::Any
            };
            Some(convert::to_bson(value, target))
        }
        other => Some(convert::to_bson(other, Target::Any)),
    }
}

/// Documents whose `_id` sorts after `last` (before it when `descending`).
pub(crate) fn seek(last: &Bson, descending: bool) -> Document {
    let rank = convert::sort_rank(last);
    let (op, other_types) = if descending {
        ("$lt", convert::types_ranked_below(rank))
    } else {
        ("$gt", convert::types_ranked_above(rank))
    };
    let mut beyond = Document::new();
    beyond.insert(op, last.clone());
    let mut branches = vec![doc! { "_id": beyond }];
    if !other_types.is_empty() {
        branches.push(doc! { "_id": { "$type": other_types } });
    }
    match branches.len() {
        1 => branches.pop().unwrap_or_default(),
        _ => doc! { "$or": branches },
    }
}

/// A `find` command for a shell `find`, read in batches of `batch`. `comment` tags the
/// operation so it can be found and stopped on the server.
pub(crate) fn find_command(collection: &str, find: &Find, batch: u32, comment: &str) -> Document {
    let mut command = doc! { "find": collection, "filter": find.filter.clone() };
    if let Some(sort) = &find.sort {
        command.insert("sort", sort.clone());
    }
    if let Some(projection) = &find.projection {
        command.insert("projection", projection.clone());
    }
    if let Some(skip) = find.skip.filter(|s| *s > 0) {
        command.insert("skip", i64::try_from(skip).unwrap_or(i64::MAX));
    }
    let mut single_batch = false;
    let limit = if find.one {
        single_batch = true;
        Some(1)
    } else {
        // A negative limit is the shell's way of asking for one batch.
        find.limit.filter(|l| *l != 0).map(|l| {
            single_batch |= l < 0;
            l.saturating_abs()
        })
    };
    if let Some(limit) = limit {
        command.insert("limit", limit);
    }
    let batch = find.batch_size.unwrap_or(batch).max(1);
    let batch = match limit {
        Some(limit) => i64::from(batch).min(limit),
        None => i64::from(batch),
    };
    command.insert("batchSize", batch);
    if single_batch {
        command.insert("singleBatch", true);
    }
    command.insert("comment", comment);
    command
}

/// An `aggregate` command. Options typed after the pipeline (`allowDiskUse`, `collation`,
/// `hint`, `let`, `maxTimeMS`) pass through; `batchSize` goes where the server wants it.
pub(crate) fn aggregate_command(
    target: Bson,
    pipeline: &[Document],
    options: &Document,
    batch: u32,
    comment: &str,
) -> Document {
    let mut command = Document::new();
    command.insert("aggregate", target);
    command.insert("pipeline", pipeline.to_vec());
    let mut batch = i64::from(batch.max(1));
    for (key, value) in options {
        match key.as_str() {
            "batchSize" => {
                if let Some(n) = value.as_i64().or_else(|| value.as_i32().map(i64::from)) {
                    batch = n.max(1);
                }
            }
            "cursor" | "aggregate" | "pipeline" => {}
            _ => {
                command.insert(key.clone(), value.clone());
            }
        }
    }
    command.insert("cursor", doc! { "batchSize": batch });
    if !command.contains_key("comment") {
        command.insert("comment", comment);
    }
    command
}

/// Whether a raw command document answers with a cursor to read in batches.
pub(crate) fn returns_cursor(command: &Document) -> bool {
    matches!(
        command.keys().next().map(String::as_str),
        Some("find" | "aggregate" | "listCollections" | "listIndexes")
    )
}

/// A raw cursor command made ready to read in batches: `aggregate` needs a `cursor`
/// field and the others take a first batch size.
pub(crate) fn prepare_cursor_command(mut command: Document, batch: u32) -> Document {
    let first = command.keys().next().cloned().unwrap_or_default();
    let batch = i64::from(batch.max(1));
    match first.as_str() {
        "aggregate" | "listCollections" | "listIndexes" => {
            if !command.contains_key("cursor") {
                command.insert("cursor", doc! { "batchSize": batch });
            }
        }
        "find" if !command.contains_key("batchSize") => {
            command.insert("batchSize", batch);
        }
        _ => {}
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::oid::ObjectId;
    use deltos_data::{Filter, Sort};

    fn request() -> BrowseRequest {
        BrowseRequest {
            limit: 10,
            ..Default::default()
        }
    }

    #[test]
    fn builds_filters_with_field_types() {
        let mut r = request();
        r.filters = vec![
            Filter {
                column: "_id".into(),
                op: FilterOp::Eq,
                value: Value::text("65a1b2c3d4e5f60718293a4b"),
                values: Vec::new(),
            },
            Filter {
                column: "age".into(),
                op: FilterOp::GtEq,
                value: Value::text("30"),
                values: Vec::new(),
            },
            Filter {
                column: "name".into(),
                op: FilterOp::Contains,
                value: Value::text("a.b"),
                values: Vec::new(),
            },
            Filter {
                column: "gone".into(),
                op: FilterOp::IsNull,
                value: Value::Null,
                values: Vec::new(),
            },
        ];
        r.raw_where = Some("{status: 'open'}".into());
        let kinds = |name: &str| match name {
            "_id" => Some(ValueKind::ObjectId),
            "age" => Some(ValueKind::Int),
            _ => None,
        };
        let all = filter(&r, kinds).expect("filter");
        assert_eq!(all.len(), 5);
        assert!(matches!(all[0].get("_id"), Some(Bson::ObjectId(_))));
        assert_eq!(all[1], doc! { "age": { "$gte": 30_i64 } });
        assert_eq!(
            all[2],
            doc! { "name": { "$regex": "a\\.b", "$options": "i" } }
        );
        assert_eq!(all[3], doc! { "gone": Bson::Null });
        assert_eq!(all[4], doc! { "status": "open" });
        let joined = all_of(all);
        assert!(joined.contains_key("$and"));
        assert_eq!(all_of(Vec::new()), Document::new());
    }

    #[test]
    fn builds_every_operator() {
        let mut r = request();
        r.filters = vec![
            Filter::new("name", FilterOp::NotContains, Value::text("x.")),
            Filter::new("name", FilterOp::EndsWith, Value::text("son")),
            Filter::list(
                "age",
                FilterOp::In,
                vec![Value::text("1"), Value::text("2")],
            ),
            Filter::list("age", FilterOp::NotIn, vec![Value::Int(3)]),
            Filter::list(
                "age",
                FilterOp::Between,
                vec![Value::text("10"), Value::text("20")],
            ),
            Filter::new("code", FilterOp::Like, Value::text("A_%.\\%")),
            Filter::new("note", FilterOp::IsEmpty, Value::Null),
            Filter::new("note", FilterOp::IsNotEmpty, Value::Null),
        ];
        let kinds = |name: &str| (name == "age").then_some(ValueKind::Int);
        let all = filter(&r, kinds).expect("filter");
        assert_eq!(
            all[0],
            doc! { "name": { "$not": { "$regex": "x\\.", "$options": "i" } } }
        );
        assert_eq!(
            all[1],
            doc! { "name": { "$regex": "son$", "$options": "i" } }
        );
        assert_eq!(all[2], doc! { "age": { "$in": [1_i64, 2_i64] } });
        assert_eq!(all[3], doc! { "age": { "$nin": [3_i64] } });
        assert_eq!(all[4], doc! { "age": { "$gte": 10_i64, "$lte": 20_i64 } });
        assert_eq!(
            all[5],
            doc! { "code": { "$regex": "^A..*\\.\\%$", "$options": "s" } }
        );
        assert_eq!(all[6], doc! { "note": { "$in": [Bson::Null, "", []] } });
        assert_eq!(all[7], doc! { "note": { "$nin": [Bson::Null, "", []] } });

        r.filters = vec![Filter::list("age", FilterOp::Between, vec![Value::Int(1)])];
        assert!(filter(&r, kinds).is_err());
    }

    #[test]
    fn any_puts_conditions_in_one_or() {
        let mut r = request();
        r.match_any = true;
        r.filters = vec![
            Filter::new("a", FilterOp::Eq, Value::Int(1)),
            Filter::new("b", FilterOp::Eq, Value::Int(2)),
        ];
        r.raw_where = Some("{c: 3}".into());
        let all = filter(&r, |_| None).expect("filter");
        assert_eq!(
            all,
            vec![
                doc! { "$or": [{ "a": 1_i64 }, { "b": 2_i64 }] },
                doc! { "c": 3 },
            ]
        );
        r.filters.pop();
        r.raw_where = None;
        assert_eq!(
            filter(&r, |_| None).expect("filter"),
            vec![doc! { "a": 1_i64 }]
        );
    }

    #[test]
    fn reports_bad_typed_filters() {
        let mut r = request();
        r.raw_where = Some("{status: ".into());
        let err = filter(&r, |_| None).expect_err("bad filter");
        assert!(err.to_string().contains("not valid"), "{err}");
        r.raw_where = Some("   ".into());
        assert!(filter(&r, |_| None).expect("blank").is_empty());
    }

    #[test]
    fn sorts_with_id_last() {
        let mut r = request();
        assert_eq!(sort(&r), (doc! { "_id": 1 }, Some(false)));
        r.sort = vec![Sort {
            column: "_id".into(),
            direction: SortDirection::Descending,
        }];
        assert_eq!(sort(&r), (doc! { "_id": -1 }, Some(true)));
        r.sort.insert(
            0,
            Sort {
                column: "age".into(),
                direction: SortDirection::Ascending,
            },
        );
        assert_eq!(sort(&r), (doc! { "age": 1, "_id": -1 }, None));
    }

    #[test]
    fn seeks_across_types() {
        let oid = ObjectId::new();
        let after = seek(&Bson::ObjectId(oid), false);
        let branches = after.get_array("$or").expect("or");
        assert_eq!(branches[0], Bson::Document(doc! { "_id": { "$gt": oid } }));
        let types = branches[1]
            .as_document()
            .and_then(|d| d.get_document("_id").ok())
            .and_then(|d| d.get_array("$type").ok())
            .expect("types");
        assert!(types.contains(&Bson::String("date".into())));
        assert!(!types.contains(&Bson::String("string".into())));
        let before = seek(&Bson::String("m".into()), true);
        let branches = before.get_array("$or").expect("or");
        let types = branches[1].as_document().expect("doc").to_string();
        assert!(
            types.contains("double") && types.contains("null"),
            "{types}"
        );
        assert_eq!(
            seek(&Bson::MaxKey, false),
            doc! { "_id": { "$gt": Bson::MaxKey } }
        );
    }

    #[test]
    fn reads_seek_keys() {
        let oid = ObjectId::new();
        let canonical = convert::canonical_text(Bson::ObjectId(oid));
        let key = vec![("_id".to_string(), Value::Json(canonical.into()))];
        assert_eq!(seek_value(&key), Some(Bson::ObjectId(oid)));
        let key = vec![("_id".to_string(), Value::text(oid.to_hex()))];
        assert_eq!(seek_value(&key), Some(Bson::ObjectId(oid)));
        let key = vec![("_id".to_string(), Value::Int(5))];
        assert_eq!(seek_value(&key), Some(Bson::Int64(5)));
        assert_eq!(seek_value(&[]), None);
    }

    #[test]
    fn builds_find_commands() {
        let find = Find {
            filter: doc! { "a": 1 },
            sort: Some(doc! { "b": -1 }),
            limit: Some(-5),
            skip: Some(2),
            ..Default::default()
        };
        let command = find_command("orders", &find, 1000, "tag");
        assert_eq!(
            command,
            doc! {
                "find": "orders", "filter": { "a": 1 }, "sort": { "b": -1 }, "skip": 2_i64,
                "limit": 5_i64, "batchSize": 5_i64, "singleBatch": true, "comment": "tag",
            }
        );
        let one = Find {
            one: true,
            ..Default::default()
        };
        let command = find_command("orders", &one, 1000, "tag");
        assert_eq!(command.get_i64("limit").ok(), Some(1));
    }

    #[test]
    fn builds_aggregate_commands() {
        let command = aggregate_command(
            Bson::String("orders".into()),
            &[doc! { "$match": {} }],
            &doc! { "allowDiskUse": true, "batchSize": 7 },
            1000,
            "tag",
        );
        assert_eq!(command.get_bool("allowDiskUse").ok(), Some(true));
        assert_eq!(
            command.get_document("cursor").ok(),
            Some(&doc! { "batchSize": 7_i64 })
        );
        assert!(returns_cursor(&command));
        assert!(!returns_cursor(&doc! { "ping": 1 }));
        let raw = prepare_cursor_command(doc! { "aggregate": 1, "pipeline": [] }, 50);
        assert!(raw.contains_key("cursor"));
    }
}
