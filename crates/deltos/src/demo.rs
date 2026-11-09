//! A built-in connection with sample data, for `DELTOS_DEMO=1`, screenshots and trying
//! the app without a server. Nothing touches the network or the disk.
//!
//! Rows are not stored: each value is computed from its row id and column with a small
//! hash, so the same cell always reads the same, and a table of fifty million rows costs
//! nothing until it is looked at. Browsing in id order (either direction) is random access
//! at any offset, which is what lets the grid jump straight to row 49,999,000. The small
//! tables also sort and filter by any column, by materializing a sorted list of row ids
//! once per request shape; the large ones sort by id only.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use deltos_data::{
    BrowsePage, BrowseRequest, Capabilities, Column, Connection, DriverKind, Error, ExecuteOptions,
    Filter, FilterOp, Index, Namespace, NamespaceKind, ObjectEntry, ObjectInfo, ObjectKind,
    ObjectRef, Result, Row, RowBatch, RowStream, SortDirection, Value, ValueKind,
};
use futures::future::BoxFuture;
use parking_lot::Mutex;

/// Tables up to this many rows sort and filter by any column.
const SORTABLE_LIMIT: u64 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Table {
    Users,
    Orders,
    Products,
    OrderItems,
    EventsWide,
    DailyRevenue,
    Sessions,
    Orders2019,
    Documents,
}

const TABLES: [Table; 9] = [
    Table::Users,
    Table::Orders,
    Table::Products,
    Table::OrderItems,
    Table::EventsWide,
    Table::DailyRevenue,
    Table::Sessions,
    Table::Orders2019,
    Table::Documents,
];

const FIRST: [&str; 24] = [
    "Ada",
    "Grace",
    "Linus",
    "Margaret",
    "Alan",
    "Barbara",
    "Dennis",
    "Frances",
    "Ken",
    "Radia",
    "Edsger",
    "Hedy",
    "Donald",
    "Sophie",
    "John",
    "Katherine",
    "Tim",
    "Joan",
    "Niklaus",
    "Anita",
    "Bjarne",
    "Lynn",
    "Guido",
    "Shafi",
];
const LAST: [&str; 24] = [
    "Lovelace",
    "Hopper",
    "Torvalds",
    "Hamilton",
    "Turing",
    "Liskov",
    "Ritchie",
    "Allen",
    "Thompson",
    "Perlman",
    "Dijkstra",
    "Lamarr",
    "Knuth",
    "Wilson",
    "McCarthy",
    "Johnson",
    "Berners-Lee",
    "Clarke",
    "Wirth",
    "Borg",
    "Stroustrup",
    "Conway",
    "van Rossum",
    "Goldwasser",
];
const DOMAINS: [&str; 6] = [
    "example.com",
    "mail.test",
    "corp.example",
    "inbox.test",
    "acme.example",
    "proton.test",
];
const COUNTRIES: [&str; 12] = [
    "US", "DE", "FR", "GB", "JP", "BR", "IN", "CA", "AU", "NL", "SE", "KR",
];
const PLANS: [&str; 4] = ["free", "pro", "team", "enterprise"];
const STATUSES: [&str; 5] = ["pending", "paid", "shipped", "delivered", "refunded"];
const CURRENCIES: [&str; 4] = ["USD", "EUR", "GBP", "JPY"];
const CATEGORIES: [&str; 8] = [
    "Keyboards",
    "Monitors",
    "Cables",
    "Storage",
    "Audio",
    "Chairs",
    "Desks",
    "Lighting",
];
const ADJECTIVES: [&str; 10] = [
    "Compact",
    "Pro",
    "Silent",
    "Ergonomic",
    "Wireless",
    "Ultra",
    "Classic",
    "Modular",
    "Slim",
    "Studio",
];
const NOUNS: [&str; 10] = [
    "Keyboard", "Display", "Cable", "Drive", "Headset", "Chair", "Desk", "Lamp", "Hub", "Dock",
];
const DEVICES: [&str; 4] = ["desktop", "mobile", "tablet", "tv"];
const WORDS: [&str; 16] = [
    "fast", "native", "query", "index", "vacuum", "replica", "shard", "cursor", "commit",
    "rollback", "schema", "column", "tuple", "page", "buffer", "lock",
];

/// SplitMix64: a well-mixed hash of a row and column, so cells look random but repeat.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn h(table: Table, id: u64, col: u64) -> u64 {
    mix(id.wrapping_mul(1_000_003) ^ col.wrapping_mul(0x5851_F42D) ^ (table as u64) << 56)
}

fn pick<'a>(list: &[&'a str], seed: u64) -> &'a str {
    list[(seed % list.len() as u64) as usize]
}

/// 2015-01-01 00:00:00 UTC, in seconds.
const EPOCH_2015: i64 = 1_420_070_400;

/// A Unix time as `YYYY-MM-DD HH:MM:SS`.
fn datetime(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn date(days: i64) -> String {
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn money(cents: u64) -> Value {
    Value::text(format!("{}.{:02}", cents / 100, cents % 100))
}

fn col(name: &str, type_name: &str, kind: ValueKind, nullable: bool) -> Column {
    Column {
        nullable: Some(nullable),
        ..Column::new(name, type_name, kind)
    }
}

fn pk(name: &str, type_name: &str, kind: ValueKind) -> Column {
    Column {
        nullable: Some(false),
        primary_key: Some(0),
        ..Column::new(name, type_name, kind)
    }
}

impl Table {
    fn namespace(self) -> &'static str {
        match self {
            Table::DailyRevenue | Table::Sessions => "analytics",
            Table::Orders2019 | Table::Documents => "archive",
            _ => "public",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Table::Users => "users",
            Table::Orders => "orders",
            Table::Products => "products",
            Table::OrderItems => "order_items",
            Table::EventsWide => "events_wide",
            Table::DailyRevenue => "daily_revenue",
            Table::Sessions => "sessions",
            Table::Orders2019 => "orders_2019",
            Table::Documents => "documents",
        }
    }

    fn kind(self) -> ObjectKind {
        match self {
            Table::OrderItems | Table::DailyRevenue => ObjectKind::View,
            _ => ObjectKind::Table,
        }
    }

    fn rows(self) -> u64 {
        match self {
            Table::Users => 12_480,
            Table::Orders => 50_000_000,
            Table::Products => 1_250,
            Table::OrderItems => 3_400_000,
            Table::EventsWide => 250_000,
            Table::DailyRevenue => 3_650,
            Table::Sessions => 2_000_000,
            Table::Orders2019 => 812_113,
            Table::Documents => 5_000,
        }
    }

    fn comment(self) -> Option<&'static str> {
        match self {
            Table::Orders => Some("Fifty million computed rows: scroll anywhere."),
            Table::EventsWide => Some("Three hundred columns."),
            _ => None,
        }
    }

    fn columns(self) -> Vec<Column> {
        use ValueKind as K;
        match self {
            Table::Users => vec![
                pk("id", "int4", K::Int),
                col("email", "varchar(255)", K::Text, false),
                col("name", "text", K::Text, false),
                col("country", "char(2)", K::Text, false),
                col("plan", "text", K::Text, false),
                col("is_active", "bool", K::Bool, false),
                col("score", "float8", K::Float, true),
                col("created_at", "timestamptz", K::DateTime, false),
                col("last_login", "timestamptz", K::DateTime, true),
                col("metadata", "jsonb", K::Json, true),
            ],
            Table::Orders | Table::Orders2019 => vec![
                pk("id", "int8", K::Int),
                col("user_id", "int4", K::Int, false),
                col("status", "text", K::Text, false),
                col("total", "numeric(12,2)", K::Decimal, false),
                col("currency", "char(3)", K::Text, false),
                col("items", "int2", K::Int, false),
                col("created_at", "timestamptz", K::DateTime, false),
                col("shipped_at", "timestamptz", K::DateTime, true),
                col("note", "text", K::Text, true),
            ],
            Table::Products => vec![
                pk("id", "int4", K::Int),
                col("sku", "varchar(32)", K::Text, false),
                col("name", "text", K::Text, false),
                col("category", "text", K::Text, false),
                col("price", "numeric(10,2)", K::Decimal, false),
                col("stock", "int4", K::Int, false),
                col("discontinued", "bool", K::Bool, false),
                col("attributes", "jsonb", K::Json, true),
            ],
            Table::OrderItems => vec![
                col("order_id", "int8", K::Int, false),
                col("product_id", "int4", K::Int, false),
                col("quantity", "int2", K::Int, false),
                col("unit_price", "numeric(10,2)", K::Decimal, false),
            ],
            Table::EventsWide => {
                let mut cols = vec![pk("id", "int8", K::Int)];
                for i in 1..300u32 {
                    let (t, k) = match i % 6 {
                        0 => ("int4", K::Int),
                        1 => ("text", K::Text),
                        2 => ("float8", K::Float),
                        3 => ("bool", K::Bool),
                        4 => ("timestamptz", K::DateTime),
                        _ => ("numeric", K::Decimal),
                    };
                    cols.push(col(&format!("metric_{i:03}"), t, k, true));
                }
                cols
            }
            Table::DailyRevenue => vec![
                col("day", "date", K::Date, false),
                col("orders", "int8", K::Int, false),
                col("revenue", "numeric", K::Decimal, false),
                col("refunds", "numeric", K::Decimal, false),
            ],
            Table::Sessions => vec![
                pk("id", "uuid", K::Uuid),
                col("user_id", "int4", K::Int, false),
                col("device", "text", K::Text, false),
                col("started_at", "timestamptz", K::DateTime, false),
                col("duration_s", "int4", K::Int, false),
                col("pages", "int2", K::Int, false),
            ],
            Table::Documents => vec![
                pk("_id", "objectId", K::ObjectId),
                col("title", "string", K::Text, false),
                col("tags", "array", K::Array, true),
                col("body", "object", K::Json, true),
            ],
        }
    }

    /// The value of column `c` in the row with id `id` (1-based, in key order).
    fn value(self, id: u64, c: usize) -> Value {
        let r = |n: u64| h(self, id, n);
        let c64 = c as u64;
        match (self, c) {
            (Table::Users, 0) => Value::Int(id as i64),
            (Table::Users, 1) => {
                let first = pick(&FIRST, r(2)).to_lowercase();
                let last = pick(&LAST, r(3)).to_lowercase().replace(' ', "");
                Value::text(format!("{first}.{last}{}@{}", id % 997, pick(&DOMAINS, r(1))))
            }
            (Table::Users, 2) => {
                Value::text(format!("{} {}", pick(&FIRST, r(2)), pick(&LAST, r(3))))
            }
            (Table::Users, 3) => Value::text(pick(&COUNTRIES, r(4))),
            (Table::Users, 4) => Value::text(pick(&PLANS, r(5) % 7 % 4)),
            (Table::Users, 5) => Value::Bool(r(6) % 10 < 8),
            (Table::Users, 6) => {
                if r(7) % 9 == 0 {
                    Value::Null
                } else {
                    Value::Float((r(7) % 100_000) as f64 / 1000.0)
                }
            }
            (Table::Users, 7) => Value::text(datetime(EPOCH_2015 + id as i64 * 21_600)),
            (Table::Users, 8) => {
                if r(8) % 10 == 0 {
                    Value::Null
                } else {
                    Value::text(datetime(EPOCH_2015 + id as i64 * 21_600 + (r(8) % 90_000_000) as i64))
                }
            }
            (Table::Users, _) => {
                if r(9) % 5 == 0 {
                    Value::Null
                } else {
                    Value::Json(
                        format!(
                            r#"{{"theme":"{}","beta":{},"seats":{},"tags":["{}","{}"]}}"#,
                            if r(10) % 2 == 0 { "dark" } else { "light" },
                            r(11) % 3 == 0,
                            r(12) % 50 + 1,
                            pick(&WORDS, r(13)),
                            pick(&WORDS, r(14))
                        )
                        .into(),
                    )
                }
            }
            (Table::Orders | Table::Orders2019, 0) => Value::Int(id as i64),
            (Table::Orders | Table::Orders2019, 1) => Value::Int((r(1) % 12_480 + 1) as i64),
            (Table::Orders | Table::Orders2019, 2) => Value::text(pick(&STATUSES, r(2))),
            (Table::Orders | Table::Orders2019, 3) => money(r(3) % 250_000 + 199),
            (Table::Orders | Table::Orders2019, 4) => Value::text(pick(&CURRENCIES, r(4) % 9 % 4)),
            (Table::Orders | Table::Orders2019, 5) => Value::Int((r(5) % 9 + 1) as i64),
            (Table::Orders | Table::Orders2019, 6) => Value::text(datetime(self.order_time(id))),
            (Table::Orders | Table::Orders2019, 7) => {
                if r(2) % 5 < 2 {
                    Value::Null
                } else {
                    Value::text(datetime(self.order_time(id) + (r(7) % 400_000) as i64))
                }
            }
            (Table::Orders | Table::Orders2019, _) => {
                if r(8) % 6 != 0 {
                    Value::Null
                } else {
                    Value::text(format!(
                        "{} {} please",
                        pick(&["Leave at door", "Gift wrap", "Call first", "No invoice"], r(9)),
                        pick(&WORDS, r(10))
                    ))
                }
            }
            (Table::Products, 0) => Value::Int(id as i64),
            (Table::Products, 1) => Value::text(format!(
                "{}-{:05}",
                pick(&CATEGORIES, r(3))[..3].to_uppercase(),
                id * 7
            )),
            (Table::Products, 2) => Value::text(format!(
                "{} {} {}",
                pick(&ADJECTIVES, r(1)),
                pick(&NOUNS, r(2)),
                r(4) % 9 + 1
            )),
            (Table::Products, 3) => Value::text(pick(&CATEGORIES, r(3))),
            (Table::Products, 4) => money(r(5) % 90_000 + 499),
            (Table::Products, 5) => Value::Int((r(6) % 2_000) as i64),
            (Table::Products, 6) => Value::Bool(r(7) % 12 == 0),
            (Table::Products, _) => Value::Json(
                format!(
                    r#"{{"color":"{}","weight_g":{},"warranty_years":{}}}"#,
                    pick(&["black", "white", "silver", "graphite"], r(8)),
                    r(9) % 5_000 + 50,
                    r(10) % 4 + 1
                )
                .into(),
            ),
            (Table::OrderItems, 0) => Value::Int((id / 3 + 1) as i64),
            (Table::OrderItems, 1) => Value::Int((r(1) % 1_250 + 1) as i64),
            (Table::OrderItems, 2) => Value::Int((r(2) % 5 + 1) as i64),
            (Table::OrderItems, _) => money(r(3) % 90_000 + 499),
            (Table::EventsWide, 0) => Value::Int(id as i64),
            (Table::EventsWide, _) => {
                let v = r(c64);
                if v % 13 == 0 {
                    return Value::Null;
                }
                match c % 6 {
                    0 => Value::Int((v % 100_000) as i64 - 50_000),
                    1 => Value::text(format!("{}-{}", pick(&WORDS, v), v % 1000)),
                    2 => Value::Float(((v % 2_000_000) as f64 - 1_000_000.0) / 997.0),
                    3 => Value::Bool(v % 2 == 0),
                    4 => Value::text(datetime(EPOCH_2015 + (v % 300_000_000) as i64)),
                    _ => money(v % 10_000_000),
                }
            }
            (Table::DailyRevenue, 0) => Value::text(date(EPOCH_2015 / 86_400 + id as i64 - 1)),
            (Table::DailyRevenue, 1) => Value::Int((r(1) % 4_000 + 9_000) as i64),
            (Table::DailyRevenue, 2) => money(r(2) % 50_000_000 + 90_000_000),
            (Table::DailyRevenue, _) => money(r(3) % 2_000_000),
            (Table::Sessions, 0) => {
                let (a, b) = (r(1), r(2));
                Value::text(format!(
                    "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
                    a >> 32,
                    (a >> 16) & 0xffff,
                    a & 0xfff,
                    (b >> 48) & 0x3fff | 0x8000,
                    b & 0xffff_ffff_ffff
                ))
            }
            (Table::Sessions, 1) => Value::Int((r(3) % 12_480 + 1) as i64),
            (Table::Sessions, 2) => Value::text(pick(&DEVICES, r(4) % 7 % 4)),
            (Table::Sessions, 3) => Value::text(datetime(EPOCH_2015 + id as i64 * 157)),
            (Table::Sessions, 4) => Value::Int((r(5) % 3_600 + 4) as i64),
            (Table::Sessions, _) => Value::Int((r(6) % 40 + 1) as i64),
            (Table::Documents, 0) => Value::text(format!("65f{:021x}", mix(id) >> 48 << 40 | id)),
            (Table::Documents, 1) => Value::text(format!(
                "{} {} notes",
                pick(&ADJECTIVES, r(1)),
                pick(&WORDS, r(2))
            )),
            (Table::Documents, 2) => Value::Json(
                format!(r#"["{}","{}"]"#, pick(&WORDS, r(3)), pick(&WORDS, r(4))).into(),
            ),
            (Table::Documents, _) => Value::Json(
                format!(
                    r#"{{"author":{{"name":"{} {}","id":{}}},"revision":{},"published":{},"sections":[{{"heading":"{}","words":{}}},{{"heading":"{}","words":{}}}]}}"#,
                    pick(&FIRST, r(5)),
                    pick(&LAST, r(6)),
                    r(7) % 12_480 + 1,
                    r(8) % 40 + 1,
                    r(9) % 3 != 0,
                    pick(&WORDS, r(10)),
                    r(11) % 2_000,
                    pick(&WORDS, r(12)),
                    r(13) % 2_000
                )
                .into(),
            ),
        }
    }

    fn order_time(self, id: u64) -> i64 {
        match self {
            Table::Orders2019 => 1_546_300_800 + (id as i64 * 38),
            _ => EPOCH_2015 + (id as i64 * 7),
        }
    }

    fn row(self, id: u64, width: usize) -> Row {
        (0..width).map(|c| self.value(id, c)).collect()
    }

    fn key_column(self) -> Option<usize> {
        match self {
            Table::OrderItems | Table::DailyRevenue => None,
            _ => Some(0),
        }
    }
}

fn find(object: &ObjectRef) -> Option<Table> {
    TABLES.into_iter().find(|t| {
        t.name() == object.name
            && object.namespace.last().map(String::as_str) == Some(t.namespace())
    })
}

/// Orders values as a database would: NULLs last, numbers by value, text by bytes.
fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        (Value::Int(x), Value::Float(y)) => (*x as f64).total_cmp(y),
        (Value::Float(x), Value::Int(y)) => x.total_cmp(&(*y as f64)),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => {
            // Decimals are text; compare them as numbers when both parse.
            match (x.parse::<f64>(), y.parse::<f64>()) {
                (Ok(p), Ok(q)) => p.total_cmp(&q),
                _ => x.cmp(y),
            }
        }
        _ => a.to_string().cmp(&b.to_string()),
    }
}

fn matches(value: &Value, filter: &Filter) -> bool {
    let ord = || compare(value, &filter.value);
    let text = || value.to_string().to_lowercase();
    let needle = || filter.value.to_string().to_lowercase();
    match filter.op {
        FilterOp::IsNull => value.is_null(),
        FilterOp::IsNotNull => !value.is_null(),
        _ if value.is_null() => false,
        FilterOp::Eq => ord() == Ordering::Equal || text() == needle(),
        FilterOp::NotEq => !(ord() == Ordering::Equal || text() == needle()),
        FilterOp::Lt => ord() == Ordering::Less,
        FilterOp::LtEq => ord() != Ordering::Greater,
        FilterOp::Gt => ord() == Ordering::Greater,
        FilterOp::GtEq => ord() != Ordering::Less,
        FilterOp::Contains => text().contains(&needle()),
        FilterOp::StartsWith => text().starts_with(&needle()),
    }
}

/// A browse request reduced to what decides row order and membership.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Shape {
    table: Table,
    sort: Vec<(usize, bool)>,
    /// Column, operator (by name: `FilterOp` is not `Hash`) and value.
    filters: Vec<(usize, String, String)>,
}

/// How a request maps positions to row ids.
enum Order {
    /// Ids 1..=n, possibly reversed: positions map to ids directly.
    Key { descending: bool, rows: u64 },
    /// A materialized list of ids, for the small tables.
    List(Arc<Vec<u64>>),
}

pub struct DemoConnection {
    orders: Mutex<HashMap<Shape, Arc<Vec<u64>>>>,
}

impl Default for DemoConnection {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoConnection {
    pub fn new() -> Self {
        Self {
            orders: Mutex::new(HashMap::new()),
        }
    }

    fn order(&self, table: Table, request: &BrowseRequest) -> Result<Order> {
        let columns = table.columns();
        let index = |name: &str| {
            columns
                .iter()
                .position(|c| c.name == name)
                .ok_or_else(|| Error::Query(format!("column \"{name}\" does not exist")))
        };
        if request
            .raw_where
            .as_ref()
            .is_some_and(|w| !w.trim().is_empty())
        {
            return Err(Error::Unsupported("a WHERE clause on the demo connection"));
        }
        let mut sort = Vec::new();
        for s in &request.sort {
            sort.push((index(&s.column)?, s.direction == SortDirection::Descending));
        }
        let mut filters = Vec::new();
        for f in &request.filters {
            filters.push((
                index(&f.column)?,
                format!("{:?}", f.op),
                f.value.to_string(),
            ));
        }
        let by_key = filters.is_empty()
            && (sort.is_empty() || (sort.len() == 1 && Some(sort[0].0) == table.key_column()));
        if by_key {
            return Ok(Order::Key {
                descending: sort.first().is_some_and(|s| s.1),
                rows: table.rows(),
            });
        }
        if table.rows() > SORTABLE_LIMIT {
            return Err(Error::Unsupported(
                "sorting or filtering a large demo table by anything but its key",
            ));
        }
        let shape = Shape {
            table,
            sort: sort.clone(),
            filters: filters.clone(),
        };
        if let Some(list) = self.orders.lock().get(&shape) {
            return Ok(Order::List(list.clone()));
        }
        let mut ids: Vec<u64> = (1..=table.rows())
            .filter(|id| {
                request
                    .filters
                    .iter()
                    .zip(&filters)
                    .all(|(f, (c, _, _))| matches(&table.value(*id, *c), f))
            })
            .collect();
        if !sort.is_empty() {
            let keys: HashMap<u64, Vec<Value>> = ids
                .iter()
                .map(|id| {
                    (
                        *id,
                        sort.iter().map(|(c, _)| table.value(*id, *c)).collect(),
                    )
                })
                .collect();
            ids.sort_by(|a, b| {
                let (ka, kb) = (&keys[a], &keys[b]);
                for (i, (_, desc)) in sort.iter().enumerate() {
                    let o = compare(&ka[i], &kb[i]);
                    let o = if *desc { o.reverse() } else { o };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                a.cmp(b)
            });
        }
        let list = Arc::new(ids);
        let mut cache = self.orders.lock();
        if cache.len() > 32 {
            cache.clear();
        }
        cache.insert(shape, list.clone());
        Ok(Order::List(list))
    }
}

impl Order {
    fn len(&self) -> u64 {
        match self {
            Order::Key { rows, .. } => *rows,
            Order::List(l) => l.len() as u64,
        }
    }

    fn id_at(&self, position: u64) -> Option<u64> {
        match self {
            Order::Key { descending, rows } => (position < *rows).then(|| {
                if *descending {
                    rows - position
                } else {
                    position + 1
                }
            }),
            Order::List(l) => l.get(position as usize).copied(),
        }
    }
}

impl Connection for DemoConnection {
    fn kind(&self) -> DriverKind {
        DriverKind::Postgres
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            schemas: true,
            transactions: false,
            edit_rows: false,
            cancel: false,
            language: "sql",
        }
    }

    fn server_version(&self) -> String {
        "PostgreSQL 16.4 (demo)".into()
    }

    fn namespaces(&self) -> BoxFuture<'_, Result<Vec<Namespace>>> {
        Box::pin(async {
            Ok(["public", "analytics", "archive"]
                .into_iter()
                .map(|s| Namespace {
                    path: vec!["demo".into(), s.into()],
                    kind: NamespaceKind::Schema,
                })
                .collect())
        })
    }

    fn objects<'a>(&'a self, namespace: &'a [String]) -> BoxFuture<'a, Result<Vec<ObjectEntry>>> {
        Box::pin(async move {
            let schema = namespace.last().map(String::as_str).unwrap_or_default();
            Ok(TABLES
                .into_iter()
                .filter(|t| t.namespace() == schema)
                .map(|t| ObjectEntry {
                    object: ObjectRef {
                        namespace: namespace.to_vec(),
                        name: t.name().into(),
                        kind: t.kind(),
                    },
                    estimated_rows: Some(t.rows()),
                    size_bytes: Some(t.rows() * t.columns().len() as u64 * 9),
                    comment: t.comment().map(Into::into),
                })
                .collect())
        })
    }

    fn describe<'a>(&'a self, object: &'a ObjectRef) -> BoxFuture<'a, Result<ObjectInfo>> {
        Box::pin(async move {
            let table = find(object).ok_or_else(|| {
                Error::Query(format!("relation \"{}\" does not exist", object.name))
            })?;
            let columns = table.columns();
            let mut indexes = Vec::new();
            if let Some(k) = table.key_column() {
                indexes.push(Index {
                    name: format!("{}_pkey", table.name()),
                    columns: vec![columns[k].name.clone()],
                    unique: true,
                    primary: true,
                    method: Some("btree".into()),
                });
            }
            if matches!(table, Table::Orders | Table::Orders2019) {
                indexes.push(Index {
                    name: format!("{}_user_id_idx", table.name()),
                    columns: vec!["user_id".into()],
                    unique: false,
                    primary: false,
                    method: Some("btree".into()),
                });
            }
            let mut foreign_keys = Vec::new();
            if matches!(table, Table::Orders) {
                foreign_keys.push(deltos_data::ForeignKey {
                    name: "orders_user_id_fkey".into(),
                    columns: vec!["user_id".into()],
                    target: ObjectRef {
                        namespace: object.namespace.clone(),
                        name: "users".into(),
                        kind: ObjectKind::Table,
                    },
                    target_columns: vec!["id".into()],
                });
            }
            let definition = Some(format!(
                "CREATE {} {}.{} (\n{}\n);",
                if table.kind() == ObjectKind::View {
                    "VIEW"
                } else {
                    "TABLE"
                },
                table.namespace(),
                table.name(),
                columns
                    .iter()
                    .map(|c| format!(
                        "    {} {}{}",
                        c.name,
                        c.type_name,
                        if c.nullable == Some(false) {
                            " NOT NULL"
                        } else {
                            ""
                        }
                    ))
                    .collect::<Vec<_>>()
                    .join(",\n")
            ));
            Ok(ObjectInfo {
                estimated_rows: Some(table.rows()),
                size_bytes: Some(table.rows() * columns.len() as u64 * 9),
                comment: table.comment().map(Into::into),
                columns,
                indexes,
                foreign_keys,
                definition,
            })
        })
    }

    fn browse<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
    ) -> BoxFuture<'a, Result<BrowsePage>> {
        Box::pin(async move {
            let table = find(object).ok_or_else(|| {
                Error::Query(format!("relation \"{}\" does not exist", object.name))
            })?;
            let order = self.order(table, request)?;
            let columns: Arc<[Column]> = table.columns().into();
            let width = columns.len();
            let end = request
                .offset
                .saturating_add(u64::from(request.limit))
                .min(order.len());
            let rows: Vec<Row> = (request.offset..end)
                .filter_map(|pos| order.id_at(pos))
                .map(|id| table.row(id, width))
                .collect();
            let last_key = table.key_column().and_then(|k| {
                rows.last()
                    .map(|r| vec![(columns[k].name.clone(), r[k].clone())])
            });
            Ok(BrowsePage {
                columns,
                rows,
                more: end < order.len(),
                last_key,
            })
        })
    }

    fn count<'a>(
        &'a self,
        object: &'a ObjectRef,
        request: &'a BrowseRequest,
        _exact: bool,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let table = find(object).ok_or_else(|| {
                Error::Query(format!("relation \"{}\" does not exist", object.name))
            })?;
            Ok(self.order(table, request)?.len())
        })
    }

    fn execute<'a>(
        &'a self,
        statement: &'a str,
        options: &'a ExecuteOptions,
    ) -> BoxFuture<'a, Result<RowStream>> {
        Box::pin(async move {
            let query = parse_select(statement)?;
            let table = TABLES
                .into_iter()
                .find(|t| {
                    t.name() == query.table
                        || format!("{}.{}", t.namespace(), t.name()) == query.table
                })
                .ok_or_else(|| {
                    Error::Query(format!("relation \"{}\" does not exist", query.table))
                })?;
            if query.count {
                let columns: Arc<[Column]> =
                    vec![col("count", "int8", ValueKind::Int, false)].into();
                let row: Row = vec![Value::Int(table.rows() as i64)].into();
                return Ok(RowStream {
                    columns,
                    batches: Box::pin(futures::stream::iter([Ok(RowBatch { rows: vec![row] })])),
                    affected: None,
                    messages: Vec::new(),
                });
            }
            let columns: Arc<[Column]> = table.columns().into();
            let width = columns.len();
            let total = query.limit.unwrap_or(u64::MAX).min(table.rows());
            let batch = u64::from(options.batch_size.max(1));
            // Rows are made only as the caller pulls each batch.
            let batches = futures::stream::unfold(0u64, move |next| async move {
                if next >= total {
                    return None;
                }
                let end = (next + batch).min(total);
                let rows = (next..end).map(|i| table.row(i + 1, width)).collect();
                Some((Ok(RowBatch { rows }), end))
            });
            Ok(RowStream {
                columns,
                batches: Box::pin(batches),
                affected: None,
                messages: Vec::new(),
            })
        })
    }
}

struct Select {
    table: String,
    count: bool,
    limit: Option<u64>,
}

/// Understands `SELECT * FROM t [LIMIT n]` and `SELECT COUNT(*) FROM t`, which is what
/// the demo needs to show results streaming in.
fn parse_select(statement: &str) -> Result<Select> {
    let text = statement.trim().trim_end_matches(';').trim();
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    let help = || {
        Error::Query(
            "the demo connection runs SELECT * FROM <table> [LIMIT n] and SELECT COUNT(*) FROM <table>"
                .into(),
        )
    };
    if words.len() < 4 || words[0] != "select" || words[2] != "from" {
        return Err(help());
    }
    let count = match words[1].as_str() {
        "*" => false,
        "count(*)" => true,
        _ => return Err(help()),
    };
    let table = words[3].trim_matches('"').replace("\".\"", ".");
    let limit = match words.get(4).map(String::as_str) {
        None => None,
        Some("limit") => Some(
            words
                .get(5)
                .and_then(|n| n.parse::<u64>().ok())
                .ok_or_else(help)?,
        ),
        Some(_) => return Err(help()),
    };
    Ok(Select {
        table,
        count,
        limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn object(ns: &str, name: &str) -> ObjectRef {
        ObjectRef {
            namespace: vec!["demo".into(), ns.into()],
            name: name.into(),
            kind: ObjectKind::Table,
        }
    }

    #[test]
    fn dates_are_civil() {
        assert_eq!(datetime(EPOCH_2015), "2015-01-01 00:00:00");
        assert_eq!(date(19_782), "2024-02-29");
    }

    #[test]
    fn browses_anywhere_in_the_big_table() {
        let demo = DemoConnection::new();
        let orders = object("public", "orders");
        let page = futures::executor::block_on(demo.browse(
            &orders,
            &BrowseRequest {
                offset: 49_999_990,
                limit: 50,
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(page.rows.len(), 10);
        assert!(!page.more);
        assert_eq!(page.rows[0][0], Value::Int(49_999_991));
        // The same cell reads the same every time.
        assert_eq!(Table::Orders.value(7, 2), Table::Orders.value(7, 2));
        let desc = futures::executor::block_on(demo.browse(
            &orders,
            &BrowseRequest {
                limit: 1,
                sort: vec![deltos_data::Sort {
                    column: "id".into(),
                    direction: SortDirection::Descending,
                }],
                ..Default::default()
            },
        ))
        .unwrap();
        assert_eq!(desc.rows[0][0], Value::Int(50_000_000));
    }

    #[test]
    fn small_tables_sort_and_filter() {
        let demo = DemoConnection::new();
        let users = object("public", "users");
        let request = BrowseRequest {
            limit: 100,
            sort: vec![deltos_data::Sort {
                column: "score".into(),
                direction: SortDirection::Descending,
            }],
            filters: vec![Filter {
                column: "country".into(),
                op: FilterOp::Eq,
                value: Value::text("DE"),
            }],
            ..Default::default()
        };
        let page = futures::executor::block_on(demo.browse(&users, &request)).unwrap();
        assert!(!page.rows.is_empty());
        assert!(page.rows.iter().all(|r| r[3] == Value::text("DE")));
        let scores: Vec<&Value> = page.rows.iter().map(|r| &r[6]).collect();
        assert!(
            scores
                .windows(2)
                .all(|w| compare(w[0], w[1]) != Ordering::Less)
        );
        let n = futures::executor::block_on(demo.count(&users, &request, true)).unwrap();
        assert!(n < 12_480);
    }

    #[test]
    fn wide_table_has_three_hundred_columns() {
        assert_eq!(Table::EventsWide.columns().len(), 300);
        assert_eq!(Table::EventsWide.row(5, 300).len(), 300);
    }

    #[test]
    fn executes_streaming_selects() {
        let demo = DemoConnection::new();
        let options = ExecuteOptions {
            batch_size: 1000,
            namespace: None,
        };
        let stream =
            futures::executor::block_on(demo.execute("select * from orders limit 2500", &options))
                .unwrap();
        let batches: Vec<_> = futures::executor::block_on(stream.batches.collect::<Vec<_>>());
        assert_eq!(batches.len(), 3);
        assert!(parse_select("delete from users").is_err());
        assert!(parse_select("select * from users limit x").is_err());
    }
}
