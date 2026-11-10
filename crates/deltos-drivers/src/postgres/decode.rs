//! PostgreSQL values in the binary wire format, decoded into [`Value`]s.
//!
//! tokio-postgres asks for every result column in binary. We read the raw bytes and
//! decode the built-in types ourselves, into the same text the server would print for the
//! ones the grid only displays (dates, numerics, network addresses), so a value read here
//! can go back to the server as a text parameter unchanged. Arrays, ranges and records
//! are decoded recursively; arrays and records become JSON. A type with no decoder here
//! is shown as text when its bytes are UTF-8 and as bytes otherwise; the table view
//! avoids that case by selecting such columns as `::text`.

use std::fmt::Write;
use std::net::{Ipv4Addr, Ipv6Addr};

use deltos_data::{Value, ValueKind};
use tokio_postgres::types::{FromSql, Kind, Type};

use crate::util::{self, civil_from_days, push_hex, push_time};

/// The raw bytes of one cell, whatever its type.
pub(crate) struct Raw<'a>(pub Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(Some(raw)))
    }

    fn from_sql_null(_ty: &Type) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(None))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

/// How to read one column.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Decoder {
    Bool,
    Int2,
    Int4,
    Int8,
    /// Unsigned 32-bit identifiers: `oid`, `xid`, `cid` and the `reg*` types.
    Oid,
    Xid8,
    Float4,
    Float8,
    Numeric,
    Money,
    Text,
    /// Text after a one byte format version: `jsonb`, `jsonpath`, `ltree`.
    VersionedText,
    Json,
    Jsonb,
    Char,
    Bytea,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
    Uuid,
    Inet,
    MacAddr,
    Bit,
    Tid,
    Lsn,
    Point,
    Lseg,
    Box,
    Line,
    Path,
    Polygon,
    Circle,
    Hstore,
    Vector,
    Array(std::boxed::Box<Decoder>),
    Range(std::boxed::Box<Decoder>),
    Record(Vec<(String, Decoder)>),
    AnonymousRecord,
    Fallback,
}

impl Decoder {
    /// The decoder for a built-in type, by OID.
    pub(crate) fn for_oid(oid: u32) -> Option<Decoder> {
        use Decoder::*;
        Some(match oid {
            16 => Bool,
            17 => Bytea,
            18 => Char,
            19 | 25 | 142 | 705 | 1042 | 1043 | 1790 => Text,
            20 => Int8,
            21 => Int2,
            23 => Int4,
            24 | 26 | 28 | 29 | 2202 | 2203 | 2204 | 2205 | 2206 | 3734 | 3769 | 4089 | 4096 => Oid,
            5069 => Xid8,
            27 => Tid,
            114 => Json,
            3802 => Jsonb,
            4072 => VersionedText,
            600 => Point,
            601 => Lseg,
            602 => Path,
            603 => Box,
            604 => Polygon,
            628 => Line,
            718 => Circle,
            650 | 869 => Inet,
            700 => Float4,
            701 => Float8,
            774 | 829 => MacAddr,
            790 => Money,
            1082 => Date,
            1083 => Time,
            1114 => Timestamp,
            1184 => TimestampTz,
            1186 => Interval,
            1266 => TimeTz,
            1560 | 1562 => Bit,
            1700 => Numeric,
            2249 => AnonymousRecord,
            2950 => Uuid,
            3220 => Lsn,
            _ => return None,
        })
    }

    /// The decoder for any type tokio-postgres describes, built-in or not.
    pub(crate) fn for_type(ty: &Type) -> Decoder {
        if let Some(decoder) = Decoder::for_oid(ty.oid()) {
            return decoder;
        }
        match ty.kind() {
            Kind::Enum(_) => Decoder::Text,
            Kind::Domain(base) => Decoder::for_type(base),
            Kind::Array(element) => {
                Decoder::Array(std::boxed::Box::new(Decoder::for_type(element)))
            }
            Kind::Range(element) => {
                Decoder::Range(std::boxed::Box::new(Decoder::for_type(element)))
            }
            Kind::Composite(fields) => Decoder::Record(
                fields
                    .iter()
                    .map(|f| (f.name().to_string(), Decoder::for_type(f.type_())))
                    .collect(),
            ),
            _ => Decoder::for_extension(ty.name()).unwrap_or(Decoder::Fallback),
        }
    }

    /// Decoders for common extension types, which have no fixed OID.
    pub(crate) fn for_extension(name: &str) -> Option<Decoder> {
        Some(match name {
            "citext" => Decoder::Text,
            "ltree" | "lquery" | "ltxtquery" => Decoder::VersionedText,
            "hstore" => Decoder::Hstore,
            "vector" => Decoder::Vector,
            _ => return None,
        })
    }

    pub(crate) fn kind(&self) -> ValueKind {
        use Decoder::*;
        match self {
            Bool => ValueKind::Bool,
            Int2 | Int4 | Int8 | Oid | Xid8 => ValueKind::Int,
            Float4 | Float8 => ValueKind::Float,
            Numeric | Money => ValueKind::Decimal,
            Text | VersionedText | Char => ValueKind::Text,
            Json | Jsonb | Hstore | Record(_) | AnonymousRecord => ValueKind::Json,
            Bytea => ValueKind::Bytes,
            Date => ValueKind::Date,
            Time | TimeTz => ValueKind::Time,
            Timestamp | TimestampTz => ValueKind::DateTime,
            Uuid => ValueKind::Uuid,
            Array(_) | Vector => ValueKind::Array,
            Interval | Inet | MacAddr | Bit | Tid | Lsn | Point | Lseg | Box | Line | Path
            | Polygon | Circle | Range(_) | Fallback => ValueKind::Other,
        }
    }

    /// Decodes one cell. Malformed bytes never fail a row; they come back as bytes.
    pub(crate) fn decode(&self, raw: Option<&[u8]>) -> Value {
        let Some(raw) = raw else {
            return Value::Null;
        };
        self.decode_bytes(raw)
            .unwrap_or_else(|| Value::Bytes(raw.into()))
    }

    fn decode_bytes(&self, raw: &[u8]) -> Option<Value> {
        use Decoder::*;
        let mut r = Reader(raw);
        Some(match self {
            Bool => Value::Bool(r.u8()? != 0),
            Int2 => Value::Int(i64::from(r.i16()?)),
            Int4 => Value::Int(i64::from(r.i32()?)),
            Int8 => Value::Int(r.i64()?),
            Oid => Value::Int(i64::from(r.u32()?)),
            Xid8 => {
                let n = r.u64()?;
                i64::try_from(n).map_or_else(|_| Value::text(n.to_string()), Value::Int)
            }
            Float4 => {
                // Through the shortest f32 text, so 0.1f32 shows as 0.1 and not
                // 0.10000000149011612.
                let f = f32::from_be_bytes(r.take::<4>()?);
                Value::Float(f.to_string().parse().unwrap_or(f64::from(f)))
            }
            Float8 => Value::Float(f64::from_be_bytes(r.take::<8>()?)),
            Numeric => Value::text(numeric(raw)?),
            Money => Value::text(money(r.i64()?)),
            Text => util::text_value(raw),
            VersionedText => util::text_value(raw.get(1..)?),
            Json => Value::Json(std::str::from_utf8(raw).ok()?.into()),
            Jsonb => Value::Json(std::str::from_utf8(raw.get(1..)?).ok()?.into()),
            Char => {
                let byte = r.u8()?;
                Value::text(if byte.is_ascii() {
                    (byte as char).to_string()
                } else {
                    format!("\\{byte:03o}")
                })
            }
            Bytea => Value::Bytes(raw.into()),
            Date => Value::text(date(r.i32()?)),
            Time => {
                let mut out = String::new();
                push_clock(&mut out, r.i64()?);
                Value::text(out)
            }
            TimeTz => {
                let micros = r.i64()?;
                let zone = r.i32()?;
                let mut out = String::new();
                push_clock(&mut out, micros);
                push_offset(&mut out, -zone);
                Value::text(out)
            }
            Timestamp => Value::text(timestamp(r.i64()?, false)),
            TimestampTz => Value::text(timestamp(r.i64()?, true)),
            Interval => {
                let micros = r.i64()?;
                let days = r.i32()?;
                let months = r.i32()?;
                Value::text(interval(months, days, micros))
            }
            Uuid => Value::text(uuid(&r.take::<16>()?)),
            Inet => Value::text(inet(raw)?),
            MacAddr => {
                let mut out = String::new();
                for (i, byte) in raw.iter().enumerate() {
                    if i > 0 {
                        out.push(':');
                    }
                    let _ = write!(out, "{byte:02x}");
                }
                Value::text(out)
            }
            Bit => {
                let bits = usize::try_from(r.i32()?).ok()?;
                let bytes = r.rest();
                if bytes.len() * 8 < bits {
                    return None;
                }
                let text: String = (0..bits)
                    .map(|i| {
                        if bytes[i / 8] & (0x80 >> (i % 8)) != 0 {
                            '1'
                        } else {
                            '0'
                        }
                    })
                    .collect();
                Value::text(text)
            }
            Tid => {
                let block = r.u32()?;
                let offset = r.u16()?;
                Value::text(format!("({block},{offset})"))
            }
            Lsn => {
                let n = r.u64()?;
                Value::text(format!("{:X}/{:X}", n >> 32, n & 0xffff_ffff))
            }
            Point => Value::text(point(&mut r)?),
            Lseg => Value::text(format!("[{},{}]", point(&mut r)?, point(&mut r)?)),
            Box => Value::text(format!("{},{}", point(&mut r)?, point(&mut r)?)),
            Line => Value::text(format!("{{{},{},{}}}", r.f64()?, r.f64()?, r.f64()?)),
            Path => {
                let closed = r.u8()? != 0;
                let points = points(&mut r)?;
                Value::text(if closed {
                    format!("({points})")
                } else {
                    format!("[{points}]")
                })
            }
            Polygon => Value::text(format!("({})", points(&mut r)?)),
            Circle => {
                let center = point(&mut r)?;
                Value::text(format!("<{center},{}>", r.f64()?))
            }
            Hstore => Value::Json(hstore(&mut r)?.into()),
            Vector => {
                let dims = usize::try_from(r.i16()?).ok()?;
                r.i16()?;
                let mut out = String::from("[");
                for i in 0..dims {
                    if i > 0 {
                        out.push(',');
                    }
                    let f = f32::from_be_bytes(r.take::<4>()?);
                    let _ = write!(out, "{f}");
                }
                out.push(']');
                Value::Json(out.into())
            }
            Array(element) => Value::Json(array(&mut r, element)?.into()),
            Range(element) => Value::text(range(&mut r, element)?),
            Record(fields) => Value::Json(record(&mut r, Some(fields))?.into()),
            AnonymousRecord => Value::Json(record(&mut r, None)?.into()),
            Fallback => match std::str::from_utf8(raw) {
                Ok(text) if !text.contains('\0') => Value::text(text),
                _ => Value::Bytes(raw.into()),
            },
        })
    }
}

/// A cursor over a value's bytes that returns `None` instead of reading past the end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, tail) = self.0.split_first_chunk::<N>()?;
        self.0 = tail;
        Some(*head)
    }

    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take::<1>()?[0])
    }

    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_be_bytes(self.take()?))
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take()?))
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take()?))
    }

    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_be_bytes(self.take()?))
    }

    /// A length-prefixed element: `None` inside for SQL NULL.
    fn element(&mut self) -> Option<Option<&'a [u8]>> {
        let len = self.i32()?;
        if len < 0 {
            return Some(None);
        }
        Some(Some(self.bytes(usize::try_from(len).ok()?)?))
    }
}

const PG_EPOCH_DAYS: i64 = 10_957;
const MICROS_PER_DAY: i64 = 86_400_000_000;

fn push_date(out: &mut String, days_since_2000: i64) -> bool {
    let (year, month, day) = civil_from_days(days_since_2000 + PG_EPOCH_DAYS);
    let bc = year <= 0;
    let shown = if bc { 1 - year } else { year };
    let _ = write!(out, "{shown:04}-{month:02}-{day:02}");
    bc
}

fn date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".to_string(),
        i32::MIN => "-infinity".to_string(),
        days => {
            let mut out = String::with_capacity(13);
            if push_date(&mut out, i64::from(days)) {
                out.push_str(" BC");
            }
            out
        }
    }
}

fn push_clock(out: &mut String, micros: i64) {
    let micros = micros.max(0) as u64;
    let seconds = micros / 1_000_000;
    push_time(
        out,
        seconds / 3600,
        ((seconds / 60) % 60) as u32,
        (seconds % 60) as u32,
        (micros % 1_000_000) as u32,
    );
}

/// `+HH`, `+HH:MM` or `+HH:MM:SS` for an offset in seconds east of UTC.
fn push_offset(out: &mut String, seconds_east: i32) {
    let sign = if seconds_east < 0 { '-' } else { '+' };
    let total = seconds_east.unsigned_abs();
    let _ = write!(out, "{sign}{:02}", total / 3600);
    if !total.is_multiple_of(3600) {
        let _ = write!(out, ":{:02}", (total / 60) % 60);
        if !total.is_multiple_of(60) {
            let _ = write!(out, ":{:02}", total % 60);
        }
    }
}

fn timestamp(micros: i64, utc: bool) -> String {
    match micros {
        i64::MAX => return "infinity".to_string(),
        i64::MIN => return "-infinity".to_string(),
        _ => {}
    }
    let days = micros.div_euclid(MICROS_PER_DAY);
    let time = micros.rem_euclid(MICROS_PER_DAY);
    let mut out = String::with_capacity(32);
    let bc = push_date(&mut out, days);
    out.push(' ');
    push_clock(&mut out, time);
    if utc {
        out.push_str("+00");
    }
    if bc {
        out.push_str(" BC");
    }
    out
}

/// The `postgres` interval style: `1 year 2 mons 3 days 04:05:06`.
fn interval(months: i32, days: i32, micros: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    let years = months / 12;
    let months = months % 12;
    // PostgreSQL uses the singular only for exactly one: `1 day`, `-1 days`.
    let unit = |n: i64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    if years != 0 {
        parts.push(unit(i64::from(years), "year", "years"));
    }
    if months != 0 {
        parts.push(unit(i64::from(months), "mon", "mons"));
    }
    if days != 0 {
        parts.push(unit(i64::from(days), "day", "days"));
    }
    if micros != 0 || parts.is_empty() {
        let mut clock = String::new();
        if micros < 0 {
            clock.push('-');
        }
        let abs = micros.unsigned_abs();
        let seconds = abs / 1_000_000;
        push_time(
            &mut clock,
            seconds / 3600,
            ((seconds / 60) % 60) as u32,
            (seconds % 60) as u32,
            (abs % 1_000_000) as u32,
        );
        parts.push(clock);
    }
    parts.join(" ")
}

fn numeric(raw: &[u8]) -> Option<String> {
    let mut r = Reader(raw);
    let ndigits = usize::try_from(r.i16()?).ok()?;
    let weight = i32::from(r.i16()?);
    let sign = r.u16()?;
    let dscale = usize::from(r.u16()?);
    match sign {
        0xC000 => return Some("NaN".to_string()),
        0xD000 => return Some("Infinity".to_string()),
        0xF000 => return Some("-Infinity".to_string()),
        _ => {}
    }
    let mut digits = Vec::with_capacity(ndigits);
    for _ in 0..ndigits {
        digits.push(r.u16()?);
    }
    let digit = |i: i32| -> u16 {
        usize::try_from(i)
            .ok()
            .and_then(|i| digits.get(i).copied())
            .unwrap_or(0)
    };
    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                let _ = write!(out, "{}", digit(i));
            } else {
                let _ = write!(out, "{:04}", digit(i));
            }
        }
    }
    if dscale > 0 {
        out.push('.');
        let start = out.len();
        let mut i = weight + 1;
        while out.len() - start < dscale {
            let _ = write!(out, "{:04}", digit(i));
            i += 1;
        }
        out.truncate(start + dscale);
    }
    Some(out)
}

fn money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

fn uuid(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, chunk) in [
        &bytes[0..4],
        &bytes[4..6],
        &bytes[6..8],
        &bytes[8..10],
        &bytes[10..16],
    ]
    .iter()
    .enumerate()
    {
        if i > 0 {
            out.push('-');
        }
        push_hex(&mut out, chunk);
    }
    out
}

fn inet(raw: &[u8]) -> Option<String> {
    let mut r = Reader(raw);
    let family = r.u8()?;
    let bits = r.u8()?;
    let is_cidr = r.u8()? != 0;
    let len = usize::from(r.u8()?);
    let addr = r.bytes(len)?;
    let (text, max) = match (family, addr.len()) {
        (2, 4) => (
            Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]).to_string(),
            32,
        ),
        (3, 16) => {
            let octets: [u8; 16] = addr.try_into().ok()?;
            (Ipv6Addr::from(octets).to_string(), 128)
        }
        _ => return None,
    };
    Some(if is_cidr || bits != max {
        format!("{text}/{bits}")
    } else {
        text
    })
}

fn point(r: &mut Reader<'_>) -> Option<String> {
    Some(format!("({},{})", r.f64()?, r.f64()?))
}

fn points(r: &mut Reader<'_>) -> Option<String> {
    let n = usize::try_from(r.i32()?).ok()?;
    let mut out = String::new();
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&point(r)?);
    }
    Some(out)
}

fn hstore(r: &mut Reader<'_>) -> Option<String> {
    let n = usize::try_from(r.i32()?).ok()?;
    let mut map = serde_json::Map::new();
    for _ in 0..n {
        let key = r.element()??;
        let value = r.element()?;
        map.insert(
            String::from_utf8_lossy(key).into_owned(),
            match value {
                Some(v) => serde_json::Value::String(String::from_utf8_lossy(v).into_owned()),
                None => serde_json::Value::Null,
            },
        );
    }
    Some(serde_json::Value::Object(map).to_string())
}

/// A value inside an array or record, as JSON.
fn to_json(value: Value) -> serde_json::Value {
    use serde_json::Value as J;
    match value {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(b),
        Value::Int(i) => J::from(i),
        Value::Float(f) => {
            serde_json::Number::from_f64(f).map_or_else(|| J::String(f.to_string()), J::Number)
        }
        Value::Text(s) => J::String(s.into_string()),
        Value::Json(s) => serde_json::from_str(&s).unwrap_or_else(|_| J::String(s.into_string())),
        Value::Bytes(b) => {
            let mut out = String::from("\\x");
            push_hex(&mut out, &b);
            J::String(out)
        }
    }
}

fn array(r: &mut Reader<'_>, element: &Decoder) -> Option<String> {
    let dims = usize::try_from(r.i32()?).ok()?;
    let _has_nulls = r.i32()?;
    let _element_oid = r.u32()?;
    if dims > 6 {
        return None;
    }
    let mut lengths = Vec::with_capacity(dims);
    for _ in 0..dims {
        lengths.push(usize::try_from(r.i32()?).ok()?);
        let _lower_bound = r.i32()?;
    }
    if dims == 0 {
        return Some("[]".to_string());
    }
    let value = nested(r, element, &lengths)?;
    Some(value.to_string())
}

fn nested(r: &mut Reader<'_>, element: &Decoder, lengths: &[usize]) -> Option<serde_json::Value> {
    let (&len, rest) = lengths.split_first()?;
    // Each element takes at least four bytes, which bounds a malformed length.
    let mut items = Vec::with_capacity(len.min(r.0.len() / 4 + 1));
    for _ in 0..len {
        if rest.is_empty() {
            items.push(to_json(element.decode(r.element()?)));
        } else {
            items.push(nested(r, element, rest)?);
        }
    }
    Some(serde_json::Value::Array(items))
}

fn range(r: &mut Reader<'_>, element: &Decoder) -> Option<String> {
    const EMPTY: u8 = 0x01;
    const LOWER_INCLUSIVE: u8 = 0x02;
    const UPPER_INCLUSIVE: u8 = 0x04;
    const LOWER_INFINITE: u8 = 0x08;
    const UPPER_INFINITE: u8 = 0x10;
    let flags = r.u8()?;
    if flags & EMPTY != 0 {
        return Some("empty".to_string());
    }
    let mut out = String::new();
    out.push(if flags & LOWER_INCLUSIVE != 0 {
        '['
    } else {
        '('
    });
    if flags & LOWER_INFINITE == 0 {
        out.push_str(&element.decode(r.element()?).to_string());
    }
    out.push(',');
    if flags & UPPER_INFINITE == 0 {
        out.push_str(&element.decode(r.element()?).to_string());
    }
    out.push(if flags & UPPER_INCLUSIVE != 0 {
        ']'
    } else {
        ')'
    });
    Some(out)
}

fn record(r: &mut Reader<'_>, fields: Option<&[(String, Decoder)]>) -> Option<String> {
    let n = usize::try_from(r.i32()?).ok()?;
    match fields {
        Some(fields) => {
            let mut map = serde_json::Map::new();
            for i in 0..n {
                let _oid = r.u32()?;
                let raw = r.element()?;
                let (name, decoder) = fields.get(i)?;
                map.insert(name.clone(), to_json(decoder.decode(raw)));
            }
            Some(serde_json::Value::Object(map).to_string())
        }
        None => {
            let mut items = Vec::new();
            for _ in 0..n {
                let oid = r.u32()?;
                let raw = r.element()?;
                let decoder = Decoder::for_oid(oid).unwrap_or(Decoder::Fallback);
                items.push(to_json(decoder.decode(raw)));
            }
            Some(serde_json::Value::Array(items).to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(decoder: Decoder, raw: &[u8]) -> String {
        decoder.decode(Some(raw)).to_string()
    }

    #[test]
    fn numbers() {
        assert_eq!(
            Decoder::Int4.decode(Some(&(-5i32).to_be_bytes())),
            Value::Int(-5)
        );
        assert_eq!(
            Decoder::Float4.decode(Some(&0.1f32.to_be_bytes())),
            Value::Float(0.1)
        );
        // 12345.678 = weight 1, digits [1, 2345, 6780], dscale 3
        let mut raw = Vec::new();
        for n in [3i16, 1, 0, 3, 1, 2345, 6780] {
            raw.extend_from_slice(&n.to_be_bytes());
        }
        assert_eq!(text(Decoder::Numeric, &raw), "12345.678");
        // -0.0012 = weight -1, digits [12], dscale 4
        let mut raw = Vec::new();
        for n in [1u16, 0xffff, 0x4000, 4, 12] {
            raw.extend_from_slice(&n.to_be_bytes());
        }
        assert_eq!(text(Decoder::Numeric, &raw), "-0.0012");
        assert_eq!(money(-12345), "-123.45");
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(date(0), "2000-01-01");
        assert_eq!(date(-730_485), "0001-01-01 BC");
        assert_eq!(date(-730_120), "0001-12-31 BC");
        assert_eq!(timestamp(0, true), "2000-01-01 00:00:00+00");
        assert_eq!(timestamp(-1, false), "1999-12-31 23:59:59.999999");
        assert_eq!(timestamp(i64::MAX, false), "infinity");
        assert_eq!(
            interval(14, 3, 4 * 3_600_000_000 + 5_000_000),
            "1 year 2 mons 3 days 04:00:05"
        );
        assert_eq!(interval(0, 0, 0), "00:00:00");
        assert_eq!(interval(0, -1, -1_500_000), "-1 days -00:00:01.5");
        let mut raw = (3_600_000_000i64).to_be_bytes().to_vec();
        raw.extend_from_slice(&(-19_800i32).to_be_bytes());
        assert_eq!(text(Decoder::TimeTz, &raw), "01:00:00+05:30");
    }

    #[test]
    fn identifiers_and_addresses() {
        let bytes: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            text(Decoder::Uuid, &bytes),
            "00010203-0405-0607-0809-0a0b0c0d0e0f"
        );
        assert_eq!(text(Decoder::Inet, &[2, 32, 0, 4, 10, 0, 0, 1]), "10.0.0.1");
        assert_eq!(
            text(Decoder::Inet, &[2, 24, 1, 4, 10, 0, 0, 0]),
            "10.0.0.0/24"
        );
        assert_eq!(
            text(Decoder::MacAddr, &[8, 0, 0x2b, 1, 2, 3]),
            "08:00:2b:01:02:03"
        );
        let mut bit = 5i32.to_be_bytes().to_vec();
        bit.push(0b1010_1000);
        assert_eq!(text(Decoder::Bit, &bit), "10101");
        let mut tid = 7u32.to_be_bytes().to_vec();
        tid.extend_from_slice(&3u16.to_be_bytes());
        assert_eq!(text(Decoder::Tid, &tid), "(7,3)");
    }

    #[test]
    fn arrays_and_ranges() {
        // int4[] {1, NULL, 3}
        let mut raw = Vec::new();
        for n in [1i32, 1, 23, 3, 1] {
            raw.extend_from_slice(&n.to_be_bytes());
        }
        for item in [Some(1i32), None, Some(3)] {
            match item {
                Some(v) => {
                    raw.extend_from_slice(&4i32.to_be_bytes());
                    raw.extend_from_slice(&v.to_be_bytes());
                }
                None => raw.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        let decoder = Decoder::Array(Box::new(Decoder::Int4));
        assert_eq!(decoder.decode(Some(&raw)), Value::Json("[1,null,3]".into()));
        // int4range [1,10)
        let mut raw = vec![0x02];
        for v in [1i32, 10] {
            raw.extend_from_slice(&4i32.to_be_bytes());
            raw.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(
            text(Decoder::Range(Box::new(Decoder::Int4)), &raw),
            "[1,10)"
        );
        assert_eq!(text(Decoder::Range(Box::new(Decoder::Int4)), &[1]), "empty");
    }

    #[test]
    fn never_panics_on_garbage() {
        let decoders = [
            Decoder::Bool,
            Decoder::Int8,
            Decoder::Numeric,
            Decoder::Interval,
            Decoder::Inet,
            Decoder::Bit,
            Decoder::Path,
            Decoder::Hstore,
            Decoder::Vector,
            Decoder::Array(Box::new(Decoder::Array(Box::new(Decoder::Text)))),
            Decoder::Range(Box::new(Decoder::Timestamp)),
            Decoder::Record(vec![("a".into(), Decoder::Int4)]),
            Decoder::AnonymousRecord,
            Decoder::Jsonb,
            Decoder::Fallback,
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for len in 0..400usize {
            let raw: Vec<u8> = (0..len % 64)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    (seed >> 24) as u8
                })
                .collect();
            for decoder in &decoders {
                let _ = decoder.decode(Some(&raw));
            }
        }
    }
}
