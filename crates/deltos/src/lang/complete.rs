//! What to offer at the cursor: reading the statement around it to know whether a table,
//! a column or a keyword fits there, and ranking what matches what has been typed.
//!
//! The analysis looks only at the text. The caller fills in names from the connection's
//! object tree and column lists, then calls [`rank`].

use std::ops::Range;

use super::Dialect;
use super::lex::{self, Kind, Token};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    Column,
    Table,
    View,
    Collection,
    Schema,
    Keyword,
    Function,
    Method,
    /// A saved query, which replaces what was typed with its text.
    Snippet,
}

/// One entry of the popup.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub label: String,
    pub kind: ItemKind,
    /// A short note at the right: a column's type, a table's schema.
    pub detail: Option<String>,
    /// The text that replaces what was typed.
    pub insert: String,
    /// Lower comes first among equally good matches.
    pub priority: u8,
}

impl Item {
    pub fn new(label: impl Into<String>, kind: ItemKind, priority: u8) -> Self {
        let label = label.into();
        Self {
            insert: label.clone(),
            label,
            kind,
            detail: None,
            priority,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn insert(mut self, insert: impl Into<String>) -> Self {
        self.insert = insert.into();
        self
    }
}

/// A table named in a statement, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    /// Inside a string, a quoted name or a comment, where nothing should pop up.
    Nothing,
    /// After `FROM`, `JOIN`, `INTO`, `UPDATE` or `TABLE`: tables and views.
    Tables,
    /// After `name.`: the columns of the table or alias `name`, or the tables of the
    /// schema `name`. The qualifier holds every part before the dot, outermost first.
    Member { qualifier: Vec<String> },
    /// Anywhere else: the statement's columns, keywords and functions.
    General,
    /// After `db.` in the MongoDB shell.
    Collections,
    /// After `db.name.`.
    CollectionMethods,
    /// After `db.name.find(...).`.
    CursorMethods,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    /// The part of the word being typed before the cursor, which an accepted item
    /// replaces. Offsets are into the text given to the analysis.
    pub prefix: Range<usize>,
    pub want: Want,
    /// Every table the statement names, with its alias.
    pub tables: Vec<TableRef>,
}

impl Analysis {
    fn nothing(cursor: usize) -> Self {
        Self {
            prefix: cursor..cursor,
            want: Want::Nothing,
            tables: Vec::new(),
        }
    }
}

/// Words after which a table name comes.
const TABLE_WORDS: &[&str] = &["from", "join", "into", "update", "table", "truncate"];

/// Words that end a table reference, so they are never read as its alias.
const NOT_ALIAS: &[&str] = &[
    "where",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "full",
    "cross",
    "natural",
    "on",
    "using",
    "group",
    "order",
    "having",
    "limit",
    "offset",
    "union",
    "intersect",
    "except",
    "set",
    "values",
    "select",
    "returning",
    "window",
    "for",
    "lateral",
    "default",
    "fetch",
    "as",
    "with",
    "when",
    "then",
    "and",
    "or",
];

/// Reads the SQL statement `text` around byte `cursor`.
pub fn analyze_sql(text: &str, cursor: usize, dialect: Dialect) -> Analysis {
    let cursor = floor_boundary(text, cursor);
    let tokens = lex::tokens(text, dialect);
    // The token the cursor is in or just after.
    let here = tokens
        .iter()
        .find(|t| t.range.start < cursor && cursor <= t.range.end);
    let mut prefix = cursor..cursor;
    if let Some(t) = here {
        let inside = cursor < t.range.end || t.open;
        match t.kind {
            Kind::String | Kind::Quoted | Kind::BlockComment if inside => {
                return Analysis::nothing(cursor);
            }
            Kind::LineComment => return Analysis::nothing(cursor),
            Kind::Number | Kind::Param => return Analysis::nothing(cursor),
            Kind::Word => prefix = t.range.start..cursor,
            _ => {}
        }
    }
    let code: Vec<&Token> = tokens.iter().filter(|t| t.is_code()).collect();
    let before: Vec<&Token> = code
        .iter()
        .copied()
        .filter(|t| t.range.end <= prefix.start)
        .collect();
    let tables = table_refs(text, &code, &prefix);
    let word = |t: &Token| t.text(text).to_ascii_lowercase();

    // `name.` right before the word: a member of a table, alias or schema.
    if let Some(dot) = before.last()
        && dot.kind == Kind::Punct
        && dot.text(text) == "."
        && dot.range.end == prefix.start
    {
        let mut qualifier = Vec::new();
        let mut i = before.len() - 1;
        while let Some(name) = i.checked_sub(1).map(|j| before[j]) {
            if !matches!(name.kind, Kind::Word | Kind::Quoted)
                || name.range.end != before[i].range.start
            {
                break;
            }
            qualifier.insert(0, lex::unquote(name.text(text)));
            match i.checked_sub(2).map(|j| before[j]) {
                Some(d) if d.text(text) == "." && d.range.end == name.range.start => i -= 2,
                _ => break,
            }
        }
        if qualifier.is_empty() {
            return Analysis::nothing(cursor);
        }
        return Analysis {
            prefix,
            want: Want::Member { qualifier },
            tables,
        };
    }

    let want = match before.last() {
        Some(t) if t.kind == Kind::Word && TABLE_WORDS.contains(&word(t).as_str()) => Want::Tables,
        Some(t) if t.text(text) == "," && clause_of(text, &before) == Some("from") => Want::Tables,
        _ => Want::General,
    };
    Analysis {
        prefix,
        want,
        tables,
    }
}

/// The nearest clause keyword before the end of `before`, at the same bracket depth.
fn clause_of(text: &str, before: &[&Token]) -> Option<&'static str> {
    const CLAUSES: &[&str] = &[
        "select",
        "from",
        "where",
        "group",
        "order",
        "having",
        "limit",
        "set",
        "values",
        "on",
        "join",
        "returning",
        "into",
        "update",
    ];
    let mut depth = 0i32;
    for t in before.iter().rev() {
        let s = t.text(text);
        match s {
            ")" => depth += 1,
            "(" if depth == 0 => return None,
            "(" => depth -= 1,
            _ if depth == 0 && t.kind == Kind::Word => {
                let lower = s.to_ascii_lowercase();
                if let Some(c) = CLAUSES.iter().find(|c| **c == lower) {
                    return Some(*c);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every table reference in the statement: the names after `FROM` (and its commas),
/// `JOIN`, `UPDATE` and `INTO`, each with the alias that follows it. The word being
/// typed is left out, since it is not a name yet.
fn table_refs(text: &str, code: &[&Token], typing: &Range<usize>) -> Vec<TableRef> {
    let mut out = Vec::new();
    let lower = |t: &Token| t.text(text).to_ascii_lowercase();
    let is_name = |t: &Token| {
        matches!(t.kind, Kind::Word | Kind::Quoted)
            && !(t.range.start == typing.start && !typing.is_empty())
    };
    let mut i = 0;
    while i < code.len() {
        let w = lower(code[i]);
        let trigger = code[i].kind == Kind::Word && TABLE_WORDS[..4].contains(&w.as_str());
        i += 1;
        if !trigger {
            continue;
        }
        loop {
            // A name: parts joined by dots.
            let mut parts = Vec::new();
            while i < code.len() && is_name(code[i]) {
                if code[i].kind == Kind::Word
                    && parts.is_empty()
                    && NOT_ALIAS.contains(&lower(code[i]).as_str())
                {
                    break;
                }
                parts.push(lex::unquote(code[i].text(text)));
                i += 1;
                if i < code.len() && code[i].text(text) == "." {
                    i += 1;
                } else {
                    break;
                }
            }
            let Some(name) = parts.pop() else { break };
            let schema = parts.pop();
            // An alias, with or without AS.
            let mut alias = None;
            if i < code.len() && lower(code[i]) == "as" {
                i += 1;
            }
            if i < code.len()
                && is_name(code[i])
                && !(code[i].kind == Kind::Word && NOT_ALIAS.contains(&lower(code[i]).as_str()))
                && code.get(i + 1).is_none_or(|n| n.text(text) != "(")
            {
                alias = Some(lex::unquote(code[i].text(text)));
                i += 1;
            }
            out.push(TableRef {
                schema,
                name,
                alias,
            });
            if w == "from" && i < code.len() && code[i].text(text) == "," {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
}

/// The table a qualifier names: an alias first, then a table name.
pub fn resolve<'a>(tables: &'a [TableRef], qualifier: &[String]) -> Option<&'a TableRef> {
    let last = qualifier.last()?;
    if qualifier.len() == 1
        && let Some(t) = tables.iter().find(|t| {
            t.alias
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(last))
        })
    {
        return Some(t);
    }
    let schema = qualifier.len().checked_sub(2).map(|i| &qualifier[i]);
    tables.iter().find(|t| {
        t.name.eq_ignore_ascii_case(last)
            && match (schema, &t.schema) {
                (Some(s), Some(ts)) => s.eq_ignore_ascii_case(ts),
                (Some(_), None) => true,
                (None, _) => true,
            }
    })
}

/// Reads the MongoDB shell statement `text` around byte `cursor`.
pub fn analyze_mongo(text: &str, cursor: usize) -> Analysis {
    let cursor = floor_boundary(text, cursor);
    let head = &text[..cursor];
    if in_shell_string(head) {
        return Analysis::nothing(cursor);
    }
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '$')
        .last()
        .map_or(cursor, |(i, _)| i);
    let prefix = start..cursor;
    let before = head[..start].trim_end();
    let Some(before) = before.strip_suffix('.') else {
        return Analysis {
            prefix,
            want: Want::Nothing,
            tables: Vec::new(),
        };
    };
    let before = before.trim_end();
    let want = if before.ends_with(']') {
        match call_name(before) {
            Some("db") => Want::CollectionMethods,
            _ => Want::Nothing,
        }
    } else if before.ends_with(')') {
        match call_name(before) {
            Some("getCollection") => Want::CollectionMethods,
            Some("getSiblingDB" | "getSisterDB") => Want::Collections,
            Some(_) => Want::CursorMethods,
            None => Want::Nothing,
        }
    } else {
        let word_start = before
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '$')
            .last()
            .map(|(i, _)| i);
        match word_start {
            Some(ws) => {
                let word = &before[ws..];
                let ahead = before[..ws].trim_end();
                if word == "db" && !ahead.ends_with('.') {
                    Want::Collections
                } else if ahead.ends_with('.') && is_db_before(ahead) {
                    Want::CollectionMethods
                } else {
                    Want::Nothing
                }
            }
            None => Want::Nothing,
        }
    };
    Analysis {
        prefix,
        want,
        tables: Vec::new(),
    }
}

/// Whether `text` (ending in `.`) is `...db.` with `db` a word of its own.
fn is_db_before(text: &str) -> bool {
    let Some(rest) = text.strip_suffix('.') else {
        return false;
    };
    let rest = rest.trim_end();
    rest.strip_suffix("db").is_some_and(|ahead| {
        !ahead
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '.')
    })
}

/// The name of the call or index that `text` ends with: `find` for `db.a.find({..})`.
fn call_name(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let (open, close) = match bytes.last()? {
        b')' => (b'(', b')'),
        b']' => (b'[', b']'),
        _ => return None,
    };
    let mut depth = 0i32;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if bytes[i] == close {
            depth += 1;
        } else if bytes[i] == open {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
    }
    if depth != 0 {
        return None;
    }
    let name_end = text[..i].trim_end().len();
    let name = &text[..name_end];
    let start = name
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '$')
        .last()
        .map(|(i, _)| i)?;
    Some(&name[start..])
}

fn in_shell_string(head: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = head.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(_) if c == '\\' => {
                chars.next();
            }
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' || c == '`' => quote = Some(c),
            None if c == '/' && chars.peek() == Some(&'/') => {
                // A line comment hides the rest of its line.
                let mut rest_of_line = true;
                for n in chars.by_ref() {
                    if n == '\n' {
                        rest_of_line = false;
                        break;
                    }
                }
                if rest_of_line {
                    return true;
                }
            }
            None => {}
        }
    }
    quote.is_some()
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// How well `label` matches what was typed, or `None` for no match: the whole word, then
/// a prefix, then the start of a part after `_`, then the letters in order.
fn score(prefix: &str, label: &str) -> Option<i32> {
    if prefix.is_empty() {
        return Some(0);
    }
    let l = label.to_lowercase();
    let p = prefix.to_lowercase();
    if l == p {
        return Some(1000);
    }
    if l.starts_with(&p) {
        return Some(900 - (l.len() - p.len()).min(200) as i32);
    }
    if l.split(['_', ' ', '.'])
        .skip(1)
        .any(|part| part.starts_with(&p))
    {
        return Some(600);
    }
    // Letters in order, preferring them close together.
    let mut gaps = 0i32;
    let mut it = l.chars();
    let mut first = true;
    for pc in p.chars() {
        let mut skipped = 0;
        loop {
            match it.next() {
                Some(c) if c == pc => break,
                Some(_) => skipped += 1,
                None => return None,
            }
        }
        if !first {
            gaps += skipped;
        }
        first = false;
    }
    Some(300 - gaps.min(250))
}

/// Whether `label` matches what was typed at all, as [`rank`] reads it.
pub fn matches(prefix: &str, label: &str) -> bool {
    score(prefix, label).is_some()
}

/// The items that match `prefix`, best first, at most `limit`. Equal matches keep the
/// order given, so columns stay in table order. Duplicates are dropped.
pub fn rank(prefix: &str, items: Vec<Item>, limit: usize) -> Vec<Item> {
    let mut scored: Vec<(i32, Item)> = items
        .into_iter()
        .filter_map(|item| score(prefix, &item.label).map(|s| (s, item)))
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| sb.cmp(sa).then(a.priority.cmp(&b.priority)));
    let mut seen = std::collections::HashSet::new();
    scored
        .into_iter()
        .map(|(_, item)| item)
        .filter(|item| seen.insert((item.label.to_lowercase(), item.kind)))
        .take(limit)
        .collect()
}

/// A keyword in the case the person types in: lowercase after a lowercase prefix,
/// uppercase otherwise.
pub fn keyword_case(prefix: &str, keyword: &str) -> String {
    let lower = !prefix.is_empty() && !prefix.chars().any(|c| c.is_ascii_uppercase());
    if lower {
        keyword.to_ascii_lowercase()
    } else {
        keyword.to_ascii_uppercase()
    }
}

/// `name` as it must be written: quoted when it is not a plain lowercase name (which
/// PostgreSQL would fold) or a keyword.
pub fn quote_if_needed(dialect: Dialect, name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| match dialect {
            Dialect::Postgres => c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_',
            _ => c.is_ascii_alphanumeric() || c == '_',
        })
        && !RESERVED.contains(&name.to_ascii_lowercase().as_str());
    if plain {
        name.to_owned()
    } else {
        dialect.quote_ident(name)
    }
}

/// Words no dialect takes as a bare name.
const RESERVED: &[&str] = &[
    "all",
    "and",
    "as",
    "asc",
    "between",
    "by",
    "case",
    "check",
    "column",
    "constraint",
    "create",
    "cross",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "except",
    "exists",
    "false",
    "for",
    "foreign",
    "from",
    "full",
    "group",
    "having",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "limit",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "right",
    "select",
    "set",
    "table",
    "then",
    "to",
    "true",
    "union",
    "unique",
    "update",
    "user",
    "using",
    "values",
    "when",
    "where",
    "with",
];

/// Keywords offered while typing.
pub fn keywords(dialect: Dialect) -> Vec<&'static str> {
    let mut list = vec![
        "SELECT",
        "FROM",
        "WHERE",
        "AND",
        "OR",
        "NOT",
        "IN",
        "IS NULL",
        "IS NOT NULL",
        "LIKE",
        "BETWEEN",
        "EXISTS",
        "AS",
        "ON",
        "USING",
        "JOIN",
        "LEFT JOIN",
        "INNER JOIN",
        "RIGHT JOIN",
        "CROSS JOIN",
        "GROUP BY",
        "ORDER BY",
        "HAVING",
        "LIMIT",
        "OFFSET",
        "ASC",
        "DESC",
        "DISTINCT",
        "UNION",
        "UNION ALL",
        "CASE",
        "WHEN",
        "THEN",
        "ELSE",
        "END",
        "INSERT INTO",
        "VALUES",
        "UPDATE",
        "SET",
        "DELETE FROM",
        "CREATE TABLE",
        "CREATE INDEX",
        "CREATE VIEW",
        "ALTER TABLE",
        "DROP TABLE",
        "TRUNCATE",
        "WITH",
        "NULL",
        "TRUE",
        "FALSE",
        "DEFAULT",
        "PRIMARY KEY",
        "FOREIGN KEY",
        "REFERENCES",
        "OVER",
        "PARTITION BY",
        "EXPLAIN",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
    ];
    match dialect {
        Dialect::Postgres => list.extend([
            "ILIKE",
            "FULL JOIN",
            "RETURNING",
            "NULLS FIRST",
            "NULLS LAST",
            "ON CONFLICT",
            "DO NOTHING",
            "LATERAL",
            "FILTER",
        ]),
        Dialect::Mysql => list.extend(["SHOW TABLES", "DESCRIBE", "ON DUPLICATE KEY UPDATE"]),
        Dialect::Sqlite => list.extend(["RETURNING", "PRAGMA", "ON CONFLICT", "GLOB"]),
    }
    list
}

/// Functions offered while typing; an accepted one brings its opening parenthesis.
pub fn functions(dialect: Dialect) -> Vec<&'static str> {
    let mut list = vec![
        "count",
        "sum",
        "avg",
        "min",
        "max",
        "coalesce",
        "nullif",
        "lower",
        "upper",
        "length",
        "trim",
        "replace",
        "substr",
        "round",
        "abs",
        "cast",
        "row_number",
        "rank",
        "dense_rank",
        "lag",
        "lead",
    ];
    match dialect {
        Dialect::Postgres => list.extend([
            "now",
            "date_trunc",
            "extract",
            "to_char",
            "string_agg",
            "array_agg",
            "json_agg",
            "jsonb_build_object",
            "generate_series",
            "regexp_replace",
            "concat",
            "greatest",
            "least",
        ]),
        Dialect::Mysql => list.extend([
            "now",
            "concat",
            "group_concat",
            "ifnull",
            "date_format",
            "json_extract",
            "greatest",
            "least",
        ]),
        Dialect::Sqlite => list.extend([
            "ifnull",
            "group_concat",
            "datetime",
            "date",
            "strftime",
            "julianday",
            "printf",
            "instr",
            "json_extract",
        ]),
    }
    list
}

pub const MONGO_DB_METHODS: &[&str] = &[
    "getCollection",
    "getSiblingDB",
    "getCollectionNames",
    "runCommand",
    "adminCommand",
];

pub const MONGO_COLLECTION_METHODS: &[&str] = &[
    "find",
    "findOne",
    "aggregate",
    "countDocuments",
    "estimatedDocumentCount",
    "distinct",
    "insertOne",
    "insertMany",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "drop",
    "explain",
];

pub const MONGO_CURSOR_METHODS: &[&str] = &[
    "sort",
    "limit",
    "skip",
    "projection",
    "batchSize",
    "count",
    "explain",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Analyses text with `|` marking the cursor.
    fn sql(marked: &str) -> (Analysis, String) {
        let cursor = marked.find('|').unwrap();
        let text = marked.replace('|', "");
        (analyze_sql(&text, cursor, Dialect::Postgres), text)
    }

    fn prefix(a: &Analysis, text: &str) -> String {
        text[a.prefix.clone()].to_owned()
    }

    #[test]
    fn tables_after_from_and_join() {
        let (a, t) = sql("select * from or|");
        assert_eq!(a.want, Want::Tables);
        assert_eq!(prefix(&a, &t), "or");
        let (a, _) = sql("select * from orders o join |");
        assert_eq!(a.want, Want::Tables);
        let (a, _) = sql("select * from orders o, |");
        assert_eq!(a.want, Want::Tables);
        let (a, _) = sql("insert into |");
        assert_eq!(a.want, Want::Tables);
        let (a, _) = sql("UPDATE |");
        assert_eq!(a.want, Want::Tables);
        // A comma in the select list is not a table list.
        let (a, _) = sql("select a, | from orders");
        assert_eq!(a.want, Want::General);
    }

    #[test]
    fn members_after_a_dot_resolve_aliases() {
        let (a, t) = sql("select o.to| from public.orders as o join users u on u.id = o.user_id");
        assert_eq!(
            a.want,
            Want::Member {
                qualifier: vec!["o".into()]
            }
        );
        assert_eq!(prefix(&a, &t), "to");
        assert_eq!(
            a.tables,
            vec![
                TableRef {
                    schema: Some("public".into()),
                    name: "orders".into(),
                    alias: Some("o".into())
                },
                TableRef {
                    schema: None,
                    name: "users".into(),
                    alias: Some("u".into())
                },
            ]
        );
        let found = resolve(&a.tables, &["o".into()]).unwrap();
        assert_eq!(found.name, "orders");
        let found = resolve(&a.tables, &["users".into()]).unwrap();
        assert_eq!(found.alias.as_deref(), Some("u"));
        assert!(resolve(&a.tables, &["nope".into()]).is_none());

        let (a, _) = sql("select * from public.|");
        assert_eq!(
            a.want,
            Want::Member {
                qualifier: vec!["public".into()]
            }
        );
        let (a, _) = sql("select \"My Table\".| from \"My Table\"");
        assert_eq!(
            a.want,
            Want::Member {
                qualifier: vec!["My Table".into()]
            }
        );
        assert_eq!(a.tables[0].name, "My Table");
    }

    #[test]
    fn general_context_lists_statement_tables() {
        let (a, t) = sql("select na| from users where id in (select user_id from orders)");
        assert_eq!(a.want, Want::General);
        assert_eq!(prefix(&a, &t), "na");
        let names: Vec<&str> = a.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["users", "orders"]);
        // Keywords after a table are not aliases.
        let (a, _) = sql("select * from users where |");
        assert_eq!(a.tables[0].alias, None);
        let (a, _) = sql("select * from users left join orders on true|");
        assert_eq!(a.tables.len(), 2);
        assert_eq!(a.tables[0].alias, None);
    }

    #[test]
    fn quiet_inside_strings_and_comments() {
        assert_eq!(sql("select 'fr|").0.want, Want::Nothing);
        assert_eq!(sql("select 'a' + 'b|'").0.want, Want::Nothing);
        assert_eq!(sql("select 1 -- from |").0.want, Want::Nothing);
        assert_eq!(sql("select /* fr| */ 1").0.want, Want::Nothing);
        assert_eq!(sql("select 12|").0.want, Want::Nothing);
        // Right after a closed string is code again.
        assert_eq!(sql("select 'a' |").0.want, Want::General);
    }

    fn mongo(marked: &str) -> Want {
        let cursor = marked.find('|').unwrap();
        analyze_mongo(&marked.replace('|', ""), cursor).want
    }

    #[test]
    fn mongo_contexts() {
        assert_eq!(mongo("db.ord|"), Want::Collections);
        assert_eq!(mongo("db.|"), Want::Collections);
        assert_eq!(mongo("db.orders.fi|"), Want::CollectionMethods);
        assert_eq!(mongo("db.orders.find({a: 1}).so|"), Want::CursorMethods);
        assert_eq!(mongo("db.getCollection('x.y').|"), Want::CollectionMethods);
        assert_eq!(mongo("db['x.y'].|"), Want::CollectionMethods);
        assert_eq!(mongo("db.orders.find({name: 'db.|"), Want::Nothing);
        assert_eq!(mongo("mydb.|"), Want::Nothing);
        assert_eq!(mongo("// db.|"), Want::Nothing);
        assert_eq!(mongo("db.a.find({x: 1})\n  .|"), Want::CursorMethods);
    }

    #[test]
    fn ranking_prefers_prefixes_then_parts_then_letters() {
        let items = vec![
            Item::new("customer_id", ItemKind::Column, 0),
            Item::new("id", ItemKind::Column, 0),
            Item::new("idle", ItemKind::Column, 0),
            Item::new("is_deleted", ItemKind::Column, 0),
            Item::new("INDEX", ItemKind::Keyword, 5),
            Item::new("total", ItemKind::Column, 0),
        ];
        let ranked: Vec<String> = rank("id", items.clone(), 10)
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert_eq!(
            ranked,
            vec!["id", "idle", "customer_id", "INDEX", "is_deleted"]
        );
        // With nothing typed, the given order stands, by priority.
        let ranked: Vec<String> = rank("", items, 3).into_iter().map(|i| i.label).collect();
        assert_eq!(ranked, vec!["customer_id", "id", "idle"]);
    }

    #[test]
    fn ranking_drops_duplicates() {
        let items = vec![
            Item::new("orders", ItemKind::Table, 0).detail("public"),
            Item::new("orders", ItemKind::Table, 0).detail("archive"),
            Item::new("orders", ItemKind::Collection, 0),
        ];
        assert_eq!(rank("ord", items, 10).len(), 2);
    }

    #[test]
    fn names_are_quoted_when_needed() {
        assert_eq!(quote_if_needed(Dialect::Postgres, "orders"), "orders");
        assert_eq!(quote_if_needed(Dialect::Postgres, "Orders"), "\"Orders\"");
        assert_eq!(quote_if_needed(Dialect::Mysql, "Orders"), "Orders");
        assert_eq!(quote_if_needed(Dialect::Mysql, "order"), "`order`");
        assert_eq!(quote_if_needed(Dialect::Sqlite, "my table"), "\"my table\"");
        assert_eq!(keyword_case("sel", "SELECT"), "select");
        assert_eq!(keyword_case("Sel", "SELECT"), "SELECT");
        assert_eq!(keyword_case("", "group by"), "GROUP BY");
    }

    #[test]
    fn never_panics() {
        let texts = [
            "",
            ".",
            "a.",
            "..",
            "select é|",
            "from",
            "from ,",
            "select * from a.b.c.d x",
            "db.",
            "db.(",
            "db.a.find(].",
            "))",
            "'",
            "é.é",
        ];
        for text in texts {
            for at in 0..=text.len() + 1 {
                let _ = analyze_sql(text, at, Dialect::Postgres);
                let _ = analyze_sql(text, at, Dialect::Mysql);
                let _ = analyze_mongo(text, at);
            }
        }
    }
}
