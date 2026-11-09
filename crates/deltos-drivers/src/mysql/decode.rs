//! MySQL and MariaDB values, from either wire protocol, as [`Value`]s.
//!
//! Statements the person runs use the text protocol, where every value arrives as text;
//! the table view uses prepared statements, where numbers and dates arrive typed. The
//! column's type decides the result in both cases, so a column reads the same either
//! way.

use std::fmt::Write;

use deltos_data::{Value, ValueKind};
use mysql_async::consts::{ColumnFlags, ColumnType};

use crate::util;

/// The binary collation, which marks bytes rather than text.
const BINARY_CHARSET: u16 = 63;

/// What decoding needs to know about one result column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnInfo {
    pub ty: ColumnType,
    pub unsigned: bool,
    pub binary: bool,
    /// `TINYINT(1)`, the type MySQL uses for booleans.
    pub boolean: bool,
}

impl ColumnInfo {
    pub fn from_column(column: &mysql_async::Column) -> Self {
        let ty = column.column_type();
        Self {
            ty,
            unsigned: column.flags().contains(ColumnFlags::UNSIGNED_FLAG),
            binary: column.character_set() == BINARY_CHARSET,
            boolean: ty == ColumnType::MYSQL_TYPE_TINY && column.column_length() == 1,
        }
    }

    pub fn kind(&self) -> ValueKind {
        use ColumnType::*;
        match self.ty {
            MYSQL_TYPE_TINY if self.boolean => ValueKind::Bool,
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR | MYSQL_TYPE_BIT => ValueKind::Int,
            MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => ValueKind::Float,
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => ValueKind::Decimal,
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => ValueKind::Date,
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => ValueKind::Time,
            MYSQL_TYPE_DATETIME
            | MYSQL_TYPE_DATETIME2
            | MYSQL_TYPE_TIMESTAMP
            | MYSQL_TYPE_TIMESTAMP2 => ValueKind::DateTime,
            MYSQL_TYPE_JSON => ValueKind::Json,
            MYSQL_TYPE_GEOMETRY
            | MYSQL_TYPE_VECTOR
            | MYSQL_TYPE_NULL
            | MYSQL_TYPE_UNKNOWN
            | MYSQL_TYPE_TYPED_ARRAY => ValueKind::Other,
            _ if self.binary => ValueKind::Bytes,
            _ => ValueKind::Text,
        }
    }

    /// A type name for result columns, which carry no declared type.
    pub fn type_name(&self) -> &'static str {
        use ColumnType::*;
        match self.ty {
            MYSQL_TYPE_TINY if self.boolean => "tinyint(1)",
            MYSQL_TYPE_TINY => "tinyint",
            MYSQL_TYPE_SHORT => "smallint",
            MYSQL_TYPE_INT24 => "mediumint",
            MYSQL_TYPE_LONG => "int",
            MYSQL_TYPE_LONGLONG => "bigint",
            MYSQL_TYPE_YEAR => "year",
            MYSQL_TYPE_BIT => "bit",
            MYSQL_TYPE_FLOAT => "float",
            MYSQL_TYPE_DOUBLE => "double",
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
            MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
            MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
            MYSQL_TYPE_JSON => "json",
            MYSQL_TYPE_ENUM => "enum",
            MYSQL_TYPE_SET => "set",
            MYSQL_TYPE_GEOMETRY => "geometry",
            MYSQL_TYPE_VECTOR => "vector",
            MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB => {
                if self.binary {
                    "blob"
                } else {
                    "text"
                }
            }
            MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => {
                if self.binary {
                    "varbinary"
                } else {
                    "varchar"
                }
            }
            MYSQL_TYPE_STRING => {
                if self.binary {
                    "binary"
                } else {
                    "char"
                }
            }
            _ => "",
        }
    }

    pub fn decode(&self, value: mysql_async::Value) -> Value {
        use ColumnType::*;
        use mysql_async::Value as My;
        match value {
            My::NULL => Value::Null,
            My::Int(i) => self.integer(i),
            My::UInt(u) => {
                i64::try_from(u).map_or_else(|_| Value::text(u.to_string()), |i| self.integer(i))
            }
            My::Float(f) => Value::Float(f.to_string().parse().unwrap_or(f64::from(f))),
            My::Double(d) => Value::Float(d),
            My::Date(year, month, day, hour, minute, second, micros) => {
                let mut out = format!("{year:04}-{month:02}-{day:02}");
                if !matches!(self.ty, MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE) {
                    out.push(' ');
                    util::push_time(
                        &mut out,
                        u64::from(hour),
                        u32::from(minute),
                        u32::from(second),
                        micros,
                    );
                }
                Value::text(out)
            }
            My::Time(negative, days, hours, minutes, seconds, micros) => {
                let mut out = String::new();
                if negative {
                    out.push('-');
                }
                let hours = u64::from(days) * 24 + u64::from(hours);
                util::push_time(
                    &mut out,
                    hours,
                    u32::from(minutes),
                    u32::from(seconds),
                    micros,
                );
                Value::text(out)
            }
            My::Bytes(bytes) => self.decode_bytes(bytes),
        }
    }

    fn integer(&self, i: i64) -> Value {
        if self.boolean {
            Value::Bool(i != 0)
        } else {
            Value::Int(i)
        }
    }

    /// A value that arrived as bytes: every value of the text protocol, and strings,
    /// decimals, JSON and bit fields in the binary one.
    fn decode_bytes(&self, bytes: Vec<u8>) -> Value {
        use ColumnType::*;
        let text = || std::str::from_utf8(&bytes).ok().map(str::trim);
        match self.ty {
            MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => match text() {
                Some(t) => match t.parse::<i64>() {
                    Ok(i) => self.integer(i),
                    Err(_) => Value::text(t),
                },
                None => Value::Bytes(bytes.into()),
            },
            MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
                match text().and_then(|t| t.parse::<f64>().ok()) {
                    Some(f) => Value::Float(f),
                    None => util::text_value(&bytes),
                }
            }
            MYSQL_TYPE_BIT if bytes.len() <= 8 => {
                Value::Int(bytes.iter().fold(0i64, |acc, b| (acc << 8) | i64::from(*b)))
            }
            MYSQL_TYPE_JSON => match String::from_utf8(bytes) {
                Ok(json) => Value::Json(json.into()),
                Err(e) => Value::Bytes(e.into_bytes().into()),
            },
            MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
            | MYSQL_TYPE_DATE
            | MYSQL_TYPE_NEWDATE
            | MYSQL_TYPE_TIME
            | MYSQL_TYPE_TIME2
            | MYSQL_TYPE_DATETIME
            | MYSQL_TYPE_DATETIME2
            | MYSQL_TYPE_TIMESTAMP
            | MYSQL_TYPE_TIMESTAMP2
            | MYSQL_TYPE_ENUM
            | MYSQL_TYPE_SET => util::text_value(&bytes),
            MYSQL_TYPE_GEOMETRY | MYSQL_TYPE_VECTOR | MYSQL_TYPE_BIT => Value::Bytes(bytes.into()),
            _ if self.binary => Value::Bytes(bytes.into()),
            _ => match String::from_utf8(bytes) {
                Ok(text) => Value::Text(text.into()),
                Err(e) => Value::Text(String::from_utf8_lossy(e.as_bytes()).into()),
            },
        }
    }
}

/// A value as a statement parameter.
pub(crate) fn param(value: &Value) -> mysql_async::Value {
    use mysql_async::Value as My;
    match value {
        Value::Null => My::NULL,
        Value::Bool(b) => My::Int(i64::from(*b)),
        Value::Int(i) => My::Int(*i),
        Value::Float(f) => My::Double(*f),
        Value::Text(s) | Value::Json(s) => My::Bytes(s.as_bytes().to_vec()),
        Value::Bytes(b) => My::Bytes(b.to_vec()),
    }
}

pub(crate) fn params(values: &[Value]) -> mysql_async::Params {
    if values.is_empty() {
        mysql_async::Params::Empty
    } else {
        mysql_async::Params::Positional(values.iter().map(param).collect())
    }
}

/// A catalog value as text, whatever form the server sent it in.
pub(crate) fn text(value: Option<&mysql_async::Value>) -> Option<String> {
    use mysql_async::Value as My;
    match value? {
        My::NULL => None,
        My::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        My::Int(i) => Some(i.to_string()),
        My::UInt(u) => Some(u.to_string()),
        My::Float(f) => Some(f.to_string()),
        My::Double(d) => Some(d.to_string()),
        other => {
            let mut out = String::new();
            let _ = write!(out, "{other:?}");
            Some(out)
        }
    }
}

pub(crate) fn number(value: Option<&mysql_async::Value>) -> Option<u64> {
    use mysql_async::Value as My;
    match value? {
        My::Int(i) => u64::try_from(*i).ok(),
        My::UInt(u) => Some(*u),
        My::Double(d) if *d >= 0.0 => Some(*d as u64),
        My::Bytes(b) => std::str::from_utf8(b)
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| *n >= 0.0)
            .map(|n| n as u64),
        _ => None,
    }
}

/// The kind for a column as `information_schema.COLUMNS` describes it.
pub(crate) fn kind_for(data_type: &str, column_type: &str) -> ValueKind {
    let column_type = column_type.to_ascii_lowercase();
    match data_type.to_ascii_lowercase().as_str() {
        "tinyint" if column_type.starts_with("tinyint(1)") => ValueKind::Bool,
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" | "bit" => {
            ValueKind::Int
        }
        "float" | "double" | "real" => ValueKind::Float,
        "decimal" | "numeric" => ValueKind::Decimal,
        "date" => ValueKind::Date,
        "time" => ValueKind::Time,
        "datetime" | "timestamp" => ValueKind::DateTime,
        "json" => ValueKind::Json,
        "uuid" => ValueKind::Uuid,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set" => {
            ValueKind::Text
        }
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => {
            ValueKind::Bytes
        }
        _ => ValueKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::Value as My;

    fn column(ty: ColumnType) -> ColumnInfo {
        ColumnInfo {
            ty,
            unsigned: false,
            binary: false,
            boolean: false,
        }
    }

    #[test]
    fn decodes_both_protocols_alike() {
        let int = column(ColumnType::MYSQL_TYPE_LONGLONG);
        assert_eq!(int.decode(My::Bytes(b"-42".to_vec())), Value::Int(-42));
        assert_eq!(int.decode(My::Int(-42)), Value::Int(-42));
        assert_eq!(
            int.decode(My::UInt(u64::MAX)),
            Value::text("18446744073709551615")
        );
        assert_eq!(
            int.decode(My::Bytes(b"18446744073709551615".to_vec())),
            Value::text("18446744073709551615")
        );
        let boolean = ColumnInfo {
            boolean: true,
            ..column(ColumnType::MYSQL_TYPE_TINY)
        };
        assert_eq!(boolean.decode(My::Bytes(b"1".to_vec())), Value::Bool(true));
        assert_eq!(boolean.decode(My::Int(0)), Value::Bool(false));
        let datetime = column(ColumnType::MYSQL_TYPE_DATETIME);
        assert_eq!(
            datetime.decode(My::Date(2024, 2, 29, 1, 2, 3, 500_000)),
            Value::text("2024-02-29 01:02:03.5")
        );
        assert_eq!(
            column(ColumnType::MYSQL_TYPE_DATE).decode(My::Date(2024, 2, 29, 0, 0, 0, 0)),
            Value::text("2024-02-29")
        );
        assert_eq!(
            column(ColumnType::MYSQL_TYPE_TIME).decode(My::Time(true, 1, 2, 3, 4, 0)),
            Value::text("-26:03:04")
        );
        assert_eq!(
            column(ColumnType::MYSQL_TYPE_BIT).decode(My::Bytes(vec![1, 0])),
            Value::Int(256)
        );
        assert_eq!(
            column(ColumnType::MYSQL_TYPE_FLOAT).decode(My::Float(0.1)),
            Value::Float(0.1)
        );
        let blob = ColumnInfo {
            binary: true,
            ..column(ColumnType::MYSQL_TYPE_BLOB)
        };
        assert_eq!(
            blob.decode(My::Bytes(vec![0, 255])),
            Value::Bytes(vec![0, 255].into())
        );
        assert_eq!(blob.kind(), ValueKind::Bytes);
        let json = column(ColumnType::MYSQL_TYPE_JSON);
        assert_eq!(
            json.decode(My::Bytes(b"[1]".to_vec())),
            Value::Json("[1]".into())
        );
    }

    #[test]
    fn kinds_from_the_catalog() {
        assert_eq!(kind_for("tinyint", "tinyint(1)"), ValueKind::Bool);
        assert_eq!(kind_for("tinyint", "tinyint(4) unsigned"), ValueKind::Int);
        assert_eq!(kind_for("varchar", "varchar(20)"), ValueKind::Text);
        assert_eq!(kind_for("geometry", "geometry"), ValueKind::Other);
    }
}
