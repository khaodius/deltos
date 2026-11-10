//! SQL text shared by the SQL drivers: identifier quoting per dialect, statement
//! splitting, and the `SELECT` and `COUNT` statements behind the table view.
//!
//! # Ordering and seeking
//!
//! A browse orders rows by the sort columns, then by tie-break columns (the primary key or
//! the row identity) that make the order total. To read the page after a known row, the
//! driver adds a condition that keeps only rows after that row in this order:
//!
//! - When every column runs the same way and none can be NULL, it is a row value
//!   comparison, `(a, b) > ($1, $2)`, which PostgreSQL and SQLite answer with one index
//!   range scan.
//! - Otherwise it is the expanded form `a > $1 OR (a = $1 AND b > $2) OR ...`, with the
//!   comparison flipped for descending columns. MySQL always gets this form, since its
//!   optimizer handles it better than row values. When the first column cannot be NULL
//!   the condition also carries `a >= $1`, which lets the planner start an index range.
//!
//! NULLs sort where the database puts them by default, so the order matches the
//! database's indexes: PostgreSQL treats NULL as larger than any value (last when
//! ascending, first when descending), MySQL and SQLite as smaller. The seek condition
//! follows the same rule. After a NULL in a column where NULLs come last, nothing in that
//! column is later; where NULLs come first, every non-NULL value is. After a value, the
//! later rows of a nullable column where NULLs come last include the NULLs. Equality on a
//! NULL in the expanded form is written `IS NULL`.
//!
//! # Values
//!
//! Every value is a bound parameter. The builders return the SQL text with placeholders
//! (`$1` for PostgreSQL, `?` for the others) and the values in placeholder order, and each
//! driver binds them its own way. Only numbers this crate produces, `LIMIT` and `OFFSET`,
//! are written into the text.

use deltos_data::{
    BrowseRequest, Error, Filter, FilterOp, Result, RowEdit, Sort, SortDirection, Value,
};

mod split;

pub use split::split_statements;

/// The SQL flavour a statement is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Mysql,
    Sqlite,
}

impl Dialect {
    /// Appends `name` as a quoted identifier. NUL characters, which no server accepts in
    /// a name, are dropped.
    pub fn push_ident(self, out: &mut String, name: &str) {
        let quote = match self {
            Dialect::Mysql => '`',
            Dialect::Postgres | Dialect::Sqlite => '"',
        };
        out.reserve(name.len() + 2);
        out.push(quote);
        for c in name.chars() {
            if c == '\0' {
                continue;
            }
            if c == quote {
                out.push(quote);
            }
            out.push(c);
        }
        out.push(quote);
    }

    pub fn quote_ident(self, name: &str) -> String {
        let mut out = String::new();
        self.push_ident(&mut out, name);
        out
    }

    /// `"schema"."name"`, with as many leading parts as given.
    pub fn qualified<S: AsRef<str>>(self, parts: &[S], name: &str) -> String {
        let mut out = String::new();
        for part in parts {
            self.push_ident(&mut out, part.as_ref());
            out.push('.');
        }
        self.push_ident(&mut out, name);
        out
    }

    /// Appends `text` as a string literal. Used only for catalog names in statements that
    /// cannot take parameters, such as MySQL's `SHOW CREATE TABLE`.
    pub fn push_string_literal(self, out: &mut String, text: &str) {
        out.push('\'');
        for c in text.chars() {
            match c {
                '\0' => {}
                '\'' => out.push_str("''"),
                '\\' if self == Dialect::Mysql => out.push_str("\\\\"),
                c => out.push(c),
            }
        }
        out.push('\'');
    }

    /// Whether NULL sorts after every value in an ascending order.
    fn nulls_high(self) -> bool {
        self == Dialect::Postgres
    }

    fn row_values(self) -> bool {
        self != Dialect::Mysql
    }

    fn placeholder(self, out: &mut String, n: usize) {
        match self {
            Dialect::Postgres => {
                out.push('$');
                out.push_str(&n.to_string());
            }
            Dialect::Mysql | Dialect::Sqlite => out.push('?'),
        }
    }

    /// The column as text, for substring matches on columns of any type.
    fn push_as_text(self, out: &mut String, column: &str) {
        match self {
            Dialect::Postgres => {
                out.push_str("CAST(");
                self.push_ident(out, column);
                out.push_str(" AS text)");
            }
            Dialect::Mysql => {
                out.push_str("CAST(");
                self.push_ident(out, column);
                out.push_str(" AS CHAR)");
            }
            Dialect::Sqlite => self.push_ident(out, column),
        }
    }

    fn like(self) -> &'static str {
        match self {
            Dialect::Postgres => " ILIKE ",
            Dialect::Mysql | Dialect::Sqlite => " LIKE ",
        }
    }
}

/// Escapes `%`, `_` and the escape character itself for a `LIKE ... ESCAPE '!'` pattern.
/// `!` works the same on every server, whatever their backslash settings.
pub fn escape_like(out: &mut String, text: &str) {
    for c in text.chars() {
        if matches!(c, '%' | '_' | '!') {
            out.push('!');
        }
        out.push(c);
    }
}

/// SQL text under construction with its parameters.
#[derive(Debug)]
pub struct SqlBuilder {
    dialect: Dialect,
    sql: String,
    params: Vec<Value>,
}

impl SqlBuilder {
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            sql: String::with_capacity(256),
            params: Vec::new(),
        }
    }

    pub fn push(&mut self, text: &str) -> &mut Self {
        self.sql.push_str(text);
        self
    }

    pub fn ident(&mut self, name: &str) -> &mut Self {
        self.dialect.push_ident(&mut self.sql, name);
        self
    }

    pub fn param(&mut self, value: Value) -> &mut Self {
        self.params.push(value);
        self.dialect.placeholder(&mut self.sql, self.params.len());
        self
    }

    pub fn finish(self) -> (String, Vec<Value>) {
        (self.sql, self.params)
    }
}

/// One column of the browse order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderKey {
    pub column: String,
    pub descending: bool,
    /// Whether the column can hold NULL. When unsure, say true: the seek condition is
    /// then correct either way, only less simple.
    pub nullable: bool,
}

/// The browse order: the sort columns, then each tie-break column not already sorted on,
/// ascending.
pub fn order_keys(
    sort: &[Sort],
    tie_break: &[String],
    nullable: impl Fn(&str) -> bool,
) -> Vec<OrderKey> {
    let mut keys: Vec<OrderKey> = Vec::with_capacity(sort.len() + tie_break.len());
    for s in sort {
        if keys.iter().any(|k| k.column == s.column) {
            continue;
        }
        keys.push(OrderKey {
            column: s.column.clone(),
            descending: s.direction == SortDirection::Descending,
            nullable: nullable(&s.column),
        });
    }
    for column in tie_break {
        if keys.iter().any(|k| &k.column == column) {
            continue;
        }
        keys.push(OrderKey {
            column: column.clone(),
            descending: false,
            nullable: nullable(column),
        });
    }
    keys
}

/// What to read for one page of a table.
#[derive(Debug)]
pub struct BrowseSql<'a> {
    pub dialect: Dialect,
    /// The select list, already quoted.
    pub select_list: &'a str,
    /// The qualified table name, already quoted.
    pub from: &'a str,
    pub order: &'a [OrderKey],
    /// Whether `order` is total, ending in a key that tells every row apart. Seeking
    /// over an order with ties would skip rows, so only a total order seeks.
    pub total: bool,
}

/// A browse statement and how it was built.
#[derive(Debug)]
pub struct BuiltBrowse {
    pub sql: String,
    pub params: Vec<Value>,
    /// Whether the statement seeks by key instead of skipping `offset` rows.
    pub seeks: bool,
}

impl BrowseSql<'_> {
    /// `SELECT ... WHERE ... ORDER BY ... LIMIT limit + 1`, seeking when the request has a
    /// complete `seek_after` for this order, skipping `offset` rows otherwise. One more row
    /// than asked is read to tell whether more follow.
    pub fn build(&self, request: &BrowseRequest) -> Result<BuiltBrowse> {
        let mut b = SqlBuilder::new(self.dialect);
        b.push("SELECT ")
            .push(self.select_list)
            .push(" FROM ")
            .push(self.from);
        let seek = request
            .seek_after
            .as_deref()
            .filter(|_| self.total)
            .and_then(|after| seek_values(self.order, after));
        let mut first = true;
        push_conditions(&mut b, &mut first, &request.filters, &request.raw_where)?;
        if let Some(values) = &seek {
            b.push(if first { " WHERE " } else { " AND " });
            push_seek(&mut b, self.order, values);
        }
        push_order_by(&mut b, self.order);
        let limit = u64::from(request.limit).saturating_add(1);
        b.push(" LIMIT ").push(&limit.to_string());
        if seek.is_none() && request.offset > 0 {
            b.push(" OFFSET ").push(&request.offset.to_string());
        }
        let (sql, params) = b.finish();
        Ok(BuiltBrowse {
            sql,
            params,
            seeks: seek.is_some(),
        })
    }
}

/// `SELECT COUNT(*) FROM from WHERE ...` for the request's filters.
pub fn count_sql(
    dialect: Dialect,
    from: &str,
    request: &BrowseRequest,
) -> Result<(String, Vec<Value>)> {
    let mut b = SqlBuilder::new(dialect);
    b.push("SELECT COUNT(*) FROM ").push(from);
    let mut first = true;
    push_conditions(&mut b, &mut first, &request.filters, &request.raw_where)?;
    Ok(b.finish())
}

/// Appends ` WHERE ...` (or ` AND ...` when `first` is false) for the filters and the raw
/// condition.
pub fn push_conditions(
    b: &mut SqlBuilder,
    first: &mut bool,
    filters: &[Filter],
    raw_where: &Option<String>,
) -> Result<()> {
    for filter in filters {
        b.push(if *first { " WHERE " } else { " AND " });
        *first = false;
        push_filter(b, filter)?;
    }
    if let Some(raw) = raw_where
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        b.push(if *first { " WHERE " } else { " AND " });
        *first = false;
        // On their own lines, so a trailing `--` comment in the condition cannot swallow
        // the rest of the statement.
        b.push("(\n").push(raw).push("\n)");
    }
    Ok(())
}

fn push_filter(b: &mut SqlBuilder, filter: &Filter) -> Result<()> {
    if filter.column.is_empty() {
        return Err(Error::Query("A filter needs a column.".to_string()));
    }
    let op = match (filter.op, filter.value.is_null()) {
        (FilterOp::IsNull, _) | (FilterOp::Eq, true) => {
            b.ident(&filter.column).push(" IS NULL");
            return Ok(());
        }
        (FilterOp::IsNotNull, _) | (FilterOp::NotEq, true) => {
            b.ident(&filter.column).push(" IS NOT NULL");
            return Ok(());
        }
        (FilterOp::Contains | FilterOp::StartsWith, _) => {
            let text = filter.value.to_string();
            let mut pattern = String::with_capacity(text.len() + 2);
            if filter.op == FilterOp::Contains {
                pattern.push('%');
            }
            escape_like(&mut pattern, &text);
            pattern.push('%');
            let dialect = b.dialect;
            dialect.push_as_text(&mut b.sql, &filter.column);
            b.push(dialect.like())
                .param(Value::Text(pattern.into()))
                .push(" ESCAPE '!'");
            return Ok(());
        }
        (FilterOp::Eq, false) => " = ",
        (FilterOp::NotEq, false) => " <> ",
        (FilterOp::Lt, _) => " < ",
        (FilterOp::LtEq, _) => " <= ",
        (FilterOp::Gt, _) => " > ",
        (FilterOp::GtEq, _) => " >= ",
    };
    b.ident(&filter.column).push(op).param(filter.value.clone());
    Ok(())
}

/// The seek values in order-key order, when `after` names every order column.
fn seek_values(order: &[OrderKey], after: &[(String, Value)]) -> Option<Vec<Value>> {
    if order.is_empty() {
        return None;
    }
    order
        .iter()
        .map(|key| {
            after
                .iter()
                .find(|(name, _)| *name == key.column)
                .map(|(_, value)| value.clone())
        })
        .collect()
}

fn push_seek(b: &mut SqlBuilder, order: &[OrderKey], after: &[Value]) {
    let same_direction = order.iter().all(|k| k.descending == order[0].descending);
    let no_nulls = order.iter().all(|k| !k.nullable) && after.iter().all(|v| !v.is_null());
    if same_direction && no_nulls && (order.len() == 1 || b.dialect.row_values()) {
        let op = if order[0].descending { " < " } else { " > " };
        if order.len() == 1 {
            b.ident(&order[0].column).push(op).param(after[0].clone());
            return;
        }
        b.push("(");
        for (i, key) in order.iter().enumerate() {
            if i > 0 {
                b.push(", ");
            }
            b.ident(&key.column);
        }
        b.push(")").push(op).push("(");
        for (i, value) in after.iter().enumerate() {
            if i > 0 {
                b.push(", ");
            }
            b.param(value.clone());
        }
        b.push(")");
        return;
    }

    b.push("(");
    let first = &order[0];
    let hint = !first.nullable && !after[0].is_null();
    if hint {
        b.ident(&first.column)
            .push(if first.descending { " <= " } else { " >= " })
            .param(after[0].clone())
            .push(" AND (");
    }
    let mut any = false;
    for i in 0..order.len() {
        let key = &order[i];
        let nulls_last = b.dialect.nulls_high() != key.descending;
        let value = &after[i];
        // Nothing in this column comes after a NULL that sorts last.
        if value.is_null() && nulls_last {
            continue;
        }
        if any {
            b.push(" OR ");
        }
        any = true;
        b.push("(");
        for (prefix, prefix_value) in order[..i].iter().zip(after) {
            b.ident(&prefix.column);
            if prefix_value.is_null() {
                b.push(" IS NULL AND ");
            } else {
                b.push(" = ").param(prefix_value.clone()).push(" AND ");
            }
        }
        if value.is_null() {
            b.ident(&key.column).push(" IS NOT NULL");
        } else if key.nullable && nulls_last {
            b.push("(")
                .ident(&key.column)
                .push(if key.descending { " < " } else { " > " })
                .param(value.clone())
                .push(" OR ")
                .ident(&key.column)
                .push(" IS NULL)");
        } else {
            b.ident(&key.column)
                .push(if key.descending { " < " } else { " > " })
                .param(value.clone());
        }
        b.push(")");
    }
    if !any {
        b.push("1 = 0");
    }
    if hint {
        b.push(")");
    }
    b.push(")");
}

fn push_order_by(b: &mut SqlBuilder, order: &[OrderKey]) {
    for (i, key) in order.iter().enumerate() {
        b.push(if i == 0 { " ORDER BY " } else { ", " });
        b.ident(&key.column)
            .push(if key.descending { " DESC" } else { " ASC" });
    }
}

/// The first word of a statement, uppercased, past whitespace, comments and opening
/// parentheses. Drivers use it to tell a statement that changes rows from one that
/// changes the schema when the server reports the same count for both.
pub fn first_keyword(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
        if let Some(after) = rest.strip_prefix("--").or_else(|| rest.strip_prefix('#')) {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    rest.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Whether a statement starting with `keyword` reports a row count worth showing even
/// when it is zero.
pub fn counts_rows(keyword: &str) -> bool {
    matches!(
        keyword,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" | "COPY" | "WITH" | "SELECT"
    )
}

/// One edit from the table view as a statement.
#[derive(Debug)]
pub struct EditSql {
    pub sql: String,
    pub params: Vec<Value>,
    /// Updates and deletes must touch exactly one row; more or fewer means the key did
    /// not identify the row the person edited, and the edits are rolled back.
    pub expect_one: bool,
}

/// The statement for `edit` on `table` (already quoted), or `None` for an update that
/// changes nothing.
pub fn edit_sql(dialect: Dialect, table: &str, edit: &RowEdit) -> Result<Option<EditSql>> {
    let mut b = SqlBuilder::new(dialect);
    let expect_one = match edit {
        RowEdit::Update { key, changes } => {
            if changes.is_empty() {
                return Ok(None);
            }
            require_key(key)?;
            b.push("UPDATE ").push(table).push(" SET ");
            for (i, (column, value)) in changes.iter().enumerate() {
                if i > 0 {
                    b.push(", ");
                }
                b.ident(column).push(" = ").param(value.clone());
            }
            push_key(&mut b, key);
            true
        }
        RowEdit::Delete { key } => {
            require_key(key)?;
            b.push("DELETE FROM ").push(table);
            push_key(&mut b, key);
            true
        }
        RowEdit::Insert { values } => {
            b.push("INSERT INTO ").push(table);
            if values.is_empty() {
                b.push(match dialect {
                    Dialect::Mysql => " () VALUES ()",
                    Dialect::Postgres | Dialect::Sqlite => " DEFAULT VALUES",
                });
            } else {
                b.push(" (");
                for (i, (column, _)) in values.iter().enumerate() {
                    if i > 0 {
                        b.push(", ");
                    }
                    b.ident(column);
                }
                b.push(") VALUES (");
                for (i, (_, value)) in values.iter().enumerate() {
                    if i > 0 {
                        b.push(", ");
                    }
                    b.param(value.clone());
                }
                b.push(")");
            }
            false
        }
    };
    let (sql, params) = b.finish();
    Ok(Some(EditSql {
        sql,
        params,
        expect_one,
    }))
}

fn require_key(key: &[(String, Value)]) -> Result<()> {
    if key.is_empty() {
        return Err(Error::Query(
            "This row cannot be changed: the table has no key that identifies it.".to_string(),
        ));
    }
    Ok(())
}

fn push_key(b: &mut SqlBuilder, key: &[(String, Value)]) {
    for (i, (column, value)) in key.iter().enumerate() {
        b.push(if i == 0 { " WHERE " } else { " AND " });
        if value.is_null() {
            b.ident(column).push(" IS NULL");
        } else {
            b.ident(column).push(" = ").param(value.clone());
        }
    }
}

/// The message for an edit whose key matched `n` rows instead of one.
pub fn edit_mismatch(n: u64) -> Error {
    if n == 0 {
        Error::Query(
            "A row you edited no longer exists or was changed by someone else. Nothing was saved."
                .to_string(),
        )
    } else {
        Error::Query(format!(
            "An edit would have changed {n} rows instead of one, because the key does not identify a single row. Nothing was saved."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(column: &str, descending: bool, nullable: bool) -> OrderKey {
        OrderKey {
            column: column.to_string(),
            descending,
            nullable,
        }
    }

    fn build(dialect: Dialect, order: &[OrderKey], request: &BrowseRequest) -> BuiltBrowse {
        BrowseSql {
            dialect,
            select_list: "*",
            from: "\"t\"",
            order,
            total: true,
        }
        .build(request)
        .expect("builds")
    }

    #[test]
    fn quotes_identifiers() {
        assert_eq!(Dialect::Postgres.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(Dialect::Mysql.quote_ident("a`b"), "`a``b`");
        assert_eq!(Dialect::Sqlite.quote_ident("x\0y"), "\"xy\"");
        assert_eq!(
            Dialect::Postgres.qualified(&["public"], "Users"),
            "\"public\".\"Users\""
        );
        let mut out = String::new();
        Dialect::Mysql.push_string_literal(&mut out, "it's \\ ok");
        assert_eq!(out, "'it''s \\\\ ok'");
    }

    #[test]
    fn escapes_like_patterns() {
        let mut out = String::new();
        escape_like(&mut out, "50%_off!");
        assert_eq!(out, "50!%!_off!!");
    }

    #[test]
    fn offset_without_seek() {
        let order = [key("id", false, false)];
        let request = BrowseRequest {
            offset: 200,
            limit: 100,
            ..Default::default()
        };
        let built = build(Dialect::Postgres, &order, &request);
        assert_eq!(
            built.sql,
            "SELECT * FROM \"t\" ORDER BY \"id\" ASC LIMIT 101 OFFSET 200"
        );
        assert!(!built.seeks);
    }

    #[test]
    fn row_value_seek() {
        let order = [key("a", false, false), key("id", false, false)];
        let request = BrowseRequest {
            offset: 200,
            limit: 100,
            seek_after: Some(vec![
                ("id".into(), Value::Int(7)),
                ("a".into(), Value::text("x")),
            ]),
            ..Default::default()
        };
        let built = build(Dialect::Postgres, &order, &request);
        assert_eq!(
            built.sql,
            "SELECT * FROM \"t\" WHERE (\"a\", \"id\") > ($1, $2) ORDER BY \"a\" ASC, \"id\" ASC LIMIT 101"
        );
        assert_eq!(built.params, vec![Value::text("x"), Value::Int(7)]);
        assert!(built.seeks);
    }

    #[test]
    fn expanded_seek_for_mixed_directions() {
        let order = [key("a", true, false), key("id", false, false)];
        let request = BrowseRequest {
            limit: 10,
            seek_after: Some(vec![
                ("a".into(), Value::Int(5)),
                ("id".into(), Value::Int(9)),
            ]),
            ..Default::default()
        };
        let built = build(Dialect::Sqlite, &order, &request);
        assert_eq!(
            built.sql,
            "SELECT * FROM \"t\" WHERE (\"a\" <= ? AND ((\"a\" < ?) OR (\"a\" = ? AND \"id\" > ?))) ORDER BY \"a\" DESC, \"id\" ASC LIMIT 11"
        );
        assert_eq!(built.params.len(), 4);
    }

    #[test]
    fn nulls_follow_the_dialect() {
        let order = [key("a", false, true), key("id", false, false)];
        let after_null = BrowseRequest {
            limit: 10,
            seek_after: Some(vec![
                ("a".into(), Value::Null),
                ("id".into(), Value::Int(3)),
            ]),
            ..Default::default()
        };
        // PostgreSQL: NULL sorts last when ascending, so only later NULLs remain.
        let pg = build(Dialect::Postgres, &order, &after_null);
        assert!(
            pg.sql.contains("WHERE ((\"a\" IS NULL AND \"id\" > $1))"),
            "{}",
            pg.sql
        );
        // SQLite: NULL sorts first, so every non-NULL value comes later.
        let lite = build(Dialect::Sqlite, &order, &after_null);
        assert!(
            lite.sql
                .contains("WHERE ((\"a\" IS NOT NULL) OR (\"a\" IS NULL AND \"id\" > ?))"),
            "{}",
            lite.sql
        );

        let after_value = BrowseRequest {
            limit: 10,
            seek_after: Some(vec![
                ("a".into(), Value::Int(1)),
                ("id".into(), Value::Int(3)),
            ]),
            ..Default::default()
        };
        let pg = build(Dialect::Postgres, &order, &after_value);
        assert!(
            pg.sql
                .contains("((\"a\" > $1 OR \"a\" IS NULL)) OR (\"a\" = $2 AND \"id\" > $3)"),
            "{}",
            pg.sql
        );
    }

    #[test]
    fn nothing_after_the_last_null() {
        let order = [key("a", false, true)];
        let request = BrowseRequest {
            limit: 10,
            seek_after: Some(vec![("a".into(), Value::Null)]),
            ..Default::default()
        };
        let built = build(Dialect::Postgres, &order, &request);
        assert!(built.sql.contains("WHERE (1 = 0)"), "{}", built.sql);
    }

    #[test]
    fn incomplete_seek_falls_back_to_offset() {
        let order = [key("a", false, false), key("id", false, false)];
        let request = BrowseRequest {
            offset: 50,
            limit: 10,
            seek_after: Some(vec![("a".into(), Value::Int(1))]),
            ..Default::default()
        };
        let built = build(Dialect::Mysql, &order, &request);
        assert!(!built.seeks);
        assert!(built.sql.ends_with("LIMIT 11 OFFSET 50"));
    }

    #[test]
    fn filters_bind_values() {
        let request = BrowseRequest {
            limit: 10,
            filters: vec![
                Filter {
                    column: "name".into(),
                    op: FilterOp::Contains,
                    value: Value::text("50%"),
                },
                Filter {
                    column: "age".into(),
                    op: FilterOp::GtEq,
                    value: Value::Int(18),
                },
                Filter {
                    column: "deleted".into(),
                    op: FilterOp::Eq,
                    value: Value::Null,
                },
                Filter {
                    column: "x".into(),
                    op: FilterOp::StartsWith,
                    value: Value::text("a_"),
                },
            ],
            raw_where: Some("score > 3 -- comment".into()),
            ..Default::default()
        };
        let (sql, params) = count_sql(Dialect::Postgres, "\"t\"", &request).expect("builds");
        assert_eq!(
            sql,
            "SELECT COUNT(*) FROM \"t\" WHERE CAST(\"name\" AS text) ILIKE $1 ESCAPE '!' AND \"age\" >= $2 AND \"deleted\" IS NULL AND CAST(\"x\" AS text) ILIKE $3 ESCAPE '!' AND (\nscore > 3 -- comment\n)"
        );
        assert_eq!(
            params,
            vec![Value::text("%50!%%"), Value::Int(18), Value::text("a!_%")]
        );
        let (sql, _) = count_sql(Dialect::Mysql, "`t`", &request).expect("builds");
        assert!(sql.contains("CAST(`name` AS CHAR) LIKE ? ESCAPE '!'"));
    }

    #[test]
    fn first_keywords() {
        assert_eq!(first_keyword("  -- x\n /* y */ (select 1)"), "SELECT");
        assert_eq!(first_keyword("insert into t"), "INSERT");
        assert_eq!(first_keyword("/* open"), "");
        assert_eq!(first_keyword(""), "");
    }

    #[test]
    fn edits() {
        let update = RowEdit::Update {
            key: vec![("id".into(), Value::Int(1)), ("k".into(), Value::Null)],
            changes: vec![("name".into(), Value::text("x"))],
        };
        let built = edit_sql(Dialect::Postgres, "\"t\"", &update)
            .expect("builds")
            .expect("not empty");
        assert_eq!(
            built.sql,
            "UPDATE \"t\" SET \"name\" = $1 WHERE \"id\" = $2 AND \"k\" IS NULL"
        );
        assert!(built.expect_one);
        let insert = RowEdit::Insert { values: vec![] };
        let built = edit_sql(Dialect::Mysql, "`t`", &insert)
            .expect("builds")
            .expect("not empty");
        assert_eq!(built.sql, "INSERT INTO `t` () VALUES ()");
        let delete = RowEdit::Delete { key: vec![] };
        assert!(edit_sql(Dialect::Sqlite, "t", &delete).is_err());
        let nothing = RowEdit::Update {
            key: vec![("id".into(), Value::Int(1))],
            changes: vec![],
        };
        assert!(
            edit_sql(Dialect::Sqlite, "t", &nothing)
                .expect("builds")
                .is_none()
        );
    }

    #[test]
    fn order_keys_skip_duplicates() {
        let sort = vec![Sort {
            column: "id".into(),
            direction: SortDirection::Descending,
        }];
        let keys = order_keys(&sort, &["id".to_string(), "b".to_string()], |c| c == "b");
        assert_eq!(keys, vec![key("id", true, false), key("b", false, true)]);
    }

    /// A tiny deterministic generator, so the loop below explores the same inputs on
    /// every run.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn text(&mut self) -> String {
            const PIECES: &[&str] = &[
                "a", "\"", "'", "`", "\\", "%", "_", "!", "\0", "é", "--", "/*", ";", " ", "$",
            ];
            (0..self.next() % 6)
                .map(|_| PIECES[(self.next() as usize) % PIECES.len()])
                .collect()
        }

        fn value(&mut self) -> Value {
            match self.next() % 6 {
                0 => Value::Null,
                1 => Value::Int(self.next() as i64 - (1 << 30)),
                2 => Value::Float(self.next() as f64 / 7.0),
                3 => Value::Bool(self.next().is_multiple_of(2)),
                4 => Value::Bytes(vec![0, 255, 7].into()),
                _ => Value::Text(self.text().into()),
            }
        }
    }

    #[test]
    fn builders_never_panic() {
        const OPS: [FilterOp; 10] = [
            FilterOp::Eq,
            FilterOp::NotEq,
            FilterOp::Lt,
            FilterOp::LtEq,
            FilterOp::Gt,
            FilterOp::GtEq,
            FilterOp::Contains,
            FilterOp::StartsWith,
            FilterOp::IsNull,
            FilterOp::IsNotNull,
        ];
        let mut rng = Lcg(7);
        for round in 0..3000 {
            let dialect = [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite][round % 3];
            let order: Vec<OrderKey> = (0..rng.next() % 4)
                .map(|_| {
                    key(
                        &rng.text(),
                        rng.next().is_multiple_of(2),
                        rng.next().is_multiple_of(2),
                    )
                })
                .collect();
            let filters = (0..rng.next() % 3)
                .map(|_| Filter {
                    column: rng.text(),
                    op: OPS[(rng.next() as usize) % OPS.len()],
                    value: rng.value(),
                })
                .collect();
            let seek_after = rng.next().is_multiple_of(2).then(|| {
                order
                    .iter()
                    .map(|k| (k.column.clone(), rng.value()))
                    .collect()
            });
            let request = BrowseRequest {
                offset: rng.next(),
                limit: rng.next() as u32,
                sort: Vec::new(),
                filters,
                raw_where: rng.next().is_multiple_of(3).then(|| rng.text()),
                seek_after,
            };
            let built = BrowseSql {
                dialect,
                select_list: "*",
                from: "t",
                order: &order,
                total: round % 5 != 0,
            }
            .build(&request);
            if let Ok(built) = built {
                let placeholders = match dialect {
                    Dialect::Postgres => (1..=built.params.len())
                        .filter(|n| built.sql.contains(&format!("${n}")))
                        .count(),
                    _ => built.params.len(),
                };
                assert_eq!(placeholders, built.params.len());
            }
            let _ = count_sql(dialect, "t", &request);
        }
    }
}
