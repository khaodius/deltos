//! Lays out SQL: one line per major clause, list items one per line when a list has
//! several, `AND` and `OR` of a condition on lines of their own, sub-queries indented,
//! and keywords in uppercase when asked. Strings, quoted names and comments come out
//! exactly as they went in, and formatting formatted text changes nothing.
//!
//! It works on the tokens of [`super::lex`], so whitespace in the input never matters;
//! only where a comment sat (on its own line or after code) is kept.

use super::Dialect;
use super::lex::{self, Kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Writes keywords in uppercase; otherwise they keep the case they were typed in.
    pub uppercase: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { uppercase: true }
    }
}

const INDENT: &str = "    ";

/// Words written in uppercase, when they are not a name (before a `.`, or where MySQL
/// expects a table, whose names can be case sensitive there).
const KEYWORDS: &[&str] = &[
    "add",
    "all",
    "alter",
    "analyze",
    "and",
    "any",
    "as",
    "asc",
    "begin",
    "between",
    "by",
    "cascade",
    "case",
    "check",
    "column",
    "commit",
    "conflict",
    "constraint",
    "create",
    "cross",
    "current_date",
    "current_time",
    "current_timestamp",
    "database",
    "default",
    "delete",
    "desc",
    "distinct",
    "do",
    "drop",
    "else",
    "end",
    "escape",
    "except",
    "exists",
    "explain",
    "false",
    "fetch",
    "filter",
    "first",
    "following",
    "for",
    "foreign",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "if",
    "ilike",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "interval",
    "into",
    "is",
    "join",
    "key",
    "last",
    "lateral",
    "left",
    "like",
    "limit",
    "materialized",
    "natural",
    "next",
    "not",
    "nothing",
    "null",
    "nulls",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "over",
    "partition",
    "preceding",
    "primary",
    "range",
    "recursive",
    "references",
    "regexp",
    "rename",
    "restrict",
    "returning",
    "revoke",
    "right",
    "rollback",
    "row",
    "rows",
    "schema",
    "select",
    "set",
    "similar",
    "some",
    "table",
    "temporary",
    "then",
    "ties",
    "to",
    "true",
    "truncate",
    "unbounded",
    "union",
    "unique",
    "update",
    "using",
    "values",
    "view",
    "when",
    "where",
    "window",
    "with",
];

/// Function names written in uppercase when they are called.
const FUNCTIONS: &[&str] = &[
    "avg",
    "cast",
    "coalesce",
    "count",
    "extract",
    "lower",
    "max",
    "min",
    "now",
    "nullif",
    "round",
    "substring",
    "sum",
    "trim",
    "upper",
];

/// Keywords that take a space before a parenthesis after them.
const SPACED: &[&str] = &[
    "and",
    "as",
    "all",
    "any",
    "exists",
    "from",
    "in",
    "join",
    "not",
    "on",
    "or",
    "over",
    "select",
    "some",
    "then",
    "using",
    "values",
    "when",
    "where",
    "with",
    "else",
    "by",
    "is",
    "filter",
    "table",
    "into",
    "set",
    "returning",
    "conflict",
    "lateral",
    "recursive",
    "key",
    "references",
    "unique",
    "check",
    "between",
    "like",
    "ilike",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clause {
    Select,
    From,
    Where,
    GroupBy,
    OrderBy,
    Having,
    Limit,
    Values,
    Set,
    Returning,
    Join,
    On,
    SetOp,
    With,
    Statement,
    Window,
}

impl Clause {
    fn is_list(self) -> bool {
        matches!(
            self,
            Clause::Select
                | Clause::GroupBy
                | Clause::OrderBy
                | Clause::Values
                | Clause::Set
                | Clause::Returning
        )
    }

    fn is_condition(self) -> bool {
        matches!(
            self,
            Clause::Where | Clause::Having | Clause::On | Clause::Join
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameKind {
    Root,
    /// `( SELECT ...)`: clauses inside start lines of their own.
    Subquery,
    /// The column list of `CREATE TABLE`: one item per line.
    Columns,
    /// Any other parenthesis: everything inline.
    Paren,
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    kind: FrameKind,
    /// Indent of the clause keywords in this frame.
    base: usize,
    clause: Option<Clause>,
    /// The clause's list items go on lines of their own.
    multiline: bool,
    /// The newline before the first list item is still to come.
    list_pending: bool,
    /// A `BETWEEN` waits for its `AND`, which stays inline.
    between: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Break {
    /// Nothing between the tokens.
    Tight,
    Space,
    Newline(usize),
    /// An empty line, between statements.
    Blank,
}

struct Writer {
    out: String,
    /// Text up to here was copied as it stood and keeps even its trailing space.
    verbatim: usize,
    pending: Option<Break>,
    line_indent: usize,
    line_has_text: bool,
}

impl Writer {
    fn request(&mut self, b: Break) {
        self.pending = match (self.pending, b) {
            (Some(Break::Blank), _) => Some(Break::Blank),
            (Some(Break::Newline(n)), Break::Space | Break::Tight) => Some(Break::Newline(n)),
            _ => Some(b),
        };
    }

    fn flush(&mut self, default: Break) {
        let b = self.pending.take().unwrap_or(default);
        match b {
            Break::Tight => {}
            Break::Space => {
                if self.line_has_text && !self.out.ends_with(' ') {
                    self.out.push(' ');
                }
            }
            Break::Newline(n) => self.newline(n, false),
            Break::Blank => self.newline(0, true),
        }
    }

    fn newline(&mut self, indent: usize, blank: bool) {
        let trimmed = self.out.trim_end_matches([' ', '\t']).len();
        self.out.truncate(trimmed.max(self.verbatim));
        if !self.out.is_empty() {
            if !self.out.ends_with('\n') {
                self.out.push('\n');
            }
            if blank && !self.out.ends_with("\n\n") {
                self.out.push('\n');
            }
        }
        for _ in 0..indent {
            self.out.push_str(INDENT);
        }
        self.line_indent = indent;
        self.line_has_text = false;
    }

    fn push(&mut self, text: &str) {
        self.out.push_str(text);
        self.line_has_text = true;
        if let Some(last_line) = text.rsplit('\n').next()
            && text.contains('\n')
        {
            // A multi-line block comment: the line it ends on has text.
            self.line_has_text = !last_line.trim().is_empty();
        }
    }
}

/// A code token or comment, with what the formatter needs about it.
struct Tok<'a> {
    kind: Kind,
    text: &'a str,
    lower: String,
    /// Where it starts in the input.
    start: usize,
    /// The input had a line break right before it.
    newline_before: bool,
    /// The input had space or a comment right before it.
    space_before: bool,
    newline_after: bool,
}

impl Tok<'_> {
    fn is(&self, word: &str) -> bool {
        self.kind == Kind::Word && self.lower == word
    }

    fn is_comment(&self) -> bool {
        matches!(self.kind, Kind::LineComment | Kind::BlockComment)
    }
}

pub fn format_sql(text: &str, dialect: Dialect, options: Options) -> String {
    let raw = lex::tokens(text, dialect);
    let mut toks: Vec<Tok> = Vec::new();
    let mut newline = false;
    let mut space = false;
    for t in &raw {
        let s = t.text(text);
        if t.kind == Kind::Space {
            newline |= s.contains('\n');
            space = true;
            if let Some(last) = toks.last_mut() {
                last.newline_after |= s.contains('\n');
            }
            continue;
        }
        let comment = matches!(t.kind, Kind::LineComment | Kind::BlockComment);
        toks.push(Tok {
            kind: t.kind,
            text: s,
            lower: if t.kind == Kind::Word {
                s.to_ascii_lowercase()
            } else {
                String::new()
            },
            start: t.range.start,
            newline_before: newline,
            space_before: space,
            newline_after: false,
        });
        newline = false;
        // What follows a comment is not joined to the code before it.
        space = comment;
    }
    let mut w = Writer {
        out: String::with_capacity(text.len() + text.len() / 4),
        verbatim: 0,
        pending: None,
        line_indent: 0,
        line_has_text: false,
    };
    let mut frames = vec![root()];
    let mut stmt_start = 0;
    // The last code token, for spacing.
    let mut prev: Option<usize> = None;
    // The end of a MySQL `DELIMITER` line written out as it was, and whether the next
    // token starts a statement.
    let mut skip_until = 0;
    let mut after_delimiter = false;
    let mut custom_delimiter = false;
    // Where the MySQL client (and the statement splitter) sees statements, to tell a
    // `DELIMITER` command from a word.
    let statements = if dialect == Dialect::Mysql {
        deltos_drivers::sql::split_statements(text, dialect)
    } else {
        Vec::new()
    };
    let in_statement = |at: usize| {
        let k = statements.partition_point(|r| r.end <= at);
        statements.get(k).is_some_and(|r| r.start <= at)
    };
    // The first code token of each of those statements.
    let mut firsts = Vec::with_capacity(statements.len());
    let mut j = 0;
    for r in &statements {
        while toks
            .get(j)
            .is_some_and(|t| t.start < r.start || t.is_comment())
        {
            j += 1;
        }
        if let Some(t) = toks.get(j) {
            firsts.push(t.start);
        }
    }
    for i in 0..toks.len() {
        let tok = &toks[i];
        if tok.start < skip_until {
            continue;
        }
        if after_delimiter {
            after_delimiter = false;
            frames = vec![root()];
            stmt_start = i;
            prev = None;
        }
        if dialect == Dialect::Mysql
            && !in_statement(tok.start)
            && let Some(end) = delimiter_command(text, tok)
        {
            if !w.out.is_empty() {
                let blank = matches!(w.pending.take(), Some(Break::Blank));
                w.newline(0, blank);
            }
            w.push(&text[tok.start..end]);
            skip_until = end;
            let value = text[tok.start + tok.text.len()..end].trim();
            if !value.is_empty() {
                custom_delimiter = value != ";";
            }
            if custom_delimiter {
                // Statements that end in another delimiter are copied as they are, up
                // to the next `DELIMITER` line: a keyword's case or a space can be
                // part of that delimiter.
                let next = toks[i + 1..]
                    .iter()
                    .find(|t| {
                        t.start >= end
                            && !in_statement(t.start)
                            && delimiter_command(text, t).is_some()
                    })
                    .map_or(text.len(), |t| t.start);
                w.push(text[end..next].trim_end());
                skip_until = next;
            }
            w.verbatim = w.out.len();
            w.pending = Some(Break::Newline(0));
            after_delimiter = true;
            continue;
        }
        // A statement that is the word `delimiter` must not start a line, where it
        // would become the command.
        if dialect == Dialect::Mysql
            && tok.is("delimiter")
            && firsts.binary_search(&tok.start).is_ok()
        {
            match w.pending {
                Some(Break::Blank | Break::Newline(0)) => w.pending = Some(Break::Newline(1)),
                None if !w.line_has_text && w.line_indent == 0 => {
                    w.out.push_str(INDENT);
                }
                _ => {}
            }
        }
        if tok.is_comment() {
            let saved = w.pending.take();
            let own_line = tok.newline_before || i == 0;
            if own_line {
                let indent = match saved {
                    Some(Break::Newline(n)) => n,
                    _ => w.line_indent,
                };
                if matches!(saved, Some(Break::Blank)) {
                    w.newline(0, true);
                } else if w.line_has_text || !w.out.is_empty() {
                    w.newline(indent, false);
                }
            } else if w.line_has_text {
                w.out.push(' ');
            }
            w.push(tok.text);
            let after = tok.kind == Kind::LineComment || tok.newline_after;
            w.pending = match saved {
                Some(b @ (Break::Newline(_) | Break::Blank)) if !own_line => Some(b),
                _ if after => Some(Break::Newline(w.line_indent)),
                _ => Some(Break::Space),
            };
            continue;
        }
        let frame = *frames.last().unwrap_or(&ROOT);
        let block = matches!(frame.kind, FrameKind::Root | FrameKind::Subquery);
        if tok.text == ";" && tok.kind == Kind::Punct {
            w.flush(Break::Tight);
            w.push(";");
            frames = vec![root()];
            stmt_start = i + 1;
            prev = None;
            // `;;` stays whole, as a MySQL client delimiter can be.
            let doubled = toks
                .get(i + 1)
                .is_some_and(|n| n.kind == Kind::Punct && n.text == ";" && !n.space_before);
            if !doubled
                && toks[i + 1..]
                    .iter()
                    .any(|t| t.kind != Kind::Punct || t.text != ";")
            {
                w.request(Break::Blank);
            }
            continue;
        }
        let clause = if block && tok.kind == Kind::Word {
            clause_at(&toks, i, stmt_start)
        } else {
            None
        };
        if let Some(c) = clause
            && let Some(f) = frames.last_mut()
        {
            f.clause = Some(c);
            f.between = false;
            f.multiline = c.is_list() && list_has_comma(&toks, i, stmt_start);
            f.list_pending = f.multiline;
            if c != Clause::On && (i != stmt_start || w.line_has_text) {
                w.request(Break::Newline(f.base));
            }
        }
        let default = match prev {
            None => Break::Space,
            Some(p) if p + 1 == i && !tok.space_before && stays_joined(&toks[p], tok, dialect) => {
                Break::Tight
            }
            Some(p) => match spacing(&toks[p], tok, prev_code(&toks, p).map(|q| &toks[q])) {
                // Joining tokens the input kept apart must not make other tokens of them.
                Break::Tight if !joins_cleanly(&toks[p], tok, dialect) => Break::Space,
                b => b,
            },
        };
        // PostgreSQL joins string constants only across a line break.
        if let Some(p) = prev
            && toks[p].kind == Kind::String
            && tok.kind == Kind::String
            && tok.newline_before
        {
            w.request(Break::Newline(w.line_indent));
        }
        let frame = *frames.last().unwrap_or(&ROOT);
        // AND and OR of a condition start lines of their own, except BETWEEN's AND.
        if block
            && (tok.is("and") || tok.is("or"))
            && frame.clause.is_some_and(Clause::is_condition)
        {
            if tok.is("and") && frame.between {
                if let Some(f) = frames.last_mut() {
                    f.between = false;
                }
            } else {
                w.request(Break::Newline(frame.base + 1));
            }
        }
        if tok.is("between")
            && let Some(f) = frames.last_mut()
        {
            f.between = true;
        }
        w.flush(default);
        w.push(&cased(&toks, i, options, dialect));
        let next = next_code(&toks, i);
        match (tok.kind, tok.text) {
            (Kind::Punct, "(") => {
                let sub = next.is_some_and(|n| toks[n].is("select") || toks[n].is("with"));
                let kind = if sub {
                    FrameKind::Subquery
                } else if frame.kind == FrameKind::Root
                    && is_create_table_paren(&toks, i, stmt_start)
                {
                    FrameKind::Columns
                } else {
                    FrameKind::Paren
                };
                let base = match kind {
                    FrameKind::Paren => frame.base,
                    _ => w.line_indent + 1,
                };
                frames.push(Frame {
                    kind,
                    base,
                    clause: None,
                    multiline: false,
                    list_pending: false,
                    between: false,
                });
                if kind == FrameKind::Columns && next.is_some_and(|n| toks[n].text != ")") {
                    w.request(Break::Newline(base));
                }
            }
            (Kind::Punct, ")") => {
                if frames.len() > 1 {
                    frames.pop();
                }
            }
            (Kind::Punct, ",") => {
                if frame.kind == FrameKind::Columns {
                    w.request(Break::Newline(frame.base));
                } else if block && frame.multiline {
                    w.request(Break::Newline(frame.base + 1));
                }
            }
            _ => {}
        }
        // A list on lines of its own starts after its clause's keywords: `GROUP BY`,
        // `SELECT DISTINCT`.
        if let Some(f) = frames.last_mut()
            && f.list_pending
            && !(tok.text == "(" && tok.kind == Kind::Punct)
        {
            let keyword_goes_on = next.is_some_and(|n| {
                let n = &toks[n];
                ((tok.is("group") || tok.is("order")) && n.is("by"))
                    || ((tok.is("select") || tok.is("distinct"))
                        && (n.is("distinct") || n.is("all")))
            });
            if !keyword_goes_on {
                f.list_pending = false;
                w.request(Break::Newline(f.base + 1));
            }
        }
        // The closing parenthesis of a sub-query or column list goes on its own line.
        if let (Some(n), Some(f)) = (next, frames.last())
            && toks[n].kind == Kind::Punct
            && toks[n].text == ")"
            && matches!(f.kind, FrameKind::Subquery | FrameKind::Columns)
        {
            w.request(Break::Newline(f.base.saturating_sub(1)));
        }
        prev = Some(i);
    }
    // A string or comment left open runs to the end, space and all.
    if raw.last().is_some_and(|t| t.open) {
        return w.out;
    }
    let verbatim = w.verbatim;
    let mut out = w.out;
    out.truncate(out.trim_end().len().max(verbatim));
    if text.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    out
}

const ROOT: Frame = Frame {
    kind: FrameKind::Root,
    base: 0,
    clause: None,
    multiline: false,
    list_pending: false,
    between: false,
};

fn root() -> Frame {
    ROOT
}

/// Where a MySQL client `DELIMITER xx` line that starts at `tok` ends: a line that
/// starts with the word, outside every statement the splitter found.
fn delimiter_command(text: &str, tok: &Tok) -> Option<usize> {
    if !tok.is("delimiter") || (tok.start > 0 && !text[..tok.start].ends_with('\n')) {
        return None;
    }
    let after = tok.start + tok.text.len();
    let end = text[after..].find('\n').map_or(text.len(), |p| after + p);
    Some(end)
}

/// Whether `prev` and `cur` written with nothing between them still read as the same
/// two tokens: `- -1` must not become a comment, nor `:: :x` a `:::x`.
fn joins_cleanly(prev: &Tok, cur: &Tok, dialect: Dialect) -> bool {
    let joined = format!("{}{}", prev.text, cur.text);
    let tokens = lex::tokens(&joined, dialect);
    tokens.len() == 2 && tokens[0].range.end == prev.text.len() && tokens[0].kind == prev.kind
}

/// Whether two tokens that touch in the input must keep touching: operator characters
/// that the server may read as one operator (`~*`, `@@`, `?|`, `<->`, `==`), and a MySQL
/// `@@name`. A sign after a comparison or arithmetic operator may still move apart.
fn stays_joined(prev: &Tok, cur: &Tok, dialect: Dialect) -> bool {
    let op = |t: &Tok| {
        t.kind == Kind::Punct
            && !matches!(t.text, "(" | ")" | "[" | "]" | "{" | "}" | "," | ";" | ".")
    };
    let prev_op =
        op(prev) || (dialect == Dialect::Postgres && prev.kind == Kind::Param && prev.text == "?");
    let cur_op = op(cur)
        || (cur.kind == Kind::Param
            && ((cur.text.starts_with('@') && prev.text == "@")
                || (dialect == Dialect::Postgres && cur.text.starts_with('?'))));
    if !(prev_op && cur_op) {
        return false;
    }
    let sign = matches!(cur.text, "+" | "-");
    let plain = matches!(
        prev.text,
        "=" | "<" | ">" | "<=" | ">=" | "<>" | "*" | "/" | "+" | "-" | "::"
    );
    !(sign && plain)
}

fn next_code(toks: &[Tok], i: usize) -> Option<usize> {
    (i + 1..toks.len()).find(|j| !toks[*j].is_comment())
}

fn prev_code(toks: &[Tok], i: usize) -> Option<usize> {
    (0..i).rev().find(|j| !toks[*j].is_comment())
}

/// The clause a word at `i` starts, in a frame where clauses start lines.
fn clause_at(toks: &[Tok], i: usize, stmt_start: usize) -> Option<Clause> {
    let t = &toks[i];
    if t.kind != Kind::Word {
        return None;
    }
    let prev = prev_code(toks, i)
        .filter(|p| *p >= stmt_start)
        .map(|p| &toks[p]);
    let prev_word = prev.map_or("", |p| p.lower.as_str());
    let next = next_code(toks, i).map(|n| &toks[n]);
    let next_word = next.map_or("", |n| n.lower.as_str());
    let first = i == stmt_start;
    // A word used as a name (`t.order`, `order.id`) is not a keyword.
    if prev.is_some_and(|p| p.text == ".") || next.is_some_and(|n| n.text == ".") {
        return None;
    }
    Some(match t.lower.as_str() {
        "select" => Clause::Select,
        "from" if !matches!(prev_word, "delete" | "distinct") => Clause::From,
        "where" => Clause::Where,
        "group" if next_word == "by" => Clause::GroupBy,
        "order" if next_word == "by" => Clause::OrderBy,
        "having" => Clause::Having,
        "limit" | "offset" => Clause::Limit,
        "fetch" if matches!(next_word, "first" | "next") => Clause::Limit,
        "union" | "intersect" | "except" => Clause::SetOp,
        "values" if prev_word != "default" => Clause::Values,
        "set"
            if !matches!(
                prev_word,
                "delete" | "update" | "character" | "charset" | "default"
            ) =>
        {
            Clause::Set
        }
        "returning" => Clause::Returning,
        "window" => Clause::Window,
        "with" if first => Clause::With,
        "insert" | "update" | "delete" | "replace" | "merge"
            if first || prev.is_some_and(|p| p.text == ")") =>
        {
            Clause::Statement
        }
        "join"
            if !matches!(
                prev_word,
                "left"
                    | "right"
                    | "inner"
                    | "full"
                    | "cross"
                    | "natural"
                    | "outer"
                    | "straight_join"
            ) =>
        {
            Clause::Join
        }
        "left" | "right" | "inner" | "full" | "cross" | "natural" => {
            let after = next_code(toks, i).and_then(|n| {
                if toks[n].is("outer") {
                    next_code(toks, n)
                } else {
                    Some(n)
                }
            });
            if after.is_some_and(|n| toks[n].is("join")) {
                Clause::Join
            } else {
                return None;
            }
        }
        "on" if prev_word != "do" => Clause::On,
        _ => return None,
    })
}

/// Whether the list clause starting at `i` has a comma at its own depth before the next
/// clause, so its items go one per line.
fn list_has_comma(toks: &[Tok], i: usize, stmt_start: usize) -> bool {
    let mut depth = 0i32;
    for j in i + 1..toks.len() {
        let t = &toks[j];
        if t.is_comment() {
            continue;
        }
        match (t.kind, t.text) {
            (Kind::Punct, "(") => depth += 1,
            (Kind::Punct, ")") => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            (Kind::Punct, ";") => return false,
            (Kind::Punct, ",") if depth == 0 => return true,
            (Kind::Word, _) if depth == 0 && clause_at(toks, j, stmt_start).is_some() => {
                // `GROUP BY`'s own `BY` and the like are part of this clause.
                return false;
            }
            _ => {}
        }
    }
    false
}

/// Whether the `(` at `i` opens the column list of `CREATE TABLE name (`.
fn is_create_table_paren(toks: &[Tok], i: usize, stmt_start: usize) -> bool {
    if !toks.get(stmt_start).is_some_and(|t| t.is("create")) {
        return false;
    }
    let mut j = i;
    let mut seen_name = false;
    while let Some(p) = prev_code(toks, j) {
        if p < stmt_start {
            return false;
        }
        let t = &toks[p];
        if t.is("table") {
            return seen_name;
        }
        if t.text == "(" || t.text == ")" {
            return false;
        }
        seen_name |= matches!(t.kind, Kind::Word | Kind::Quoted);
        j = p;
    }
    false
}

/// The token's text, with keywords in uppercase when asked and safe.
fn cased(toks: &[Tok], i: usize, options: Options, dialect: Dialect) -> String {
    let t = &toks[i];
    if !options.uppercase || t.kind != Kind::Word {
        return t.text.to_owned();
    }
    let next = next_code(toks, i).map(|n| &toks[n]);
    let prev = prev_code(toks, i).map(|p| &toks[p]);
    if next.is_some_and(|n| n.text == ".") || prev.is_some_and(|p| p.text == "." || p.text == "::")
    {
        return t.text.to_owned();
    }
    let called = next.is_some_and(|n| n.text == "(");
    let keyword =
        KEYWORDS.contains(&t.lower.as_str()) || (called && FUNCTIONS.contains(&t.lower.as_str()));
    // MySQL table names can be case sensitive, so a word where a table goes stays.
    let table_spot = dialect == Dialect::Mysql
        && prev.is_some_and(|p| {
            matches!(
                p.lower.as_str(),
                "from" | "join" | "into" | "update" | "table" | "exists"
            )
        });
    // MySQL table aliases can be case sensitive too.
    let alias = dialect == Dialect::Mysql
        && prev.is_some_and(|p| p.lower == "as")
        && !matches!(t.lower.as_str(), "select" | "with" | "values" | "table");
    if keyword && !table_spot && !alias {
        t.text.to_ascii_uppercase()
    } else {
        t.text.to_owned()
    }
}

/// What goes between `prev` and `cur` when no line break is due.
fn spacing(prev: &Tok, cur: &Tok, before_prev: Option<&Tok>) -> Break {
    let p = prev.text;
    let c = cur.text;
    let punct = |t: &Tok, s: &str| t.kind == Kind::Punct && t.text == s;
    if punct(cur, ",") || punct(cur, ";") || punct(cur, ")") || punct(cur, "]") {
        return Break::Tight;
    }
    if punct(prev, "(") || punct(prev, "[") || punct(prev, ".") || punct(cur, ".") {
        return Break::Tight;
    }
    if punct(prev, "::") || punct(cur, "::") {
        return Break::Tight;
    }
    if punct(cur, "[") && matches!(prev.kind, Kind::Word | Kind::Quoted | Kind::Punct) && p != "," {
        return if prev.kind == Kind::Punct && p != ")" && p != "]" {
            Break::Space
        } else {
            Break::Tight
        };
    }
    if punct(cur, "(") {
        return match prev.kind {
            Kind::Word => {
                let keyword_spaced = SPACED.contains(&prev.lower.as_str());
                let after_name_spot = before_prev.is_some_and(|b| {
                    matches!(b.lower.as_str(), "into" | "table" | "exists" | "view")
                });
                if keyword_spaced || after_name_spot {
                    Break::Space
                } else {
                    Break::Tight
                }
            }
            Kind::Quoted => {
                if before_prev.is_some_and(|b| {
                    matches!(b.lower.as_str(), "into" | "table" | "exists" | "view")
                }) {
                    Break::Space
                } else {
                    Break::Tight
                }
            }
            Kind::Punct if p == ")" || p == "]" => Break::Tight,
            _ => Break::Space,
        };
    }
    // A sign right after an operator, a parenthesis or a keyword is unary.
    if prev.kind == Kind::Punct
        && (p == "-" || p == "+")
        && before_prev.is_none_or(|b| {
            (b.kind == Kind::Punct && b.text != ")" && b.text != "]")
                || (b.kind == Kind::Word && KEYWORDS.contains(&b.lower.as_str()))
        })
        && matches!(
            cur.kind,
            Kind::Number | Kind::Word | Kind::Param | Kind::Quoted
        )
        && c != "("
    {
        return Break::Tight;
    }
    Break::Space
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(text: &str) -> String {
        format_sql(text, Dialect::Postgres, Options::default())
    }

    #[test]
    fn lays_out_clauses_and_lists() {
        assert_eq!(
            fmt(
                "select o.id, o.total, c.name from orders o join customers c on c.id = o.customer_id where o.total > 10 and c.country = 'NL' order by o.total desc limit 10"
            ),
            "SELECT\n    o.id,\n    o.total,\n    c.name\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.total > 10\n    AND c.country = 'NL'\nORDER BY o.total DESC\nLIMIT 10"
        );
        assert_eq!(fmt("select * from orders"), "SELECT *\nFROM orders");
        assert_eq!(
            fmt("select distinct a, b from t group by a, b"),
            "SELECT DISTINCT\n    a,\n    b\nFROM t\nGROUP BY\n    a,\n    b"
        );
    }

    #[test]
    fn indents_sub_queries() {
        assert_eq!(
            fmt(
                "select name from users where id in (select user_id from orders where total > 100) and active"
            ),
            "SELECT name\nFROM users\nWHERE id IN (\n    SELECT user_id\n    FROM orders\n    WHERE total > 100\n)\n    AND active"
        );
    }

    #[test]
    fn keeps_function_calls_and_inline_parens() {
        assert_eq!(
            fmt("select count(*), extract(year from created_at), x::text, -1, a - 1 from t where a between 1 and 5 and b in (1, 2)"),
            "SELECT\n    COUNT(*),\n    EXTRACT(YEAR FROM created_at),\n    x::text,\n    -1,\n    a - 1\nFROM t\nWHERE a BETWEEN 1 AND 5\n    AND b IN (1, 2)"
                .replace("YEAR", "year")
        );
        assert_eq!(
            fmt("insert into t (a, b) values (1, 'x'), (2, 'y')"),
            "INSERT INTO t (a, b)\nVALUES\n    (1, 'x'),\n    (2, 'y')"
        );
        assert_eq!(
            fmt("update t set a = 1, b = now() where id = 2"),
            "UPDATE t\nSET\n    a = 1,\n    b = NOW()\nWHERE id = 2"
        );
        assert_eq!(
            fmt("create table t (id int primary key, name text not null)"),
            "CREATE TABLE t (\n    id int PRIMARY KEY,\n    name text NOT NULL\n)"
        );
    }

    #[test]
    fn literals_names_and_comments_stay_as_they_are() {
        let text = "select 'from where  select', \"Order Id\", $tag$ a;b $tag$ -- keep  this\nfrom t /* and   this */ where x = E'it\\'s'";
        let out = fmt(text);
        assert!(out.contains("'from where  select'"));
        assert!(out.contains("\"Order Id\""));
        assert!(out.contains("$tag$ a;b $tag$"));
        assert!(out.contains("-- keep  this\n"));
        assert!(out.contains("/* and   this */"));
        assert!(out.contains("E'it\\'s'"));
        let comment_first = fmt("-- report\nselect 1");
        assert_eq!(comment_first, "-- report\nSELECT 1");
        let trailing = fmt("select a, -- first\n b from t");
        assert_eq!(trailing, "SELECT\n    a, -- first\n    b\nFROM t");
    }

    #[test]
    fn operators_and_prefixed_literals_stay_whole() {
        assert_eq!(
            fmt("select a~*'x', b@@ q, j?|array['k'], p<->q, j#-'{a}', r-|- s from t where x=-1"),
            "SELECT\n    a ~* 'x',\n    b @@ q,\n    j ?| ARRAY['k'],\n    p <-> q,\n    j #- '{a}',\n    r -|-s\nFROM t\nWHERE x = -1"
                .replace("ARRAY", "array")
        );
        assert_eq!(
            fmt("select x'ff', B'01', U&'d\\0061t', a - -1"),
            "SELECT\n    x'ff',\n    B'01',\n    U&'d\\0061t',\n    a - -1"
        );
        let mysql = |s: &str| format_sql(s, Dialect::Mysql, Options::default());
        assert_eq!(
            mysql("select @@global.max_connections"),
            "SELECT @@global.max_connections"
        );
        assert_eq!(
            format_sql("select 1 where a==b", Dialect::Sqlite, Options::default()),
            "SELECT 1\nWHERE a == b"
        );
        // A table alias keeps its case where it can matter.
        assert_eq!(
            mysql("select first.id from t as first"),
            "SELECT first.id\nFROM t AS first"
        );
    }

    #[test]
    fn string_constants_keep_the_line_break_that_joins_them() {
        assert_eq!(fmt("select 'a'\n'b' from t"), "SELECT 'a'\n'b'\nFROM t");
    }

    #[test]
    fn mysql_delimiter_lines_stay_on_their_own() {
        let text =
            "DELIMITER //\ncreate procedure p() begin select 1; end //\nDELIMITER ;\nselect 2;";
        let out = format_sql(text, Dialect::Mysql, Options::default());
        assert!(out.starts_with("DELIMITER //\n"), "{out}");
        // What ends in `//` is copied as it was.
        assert!(
            out.contains("create procedure p() begin select 1; end //\nDELIMITER ;\nSELECT 2;"),
            "{out}"
        );
        assert_eq!(format_sql(&out, Dialect::Mysql, Options::default()), out);
        let doubled = format_sql(
            "DELIMITER ;;\nselect 1;;\nDELIMITER ;",
            Dialect::Mysql,
            Options::default(),
        );
        assert_eq!(doubled, "DELIMITER ;;\nselect 1;;\nDELIMITER ;");
        assert_eq!(fmt("select 1;; select 2"), "SELECT 1;;\n\nSELECT 2");
        // A column of that name is not a command.
        assert_eq!(
            format_sql(
                "select\ndelimiter, x from t",
                Dialect::Mysql,
                Options::default()
            ),
            "SELECT\n    delimiter,\n    x\nFROM t"
        );
    }

    /// Formatting moves only space: the tokens that come out are the ones that went in.
    #[test]
    fn only_space_changes() {
        let inputs = [
            "select a~*'x', b @@ q, 'a'\n'b', $$ x ; y $$, e'\\'' from \"T\" -- c\nwhere a<>-1 and b !=-2",
            "select `a b`, @@x, @v:=1, a<=>b, j->>'$.k', 'x\\'y' # note\nfrom t",
            "select [a b], a==b, x'0f', a||b from t /* c */ where a<<2",
            "select a - -1, - -2, +-3, x/-4, 1e-5, .5, a.b.c, f(x)::int[] from t",
        ];
        for input in inputs {
            for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
                let out = format_sql(input, dialect, Options { uppercase: false });
                let code = |s: &str| -> Vec<String> {
                    lex::tokens(s, dialect)
                        .iter()
                        .filter(|t| t.kind != Kind::Space)
                        .map(|t| t.text(s).to_owned())
                        .collect()
                };
                assert_eq!(code(input), code(&out), "{dialect:?}: {input:?} -> {out:?}");
            }
        }
    }

    #[test]
    fn statements_are_separated_by_a_blank_line() {
        assert_eq!(fmt("select 1;select 2;\n"), "SELECT 1;\n\nSELECT 2;\n");
    }

    #[test]
    fn case_is_kept_when_asked() {
        let out = format_sql(
            "select a from t where b = 1",
            Dialect::Postgres,
            Options { uppercase: false },
        );
        assert_eq!(out, "select a\nfrom t\nwhere b = 1");
        // Names that look like keywords are left alone.
        assert_eq!(fmt("select t.order from t"), "SELECT t.order\nFROM t");
        assert_eq!(
            format_sql("select * from view", Dialect::Mysql, Options::default()),
            "SELECT *\nFROM view"
        );
    }

    #[test]
    fn formatting_is_idempotent() {
        let inputs = [
            "select o.id, o.total from orders o join c on c.id = o.c where a = 1 and (b = 2 or c = 3) order by 1",
            "with x as (select 1 as a), y as (select 2) select * from x, y",
            "select a, -- note\n b /* inline */ from t\n-- end\n;select 2",
            "insert into t (a) values (1), (2) returning id",
            "create table t (id int primary key, name text not null, check (id > 0))",
            "select * from a where x in (select y from b where z in (select 1)) limit 5 offset 10",
            "select case when a > 1 then 'x' else 'y' end from t",
            "delete from t where id = 1; update t set a = -1",
            "select a from t union all select b from u",
            "select\n/* lead */\na from t",
            "",
            "   ",
            "-- only a comment",
        ];
        for input in inputs {
            for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
                let once = format_sql(input, dialect, Options::default());
                let twice = format_sql(&once, dialect, Options::default());
                assert_eq!(once, twice, "not idempotent for {input:?}");
            }
        }
    }

    #[test]
    fn never_panics() {
        for text in [
            "(", ")", "))((", "'", "select (", "a ,, b", "/*", "select -", ";", "é", "::",
        ] {
            for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
                let _ = format_sql(text, dialect, Options::default());
            }
        }
    }

    /// Scripts made of random pieces come out with the same tokens, split into the same
    /// statements, and formatting them twice changes nothing more.
    #[test]
    fn random_scripts_keep_their_tokens() {
        const PIECES: &[&str] = &[
            "select",
            "from",
            "where",
            "and",
            "or",
            "(",
            ")",
            ",",
            ";",
            "-",
            "+",
            "*",
            "/",
            "=",
            "<",
            ">",
            "~",
            "@",
            "?",
            "|",
            "!",
            "#",
            "%",
            "&",
            "a",
            "b.c",
            "'s'",
            "\"q\"",
            "1",
            "-- c\n",
            "/* b */",
            "\n",
            " ",
            "$$ x $$",
            "x'f'",
            "e'\\''",
            "in",
            "between",
            "join",
            "on",
            "group",
            "by",
            "order",
            "values",
            "insert",
            "into",
            "set",
            "case",
            "when",
            "then",
            "end",
            "with",
            "as",
            "union",
            "delimiter",
            "`m`",
            "[s]",
            "::",
            ":x",
            "@v",
            "$1",
            "#c\n",
            "limit",
            "U&",
            "e'x'",
            "DELIMITER //\n",
            "\nDELIMITER ;\n",
            "//",
            "$$",
            "'a'\n'b'",
            "-1",
            "~*",
            "@@",
        ];
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..3000 {
            let mut text = String::new();
            for _ in 0..next() % 25 {
                text.push_str(PIECES[(next() % PIECES.len() as u64) as usize]);
                if next() % 3 != 0 {
                    text.push(' ');
                }
            }
            for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
                let code = |s: &str| -> Vec<String> {
                    lex::tokens(s, dialect)
                        .iter()
                        .filter(|t| t.kind != Kind::Space)
                        .map(|t| t.text(s).trim_end().to_ascii_lowercase())
                        .collect()
                };
                let statements = |s: &str| -> Vec<Vec<String>> {
                    deltos_drivers::sql::split_statements(s, dialect)
                        .into_iter()
                        .map(|r| code(&s[r]))
                        .collect()
                };
                for uppercase in [false, true] {
                    let options = Options { uppercase };
                    let once = format_sql(&text, dialect, options);
                    assert_eq!(code(&text), code(&once), "{dialect:?} {text:?} -> {once:?}");
                    assert_eq!(
                        statements(&text),
                        statements(&once),
                        "{dialect:?} {text:?} -> {once:?}"
                    );
                    let twice = format_sql(&once, dialect, options);
                    assert_eq!(once, twice, "{dialect:?} {text:?}");
                }
            }
        }
    }
}
