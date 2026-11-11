//! A small subset of the MongoDB shell language, parsed by hand.
//!
//! It accepts what people paste from the shell and from documentation:
//!
//! - `db.orders.find({status: 'open'}, {total: 1}).sort({_id: -1}).limit(20)`, with
//!   `skip`, `projection`, `batchSize` and `count()`; `findOne`
//! - `db.orders.aggregate([...])`, `countDocuments`, `count`, `estimatedDocumentCount`,
//!   `distinct`
//! - `insertOne`, `insertMany`, `updateOne`, `updateMany`, `replaceOne`, `deleteOne`,
//!   `deleteMany`, `drop`
//! - `db.getCollection("odd.name")`, `db["odd.name"]`, `db.getSiblingDB("other")`,
//!   `db.getCollectionNames()`, `db.runCommand({...})`, `db.adminCommand({...})`
//! - a bare command document, such as `{"ping": 1}`
//! - `.explain(verbosity)` after `find`, `aggregate`, `count` or `distinct`, or before them
//!   as `db.orders.explain().find(...)`
//!
//! Values are relaxed JSON: unquoted keys, single quotes, trailing commas, comments,
//! `ObjectId("...")`, `ISODate("...")`, `new Date(...)`, `NumberLong`, `NumberInt`,
//! `NumberDecimal`, `UUID`, `BinData`, `Timestamp`, `MinKey`, `MaxKey`, `/regex/flags`,
//! and extended JSON such as `{"$oid": "..."}`. The parser never panics; anything it
//! cannot read is an error that says where.

use bson::oid::ObjectId;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson, DateTime, Decimal128, Document, Regex, Timestamp};

/// Nesting deeper than this is refused rather than risking the stack.
const MAX_DEPTH: usize = 100;

/// A parsed statement.
// One is parsed per statement run, so the size of the largest variant costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// A database command, from `db.runCommand`, `db.adminCommand` or a bare document.
    Run {
        database: Option<String>,
        admin: bool,
        command: Document,
    },
    ListCollections {
        database: Option<String>,
    },
    Collection {
        database: Option<String>,
        collection: String,
        operation: Operation,
    },
    /// The plan of a read: `find`, `aggregate`, `count` or `distinct`.
    Explain {
        database: Option<String>,
        collection: String,
        operation: Operation,
        /// `queryPlanner`, `executionStats` or `allPlansExecution`.
        verbosity: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    Find(Find),
    Aggregate {
        pipeline: Vec<Document>,
        options: Document,
    },
    Count {
        filter: Document,
    },
    EstimatedCount,
    Distinct {
        field: String,
        filter: Document,
    },
    InsertOne(Document),
    InsertMany(Vec<Document>),
    Update {
        filter: Document,
        update: Update,
        many: bool,
        upsert: bool,
    },
    Replace {
        filter: Document,
        replacement: Document,
        upsert: bool,
    },
    Delete {
        filter: Document,
        many: bool,
    },
    Drop,
}

/// The second argument of an update: modifiers, or an aggregation pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Document(Document),
    Pipeline(Vec<Document>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Find {
    pub filter: Document,
    pub projection: Option<Document>,
    pub sort: Option<Document>,
    pub skip: Option<u64>,
    pub limit: Option<i64>,
    pub batch_size: Option<u32>,
    /// `findOne`: one document at most.
    pub one: bool,
    /// `.count()` at the end of the chain.
    pub count: bool,
}

/// Where and why parsing stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl ParseError {
    /// The message with a line and column, for the person reading it.
    pub fn describe(&self, input: &str) -> String {
        let before = input.get(..self.offset).unwrap_or(input);
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        format!("{} (line {line}, column {column})", self.message)
    }
}

type Parsed<T> = Result<T, ParseError>;

/// Parses one statement.
pub fn parse(input: &str) -> Parsed<Command> {
    let mut p = Parser::new(input);
    p.skip_space();
    let command = if p.peek() == Some(b'{') {
        let command = p.document(0)?;
        Command::Run {
            database: None,
            admin: false,
            command,
        }
    } else {
        p.statement()?
    };
    p.skip_space();
    while p.eat(b';') {
        p.skip_space();
    }
    if !p.at_end() {
        return Err(p.error("unexpected text after the statement"));
    }
    Ok(command)
}

/// Parses a value on its own, such as a filter document typed in the filter bar.
pub fn parse_value(input: &str) -> Parsed<Bson> {
    let mut p = Parser::new(input);
    p.skip_space();
    let value = p.value(0)?;
    p.skip_space();
    if !p.at_end() {
        return Err(p.error("unexpected text after the value"));
    }
    Ok(value)
}

/// Parses a document on its own.
pub fn parse_document(input: &str) -> Parsed<Document> {
    match parse_value(input)? {
        Bson::Document(doc) => Ok(doc),
        _ => Err(ParseError {
            message: "expected a document in braces".to_string(),
            offset: 0,
        }),
    }
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn error(&self, message: &str) -> ParseError {
        ParseError {
            message: message.to_string(),
            offset: self.pos.min(self.text.len()),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8, what: &str) -> Parsed<()> {
        self.skip_space();
        if self.eat(byte) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {what}")))
        }
    }

    /// Skips whitespace and `//` and `/* */` comments.
    fn skip_space(&mut self) {
        loop {
            while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
                self.pos += 1;
            }
            let rest = &self.bytes[self.pos.min(self.bytes.len())..];
            if rest.starts_with(b"//") {
                self.pos = match rest.iter().position(|b| *b == b'\n') {
                    Some(n) => self.pos + n + 1,
                    None => self.bytes.len(),
                };
            } else if rest.starts_with(b"/*") {
                self.pos = match rest.windows(2).skip(2).position(|w| w == b"*/") {
                    Some(n) => self.pos + n + 4,
                    None => self.bytes.len(),
                };
            } else {
                return;
            }
        }
    }

    /// A JavaScript-style name: letters, digits, `_` and `$`, and any non-ASCII letter.
    fn identifier(&mut self) -> Option<&'a str> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80 {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start || self.bytes[start].is_ascii_digit() {
            self.pos = start;
            return None;
        }
        self.text.get(start..self.pos)
    }

    fn keyword(&mut self, word: &str) -> bool {
        let start = self.pos;
        match self.identifier() {
            Some(found) if found == word => true,
            _ => {
                self.pos = start;
                false
            }
        }
    }

    // -----------------------------------------------------------------------------------
    // Statements.

    fn statement(&mut self) -> Parsed<Command> {
        if !self.keyword("db") {
            return Err(self.error("expected a statement starting with db. or a command document"));
        }
        let mut database: Option<String> = None;
        loop {
            self.skip_space();
            let collection = if self.eat(b'[') {
                self.skip_space();
                let name = self.string()?;
                self.expect(b']', "]")?;
                name
            } else if self.eat(b'.') {
                self.skip_space();
                let at = self.pos;
                let name = self
                    .identifier()
                    .ok_or_else(|| self.error("expected a collection or method name"))?
                    .to_string();
                self.skip_space();
                if self.peek() == Some(b'(') {
                    match name.as_str() {
                        "getSiblingDB" | "getSisterDB" => {
                            let mut args = self.arguments()?;
                            database = Some(take_string(&mut args, 0, "a database name", at)?);
                            continue;
                        }
                        "getCollection" => {
                            let mut args = self.arguments()?;
                            take_string(&mut args, 0, "a collection name", at)?
                        }
                        "getCollectionNames" | "getCollectionInfos" => {
                            self.arguments()?;
                            return Ok(Command::ListCollections { database });
                        }
                        "runCommand" | "adminCommand" => {
                            let mut args = self.arguments()?;
                            let command =
                                take_document(&mut args, 0, at)?.ok_or_else(|| ParseError {
                                    message: format!("{name} needs a command document"),
                                    offset: at,
                                })?;
                            return Ok(Command::Run {
                                database,
                                admin: name == "adminCommand",
                                command,
                            });
                        }
                        _ => {
                            return Err(ParseError {
                                message: format!("db.{name}() is not supported here"),
                                offset: at,
                            });
                        }
                    }
                } else {
                    name
                }
            } else {
                return Err(self.error("expected . after db"));
            };
            return self.collection_call(database, collection);
        }
    }

    /// `.a.b.method(args)` after a collection name. Dotted names such as
    /// `db.system.users.find()` join every part before the method.
    fn collection_call(
        &mut self,
        database: Option<String>,
        mut collection: String,
    ) -> Parsed<Command> {
        // `db.orders.explain("executionStats").find(...)`.
        let mut explain: Option<(String, usize)> = None;
        loop {
            self.skip_space();
            if !self.eat(b'.') {
                return Err(self.error("expected a method call such as .find()"));
            }
            self.skip_space();
            let at = self.pos;
            let name = self
                .identifier()
                .ok_or_else(|| self.error("expected a method name"))?
                .to_string();
            self.skip_space();
            if self.peek() != Some(b'(') {
                collection.push('.');
                collection.push_str(&name);
                continue;
            }
            let args = self.arguments()?;
            if name == "explain" && explain.is_none() {
                explain = Some((verbosity(args, at)?, at));
                continue;
            }
            let operation = self.operation(&name, args, at)?;
            if let Some(trailing) = self.trailing_explain()? {
                explain = Some(trailing);
            }
            return match explain {
                Some((verbosity, at)) => {
                    if !matches!(
                        operation,
                        Operation::Find(_)
                            | Operation::Aggregate { .. }
                            | Operation::Count { .. }
                            | Operation::Distinct { .. }
                    ) {
                        return Err(at_error(
                            at,
                            "explain() works with find, aggregate, count and distinct",
                        ));
                    }
                    Ok(Command::Explain {
                        database,
                        collection,
                        operation,
                        verbosity,
                    })
                }
                None => Ok(Command::Collection {
                    database,
                    collection,
                    operation,
                }),
            };
        }
    }

    fn operation(&mut self, name: &str, mut args: Vec<Bson>, at: usize) -> Parsed<Operation> {
        let filter =
            |args: &mut Vec<Bson>| take_document(args, 0, at).map(Option::unwrap_or_default);
        let operation = match name {
            "find" | "findOne" => {
                let mut find = Find {
                    filter: filter(&mut args)?,
                    projection: take_document(&mut args, 1, at)?,
                    one: name == "findOne",
                    ..Default::default()
                };
                if find.one {
                    find.limit = Some(1);
                }
                self.modifiers(&mut find)?;
                return Ok(Operation::Find(find));
            }
            "aggregate" => {
                let pipeline = match args.get_mut(0).map(std::mem::take) {
                    Some(Bson::Array(stages)) => documents(stages, at)?,
                    Some(Bson::Document(stage)) => {
                        // The old shell form, aggregate(stage1, stage2, ...).
                        let mut stages = vec![stage];
                        for extra in args.drain(1..) {
                            match extra {
                                Bson::Document(doc) => stages.push(doc),
                                _ => {
                                    return Err(at_error(at, "aggregate stages must be documents"));
                                }
                            }
                        }
                        stages
                    }
                    None | Some(Bson::Null) => Vec::new(),
                    Some(_) => return Err(at_error(at, "aggregate needs an array of stages")),
                };
                Operation::Aggregate {
                    pipeline,
                    options: take_document(&mut args, 1, at)?.unwrap_or_default(),
                }
            }
            "countDocuments" | "count" => Operation::Count {
                filter: filter(&mut args)?,
            },
            "estimatedDocumentCount" => Operation::EstimatedCount,
            "distinct" => Operation::Distinct {
                field: take_string(&mut args, 0, "a field name", at)?,
                filter: take_document(&mut args, 1, at)?.unwrap_or_default(),
            },
            "insertOne" | "insert" if !matches!(args.first(), Some(Bson::Array(_))) => {
                Operation::InsertOne(
                    take_document(&mut args, 0, at)?
                        .ok_or_else(|| at_error(at, "insertOne needs a document"))?,
                )
            }
            "insertMany" | "insert" => match args.get_mut(0).map(std::mem::take) {
                Some(Bson::Array(items)) => Operation::InsertMany(documents(items, at)?),
                _ => return Err(at_error(at, "insertMany needs an array of documents")),
            },
            "updateOne" | "updateMany" => {
                let filter = filter(&mut args)?;
                let update = match args.get_mut(1).map(std::mem::take) {
                    Some(Bson::Document(doc)) => Update::Document(doc),
                    Some(Bson::Array(stages)) => Update::Pipeline(documents(stages, at)?),
                    _ => {
                        return Err(at_error(
                            at,
                            "an update needs a document of changes such as {$set: {...}}",
                        ));
                    }
                };
                Operation::Update {
                    filter,
                    update,
                    many: name == "updateMany",
                    upsert: upsert(&mut args, 2, at)?,
                }
            }
            "replaceOne" => Operation::Replace {
                filter: filter(&mut args)?,
                replacement: take_document(&mut args, 1, at)?
                    .ok_or_else(|| at_error(at, "replaceOne needs a replacement document"))?,
                upsert: upsert(&mut args, 2, at)?,
            },
            "deleteOne" | "deleteMany" | "remove" => Operation::Delete {
                filter: filter(&mut args)?,
                many: name != "deleteOne",
            },
            "drop" => Operation::Drop,
            other => return Err(at_error(at, &format!("{other}() is not supported here"))),
        };
        self.skip_space();
        Ok(operation)
    }

    /// `.explain(verbosity)` at the end of a statement.
    fn trailing_explain(&mut self) -> Parsed<Option<(String, usize)>> {
        self.skip_space();
        let start = self.pos;
        if !self.eat(b'.') {
            return Ok(None);
        }
        self.skip_space();
        let at = self.pos;
        if !self.keyword("explain") {
            self.pos = start;
            return Ok(None);
        }
        let args = self.arguments()?;
        Ok(Some((verbosity(args, at)?, at)))
    }

    /// `.sort()`, `.limit()` and the other cursor methods after `find`.
    fn modifiers(&mut self, find: &mut Find) -> Parsed<()> {
        loop {
            self.skip_space();
            let start = self.pos;
            if !self.eat(b'.') {
                return Ok(());
            }
            self.skip_space();
            let at = self.pos;
            let Some(name) = self.identifier() else {
                self.pos = start;
                return Ok(());
            };
            if name == "explain" {
                // Read by the caller, as for any other operation.
                self.pos = start;
                return Ok(());
            }
            let mut args = self.arguments()?;
            match name {
                "sort" => find.sort = take_document(&mut args, 0, at)?,
                "projection" | "project" => find.projection = take_document(&mut args, 0, at)?,
                "limit" => find.limit = Some(take_integer(&mut args, at)?),
                "skip" => find.skip = Some(take_integer(&mut args, at)?.max(0) as u64),
                "batchSize" => {
                    find.batch_size =
                        Some(take_integer(&mut args, at)?.clamp(1, i64::from(u32::MAX)) as u32)
                }
                "count" | "itcount" | "size" => find.count = true,
                "pretty" | "toArray" | "allowDiskUse" | "noCursorTimeout" | "hint" | "comment"
                | "maxTimeMS" => {}
                other => return Err(at_error(at, &format!("{other}() is not supported here"))),
            }
        }
    }

    /// `(a, b, ...)`.
    fn arguments(&mut self) -> Parsed<Vec<Bson>> {
        self.expect(b'(', "(")?;
        let mut args = Vec::new();
        loop {
            self.skip_space();
            if self.eat(b')') {
                return Ok(args);
            }
            args.push(self.value(0)?);
            self.skip_space();
            if self.eat(b',') {
                continue;
            }
            self.expect(b')', ", or )")?;
            return Ok(args);
        }
    }

    // -----------------------------------------------------------------------------------
    // Values.

    fn value(&mut self, depth: usize) -> Parsed<Bson> {
        if depth > MAX_DEPTH {
            return Err(self.error("the value nests too deeply"));
        }
        self.skip_space();
        match self.peek() {
            None => Err(self.error("expected a value")),
            Some(b'{') => Ok(extended(self.document(depth)?)),
            Some(b'[') => self.array(depth).map(Bson::Array),
            Some(b'"' | b'\'') => self.string().map(Bson::String),
            Some(b'/') => self.regex(),
            Some(b'-' | b'+' | b'.' | b'0'..=b'9') => self.number(),
            Some(_) => self.word(depth),
        }
    }

    fn document(&mut self, depth: usize) -> Parsed<Document> {
        if depth > MAX_DEPTH {
            return Err(self.error("the value nests too deeply"));
        }
        self.expect(b'{', "{")?;
        let mut doc = Document::new();
        loop {
            self.skip_space();
            if self.eat(b'}') {
                return Ok(doc);
            }
            let key = match self.peek() {
                Some(b'"' | b'\'') => self.string()?,
                _ => {
                    let start = self.pos;
                    while let Some(b) = self.peek() {
                        if b.is_ascii_alphanumeric()
                            || matches!(b, b'_' | b'$' | b'.' | b'-')
                            || b >= 0x80
                        {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    if self.pos == start {
                        return Err(self.error("expected a field name"));
                    }
                    self.text.get(start..self.pos).unwrap_or("").to_string()
                }
            };
            self.expect(b':', ": after the field name")?;
            let value = self.value(depth + 1)?;
            doc.insert(key, value);
            self.skip_space();
            if self.eat(b',') {
                continue;
            }
            self.expect(b'}', ", or }")?;
            return Ok(doc);
        }
    }

    fn array(&mut self, depth: usize) -> Parsed<Vec<Bson>> {
        self.expect(b'[', "[")?;
        let mut items = Vec::new();
        loop {
            self.skip_space();
            if self.eat(b']') {
                return Ok(items);
            }
            items.push(self.value(depth + 1)?);
            self.skip_space();
            if self.eat(b',') {
                continue;
            }
            self.expect(b']', ", or ]")?;
            return Ok(items);
        }
    }

    fn string(&mut self) -> Parsed<String> {
        let quote = match self.peek() {
            Some(q @ (b'"' | b'\'')) => q,
            _ => return Err(self.error("expected a quoted string")),
        };
        let start = self.pos;
        self.pos += 1;
        let mut out = String::new();
        loop {
            let rest = self.text.get(self.pos..).unwrap_or("");
            let mut chars = rest.chars();
            let Some(c) = chars.next() else {
                self.pos = start;
                return Err(self.error("this string has no closing quote"));
            };
            self.pos += c.len_utf8();
            if c as u32 == u32::from(quote) {
                return Ok(out);
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            let Some(escaped) = chars.next() else {
                continue;
            };
            self.pos += escaped.len_utf8();
            match escaped {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                '0' => out.push('\0'),
                'u' => {
                    let hex = self.text.get(self.pos..self.pos + 4).unwrap_or("");
                    let code = u32::from_str_radix(hex, 16)
                        .map_err(|_| self.error("expected four hex digits after \\u"))?;
                    self.pos += 4;
                    let c = if (0xD800..0xDC00).contains(&code) {
                        // A surrogate pair: 😀.
                        let low = self
                            .text
                            .get(self.pos..self.pos + 6)
                            .and_then(|s| s.strip_prefix("\\u"))
                            .and_then(|h| u32::from_str_radix(h, 16).ok())
                            .filter(|low| (0xDC00..0xE000).contains(low));
                        match low {
                            Some(low) => {
                                self.pos += 6;
                                char::from_u32(0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00))
                            }
                            None => None,
                        }
                    } else {
                        char::from_u32(code)
                    };
                    out.push(c.unwrap_or('\u{fffd}'));
                }
                other => out.push(other),
            }
        }
    }

    fn regex(&mut self) -> Parsed<Bson> {
        let start = self.pos;
        self.pos += 1;
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            let Some(c) = self.text.get(self.pos..).and_then(|r| r.chars().next()) else {
                self.pos = start;
                return Err(self.error("this regular expression has no closing /"));
            };
            self.pos += c.len_utf8();
            match c {
                '\\' => {
                    pattern.push(c);
                    if let Some(next) = self.text.get(self.pos..).and_then(|r| r.chars().next()) {
                        self.pos += next.len_utf8();
                        pattern.push(next);
                    }
                }
                '[' => {
                    in_class = true;
                    pattern.push(c);
                }
                ']' => {
                    in_class = false;
                    pattern.push(c);
                }
                '/' if !in_class => break,
                '\n' => {
                    self.pos = start;
                    return Err(self.error("this regular expression has no closing /"));
                }
                c => pattern.push(c),
            }
        }
        let mut flags: Vec<char> = Vec::new();
        while let Some(b) = self.peek() {
            if b.is_ascii_alphabetic() {
                flags.push(b as char);
                self.pos += 1;
            } else {
                break;
            }
        }
        flags.sort_unstable();
        flags.dedup();
        regex(pattern, flags.into_iter().collect()).ok_or_else(|| {
            self.pos = start;
            self.error("a regular expression cannot contain a NUL character")
        })
    }

    fn number(&mut self) -> Parsed<Bson> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.pos += 1;
        }
        if self.keyword("Infinity") {
            let negative = self.bytes.get(start) == Some(&b'-');
            return Ok(Bson::Double(if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }));
        }
        let mut float = false;
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' => {}
                b'.' | b'e' | b'E' => float = true,
                b'-' | b'+'
                    if matches!(self.bytes.get(self.pos.wrapping_sub(1)), Some(b'e' | b'E')) => {}
                _ => break,
            }
            self.pos += 1;
        }
        let text = self.text.get(start..self.pos).unwrap_or("");
        let text = text.strip_prefix('+').unwrap_or(text);
        if !float && let Ok(n) = text.parse::<i64>() {
            return Ok(match i32::try_from(n) {
                Ok(small) => Bson::Int32(small),
                Err(_) => Bson::Int64(n),
            });
        }
        match text.parse::<f64>() {
            Ok(f) => Ok(Bson::Double(f)),
            Err(_) => {
                self.pos = start;
                Err(self.error("this is not a number"))
            }
        }
    }

    /// `true`, `null`, `ObjectId("...")`, `new Date()` and the other named values.
    fn word(&mut self, depth: usize) -> Parsed<Bson> {
        let at = self.pos;
        let Some(mut name) = self.identifier() else {
            return Err(self.error("expected a value"));
        };
        if name == "new" {
            self.skip_space();
            name = self
                .identifier()
                .ok_or_else(|| self.error("expected a type after new"))?;
        }
        match name {
            "true" => return Ok(Bson::Boolean(true)),
            "false" => return Ok(Bson::Boolean(false)),
            "null" => return Ok(Bson::Null),
            "undefined" => return Ok(Bson::Undefined),
            "NaN" => return Ok(Bson::Double(f64::NAN)),
            "Infinity" => return Ok(Bson::Double(f64::INFINITY)),
            "MinKey" | "MaxKey" if self.peek_after_space() != Some(b'(') => {
                return Ok(if name == "MinKey" {
                    Bson::MinKey
                } else {
                    Bson::MaxKey
                });
            }
            _ => {}
        }
        self.skip_space();
        if self.peek() != Some(b'(') {
            return Err(at_error(
                at,
                &format!("unknown name {name}; strings need quotes"),
            ));
        }
        self.pos += 1;
        let mut args = Vec::new();
        loop {
            self.skip_space();
            if self.eat(b')') {
                break;
            }
            args.push(self.value(depth + 1)?);
            self.skip_space();
            if !self.eat(b',') {
                self.expect(b')', ", or )")?;
                break;
            }
        }
        constructor(name, args).map_err(|message| at_error(at, &message))
    }

    fn peek_after_space(&mut self) -> Option<u8> {
        let saved = self.pos;
        self.skip_space();
        let next = self.peek();
        self.pos = saved;
        next
    }
}

/// The verbosity argument of `explain()`: a name, or `true` for every plan.
fn verbosity(args: Vec<Bson>, at: usize) -> Parsed<String> {
    match args.into_iter().next() {
        None | Some(Bson::Boolean(false)) => Ok("queryPlanner".to_string()),
        Some(Bson::Boolean(true)) => Ok("allPlansExecution".to_string()),
        Some(Bson::String(v))
            if matches!(
                v.as_str(),
                "queryPlanner" | "executionStats" | "allPlansExecution"
            ) =>
        {
            Ok(v)
        }
        Some(_) => Err(at_error(
            at,
            "explain() takes queryPlanner, executionStats or allPlansExecution",
        )),
    }
}

fn at_error(offset: usize, message: &str) -> ParseError {
    ParseError {
        message: message.to_string(),
        offset,
    }
}

fn regex(pattern: String, options: String) -> Option<Bson> {
    Some(Bson::RegularExpression(Regex {
        pattern: pattern.try_into().ok()?,
        options: options.try_into().ok()?,
    }))
}

/// Builds the value of a shell constructor such as `ObjectId("...")`.
fn constructor(name: &str, mut args: Vec<Bson>) -> Result<Bson, String> {
    let first = if args.is_empty() {
        None
    } else {
        Some(args.remove(0))
    };
    let text = |value: &Option<Bson>| -> Option<String> {
        match value {
            Some(Bson::String(s)) => Some(s.clone()),
            Some(Bson::Int32(n)) => Some(n.to_string()),
            Some(Bson::Int64(n)) => Some(n.to_string()),
            Some(Bson::Double(f)) => Some(f.to_string()),
            _ => None,
        }
    };
    match name {
        "ObjectId" | "ObjectID" => match &first {
            None => Ok(Bson::ObjectId(ObjectId::new())),
            Some(Bson::String(hex)) => ObjectId::parse_str(hex)
                .map(Bson::ObjectId)
                .map_err(|_| format!("“{hex}” is not an ObjectId: it needs 24 hex digits")),
            Some(_) => Err("ObjectId takes a string of 24 hex digits".to_string()),
        },
        "ISODate" | "Date" => match &first {
            None => Ok(Bson::DateTime(DateTime::now())),
            Some(Bson::String(s)) => parse_date(s)
                .map(Bson::DateTime)
                .ok_or_else(|| format!("“{s}” is not a date such as 2024-01-31T12:00:00Z")),
            Some(Bson::Int32(ms)) => Ok(Bson::DateTime(DateTime::from_millis(i64::from(*ms)))),
            Some(Bson::Int64(ms)) => Ok(Bson::DateTime(DateTime::from_millis(*ms))),
            Some(Bson::Double(ms)) if ms.is_finite() => {
                Ok(Bson::DateTime(DateTime::from_millis(*ms as i64)))
            }
            Some(_) => Err("a date takes a string or milliseconds since 1970".to_string()),
        },
        "NumberLong" | "Long" => text(&first)
            .and_then(|t| t.trim().parse::<i64>().ok())
            .map(Bson::Int64)
            .ok_or_else(|| "NumberLong takes a whole number".to_string()),
        "NumberInt" | "Int32" => text(&first)
            .and_then(|t| t.trim().parse::<i32>().ok())
            .map(Bson::Int32)
            .ok_or_else(|| "NumberInt takes a whole number that fits in 32 bits".to_string()),
        "Double" => text(&first)
            .and_then(|t| t.trim().parse::<f64>().ok())
            .map(Bson::Double)
            .ok_or_else(|| "Double takes a number".to_string()),
        "NumberDecimal" | "Decimal128" => text(&first)
            .and_then(|t| t.trim().parse::<Decimal128>().ok())
            .map(Bson::Decimal128)
            .ok_or_else(|| "NumberDecimal takes a decimal number in quotes".to_string()),
        "UUID" => match &first {
            Some(Bson::String(s)) => bson::Uuid::parse_str(s)
                .map(|uuid| Bson::Binary(Binary::from(uuid)))
                .map_err(|_| format!("“{s}” is not a UUID")),
            _ => Err(
                "UUID takes a string such as \"123e4567-e89b-12d3-a456-426614174000\"".to_string(),
            ),
        },
        "BinData" => {
            let subtype = match &first {
                Some(Bson::Int32(n)) => u8::try_from(*n).ok(),
                _ => None,
            }
            .ok_or_else(|| "BinData takes a subtype number and a base64 string".to_string())?;
            match args.first() {
                Some(Bson::String(data)) => Binary::from_base64(data, BinarySubtype::from(subtype))
                    .map(Bson::Binary)
                    .map_err(|_| "BinData needs valid base64".to_string()),
                _ => Err("BinData takes a subtype number and a base64 string".to_string()),
            }
        }
        "Timestamp" => {
            let (time, increment) = match (&first, args.first()) {
                (Some(Bson::Document(doc)), _) => (doc.get("t").cloned(), doc.get("i").cloned()),
                (time, increment) => (time.clone(), increment.cloned()),
            };
            let number = |value: Option<Bson>| -> Option<u32> {
                match value? {
                    Bson::Int32(n) => u32::try_from(n).ok(),
                    Bson::Int64(n) => u32::try_from(n).ok(),
                    Bson::Double(f) if (0.0..=f64::from(u32::MAX)).contains(&f) => Some(f as u32),
                    _ => None,
                }
            };
            match (number(time), number(increment)) {
                (Some(time), Some(increment)) => Ok(Bson::Timestamp(Timestamp { time, increment })),
                _ => Err("Timestamp takes two whole numbers: seconds and an increment".to_string()),
            }
        }
        "RegExp" => match (&first, args.first()) {
            (Some(Bson::String(pattern)), flags) => {
                let flags = match flags {
                    Some(Bson::String(f)) => {
                        let mut chars: Vec<char> = f.chars().collect();
                        chars.sort_unstable();
                        chars.dedup();
                        chars.into_iter().collect()
                    }
                    _ => String::new(),
                };
                regex(pattern.clone(), flags).ok_or_else(|| {
                    "a regular expression cannot contain a NUL character".to_string()
                })
            }
            _ => Err("RegExp takes a pattern string".to_string()),
        },
        "MinKey" => Ok(Bson::MinKey),
        "MaxKey" => Ok(Bson::MaxKey),
        other => Err(format!("{other}() is not a value Deltos knows")),
    }
}

/// ISO dates as people write them: with or without a time, a zone or fractions.
pub fn parse_date(text: &str) -> Option<DateTime> {
    let text = text.trim();
    if let Ok(date) = DateTime::parse_rfc3339_str(text) {
        return Some(date);
    }
    let with_time = if text.len() == 10 {
        format!("{text}T00:00:00Z")
    } else {
        text.replacen(' ', "T", 1)
    };
    if let Ok(date) = DateTime::parse_rfc3339_str(&with_time) {
        return Some(date);
    }
    DateTime::parse_rfc3339_str(format!("{with_time}Z")).ok()
}

/// Extended JSON such as `{"$oid": "..."}` becomes the value it stands for. Documents
/// that only look similar, such as `{$gt: 5}`, stay as they are.
fn extended(doc: Document) -> Bson {
    const KEYS: &[&str] = &[
        "$oid",
        "$date",
        "$numberLong",
        "$numberInt",
        "$numberDouble",
        "$numberDecimal",
        "$binary",
        "$uuid",
        "$regularExpression",
        "$timestamp",
        "$minKey",
        "$maxKey",
        "$symbol",
        "$code",
        "$undefined",
    ];
    let is_extended = doc.len() <= 2
        && doc
            .keys()
            .next()
            .is_some_and(|k| KEYS.contains(&k.as_str()));
    if !is_extended {
        return Bson::Document(doc);
    }
    let json = Bson::Document(doc.clone()).into_relaxed_extjson();
    Bson::try_from(json).unwrap_or(Bson::Document(doc))
}

fn documents(items: Vec<Bson>, at: usize) -> Parsed<Vec<Document>> {
    items
        .into_iter()
        .map(|item| match item {
            Bson::Document(doc) => Ok(doc),
            _ => Err(at_error(at, "expected documents in the array")),
        })
        .collect()
}

fn take_document(args: &mut [Bson], index: usize, at: usize) -> Parsed<Option<Document>> {
    match args.get_mut(index).map(std::mem::take) {
        None | Some(Bson::Null) | Some(Bson::Undefined) => Ok(None),
        Some(Bson::Document(doc)) => Ok(Some(doc)),
        Some(_) => Err(at_error(
            at,
            &format!("argument {} must be a document", index + 1),
        )),
    }
}

fn take_string(args: &mut [Bson], index: usize, what: &str, at: usize) -> Parsed<String> {
    match args.get_mut(index).map(std::mem::take) {
        Some(Bson::String(s)) => Ok(s),
        _ => Err(at_error(at, &format!("expected {what} in quotes"))),
    }
}

fn take_integer(args: &mut [Bson], at: usize) -> Parsed<i64> {
    match args.first() {
        Some(Bson::Int32(n)) => Ok(i64::from(*n)),
        Some(Bson::Int64(n)) => Ok(*n),
        Some(Bson::Double(f)) if f.is_finite() && f.fract() == 0.0 => Ok(*f as i64),
        _ => Err(at_error(at, "expected a whole number")),
    }
}

fn upsert(args: &mut [Bson], index: usize, at: usize) -> Parsed<bool> {
    Ok(take_document(args, index, at)?
        .and_then(|options| options.get("upsert").cloned())
        .is_some_and(|value| matches!(value, Bson::Boolean(true))))
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;

    fn find(input: &str) -> Find {
        match parse(input) {
            Ok(Command::Collection {
                operation: Operation::Find(find),
                ..
            }) => find,
            other => panic!("{input}: {other:?}"),
        }
    }

    #[test]
    fn parses_find_chains() {
        let f = find(
            "db.orders.find({status: 'open', total: {$gt: 10.5}}, {total: 1}).sort({_id: -1}).skip(5).limit(20);",
        );
        assert_eq!(f.filter, doc! {"status": "open", "total": {"$gt": 10.5}});
        assert_eq!(f.projection, Some(doc! {"total": 1}));
        assert_eq!(f.sort, Some(doc! {"_id": -1}));
        assert_eq!((f.skip, f.limit), (Some(5), Some(20)));

        let f = find("db.orders.find()");
        assert_eq!(f.filter, Document::new());
        let f = find("db . orders . findOne ( { a : 1 } ) . pretty ( )");
        assert!(f.one);
        assert_eq!(f.limit, Some(1));
        assert!(find("db.orders.find({}).count()").count);
    }

    #[test]
    fn names_collections_every_way() {
        let cases = [
            ("db.system.users.find()", "system.users", None),
            ("db['odd name'].find()", "odd name", None),
            ("db.getCollection(\"a.b\").find()", "a.b", None),
            (
                "db.getSiblingDB('other').things.find()",
                "things",
                Some("other".to_string()),
            ),
        ];
        for (input, name, db) in cases {
            match parse(input) {
                Ok(Command::Collection {
                    collection,
                    database,
                    ..
                }) => {
                    assert_eq!(collection, name, "{input}");
                    assert_eq!(database, db, "{input}");
                }
                other => panic!("{input}: {other:?}"),
            }
        }
        assert_eq!(
            parse("db.getCollectionNames()"),
            Ok(Command::ListCollections { database: None })
        );
    }

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse(r#"{"ping": 1}"#),
            Ok(Command::Run {
                database: None,
                admin: false,
                command: doc! {"ping": 1},
            })
        );
        assert_eq!(
            parse("db.adminCommand({listDatabases: 1})"),
            Ok(Command::Run {
                database: None,
                admin: true,
                command: doc! {"listDatabases": 1},
            })
        );
    }

    #[test]
    fn parses_writes() {
        match parse("db.c.updateMany({a: 1}, {$set: {b: 2}}, {upsert: true})") {
            Ok(Command::Collection {
                operation:
                    Operation::Update {
                        many,
                        upsert,
                        update,
                        ..
                    },
                ..
            }) => {
                assert!(many && upsert);
                assert_eq!(update, Update::Document(doc! {"$set": {"b": 2}}));
            }
            other => panic!("{other:?}"),
        }
        match parse("db.c.updateOne({}, [{$set: {b: 2}}])") {
            Ok(Command::Collection {
                operation:
                    Operation::Update {
                        update: Update::Pipeline(stages),
                        ..
                    },
                ..
            }) => assert_eq!(stages.len(), 1),
            other => panic!("{other:?}"),
        }
        match parse("db.c.insertMany([{a: 1}, {a: 2},])") {
            Ok(Command::Collection {
                operation: Operation::InsertMany(docs),
                ..
            }) => assert_eq!(docs.len(), 2),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse("db.c.deleteMany({})"),
            Ok(Command::Collection {
                operation: Operation::Delete { many: true, .. },
                ..
            })
        ));
        assert!(matches!(
            parse("db.c.aggregate([{$match: {a: 1}}, {$group: {_id: '$b', n: {$sum: 1}}}])"),
            Ok(Command::Collection {
                operation: Operation::Aggregate { .. },
                ..
            })
        ));
    }

    #[test]
    fn parses_shell_values() {
        let value = parse_value(
            "{id: ObjectId('65a1b2c3d4e5f60718293a4b'), at: ISODate(\"2024-01-31T12:00:00Z\"), day: new Date('2024-02-01'),
              n: NumberLong(\"9007199254740993\"), i: NumberInt(7), d: NumberDecimal('1.10'), big: 3000000000,
              f: -1.5e3, re: /ab+c\\/d/ig, u: UUID('123e4567-e89b-12d3-a456-426614174000'), ts: Timestamp(1, 2),
              s: 'it\\'s', e: \"\\u00e9\\uD83D\\uDE00\", t: true, z: null, k: MinKey, // a comment
              ext: {\"$oid\": \"65a1b2c3d4e5f60718293a4b\"}, op: {$gt: 5}, list: [1, 'two', [3]],}",
        )
        .expect("parses");
        let Bson::Document(doc) = value else {
            panic!("a document");
        };
        assert!(matches!(doc.get("id"), Some(Bson::ObjectId(_))));
        assert_eq!(doc.get("ext"), doc.get("id"));
        assert!(matches!(doc.get("at"), Some(Bson::DateTime(_))));
        assert!(matches!(doc.get("day"), Some(Bson::DateTime(_))));
        assert_eq!(doc.get("n"), Some(&Bson::Int64(9_007_199_254_740_993)));
        assert_eq!(doc.get("i"), Some(&Bson::Int32(7)));
        assert!(matches!(doc.get("d"), Some(Bson::Decimal128(_))));
        assert_eq!(doc.get("big"), Some(&Bson::Int64(3_000_000_000)));
        assert_eq!(doc.get("f"), Some(&Bson::Double(-1500.0)));
        match doc.get("re") {
            Some(Bson::RegularExpression(re)) => {
                assert_eq!(re.pattern.as_str(), "ab+c\\/d");
                assert_eq!(re.options.as_str(), "gi");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(doc.get("u"), Some(Bson::Binary(_))));
        assert_eq!(
            doc.get("ts"),
            Some(&Bson::Timestamp(Timestamp {
                time: 1,
                increment: 2
            }))
        );
        assert_eq!(doc.get("s"), Some(&Bson::String("it's".into())));
        assert_eq!(doc.get("e"), Some(&Bson::String("é😀".into())));
        assert_eq!(doc.get("k"), Some(&Bson::MinKey));
        assert_eq!(doc.get("op"), Some(&Bson::Document(doc! {"$gt": 5})));
    }

    #[test]
    fn reports_where_it_stopped() {
        let input = "db.c.find({a: 1,\n  b: })";
        let error = parse(input).expect_err("fails");
        assert_eq!(error.describe(input), "expected a value (line 2, column 6)");
        assert!(parse("db.c.find({a: 'open)").is_err());
        assert!(parse("db.c.frobnicate()").is_err());
        assert!(parse("SELECT 1").is_err());
        assert!(parse("db.c.find() junk").is_err());
        assert!(parse_value("ObjectId('nope')").is_err());
        assert!(parse_value(&"[".repeat(10_000)).is_err());
    }

    #[test]
    fn dates_in_any_common_form() {
        assert!(parse_date("2024-01-31").is_some());
        assert!(parse_date("2024-01-31 12:00:00").is_some());
        assert!(parse_date("2024-01-31T12:00:00.123+02:00").is_some());
        assert!(parse_date("yesterday").is_none());
    }

    #[test]
    fn parses_explain_both_ways() {
        let explained = |input: &str| match parse(input) {
            Ok(Command::Explain {
                collection,
                operation,
                verbosity,
                ..
            }) => (collection, operation, verbosity),
            other => panic!("{input}: {other:?}"),
        };
        let (collection, operation, verbosity) =
            explained("db.orders.find({a: 1}).sort({b: -1}).limit(5).explain('executionStats')");
        assert_eq!(collection, "orders");
        assert_eq!(verbosity, "executionStats");
        match operation {
            Operation::Find(find) => {
                assert_eq!(find.limit, Some(5));
                assert!(find.sort.is_some());
            }
            other => panic!("{other:?}"),
        }
        let (_, operation, verbosity) = explained("db.orders.explain().aggregate([{$match: {}}])");
        assert_eq!(verbosity, "queryPlanner");
        assert!(matches!(operation, Operation::Aggregate { .. }));
        let (_, _, verbosity) = explained("db.orders.countDocuments({}).explain(true)");
        assert_eq!(verbosity, "allPlansExecution");
        assert!(parse("db.orders.insertOne({}).explain()").is_err());
        // The query tab adds `.explain()` on a line of its own, past a trailing comment.
        let (_, _, verbosity) = explained("db.orders.find({}) // all\n.explain('executionStats')");
        assert_eq!(verbosity, "executionStats");
        assert!(parse("db.orders.deleteMany({}) // all\n.explain()").is_err());
        assert!(parse("db.orders.find().explain('loud')").is_err());
    }

    #[test]
    fn never_panics() {
        const PIECES: &[&str] = &[
            "db",
            ".",
            "c",
            "find",
            "(",
            ")",
            "{",
            "}",
            "[",
            "]",
            ":",
            ",",
            "'",
            "\"",
            "\\",
            "\\u",
            "/",
            "*",
            "ObjectId",
            "ISODate",
            "new",
            "Date",
            "NumberLong",
            "Timestamp",
            "BinData",
            "1",
            "-",
            "e",
            "1.5",
            "$gt",
            "é",
            " ",
            "\n",
            "//",
            "/*",
            ";",
            "MinKey",
            "UUID",
            "getCollection",
            "sort",
            "limit",
            "aggregate",
            "true",
            "\0",
        ];
        let mut seed = 0x0123_4567_89ab_cdefu64;
        for round in 0..20_000 {
            let mut input = String::new();
            for _ in 0..(round % 40) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                input.push_str(PIECES[(seed % PIECES.len() as u64) as usize]);
            }
            if let Err(e) = parse(&input) {
                let _ = e.describe(&input);
            }
            let _ = parse_value(&input);
        }
    }
}
