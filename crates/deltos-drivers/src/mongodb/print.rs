//! BSON written as the shell writes it, for documents the person reads and edits.
//!
//! The text reads back with [`shell::parse_value`](super::shell::parse_value) as the same
//! BSON, types included: an Int32 is a bare number, an Int64 `NumberLong(…)`, a double
//! always has a decimal point or an exponent, a date is `ISODate("…")`, an ObjectId
//! `ObjectId("…")`. Types the shell has no constructor for here (code, DB pointers) are
//! canonical extended JSON, which the parser also reads.

use std::fmt::Write;

use bson::{Bson, Document};

use super::convert;

/// Spaces per level of indentation.
const INDENT: usize = 2;

/// Milliseconds of 0000-01-01T00:00:00Z and 9999-12-31T23:59:59.999Z.
const RFC3339_MIN: i64 = -62_167_219_200_000;
const RFC3339_MAX: i64 = 253_402_300_799_999;

/// `bson` as indented shell text.
pub fn pretty(bson: &Bson) -> String {
    let mut out = String::new();
    value(&mut out, bson, 0, true);
    out
}

/// A document as indented shell text.
pub fn pretty_document(doc: &Document) -> String {
    let mut out = String::new();
    document(&mut out, doc, 0, true);
    out
}

/// `bson` as one line of shell text.
pub fn compact(bson: &Bson) -> String {
    let mut out = String::new();
    value(&mut out, bson, 0, false);
    out
}

fn newline(out: &mut String, depth: usize, pretty: bool) {
    if pretty {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', depth * INDENT));
    }
}

fn document(out: &mut String, doc: &Document, depth: usize, pretty: bool) {
    if doc.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push('{');
    for (i, (k, v)) in doc.iter().enumerate() {
        if i > 0 {
            out.push(',');
            if !pretty {
                out.push(' ');
            }
        }
        newline(out, depth + 1, pretty);
        if !pretty && i == 0 {
            out.push(' ');
        }
        key(out, k);
        out.push_str(": ");
        value(out, v, depth + 1, pretty);
    }
    newline(out, depth, pretty);
    if !pretty {
        out.push(' ');
    }
    out.push('}');
}

fn array(out: &mut String, items: &[Bson], depth: usize, pretty: bool) {
    if items.is_empty() {
        out.push_str("[]");
        return;
    }
    // Short arrays of scalars stay on one line, as the shell prints them.
    let flat = !pretty
        || (items.len() <= 8
            && items
                .iter()
                .all(|v| !matches!(v, Bson::Document(_) | Bson::Array(_))));
    out.push('[');
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(if flat { ", " } else { "," });
        }
        if !flat {
            newline(out, depth + 1, pretty);
        }
        value(out, v, depth + 1, pretty && !flat);
    }
    if !flat {
        newline(out, depth, pretty);
    }
    out.push(']');
}

/// A key bare when the shell would accept it bare, quoted otherwise.
fn key(out: &mut String, k: &str) {
    let bare = !k.is_empty()
        && !k.starts_with(|c: char| c.is_ascii_digit())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !matches!(
            k,
            "true" | "false" | "null" | "undefined" | "NaN" | "Infinity"
        );
    if bare {
        out.push_str(k);
    } else {
        string(out, k);
    }
}

fn string(out: &mut String, s: &str) {
    // serde_json writes a valid JSON string, which the shell parser reads.
    out.push_str(&serde_json::Value::String(s.to_owned()).to_string());
}

fn value(out: &mut String, bson: &Bson, depth: usize, pretty: bool) {
    match bson {
        Bson::Document(doc) => document(out, doc, depth, pretty),
        Bson::Array(items) => array(out, items, depth, pretty),
        Bson::String(s) => string(out, s),
        Bson::Int32(n) => {
            let _ = write!(out, "{n}");
        }
        Bson::Int64(n) => {
            let _ = write!(out, "NumberLong(\"{n}\")");
        }
        Bson::Double(f) => double(out, *f),
        Bson::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Bson::Null => out.push_str("null"),
        Bson::Undefined => out.push_str("undefined"),
        Bson::ObjectId(oid) => {
            let _ = write!(out, "ObjectId(\"{}\")", oid.to_hex());
        }
        // RFC 3339 covers years 0 to 9999; the conversion clamps dates outside them.
        Bson::DateTime(dt) => match dt
            .try_to_rfc3339_string()
            .ok()
            .filter(|_| (RFC3339_MIN..=RFC3339_MAX).contains(&dt.timestamp_millis()))
        {
            Some(text) => {
                let _ = write!(out, "ISODate(\"{text}\")");
            }
            None => {
                let _ = write!(out, "new Date({})", dt.timestamp_millis());
            }
        },
        Bson::Decimal128(d) => {
            let _ = write!(out, "NumberDecimal(\"{d}\")");
        }
        Bson::Binary(b) => match convert::uuid_text(b) {
            Some(uuid) => {
                let _ = write!(out, "UUID(\"{uuid}\")");
            }
            None => binary(out, b),
        },
        Bson::Timestamp(ts) => {
            let _ = write!(out, "Timestamp({}, {})", ts.time, ts.increment);
        }
        Bson::RegularExpression(re) => {
            out.push_str("RegExp(");
            string(out, re.pattern.as_str());
            if !re.options.is_empty() {
                out.push_str(", ");
                string(out, re.options.as_str());
            }
            out.push(')');
        }
        Bson::MinKey => out.push_str("MinKey()"),
        Bson::MaxKey => out.push_str("MaxKey()"),
        Bson::Symbol(_)
        | Bson::JavaScriptCode(_)
        | Bson::JavaScriptCodeWithScope(_)
        | Bson::DbPointer(_) => {
            out.push_str(&bson.clone().into_canonical_extjson().to_string());
        }
    }
}

fn binary(out: &mut String, b: &bson::Binary) {
    let _ = write!(
        out,
        "BinData({}, \"{}\")",
        u8::from(b.subtype),
        base64(&b.bytes)
    );
}

/// Standard base64 with padding, as `BinData` takes it.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A double that reads back as a double: `1.0`, not `1`.
fn double(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("NaN");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        // Debug keeps the decimal point (`1.0`) and uses an exponent for very large or
        // small values (`1e300`), both of which the parser reads as a double.
        let _ = write!(out, "{f:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::super::shell::parse_value;
    use super::*;
    use bson::oid::ObjectId;
    use bson::spec::BinarySubtype;
    use bson::{Binary, DateTime, Regex, Timestamp, doc};

    fn round_trip(bson: Bson) {
        for text in [pretty(&bson), compact(&bson)] {
            let back = parse_value(&text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
            assert_eq!(back, bson, "{text}");
        }
    }

    #[test]
    fn reads_back_as_the_same_bson() {
        let oid = ObjectId::parse_str("64b7f0c2a1b2c3d4e5f60718").unwrap();
        round_trip(Bson::Document(doc! {
            "_id": oid,
            "n": 1,
            "big": 9_007_199_254_740_993_i64,
            "small": -5_i64,
            "f": 1.0,
            "g": 1.5e300,
            "h": -0.25,
            "s": "quote \" and \\ and\nnewline",
            "when": DateTime::from_millis(1_700_000_000_123),
            "dec": "12.3400".parse::<bson::Decimal128>().unwrap(),
            "uuid": Bson::Binary(Binary::from(bson::Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap())),
            "bin": Bson::Binary(Binary { subtype: BinarySubtype::Generic, bytes: vec![0, 1, 2, 255] }),
            "ts": Bson::Timestamp(Timestamp { time: 5, increment: 7 }),
            "re": Bson::RegularExpression(Regex { pattern: "a/b.*".to_string().try_into().unwrap(), options: "i".to_string().try_into().unwrap() }),
            "min": Bson::MinKey,
            "max": Bson::MaxKey,
            "null": Bson::Null,
            "yes": true,
            "nested": { "a": [1, 2, { "b": [] }], "empty": {} },
            "odd key": 1,
            "a.b": 2,
            "1st": 3,
            "true": 4,
            "": 5,
        }));
    }

    #[test]
    fn dates_outside_rfc3339_read_back() {
        round_trip(Bson::DateTime(DateTime::from_millis(-80_000_000_000_000)));
        round_trip(Bson::DateTime(DateTime::from_millis(300_000_000_000_000)));
    }

    #[test]
    fn indents_nested_values() {
        let text = pretty(&Bson::Document(
            doc! { "a": 1, "b": { "c": [1, 2] }, "d": [{ "e": 1 }] },
        ));
        assert_eq!(
            text,
            "{\n  a: 1,\n  b: {\n    c: [1, 2]\n  },\n  d: [\n    {\n      e: 1\n    }\n  ]\n}"
        );
        assert_eq!(
            compact(&Bson::Document(doc! { "a": 1, "b": [1, { "c": 2 }] })),
            "{ a: 1, b: [1, { c: 2 }] }"
        );
        assert_eq!(compact(&Bson::Document(doc! {})), "{}");
    }
}
