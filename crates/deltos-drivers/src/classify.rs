//! What a statement does before it runs: whether it only reads, and how much it can
//! change. The app uses this to keep a read-only connection read-only and to ask before
//! statements that remove data.
//!
//! The classifier errs towards writing: a statement it does not recognise counts as a
//! write. It reads the text with a small tokenizer that knows each dialect's strings,
//! quoted names and comments (the same ones [`split_statements`](crate::sql::split_statements)
//! knows), so a `WHERE` inside a string, a comment or a sub-select does not count as the
//! statement's own. MySQL's `/*! ... */` and MariaDB's `/*M! ... */` comments, which the
//! server runs, are read as code.
//!
//! Whether a backslash escapes a quote depends on the server's settings (MySQL's
//! `NO_BACKSLASH_ESCAPES` and `ANSI_QUOTES`, PostgreSQL's `standard_conforming_strings`),
//! so PostgreSQL and MySQL text is read both ways and judged by the riskier reading.
//!
//! Text with several statements is judged by its riskiest one.

use deltos_data::DriverKind;

use crate::sql::Dialect;

/// How much a statement can change, from least to most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Risk {
    /// Only reads: `SELECT`, `SHOW`, `EXPLAIN` without `ANALYZE`, a MongoDB `find`.
    Read,
    /// Changes data, settings or objects: `INSERT`, an `UPDATE` with a `WHERE`, `CREATE`,
    /// `SET`, or anything not known to be a read.
    Write,
    /// Changes the structure of an object: `ALTER`.
    Alter,
    /// Removes objects or every row in them: `DROP`, `TRUNCATE`, a MongoDB `drop`.
    Remove,
    /// An `UPDATE` or `DELETE` with no `WHERE`, which touches every row.
    Unbounded,
}

/// What a statement does, with words for the person: `what` names the riskiest part,
/// such as `DELETE without WHERE` or `INSERT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub risk: Risk,
    pub what: String,
}

impl Assessment {
    fn new(risk: Risk, what: impl Into<String>) -> Self {
        Self {
            risk,
            what: what.into(),
        }
    }

    fn read() -> Self {
        Self::new(Risk::Read, "")
    }

    pub fn is_read_only(&self) -> bool {
        self.risk == Risk::Read
    }

    /// The riskier of the two; the first on a tie.
    fn max(self, other: Self) -> Self {
        if other.risk > self.risk { other } else { self }
    }
}

/// Judges `text`, one or more statements, for a database of `kind`.
pub fn assess(text: &str, kind: DriverKind) -> Assessment {
    match kind {
        DriverKind::Mongodb => assess_mongo(text),
        DriverKind::Mysql => assess_sql(text, Dialect::Mysql),
        DriverKind::Sqlite => assess_sql(text, Dialect::Sqlite),
        _ => assess_sql(text, Dialect::Postgres),
    }
}

/// Judges SQL text in `dialect`.
pub fn assess_sql(text: &str, dialect: Dialect) -> Assessment {
    let judge = |other_escapes: bool| {
        tokenize(text, dialect, other_escapes)
            .split(|t| t.kind == Kind::Semi)
            .filter(|piece| !piece.is_empty())
            .map(statement)
            .fold(Assessment::read(), Assessment::max)
    };
    let usual = judge(false);
    if dialect == Dialect::Sqlite || !text.contains('\\') {
        return usual;
    }
    usual.max(judge(true))
}

// ---- tokens ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A keyword or a plain name.
    Word,
    /// A string, a quoted name, a number, a parameter or an operator.
    Other,
    Open,
    Close,
    Comma,
    Semi,
    Equals,
}

#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    kind: Kind,
    text: &'a str,
    /// Parentheses around the token. An opening parenthesis has the depth outside it, and
    /// so does its closing one.
    depth: u32,
}

impl Token<'_> {
    fn is(&self, word: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(word)
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Splits `sql` into tokens, without whitespace and comments. Never fails: an
/// unterminated string or comment runs to the end of the text.
///
/// `other_escapes` reads backslashes the way the server does with its less usual
/// setting: in MySQL as plain characters (`NO_BACKSLASH_ESCAPES`, and `"` names under
/// `ANSI_QUOTES`), in PostgreSQL as escapes in every string
/// (`standard_conforming_strings = off`).
fn tokenize(sql: &str, dialect: Dialect, other_escapes: bool) -> Vec<Token<'_>> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut pos = 0;
    let mut depth = 0u32;
    // Inside a MySQL `/*! ... */`, whose content the server runs.
    let mut in_code_comment = false;
    let mysql = dialect == Dialect::Mysql;
    let postgres = dialect == Dialect::Postgres;
    let find = |from: usize, pat: &[u8]| -> Option<usize> {
        bytes
            .get(from..)?
            .windows(pat.len())
            .position(|w| w == pat)
            .map(|p| from + p)
    };
    while pos < bytes.len() {
        let c = bytes[pos];
        let next = bytes.get(pos + 1).copied();
        let start = pos;
        let mut kind = Kind::Other;
        match c {
            c if c.is_ascii_whitespace() => {
                pos += 1;
                continue;
            }
            // MySQL needs a space after `--` for a comment; `1--1` is arithmetic there.
            b'-' if next == Some(b'-')
                && (!mysql
                    || bytes
                        .get(pos + 2)
                        .is_none_or(|b| b.is_ascii_whitespace() || b.is_ascii_control())) =>
            {
                pos = find(pos, b"\n").map_or(bytes.len(), |p| p + 1);
                continue;
            }
            b'#' if mysql => {
                pos = find(pos, b"\n").map_or(bytes.len(), |p| p + 1);
                continue;
            }
            b'*' if in_code_comment && next == Some(b'/') => {
                in_code_comment = false;
                pos += 2;
                continue;
            }
            b'/' if next == Some(b'*') => {
                let code = match (bytes.get(pos + 2), bytes.get(pos + 3)) {
                    (Some(b'!'), _) => Some(3),
                    // MariaDB's `/*M!`.
                    (Some(b'M'), Some(b'!')) => Some(4),
                    _ => None,
                };
                if let Some(code) = code.filter(|_| mysql) {
                    // `/*!` and an optional version number, then code.
                    pos += code;
                    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                        pos += 1;
                    }
                    in_code_comment = true;
                    continue;
                }
                pos = skip_block_comment(bytes, pos, postgres);
                continue;
            }
            b'\'' => {
                let e_string = postgres
                    && pos > 0
                    && matches!(bytes[pos - 1], b'e' | b'E')
                    && (pos < 2 || !is_word_byte(bytes[pos - 2]));
                let backslash = if other_escapes {
                    postgres
                } else {
                    mysql || e_string
                };
                pos = skip_quoted(bytes, pos, b'\'', backslash);
                // An `E` prefix lexed as a word just before belongs to the string.
                if e_string
                    && out.last().is_some_and(|t: &Token| {
                        t.kind == Kind::Word && t.text.eq_ignore_ascii_case("e")
                    })
                {
                    out.pop();
                }
            }
            b'"' => pos = skip_quoted(bytes, pos, b'"', mysql && !other_escapes),
            b'`' if !postgres => pos = skip_quoted(bytes, pos, b'`', false),
            b'[' if dialect == Dialect::Sqlite => {
                pos = find(pos + 1, b"]").map_or(bytes.len(), |p| p + 1);
            }
            b'$' if postgres => pos = skip_dollar(bytes, pos),
            b'(' => {
                kind = Kind::Open;
                pos += 1;
            }
            b')' => {
                kind = Kind::Close;
                depth = depth.saturating_sub(1);
                pos += 1;
            }
            b',' => {
                kind = Kind::Comma;
                pos += 1;
            }
            b';' => {
                kind = Kind::Semi;
                pos += 1;
            }
            b'=' => {
                kind = Kind::Equals;
                pos += 1;
            }
            c if c.is_ascii_digit() => {
                while pos < bytes.len() && (is_word_byte(bytes[pos]) || bytes[pos] == b'.') {
                    pos += 1;
                }
            }
            c if is_word_byte(c) => {
                kind = Kind::Word;
                while pos < bytes.len() && is_word_byte(bytes[pos]) {
                    pos += 1;
                }
            }
            _ => {
                // One character, whole: the text may hold any UTF-8 outside the cases
                // above only as a word byte, so this is ASCII punctuation.
                pos += 1;
            }
        }
        let text = sql.get(start..pos).unwrap_or("");
        out.push(Token { kind, text, depth });
        if kind == Kind::Open {
            depth += 1;
        }
    }
    out
}

fn skip_quoted(bytes: &[u8], mut pos: usize, quote: u8, backslash: bool) -> usize {
    pos += 1;
    while pos < bytes.len() {
        let c = bytes[pos];
        if backslash && c == b'\\' {
            pos += 2;
            continue;
        }
        pos += 1;
        if c == quote {
            if bytes.get(pos) == Some(&quote) {
                pos += 1;
                continue;
            }
            return pos;
        }
    }
    bytes.len()
}

fn skip_block_comment(bytes: &[u8], mut pos: usize, nested: bool) -> usize {
    let mut level = 0u32;
    while pos < bytes.len() {
        if bytes[pos..].starts_with(b"/*") {
            if level == 0 || nested {
                level += 1;
            }
            pos += 2;
        } else if bytes[pos..].starts_with(b"*/") {
            pos += 2;
            level = level.saturating_sub(1);
            if level == 0 {
                return pos;
            }
        } else {
            pos += 1;
        }
    }
    bytes.len()
}

/// `$tag$ ... $tag$`, or a lone `$` such as a `$1` parameter.
fn skip_dollar(bytes: &[u8], start: usize) -> usize {
    let mut end = start + 1;
    while end < bytes.len() && is_word_byte(bytes[end]) && bytes[end] != b'$' {
        end += 1;
    }
    let is_tag = end < bytes.len()
        && bytes[end] == b'$'
        && !bytes.get(start + 1).is_some_and(|b| b.is_ascii_digit());
    if !is_tag {
        // `$1`: the parameter's digits.
        let mut p = start + 1;
        while p < bytes.len() && bytes[p].is_ascii_digit() {
            p += 1;
        }
        return p;
    }
    let tag = &bytes[start..=end];
    let body = end + 1;
    bytes[body..]
        .windows(tag.len())
        .position(|w| w == tag)
        .map_or(bytes.len(), |p| body + p + tag.len())
}

// ---- statements -----------------------------------------------------------------------

/// The index of the parenthesis that closes the one at `open`.
fn closing(tokens: &[Token], open: usize) -> Option<usize> {
    let depth = tokens.get(open)?.depth;
    tokens
        .iter()
        .enumerate()
        .skip(open + 1)
        .find(|(_, t)| t.kind == Kind::Close && t.depth == depth)
        .map(|(i, _)| i)
}

fn word_at(tokens: &[Token], i: usize, word: &str) -> bool {
    tokens.get(i).is_some_and(|t| t.is(word))
}

/// One statement, or the body of a common table expression.
fn statement(tokens: &[Token]) -> Assessment {
    let mut i = 0;
    while tokens.get(i).is_some_and(|t| t.kind == Kind::Open) {
        i += 1;
    }
    let Some(head) = tokens.get(i) else {
        return Assessment::read();
    };
    if head.kind != Kind::Word {
        return Assessment::new(Risk::Write, "a statement Deltos can't read");
    }
    let verb = head.text.to_ascii_uppercase();
    let rest = &tokens[i..];
    match verb.as_str() {
        "WITH" => with(tokens, i + 1),
        "SELECT" | "VALUES" | "TABLE" => select(rest, &verb),
        "SHOW" => Assessment::read(),
        // MySQL takes `DESCRIBE` and `DESC` for `EXPLAIN`, `ANALYZE` included.
        "EXPLAIN" | "DESCRIBE" | "DESC" => explain(tokens, i + 1),
        "PRAGMA" => pragma(&tokens[i + 1..]),
        "UPDATE" | "DELETE" => {
            let bounded = rest.iter().any(|t| t.depth == head.depth && t.is("WHERE"));
            if bounded {
                Assessment::new(Risk::Write, verb)
            } else {
                Assessment::new(Risk::Unbounded, format!("{verb} without WHERE"))
            }
        }
        "DROP" | "TRUNCATE" => Assessment::new(Risk::Remove, verb),
        "ALTER" => Assessment::new(Risk::Alter, verb),
        _ => Assessment::new(Risk::Write, verb),
    }
}

/// Functions that act from inside a `SELECT` where a read-only transaction does not stop
/// them: `set_config` can turn the read-only session setting off, `dblink` runs its
/// statement on a connection of its own, and the others end sessions or change the
/// server.
const ACTING_FUNCTIONS: &[&str] = &[
    "set_config",
    "dblink",
    "dblink_exec",
    "dblink_send_query",
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_switch_wal",
    "pg_promote",
    "pg_create_restore_point",
    "pg_drop_replication_slot",
    "pg_create_physical_replication_slot",
    "pg_create_logical_replication_slot",
    "pg_file_write",
    "pg_file_rename",
    "pg_file_unlink",
    "pg_logical_emit_message",
    "pg_stat_reset",
    "lo_import",
    "lo_export",
    "lo_unlink",
];

/// Whether the token at `i`, called with the parenthesis after it, is one of
/// [`ACTING_FUNCTIONS`]. A quoted name counts too: `"set_config"(...)` calls the same
/// function, and so may a name spelled with escapes (`U&"..."`), which counts whatever it
/// spells.
fn calls_acting_function(tokens: &[Token], i: usize) -> bool {
    let Some(t) = tokens.get(i) else {
        return false;
    };
    if !tokens.get(i + 1).is_some_and(|n| n.kind == Kind::Open) {
        return false;
    }
    let name = match t.kind {
        Kind::Word => t.text,
        Kind::Other => {
            let escaped = t.text.starts_with('"')
                && i >= 2
                && tokens[i - 1].text == "&"
                && tokens[i - 2].is("U");
            if escaped {
                return true;
            }
            t.text.trim_matches(|c| c == '"' || c == '`')
        }
        _ => return false,
    };
    ACTING_FUNCTIONS
        .iter()
        .any(|f| name.eq_ignore_ascii_case(f))
}

/// A read, unless it stores its result (`SELECT ... INTO`), locks rows
/// (`FOR UPDATE`, `FOR SHARE`, `LOCK IN SHARE MODE`) or calls a function that acts
/// (see [`ACTING_FUNCTIONS`]).
fn select(tokens: &[Token], verb: &str) -> Assessment {
    for (i, t) in tokens.iter().enumerate() {
        if t.is("INTO") {
            return Assessment::new(Risk::Write, format!("{verb} … INTO"));
        }
        if calls_acting_function(tokens, i) {
            return Assessment::new(Risk::Write, format!("{verb} that calls {}", t.text));
        }
        let locks = (t.is("FOR")
            && ["UPDATE", "SHARE", "NO", "KEY"]
                .iter()
                .any(|w| word_at(tokens, i + 1, w)))
            || (t.is("LOCK") && word_at(tokens, i + 1, "IN"));
        if locks {
            return Assessment::new(Risk::Write, format!("{verb} that locks rows"));
        }
    }
    Assessment::read()
}

/// `WITH [RECURSIVE] name [(columns)] AS [NOT] [MATERIALIZED] (body), ... statement`.
/// Each body counts, so `WITH x AS (DELETE ...) SELECT ...` is a delete.
fn with(tokens: &[Token], mut j: usize) -> Assessment {
    let unknown = || Assessment::new(Risk::Write, "WITH");
    if word_at(tokens, j, "RECURSIVE") {
        j += 1;
    }
    let mut worst = Assessment::read();
    loop {
        match tokens.get(j) {
            Some(t) if matches!(t.kind, Kind::Word | Kind::Other) => j += 1,
            _ => return unknown(),
        }
        if tokens.get(j).is_some_and(|t| t.kind == Kind::Open) {
            let Some(close) = closing(tokens, j) else {
                return unknown();
            };
            j = close + 1;
        }
        if !word_at(tokens, j, "AS") {
            return unknown();
        }
        j += 1;
        if word_at(tokens, j, "NOT") {
            j += 1;
        }
        if word_at(tokens, j, "MATERIALIZED") {
            j += 1;
        }
        if !tokens.get(j).is_some_and(|t| t.kind == Kind::Open) {
            return unknown();
        }
        let close = closing(tokens, j).unwrap_or(tokens.len());
        worst = worst.max(statement(&tokens[j + 1..close]));
        j = close + 1;
        if tokens.get(j).is_some_and(|t| t.kind == Kind::Comma) {
            j += 1;
            continue;
        }
        break;
    }
    let main = tokens.get(j..).unwrap_or(&[]);
    if main.is_empty() {
        return worst.max(unknown());
    }
    worst.max(statement(main))
}

/// The names PostgreSQL's `EXPLAIN (...)` option list takes.
const EXPLAIN_OPTIONS: &[&str] = &[
    "ANALYZE",
    "ANALYSE",
    "VERBOSE",
    "COSTS",
    "SETTINGS",
    "GENERIC_PLAN",
    "BUFFERS",
    "SERIALIZE",
    "WAL",
    "TIMING",
    "SUMMARY",
    "MEMORY",
    "FORMAT",
];

/// `EXPLAIN` only plans a statement, except with `ANALYZE`, which runs it.
fn explain(tokens: &[Token], mut j: usize) -> Assessment {
    let mut analyze = false;
    let first = j;
    while let Some(t) = tokens.get(j) {
        // PostgreSQL's option list, `EXPLAIN (ANALYZE, BUFFERS) ...`, comes first and
        // starts with an option's name. Any other parenthesis opens the statement, as
        // in `EXPLAIN ANALYZE (SELECT ... FOR UPDATE)`.
        let options = t.kind == Kind::Open
            && j == first
            && tokens
                .get(j + 1)
                .is_some_and(|o| EXPLAIN_OPTIONS.iter().any(|w| o.is(w)));
        if options {
            let close = closing(tokens, j).unwrap_or(tokens.len());
            analyze |= tokens
                .get(j + 1..close)
                .unwrap_or(&[])
                .iter()
                .any(|t| t.is("ANALYZE") || t.is("ANALYSE"));
            j = close + 1;
        } else if t.is("ANALYZE") || t.is("ANALYSE") {
            analyze = true;
            j += 1;
        } else if ["VERBOSE", "QUERY", "PLAN", "EXTENDED", "PARTITIONS"]
            .iter()
            .any(|w| t.is(w))
        {
            j += 1;
        } else if t.is("FORMAT") {
            // MySQL: `FORMAT=JSON`, `FORMAT = TREE`.
            j += 1;
            if tokens.get(j).is_some_and(|t| t.kind == Kind::Equals) {
                j += 1;
            }
            if tokens.get(j).is_some_and(|t| t.kind == Kind::Word) {
                j += 1;
            }
        } else {
            break;
        }
    }
    if !analyze {
        return Assessment::read();
    }
    let inner = statement(tokens.get(j..).unwrap_or(&[]));
    if inner.is_read_only() {
        inner
    } else {
        Assessment::new(inner.risk, format!("EXPLAIN ANALYZE {}", inner.what))
    }
}

/// SQLite pragmas that take an argument and still only read.
const READING_PRAGMAS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_info",
    "index_xinfo",
    "index_list",
    "foreign_key_list",
    "foreign_key_check",
    "integrity_check",
    "quick_check",
];

/// SQLite pragmas that change something even without an argument.
const ACTING_PRAGMAS: &[&str] = &[
    "optimize",
    "wal_checkpoint",
    "incremental_vacuum",
    "shrink_memory",
];

/// `PRAGMA name` reads a setting; `PRAGMA name = value` and `PRAGMA name(value)` set it,
/// except for the pragmas that take an argument to report on.
fn pragma(tokens: &[Token]) -> Assessment {
    let write = || Assessment::new(Risk::Write, "PRAGMA that changes a setting");
    if tokens.iter().any(|t| t.kind == Kind::Equals) {
        return write();
    }
    let name = tokens
        .iter()
        .take_while(|t| t.kind != Kind::Open)
        .filter(|t| t.kind == Kind::Word)
        .last()
        .map(|t| t.text.to_ascii_lowercase())
        .unwrap_or_default();
    let has_argument = tokens.iter().any(|t| t.kind == Kind::Open);
    if ACTING_PRAGMAS.contains(&name.as_str())
        || (has_argument && !READING_PRAGMAS.contains(&name.as_str()))
    {
        return write();
    }
    Assessment::read()
}

// ---- MongoDB --------------------------------------------------------------------------

/// Judges one MongoDB shell statement. Text the shell parser refuses counts as a read:
/// the driver parses it the same way and refuses it too, so it never reaches the server.
#[cfg(feature = "mongodb")]
pub fn assess_mongo(text: &str) -> Assessment {
    use crate::mongodb::shell::{Command, Operation};
    let Ok(command) = crate::mongodb::shell::parse(text) else {
        return Assessment::read();
    };
    match command {
        Command::ListCollections { .. } => Assessment::read(),
        Command::Run { command, .. } => mongo_command(&command),
        // `explain()` with `executionStats` runs the operation, so it is judged the same.
        Command::Collection { operation, .. } | Command::Explain { operation, .. } => {
            match operation {
                Operation::Find(_)
                | Operation::Count { .. }
                | Operation::EstimatedCount
                | Operation::Distinct { .. } => Assessment::read(),
                Operation::Aggregate { pipeline, .. } => {
                    if pipeline.iter().any(writes_output) {
                        Assessment::new(Risk::Write, "aggregate with $out or $merge")
                    } else {
                        Assessment::read()
                    }
                }
                Operation::InsertOne(_) | Operation::InsertMany(_) => {
                    Assessment::new(Risk::Write, "insert")
                }
                Operation::Update { filter, many, .. } if many && filter.is_empty() => {
                    Assessment::new(Risk::Unbounded, "updateMany with an empty filter")
                }
                Operation::Update { .. } => Assessment::new(Risk::Write, "update"),
                Operation::Replace { .. } => Assessment::new(Risk::Write, "replaceOne"),
                Operation::Delete { filter, many } if many && filter.is_empty() => {
                    Assessment::new(Risk::Unbounded, "deleteMany with an empty filter")
                }
                Operation::Delete { .. } => Assessment::new(Risk::Write, "delete"),
                Operation::Drop => Assessment::new(Risk::Remove, "drop"),
            }
        }
    }
}

#[cfg(not(feature = "mongodb"))]
pub fn assess_mongo(_text: &str) -> Assessment {
    Assessment::new(Risk::Write, "a MongoDB statement")
}

/// Database commands that only read or report.
#[cfg(feature = "mongodb")]
const READING_COMMANDS: &[&str] = &[
    "find",
    "count",
    "distinct",
    "listCollections",
    "listDatabases",
    "listIndexes",
    "ping",
    "hello",
    "isMaster",
    "ismaster",
    "buildInfo",
    "buildinfo",
    "serverStatus",
    "dbStats",
    "dbstats",
    "collStats",
    "collstats",
    "dataSize",
    "connectionStatus",
    "hostInfo",
    "listCommands",
    "currentOp",
    "getMore",
    "whatsmyuri",
    "getCmdLineOpts",
    "replSetGetStatus",
    "usersInfo",
    "rolesInfo",
];

#[cfg(feature = "mongodb")]
fn mongo_command(command: &bson::Document) -> Assessment {
    let Some(name) = command.keys().next() else {
        return Assessment::read();
    };
    let write = |risk| Assessment::new(risk, name.clone());
    match name.as_str() {
        n if READING_COMMANDS.contains(&n) => Assessment::read(),
        "aggregate" => {
            let writes = command
                .get_array("pipeline")
                .map(|p| {
                    p.iter()
                        .any(|s| matches!(s, bson::Bson::Document(d) if writes_output(d)))
                })
                .unwrap_or(false);
            if writes {
                Assessment::new(Risk::Write, "aggregate with $out or $merge")
            } else {
                Assessment::read()
            }
        }
        "explain" => match command.get_document("explain") {
            Ok(inner) if mongo_command(inner).is_read_only() => Assessment::read(),
            _ => write(Risk::Write),
        },
        "drop" | "dropDatabase" | "dropIndexes" | "dropAllUsersFromDatabase" => write(Risk::Remove),
        "delete" | "update" => {
            // `{delete: "c", deletes: [{q: {}, limit: 0}]}` and
            // `{update: "c", updates: [{q: {}, u: ..., multi: true}]}` touch every document.
            let list = if name == "delete" {
                "deletes"
            } else {
                "updates"
            };
            let everything = command.get_array(list).is_ok_and(|items| {
                items.iter().any(|item| {
                    let bson::Bson::Document(d) = item else {
                        return false;
                    };
                    let empty = d.get_document("q").is_ok_and(|q| q.is_empty());
                    let many = if name == "delete" {
                        matches!(
                            d.get("limit"),
                            Some(bson::Bson::Int32(0) | bson::Bson::Int64(0))
                        ) || matches!(d.get("limit"), Some(bson::Bson::Double(v)) if *v == 0.0)
                    } else {
                        d.get_bool("multi").unwrap_or(false)
                    };
                    empty && many
                })
            });
            if everything {
                Assessment::new(Risk::Unbounded, format!("{name} with an empty filter"))
            } else {
                write(Risk::Write)
            }
        }
        _ => write(Risk::Write),
    }
}

/// Whether a pipeline stage, or a pipeline nested in one, writes with `$out` or `$merge`.
#[cfg(feature = "mongodb")]
fn writes_output(stage: &bson::Document) -> bool {
    fn value(v: &bson::Bson, depth: usize) -> bool {
        match v {
            bson::Bson::Document(d) => document(d, depth),
            bson::Bson::Array(items) => items.iter().any(|v| value(v, depth + 1)),
            _ => false,
        }
    }
    fn document(d: &bson::Document, depth: usize) -> bool {
        // The parser refuses deeper nesting anyway; this keeps the walk bounded.
        if depth > 128 {
            return true;
        }
        d.iter()
            .any(|(k, v)| k == "$out" || k == "$merge" || value(v, depth + 1))
    }
    document(stage, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(sql: &str) -> Risk {
        assess(sql, DriverKind::Postgres).risk
    }

    fn my(sql: &str) -> Risk {
        assess(sql, DriverKind::Mysql).risk
    }

    fn lite(sql: &str) -> Risk {
        assess(sql, DriverKind::Sqlite).risk
    }

    #[test]
    fn reads() {
        for sql in [
            "SELECT * FROM users",
            "  select 1",
            "\n\t(SELECT 1) UNION (SELECT 2)",
            "values (1, 2)",
            "TABLE users",
            "show search_path",
            "SHOW TABLES",
            "describe users",
            "DESC users",
            "-- a comment\nSELECT 1",
            "/* select */ /* nested /* deeper */ */ SELECT 1",
            "SELECT 'update t set a = 1' AS x",
            "SELECT \"delete\" FROM t",
            "SELECT $$ drop table x $$",
            "SELECT 1;",
            "select 1; select 2",
            "",
            "   ",
            "-- only a comment",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "with recursive t(n) as (values (1) union all select n + 1 from t where n < 5) select * from t",
            "WITH a AS MATERIALIZED (SELECT 1), b AS NOT MATERIALIZED (SELECT 2) SELECT * FROM a, b",
            "SELECT * FROM t FOR SYSTEM_TIME AS OF '2020-01-01'",
            "SELECT update_count, deleted, intotal FROM stats",
        ] {
            assert_eq!(pg(sql), Risk::Read, "{sql}");
        }
        assert_eq!(my("SELECT `drop` FROM t # drop table x"), Risk::Read);
        assert_eq!(lite("SELECT [delete] FROM t"), Risk::Read);
    }

    #[test]
    fn writes() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "update t set a = 1 where id = 2",
            "DELETE FROM t WHERE id = 1",
            "CREATE TABLE t (a int)",
            "GRANT SELECT ON t TO bob",
            "SET search_path = x",
            "BEGIN",
            "COPY t FROM '/tmp/x'",
            "CALL p()",
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
            "SELECT * INTO new_table FROM t",
            "SELECT * FROM t FOR UPDATE",
            "select * from t for no key update",
            "SELECT * FROM t FOR SHARE",
            "'just a string'",
            "VACUUM",
            "select 1; insert into t values (1)",
        ] {
            assert_eq!(pg(sql), Risk::Write, "{sql}");
        }
        assert_eq!(
            my("SELECT * FROM t LOCK IN SHARE MODE"),
            Risk::Write,
            "lock in share mode"
        );
        assert_eq!(my("SELECT 1 INTO OUTFILE '/tmp/x'"), Risk::Write);
        assert_eq!(my("REPLACE INTO t VALUES (1)"), Risk::Write);
        assert_eq!(my("USE other"), Risk::Write);
    }

    #[test]
    fn data_modifying_ctes_are_writes() {
        assert_eq!(
            pg("WITH x AS (DELETE FROM t WHERE id = 1 RETURNING *) SELECT * FROM x"),
            Risk::Write
        );
        assert_eq!(
            pg("WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x"),
            Risk::Unbounded
        );
        assert_eq!(
            pg("WITH x AS (SELECT 1), y AS (INSERT INTO t VALUES (1) RETURNING id) SELECT 1"),
            Risk::Write
        );
        assert_eq!(
            pg("WITH x AS (SELECT id FROM t) UPDATE t SET a = 1 FROM x WHERE t.id = x.id"),
            Risk::Write
        );
        assert_eq!(pg("with x as (select 1) delete from t"), Risk::Unbounded);
        assert_eq!(
            pg("WITH x(a) AS (SELECT 1) INSERT INTO t SELECT * FROM x"),
            Risk::Write
        );
        // Malformed or cut short: not known to be a read.
        assert_eq!(pg("WITH"), Risk::Write);
        assert_eq!(pg("WITH x AS (SELECT 1)"), Risk::Write);
        assert_eq!(pg("WITH x (SELECT 1) SELECT 1"), Risk::Write);
    }

    #[test]
    fn finds_the_statements_own_where() {
        assert_eq!(pg("DELETE FROM users"), Risk::Unbounded);
        assert_eq!(pg("delete from users where id = 3"), Risk::Write);
        assert_eq!(pg("UPDATE t SET a = 1"), Risk::Unbounded);
        assert_eq!(pg("UPDATE t SET note = 'where'"), Risk::Unbounded);
        assert_eq!(pg("UPDATE t SET a = 1 -- where id = 1"), Risk::Unbounded);
        assert_eq!(pg("UPDATE t SET a = 1 /* where id = 1 */"), Risk::Unbounded);
        assert_eq!(
            pg("UPDATE t SET a = (SELECT b FROM c WHERE c.id = 1)"),
            Risk::Unbounded
        );
        assert_eq!(
            pg("UPDATE t SET a = (SELECT b FROM c WHERE c.id = t.id) WHERE t.id > 5"),
            Risk::Write
        );
        assert_eq!(pg("UPDATE \"where\" SET a = 1"), Risk::Unbounded);
        assert_eq!(pg("UPDATE t SET a = $w$ where $w$"), Risk::Unbounded);
        assert_eq!(pg("update t set a = E'\\' where id = 1'"), Risk::Unbounded);
        assert_eq!(my("UPDATE t SET a = 'it\\'s where'"), Risk::Unbounded);
        assert_eq!(my("DELETE FROM t # WHERE id = 1"), Risk::Unbounded);
        assert_eq!(
            my("DELETE t1 FROM t1 JOIN t2 ON t1.id = t2.id"),
            Risk::Unbounded
        );
        assert_eq!(lite("DELETE FROM t WHERE rowid = 1"), Risk::Write);
        assert_eq!(pg("  Delete\nFrom t"), Risk::Unbounded);
        assert_eq!(pg("select 1; delete from t"), Risk::Unbounded);
        assert_eq!(
            assess("delete from t", DriverKind::Postgres).what,
            "DELETE without WHERE"
        );
    }

    #[test]
    fn removals_and_alters() {
        assert_eq!(pg("DROP TABLE users"), Risk::Remove);
        assert_eq!(pg("  truncate orders"), Risk::Remove);
        assert_eq!(pg("ALTER TABLE t ADD COLUMN b int"), Risk::Alter);
        assert_eq!(pg("drop table a; alter table b add c int"), Risk::Remove);
        assert_eq!(pg("select * from dropped"), Risk::Read);
    }

    #[test]
    fn explain() {
        assert_eq!(pg("EXPLAIN SELECT 1"), Risk::Read);
        assert_eq!(pg("EXPLAIN DELETE FROM t"), Risk::Read);
        assert_eq!(pg("EXPLAIN ANALYZE SELECT 1"), Risk::Read);
        assert_eq!(pg("EXPLAIN ANALYZE DELETE FROM t"), Risk::Unbounded);
        assert_eq!(
            pg("explain (analyze, buffers) insert into t values (1)"),
            Risk::Write
        );
        assert_eq!(pg("EXPLAIN (FORMAT JSON) UPDATE t SET a = 1"), Risk::Read);
        assert_eq!(pg("EXPLAIN VERBOSE SELECT 1"), Risk::Read);
        assert_eq!(my("EXPLAIN FORMAT=JSON SELECT 1"), Risk::Read);
        assert_eq!(my("EXPLAIN ANALYZE SELECT * FROM t"), Risk::Read);
        assert_eq!(my("explain users"), Risk::Read);
        assert_eq!(lite("EXPLAIN QUERY PLAN SELECT 1"), Risk::Read);
        assert_eq!(
            assess(
                "EXPLAIN ANALYZE INSERT INTO t VALUES (1)",
                DriverKind::Postgres
            )
            .what,
            "EXPLAIN ANALYZE INSERT"
        );
    }

    #[test]
    fn pragmas() {
        assert_eq!(lite("PRAGMA user_version"), Risk::Read);
        assert_eq!(lite("pragma main.journal_mode"), Risk::Read);
        assert_eq!(lite("PRAGMA table_info(users)"), Risk::Read);
        assert_eq!(lite("PRAGMA main.index_list('users')"), Risk::Read);
        assert_eq!(lite("PRAGMA user_version = 3"), Risk::Write);
        assert_eq!(lite("PRAGMA journal_mode(WAL)"), Risk::Write);
        assert_eq!(lite("PRAGMA optimize"), Risk::Write);
        assert_eq!(lite("PRAGMA wal_checkpoint"), Risk::Write);
    }

    #[test]
    fn mysql_runs_code_comments() {
        assert_eq!(
            my("SELECT 1 /*!50000 INTO OUTFILE '/tmp/x' */"),
            Risk::Write
        );
        assert_eq!(my("/*!40101 DELETE FROM t */"), Risk::Unbounded);
        assert_eq!(my("SELECT 1 /*+ MAX_EXECUTION_TIME(1000) */"), Risk::Read);
        // `--` without a space is not a comment in MySQL.
        assert_eq!(my("SELECT 1--1 INTO OUTFILE '/tmp/x'"), Risk::Write);
        assert_eq!(my("DELETE FROM t WHERE a = 1--1"), Risk::Write);
        assert_eq!(pg("SELECT 1 /*! INTO x */"), Risk::Read);
    }

    #[test]
    fn explain_runs_a_parenthesized_statement() {
        assert_eq!(
            pg("EXPLAIN ANALYZE (SELECT * FROM t FOR UPDATE)"),
            Risk::Write
        );
        assert_eq!(
            pg("EXPLAIN ANALYZE (WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d)"),
            Risk::Unbounded
        );
        assert_eq!(
            pg("EXPLAIN (ANALYZE) (WITH d AS (DELETE FROM t WHERE a = 1 RETURNING *) SELECT 1)"),
            Risk::Write
        );
        assert_eq!(pg("EXPLAIN (SELECT 1)"), Risk::Read);
        assert_eq!(pg("EXPLAIN (COSTS OFF) DELETE FROM t"), Risk::Read);
    }

    #[test]
    fn describe_is_explain_in_mysql() {
        assert_eq!(
            my("DESCRIBE ANALYZE DELETE t1 FROM t1 JOIN t2"),
            Risk::Unbounded
        );
        assert_eq!(my("desc analyze select * from t for update"), Risk::Write);
        assert_eq!(my("DESCRIBE DELETE FROM t"), Risk::Read);
        assert_eq!(my("DESC users"), Risk::Read);
        assert_eq!(my("describe users id"), Risk::Read);
    }

    #[test]
    fn mariadb_runs_its_code_comments() {
        assert_eq!(my("SELECT 1 /*M! ; DELETE FROM t */"), Risk::Unbounded);
        assert_eq!(
            my("SELECT 1 /*M!100100 INTO OUTFILE '/tmp/x' */"),
            Risk::Write
        );
        assert_eq!(my("SELECT 1 /* M! INTO x */"), Risk::Read);
    }

    #[test]
    fn reads_backslashes_both_ways() {
        // With NO_BACKSLASH_ESCAPES the string ends at the quote after the backslash.
        assert_eq!(my("SELECT 'a\\' ; DELETE FROM t; -- '"), Risk::Unbounded);
        // With ANSI_QUOTES a backslash in a quoted name is a plain character.
        assert_eq!(my("SELECT \"a\\\" ; DROP TABLE t; -- \""), Risk::Remove);
        // With standard_conforming_strings off a backslash escapes in every string.
        assert_eq!(
            pg("SELECT 'x\\' , ' ; DELETE FROM t; -- '"),
            Risk::Unbounded
        );
        assert_eq!(pg("SELECT 'C:\\data' AS path"), Risk::Read);
        assert_eq!(my("SELECT 'it\\'s' AS a, 'b' AS c"), Risk::Read);
    }

    #[test]
    fn functions_that_act_are_writes() {
        assert_eq!(
            pg("SELECT set_config('default_transaction_read_only', 'off', false)"),
            Risk::Write
        );
        assert_eq!(
            pg("select pg_catalog.SET_CONFIG('transaction_read_only', 'off', true)"),
            Risk::Write
        );
        assert_eq!(pg("SELECT \"set_config\"('a', 'b', false)"), Risk::Write);
        assert_eq!(
            pg("SELECT U&\"\\0073et_config\"('a', 'b', false)"),
            Risk::Write
        );
        assert_eq!(pg("SELECT U&\"caf\\00e9\" FROM t"), Risk::Read);
        assert_eq!(
            pg("SELECT * FROM dblink('dbname=x', 'DELETE FROM t RETURNING 1') AS r(a int)"),
            Risk::Write
        );
        assert_eq!(
            pg("SELECT pg_terminate_backend(pid) FROM pg_stat_activity"),
            Risk::Write
        );
        assert_eq!(
            pg("WITH x AS (SELECT set_config('a', 'b', false)) SELECT * FROM x"),
            Risk::Write
        );
        assert_eq!(pg("SELECT current_setting('search_path')"), Risk::Read);
        assert_eq!(pg("SELECT set_config FROM settings"), Risk::Read);
    }

    #[test]
    fn never_panics() {
        const PIECES: &[&str] = &[
            "'", "\"", "`", "[", "]", "$", "$a$", "$1", "--", "-- ", "#", "/*", "/*!", "*/", ";",
            "\n", "(", ")", ",", "=", "E", "with", "as", "select", "update", "delete", "where",
            "explain", "analyze", "pragma", "é", "\\", "x", " ", "1",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..4000 {
            let mut sql = String::new();
            for _ in 0..(round % 20) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                sql.push_str(PIECES[(seed % PIECES.len() as u64) as usize]);
            }
            for kind in [DriverKind::Postgres, DriverKind::Mysql, DriverKind::Sqlite] {
                let _ = assess(&sql, kind);
            }
        }
    }

    #[cfg(feature = "mongodb")]
    #[test]
    fn mongo() {
        let m = |s: &str| assess(s, DriverKind::Mongodb).risk;
        assert_eq!(m("db.orders.find({status: 'open'}).limit(5)"), Risk::Read);
        assert_eq!(m("db.orders.find().count()"), Risk::Read);
        assert_eq!(m("db.orders.countDocuments({})"), Risk::Read);
        assert_eq!(m("db.orders.distinct('status')"), Risk::Read);
        assert_eq!(m("db.getCollectionNames()"), Risk::Read);
        assert_eq!(
            m("db.orders.aggregate([{$match: {a: 1}}, {$group: {_id: '$a'}}])"),
            Risk::Read
        );
        assert_eq!(
            m("db.orders.aggregate([{$match: {}}, {$out: 'copy'}])"),
            Risk::Write
        );
        assert_eq!(
            m("db.orders.aggregate([{$facet: {a: [{$merge: {into: 'x'}}]}}])"),
            Risk::Write
        );
        assert_eq!(m("db.orders.insertOne({a: 1})"), Risk::Write);
        assert_eq!(m("db.orders.updateOne({}, {$set: {a: 1}})"), Risk::Write);
        assert_eq!(
            m("db.orders.updateMany({}, {$set: {a: 1}})"),
            Risk::Unbounded
        );
        assert_eq!(
            m("db.orders.updateMany({a: 1}, {$set: {a: 2}})"),
            Risk::Write
        );
        assert_eq!(m("db.orders.deleteMany({})"), Risk::Unbounded);
        assert_eq!(m("db.orders.deleteOne({})"), Risk::Write);
        assert_eq!(m("db.orders.drop()"), Risk::Remove);
        assert_eq!(m("{ping: 1}"), Risk::Read);
        assert_eq!(m("db.runCommand({listCollections: 1})"), Risk::Read);
        assert_eq!(m("db.runCommand({dropDatabase: 1})"), Risk::Remove);
        assert_eq!(m("db.runCommand({create: 'x'})"), Risk::Write);
        assert_eq!(
            m("db.runCommand({aggregate: 'o', pipeline: [{$out: 'x'}], cursor: {}})"),
            Risk::Write
        );
        assert_eq!(
            m("db.runCommand({aggregate: 'o', pipeline: [{$match: {}}], cursor: {}})"),
            Risk::Read
        );
        assert_eq!(
            m("db.runCommand({explain: {find: 'o', filter: {}}})"),
            Risk::Read
        );
        assert_eq!(
            m("db.runCommand({explain: {delete: 'o', deletes: [{q: {}, limit: 0}]}})"),
            Risk::Write
        );
        assert_eq!(
            m("db.orders.find({}).explain('executionStats')"),
            Risk::Read
        );
        assert_eq!(
            m("db.orders.explain('executionStats').aggregate([{$out: 'copy'}])"),
            Risk::Write
        );
        assert_eq!(
            m("db.runCommand({delete: 'o', deletes: [{q: {}, limit: 0}]})"),
            Risk::Unbounded
        );
        assert_eq!(
            m("db.runCommand({delete: 'o', deletes: [{q: {a: 1}, limit: 0}]})"),
            Risk::Write
        );
        assert_eq!(
            m("db.runCommand({update: 'o', updates: [{q: {}, u: {$set: {a: 1}}, multi: true}]})"),
            Risk::Unbounded
        );
        // The driver refuses what it cannot parse, so nothing runs.
        assert_eq!(m("db.orders.find({"), Risk::Read);
    }
}
