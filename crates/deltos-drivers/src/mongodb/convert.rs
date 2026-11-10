//! BSON to grid values and back.
//!
//! Scalars map onto [`Value`] directly. Documents, arrays and the BSON types that have no
//! cell form of their own (timestamps, regular expressions, `MinKey`) become
//! [`Value::Json`] holding relaxed extended JSON, which reads naturally and parses back to
//! the same BSON. An ObjectId is its hex text with [`ValueKind::ObjectId`], a date is
//! RFC 3339 text with [`ValueKind::DateTime`], a decimal is its exact text.
//!
//! Going back, text typed into a cell is read as the type the field already has, so
//! editing a date or an ObjectId keeps it a date or an ObjectId instead of turning it into
//! a string.

use bson::oid::ObjectId;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson, Decimal128};
use deltos_data::{Value, ValueKind};

use super::shell;
use crate::util;

/// A BSON value as a cell.
pub(crate) fn to_value(bson: Bson) -> Value {
    match bson {
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Boolean(b) => Value::Bool(b),
        Bson::Int32(i) => Value::Int(i64::from(i)),
        Bson::Int64(i) => Value::Int(i),
        Bson::Double(f) => Value::Float(f),
        Bson::String(s) | Bson::Symbol(s) | Bson::JavaScriptCode(s) => Value::text(s),
        Bson::ObjectId(oid) => Value::text(oid.to_hex()),
        Bson::Decimal128(d) => Value::text(d.to_string()),
        Bson::DateTime(dt) => match dt.try_to_rfc3339_string() {
            Ok(text) => Value::text(text),
            // Dates outside years 0 to 9999 have no RFC 3339 form.
            Err(_) => Value::Json(json_text(Bson::DateTime(dt)).into()),
        },
        Bson::Binary(binary) => match uuid_text(&binary) {
            Some(text) => Value::text(text),
            None if binary.subtype == BinarySubtype::Generic => {
                Value::Bytes(binary.bytes.into_boxed_slice())
            }
            None => Value::Json(json_text(Bson::Binary(binary)).into()),
        },
        other => Value::Json(json_text(other).into()),
    }
}

/// What a column of this BSON type holds in the grid.
pub(crate) fn kind_of(bson: &Bson) -> ValueKind {
    match bson {
        Bson::Boolean(_) => ValueKind::Bool,
        Bson::Int32(_) | Bson::Int64(_) => ValueKind::Int,
        Bson::Double(_) => ValueKind::Float,
        Bson::Decimal128(_) => ValueKind::Decimal,
        Bson::String(_) | Bson::Symbol(_) => ValueKind::Text,
        Bson::ObjectId(_) => ValueKind::ObjectId,
        Bson::DateTime(_) => ValueKind::DateTime,
        Bson::Document(_) => ValueKind::Json,
        Bson::Array(_) => ValueKind::Array,
        Bson::Binary(b) if uuid_text(b).is_some() => ValueKind::Uuid,
        Bson::Binary(b) if b.subtype == BinarySubtype::Generic => ValueKind::Bytes,
        _ => ValueKind::Other,
    }
}

/// The type's name as the shell's `$type` operator spells it.
pub(crate) fn type_name(bson: &Bson) -> &'static str {
    match bson {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(b) if uuid_text(b).is_some() => "uuid",
        Bson::Binary(_) => "binData",
        Bson::Undefined => "undefined",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::DbPointer(_) => "dbPointer",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::Symbol(_) => "symbol",
        Bson::JavaScriptCodeWithScope(_) => "javascriptWithScope",
        Bson::Int32(_) => "int",
        Bson::Timestamp(_) => "timestamp",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        Bson::MinKey => "minKey",
        Bson::MaxKey => "maxKey",
    }
}

/// Compact relaxed extended JSON, the form shown in cells.
pub(crate) fn json_text(bson: Bson) -> String {
    bson.into_relaxed_extjson().to_string()
}

/// Indented relaxed extended JSON, for definitions shown to the person.
pub(crate) fn pretty_json(bson: Bson) -> String {
    let json = bson.into_relaxed_extjson();
    serde_json::to_string_pretty(&json).unwrap_or_else(|_| json.to_string())
}

/// Canonical extended JSON keeps every type exactly, so a key handed out in
/// [`deltos_data::BrowsePage::last_key`] comes back as the same BSON.
pub(crate) fn canonical_text(bson: Bson) -> String {
    bson.into_canonical_extjson().to_string()
}

/// Reads JSON or shell syntax (`ObjectId("…")`, unquoted keys) as BSON.
pub(crate) fn parse_json(text: &str) -> Option<Bson> {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(text)
        && let Ok(bson) = Bson::try_from(json)
    {
        return Some(bson);
    }
    shell::parse_value(text).ok()
}

/// The BSON type a value should become, from the field's current value or the column's
/// kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Int32,
    Int64,
    Double,
    Decimal,
    Bool,
    ObjectId,
    Date,
    Uuid,
    Binary,
    /// Text stays text; other values keep their own type.
    Any,
}

impl Target {
    pub(crate) fn of(bson: &Bson) -> Self {
        match bson {
            Bson::Int32(_) => Target::Int32,
            Bson::Int64(_) => Target::Int64,
            Bson::Double(_) => Target::Double,
            Bson::Decimal128(_) => Target::Decimal,
            Bson::Boolean(_) => Target::Bool,
            Bson::ObjectId(_) => Target::ObjectId,
            Bson::DateTime(_) => Target::Date,
            Bson::Binary(b) if uuid_text(b).is_some() => Target::Uuid,
            Bson::Binary(_) => Target::Binary,
            _ => Target::Any,
        }
    }

    pub(crate) fn for_kind(kind: ValueKind) -> Self {
        match kind {
            ValueKind::Int => Target::Int64,
            ValueKind::Float => Target::Double,
            ValueKind::Decimal => Target::Decimal,
            ValueKind::Bool => Target::Bool,
            ValueKind::ObjectId => Target::ObjectId,
            ValueKind::DateTime | ValueKind::Date => Target::Date,
            ValueKind::Uuid => Target::Uuid,
            ValueKind::Bytes => Target::Binary,
            _ => Target::Any,
        }
    }
}

/// A cell value as BSON of the target type. Text that does not read as the target type
/// stays a string: a document database takes whatever it is given, and refusing would
/// leave the person no way to store it.
pub(crate) fn to_bson(value: &Value, target: Target) -> Bson {
    match value {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Int(i) => match target {
            Target::Int32 => i32::try_from(*i).map_or(Bson::Int64(*i), Bson::Int32),
            Target::Double => Bson::Double(*i as f64),
            Target::Decimal => decimal(&i.to_string()).unwrap_or(Bson::Int64(*i)),
            _ => Bson::Int64(*i),
        },
        Value::Float(f) => match target {
            Target::Int32 | Target::Int64 if f.fract() == 0.0 && f.abs() < 9.0e15 => {
                let i = *f as i64;
                match (target, i32::try_from(i)) {
                    (Target::Int32, Ok(small)) => Bson::Int32(small),
                    _ => Bson::Int64(i),
                }
            }
            Target::Decimal => decimal(&util::float_text(*f)).unwrap_or(Bson::Double(*f)),
            _ => Bson::Double(*f),
        },
        Value::Text(text) => text_to_bson(text, target),
        Value::Json(text) => parse_json(text).unwrap_or_else(|| Bson::String(text.to_string())),
        Value::Bytes(bytes) => {
            let subtype = if target == Target::Uuid && bytes.len() == 16 {
                BinarySubtype::Uuid
            } else {
                BinarySubtype::Generic
            };
            Bson::Binary(Binary {
                subtype,
                bytes: bytes.to_vec(),
            })
        }
    }
}

fn text_to_bson(text: &str, target: Target) -> Bson {
    let trimmed = text.trim();
    let converted = match target {
        Target::Int32 => trimmed
            .parse::<i64>()
            .ok()
            .map(|i| i32::try_from(i).map_or(Bson::Int64(i), Bson::Int32)),
        Target::Int64 => trimmed.parse::<i64>().ok().map(Bson::Int64),
        Target::Double => trimmed.parse::<f64>().ok().map(Bson::Double),
        Target::Decimal => decimal(trimmed),
        Target::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" => Some(Bson::Boolean(true)),
            "false" => Some(Bson::Boolean(false)),
            _ => None,
        },
        Target::ObjectId => ObjectId::parse_str(trimmed).ok().map(Bson::ObjectId),
        Target::Date => shell::parse_date(trimmed).map(Bson::DateTime),
        Target::Uuid => parse_uuid(trimmed),
        Target::Binary => util::parse_hex(trimmed).map(|bytes| {
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes,
            })
        }),
        Target::Any => None,
    };
    converted.unwrap_or_else(|| Bson::String(text.to_string()))
}

/// Every BSON value text typed as a key could stand for: the string itself, and an
/// ObjectId or UUID when it reads as one. A grid cell shows an ObjectId and a string of
/// the same hex alike, so lookups by key try both.
pub(crate) fn key_candidates(value: &Value) -> Vec<Bson> {
    match value {
        Value::Text(text) => {
            let mut out = Vec::with_capacity(2);
            if let Ok(oid) = ObjectId::parse_str(text.trim()) {
                out.push(Bson::ObjectId(oid));
            }
            if let Some(uuid) = parse_uuid(text.trim()) {
                out.push(uuid);
            }
            if let Some(date) = shell::parse_date(text.trim()).filter(|_| text.contains('T')) {
                out.push(Bson::DateTime(date));
            }
            out.push(Bson::String(text.to_string()));
            out
        }
        other => vec![to_bson(other, Target::Any)],
    }
}

fn decimal(text: &str) -> Option<Bson> {
    text.parse::<Decimal128>().ok().map(Bson::Decimal128)
}

/// A UUID in its usual 8-4-4-4-12 form, or as 32 hex digits, as BSON binary subtype 4.
pub(crate) fn parse_uuid(text: &str) -> Option<Bson> {
    let digits: String = text.chars().filter(|c| *c != '-').collect();
    if digits.len() != 32 || (text.len() != 32 && text.len() != 36) {
        return None;
    }
    if text.len() == 36 {
        let dashes: Vec<usize> = text.match_indices('-').map(|(i, _)| i).collect();
        if dashes != [8, 13, 18, 23] {
            return None;
        }
    }
    let bytes = util::parse_hex(&digits)?;
    Some(Bson::Binary(Binary {
        subtype: BinarySubtype::Uuid,
        bytes,
    }))
}

fn uuid_text(binary: &Binary) -> Option<String> {
    if binary.subtype != BinarySubtype::Uuid || binary.bytes.len() != 16 {
        return None;
    }
    let mut hex = String::with_capacity(32);
    util::push_hex(&mut hex, &binary.bytes);
    let mut out = String::with_capacity(36);
    for (i, c) in hex.chars().enumerate() {
        if matches!(i, 8 | 12 | 16 | 20) {
            out.push('-');
        }
        out.push(c);
    }
    Some(out)
}

/// The rank of a value's type in MongoDB's sort order. Values of a lower rank sort before
/// any value of a higher one, whatever the values are.
pub(crate) fn sort_rank(bson: &Bson) -> u8 {
    match bson {
        Bson::MinKey => 0,
        Bson::Null | Bson::Undefined => 1,
        Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_) => 2,
        Bson::String(_) | Bson::Symbol(_) => 3,
        Bson::Document(_) => 4,
        Bson::Array(_) => 5,
        Bson::Binary(_) => 6,
        Bson::ObjectId(_) => 7,
        Bson::Boolean(_) => 8,
        Bson::DateTime(_) => 9,
        Bson::Timestamp(_) => 10,
        Bson::RegularExpression(_) => 11,
        Bson::DbPointer(_) | Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => 11,
        Bson::MaxKey => 12,
    }
}

/// The `$type` names of every type ranked above `rank`, for a seek condition that has to
/// step from one type to the next.
pub(crate) fn types_ranked_above(rank: u8) -> Vec<Bson> {
    types_ranked(|r| r > rank)
}

/// The `$type` names of every type ranked below `rank`.
pub(crate) fn types_ranked_below(rank: u8) -> Vec<Bson> {
    types_ranked(|r| r < rank)
}

fn types_ranked(keep: impl Fn(u8) -> bool) -> Vec<Bson> {
    const RANKS: [(u8, &[&str]); 13] = [
        (0, &["minKey"]),
        (1, &["null", "undefined"]),
        (2, &["double", "int", "long", "decimal"]),
        (3, &["string", "symbol"]),
        (4, &["object"]),
        (5, &["array"]),
        (6, &["binData"]),
        (7, &["objectId"]),
        (8, &["bool"]),
        (9, &["date"]),
        (10, &["timestamp"]),
        (
            11,
            &["regex", "dbPointer", "javascript", "javascriptWithScope"],
        ),
        (12, &["maxKey"]),
    ];
    RANKS
        .iter()
        .filter(|(r, _)| keep(*r))
        .flat_map(|(_, names)| names.iter().map(|n| Bson::String((*n).to_string())))
        .collect()
}

/// A date as BSON from milliseconds, for tests and callers that have numbers.
#[cfg(test)]
pub(crate) fn date(millis: i64) -> Bson {
    Bson::DateTime(bson::DateTime::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn maps_scalars() {
        assert_eq!(to_value(Bson::Int32(7)), Value::Int(7));
        assert_eq!(to_value(Bson::Null), Value::Null);
        let oid = ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").expect("oid");
        assert_eq!(
            to_value(Bson::ObjectId(oid)),
            Value::text("65a1b2c3d4e5f60718293a4b")
        );
        assert_eq!(kind_of(&Bson::ObjectId(oid)), ValueKind::ObjectId);
        assert_eq!(to_value(date(0)), Value::text("1970-01-01T00:00:00Z"));
        assert_eq!(
            to_value(Bson::Decimal128("1.50".parse().expect("decimal"))),
            Value::text("1.50")
        );
    }

    #[test]
    fn nests_as_relaxed_json() {
        let value = to_value(Bson::Document(doc! { "a": 1, "b": [true, "x"] }));
        assert_eq!(value, Value::Json(r#"{"a":1,"b":[true,"x"]}"#.into()));
        assert_eq!(kind_of(&Bson::Array(vec![])), ValueKind::Array);
    }

    #[test]
    fn round_trips_uuids() {
        let text = "123e4567-e89b-12d3-a456-426614174000";
        let bson = parse_uuid(text).expect("uuid");
        assert_eq!(kind_of(&bson), ValueKind::Uuid);
        assert_eq!(to_value(bson), Value::text(text));
        assert!(parse_uuid("123e4567e89b12d3a456426614174000").is_some());
        assert!(parse_uuid("123e4567-e89b-12d3-a456-42661417400").is_none());
        assert!(parse_uuid("1-23e4567e89b-12d3-a456-426614174000").is_none());
    }

    #[test]
    fn keeps_the_fields_type() {
        let text = Value::text("65a1b2c3d4e5f60718293a4b");
        assert!(matches!(
            to_bson(&text, Target::ObjectId),
            Bson::ObjectId(_)
        ));
        assert!(matches!(to_bson(&text, Target::Any), Bson::String(_)));
        assert_eq!(to_bson(&Value::text("12"), Target::Int32), Bson::Int32(12));
        assert_eq!(
            to_bson(&Value::text("not a number"), Target::Int32),
            Bson::String("not a number".into())
        );
        assert!(matches!(
            to_bson(&Value::text("2024-05-01T10:00:00Z"), Target::Date),
            Bson::DateTime(_)
        ));
        assert_eq!(to_bson(&Value::Float(3.0), Target::Int32), Bson::Int32(3));
    }

    #[test]
    fn canonical_keys_round_trip() {
        let values = [
            Bson::ObjectId(ObjectId::new()),
            Bson::Int64(1 << 40),
            Bson::Int32(5),
            Bson::Double(1.5),
            date(1_700_000_000_123),
            Bson::String("x".into()),
            Bson::Decimal128("10.01".parse().expect("decimal")),
        ];
        for value in values {
            let text = canonical_text(value.clone());
            assert_eq!(parse_json(&text), Some(value), "{text}");
        }
    }

    #[test]
    fn offers_key_candidates() {
        let oid = Value::text("65a1b2c3d4e5f60718293a4b");
        let candidates = key_candidates(&oid);
        assert_eq!(candidates.len(), 2);
        assert!(matches!(candidates[0], Bson::ObjectId(_)));
        assert_eq!(key_candidates(&Value::text("plain")).len(), 1);
        assert_eq!(key_candidates(&Value::Int(4)), vec![Bson::Int64(4)]);
    }

    #[test]
    fn ranks_types() {
        assert!(sort_rank(&Bson::Null) < sort_rank(&Bson::Int32(0)));
        assert!(
            sort_rank(&Bson::String(String::new())) < sort_rank(&Bson::ObjectId(ObjectId::new()))
        );
        let above = types_ranked_above(7);
        assert!(above.contains(&Bson::String("date".into())));
        assert!(!above.contains(&Bson::String("objectId".into())));
        assert!(types_ranked_above(12).is_empty());
        assert_eq!(types_ranked_below(1), vec![Bson::String("minKey".into())]);
    }
}
