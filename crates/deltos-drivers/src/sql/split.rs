//! Splits a script into statements at the semicolons that end them.
//!
//! The splitter knows each dialect's strings, quoted names and comments well enough to
//! skip semicolons inside them: `'...'` with doubled quotes, PostgreSQL `E'...'` and
//! `$tag$...$tag$`, MySQL backslash escapes, `"..."`, `` `...` ``, SQLite `[...]`, `--`
//! and `/* */` comments (nested in PostgreSQL) and MySQL `#` comments. It also keeps
//! routine bodies whole: `BEGIN ATOMIC ... END` in PostgreSQL, `BEGIN ... END` inside a
//! MySQL `CREATE` and SQLite `CREATE TRIGGER`, and the MySQL client's `DELIMITER` lines.
//! It works on bytes and never fails: an unterminated string runs to the end of the text.

use std::ops::Range;

use super::Dialect;

/// Byte ranges of the statements in `sql`, trimmed, without their terminators. Pieces
/// that hold only whitespace and comments are left out.
pub fn split_statements(sql: &str, dialect: Dialect) -> Vec<Range<usize>> {
    Splitter::new(sql, dialect).run()
}

struct Splitter<'a> {
    text: &'a str,
    bytes: &'a [u8],
    dialect: Dialect,
    pos: usize,
    out: Vec<Range<usize>>,
    /// Start of the statement being read.
    start: usize,
    /// Whether the statement so far has anything but whitespace and comments.
    has_code: bool,
    /// Depth of `BEGIN ... END` blocks that a semicolon cannot end.
    depth: u32,
    /// The first word of the statement, uppercased, and whether it is a `CREATE`.
    words_seen: u32,
    in_create: bool,
    previous_word: Option<Range<usize>>,
    delimiter: String,
}

impl<'a> Splitter<'a> {
    fn new(text: &'a str, dialect: Dialect) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            dialect,
            pos: 0,
            out: Vec::new(),
            start: 0,
            has_code: false,
            depth: 0,
            words_seen: 0,
            in_create: false,
            previous_word: None,
            delimiter: ";".to_string(),
        }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn run(mut self) -> Vec<Range<usize>> {
        while self.pos < self.bytes.len() {
            if self.dialect == Dialect::Mysql && !self.has_code && self.at_delimiter_command() {
                continue;
            }
            if self.depth == 0 && self.bytes[self.pos..].starts_with(self.delimiter.as_bytes()) {
                let end = self.pos;
                self.pos += self.delimiter.len();
                self.finish(end);
                continue;
            }
            let c = self.bytes[self.pos];
            match c {
                b'\'' => {
                    let backslash = self.dialect == Dialect::Mysql || self.e_string_prefix();
                    self.code();
                    self.quoted(b'\'', backslash);
                }
                b'"' => {
                    self.code();
                    self.quoted(b'"', self.dialect == Dialect::Mysql);
                }
                b'`' if self.dialect != Dialect::Postgres => {
                    self.code();
                    self.quoted(b'`', false);
                }
                b'[' if self.dialect == Dialect::Sqlite => {
                    self.code();
                    self.pos = self
                        .find_byte(b']', self.pos + 1)
                        .map_or(self.bytes.len(), |p| p + 1);
                }
                b'-' if self.peek(1) == Some(b'-') => self.line_comment(),
                b'#' if self.dialect == Dialect::Mysql => self.line_comment(),
                b'/' if self.peek(1) == Some(b'*') => self.block_comment(),
                b'$' if self.dialect == Dialect::Postgres => {
                    self.code();
                    self.dollar();
                }
                c if c.is_ascii_alphabetic() || c == b'_' => {
                    self.code();
                    self.word();
                }
                c if c.is_ascii_whitespace() => self.pos += 1,
                _ => {
                    self.code();
                    self.pos += 1;
                }
            }
        }
        self.finish(self.bytes.len());
        self.out
    }

    fn code(&mut self) {
        self.has_code = true;
    }

    fn finish(&mut self, end: usize) {
        if self.has_code {
            let piece = &self.text[self.start..end];
            let lead = piece.len() - piece.trim_start().len();
            let trail = piece.len() - piece.trim_end().len();
            self.out.push(self.start + lead..end - trail);
        }
        self.start = self.pos;
        self.has_code = false;
        self.depth = 0;
        self.words_seen = 0;
        self.in_create = false;
        self.previous_word = None;
    }

    fn find_byte(&self, byte: u8, from: usize) -> Option<usize> {
        self.bytes
            .get(from..)?
            .iter()
            .position(|b| *b == byte)
            .map(|p| from + p)
    }

    fn quoted(&mut self, quote: u8, backslash: bool) {
        self.pos += 1;
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if backslash && c == b'\\' {
                self.pos += 2;
                continue;
            }
            self.pos += 1;
            if c == quote {
                if self.bytes.get(self.pos) == Some(&quote) {
                    self.pos += 1;
                    continue;
                }
                return;
            }
        }
        self.pos = self.bytes.len();
    }

    /// Whether the quote at `pos` opens a PostgreSQL `E'...'` string.
    fn e_string_prefix(&self) -> bool {
        self.dialect == Dialect::Postgres
            && self.pos > 0
            && matches!(self.bytes[self.pos - 1], b'e' | b'E')
            && (self.pos < 2 || !is_word_byte(self.bytes[self.pos - 2]))
    }

    fn line_comment(&mut self) {
        self.pos = self
            .find_byte(b'\n', self.pos)
            .map_or(self.bytes.len(), |p| p + 1);
    }

    fn block_comment(&mut self) {
        let nested = self.dialect == Dialect::Postgres;
        // MySQL runs `/*! ... */` as code; it is still one comment-shaped token here.
        let mut level = 0u32;
        while self.pos < self.bytes.len() {
            if self.bytes[self.pos..].starts_with(b"/*") {
                if level == 0 || nested {
                    level += 1;
                }
                self.pos += 2;
            } else if self.bytes[self.pos..].starts_with(b"*/") {
                self.pos += 2;
                level = level.saturating_sub(1);
                if level == 0 {
                    return;
                }
            } else {
                self.pos += 1;
            }
        }
    }

    /// `$tag$ ... $tag$`, or a lone `$` such as a `$1` parameter.
    fn dollar(&mut self) {
        let open_start = self.pos;
        let mut end = self.pos + 1;
        while end < self.bytes.len() && is_word_byte(self.bytes[end]) && self.bytes[end] != b'$' {
            end += 1;
        }
        let tag_is_valid = end < self.bytes.len()
            && self.bytes[end] == b'$'
            && !self
                .bytes
                .get(open_start + 1)
                .is_some_and(|b| b.is_ascii_digit());
        if !tag_is_valid {
            self.pos += 1;
            return;
        }
        let tag = &self.bytes[open_start..=end];
        let body = end + 1;
        self.pos = self.bytes[body..]
            .windows(tag.len())
            .position(|w| w == tag)
            .map_or(self.bytes.len(), |p| body + p + tag.len());
    }

    fn word(&mut self) {
        let start = self.pos;
        while self.pos < self.bytes.len() && is_word_byte(self.bytes[self.pos]) {
            self.pos += 1;
        }
        let word = &self.text[start..self.pos];
        self.words_seen += 1;
        if self.words_seen == 1 {
            self.in_create = word.eq_ignore_ascii_case("CREATE");
        }
        let previous = self.previous_word.take().map(|r| &self.text[r]);
        let is = |w: &str| word.eq_ignore_ascii_case(w);
        let after = |w: &str| previous.is_some_and(|p| p.eq_ignore_ascii_case(w));
        match self.dialect {
            Dialect::Postgres => {
                if (is("ATOMIC") && after("BEGIN")) || (self.depth > 0 && is("CASE")) {
                    self.depth += 1;
                } else if self.depth > 0 && is("END") {
                    self.depth -= 1;
                }
            }
            Dialect::Mysql | Dialect::Sqlite => {
                if (self.in_create && is("BEGIN"))
                    || (self.depth > 0 && is("CASE") && !after("END"))
                {
                    self.depth += 1;
                } else if self.depth > 0 && is("END") {
                    // `END IF` and friends close a construct that was never counted.
                    let next = self.next_word();
                    let uncounted = ["IF", "LOOP", "WHILE", "REPEAT"]
                        .iter()
                        .any(|w| next.eq_ignore_ascii_case(w));
                    if !uncounted {
                        self.depth -= 1;
                    }
                }
            }
        }
        self.previous_word = Some(start..self.pos);
    }

    /// The word after `pos`, past spaces, without moving.
    fn next_word(&self) -> &'a str {
        let text = self.text;
        let mut start = self.pos;
        while start < self.bytes.len() && self.bytes[start].is_ascii_whitespace() {
            start += 1;
        }
        let mut end = start;
        while end < self.bytes.len() && is_word_byte(self.bytes[end]) {
            end += 1;
        }
        text.get(start..end).unwrap_or("")
    }

    /// Handles a MySQL client `DELIMITER xx` line at the start of a statement.
    fn at_delimiter_command(&mut self) -> bool {
        const WORD: &[u8] = b"DELIMITER";
        let rest = &self.bytes[self.pos..];
        let line_start = self.pos == 0 || self.bytes[self.pos - 1] == b'\n';
        if !line_start
            || rest.len() <= WORD.len()
            || !rest[..WORD.len()].eq_ignore_ascii_case(WORD)
            || !matches!(rest[WORD.len()], b' ' | b'\t')
        {
            return false;
        }
        let line_end = self.find_byte(b'\n', self.pos).unwrap_or(self.bytes.len());
        let value = self.text[self.pos + WORD.len()..line_end].trim();
        if !value.is_empty() {
            self.delimiter = value.to_string();
        }
        self.pos = line_end;
        self.start = self.pos;
        true
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(sql: &str, dialect: Dialect) -> Vec<&str> {
        split_statements(sql, dialect)
            .into_iter()
            .map(|r| &sql[r])
            .collect()
    }

    #[test]
    fn splits_simple_scripts() {
        assert_eq!(
            split("select 1; select 2 ;\n\n select 3", Dialect::Postgres),
            vec!["select 1", "select 2", "select 3"]
        );
        assert_eq!(split(" ; ;", Dialect::Sqlite), Vec::<&str>::new());
        assert_eq!(
            split("-- only a comment\n", Dialect::Mysql),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn skips_quotes_and_comments() {
        let sql = "select 'a;b', \"c;d\" -- e;f\n/* g; /* h; */ i; */ ; select 2";
        assert_eq!(
            split(sql, Dialect::Postgres),
            vec![
                "select 'a;b', \"c;d\" -- e;f\n/* g; /* h; */ i; */",
                "select 2"
            ]
        );
        // MySQL comments do not nest, and `#` starts one.
        assert_eq!(
            split("select 1 /* a /* b */ ; # c;\nselect 2", Dialect::Mysql),
            vec!["select 1 /* a /* b */", "# c;\nselect 2"]
        );
        assert_eq!(
            split("select 'it\\'s;'; select `a;b`", Dialect::Mysql),
            vec!["select 'it\\'s;'", "select `a;b`"]
        );
        assert_eq!(
            split(
                "select E'x\\';y'; select 'a\\'; select 3",
                Dialect::Postgres
            ),
            vec!["select E'x\\';y'", "select 'a\\'", "select 3"]
        );
        assert_eq!(
            split("select [a;b] from t; select 2", Dialect::Sqlite),
            vec!["select [a;b] from t", "select 2"]
        );
    }

    #[test]
    fn keeps_dollar_quoted_bodies() {
        let sql = "create function f() returns int as $body$ select 1; $body$ language sql; select $1, $$a;b$$";
        assert_eq!(
            split(sql, Dialect::Postgres),
            vec![
                "create function f() returns int as $body$ select 1; $body$ language sql",
                "select $1, $$a;b$$"
            ]
        );
    }

    #[test]
    fn keeps_routine_bodies() {
        let pg = "create function f() returns int begin atomic select case when true then 1 end; select 2; end; select 3";
        assert_eq!(split(pg, Dialect::Postgres).len(), 2);

        let lite = "create trigger t after insert on a begin insert into b values (1); update c set x = case when 1 then 2 end; end; select 1";
        assert_eq!(split(lite, Dialect::Sqlite).len(), 2);

        let my = "create procedure p() begin if 1 then select 1; end if; while 0 do select 2; end while; end; select 3";
        assert_eq!(
            split(my, Dialect::Mysql),
            vec![
                "create procedure p() begin if 1 then select 1; end if; while 0 do select 2; end while; end",
                "select 3"
            ]
        );
        // A plain BEGIN starts a transaction and ends at its semicolon.
        assert_eq!(split("begin; select 1; commit", Dialect::Mysql).len(), 3);
    }

    #[test]
    fn honours_mysql_delimiter_lines() {
        let sql =
            "DELIMITER //\ncreate procedure p() begin select 1; end //\nDELIMITER ;\nselect 2;";
        assert_eq!(
            split(sql, Dialect::Mysql),
            vec!["create procedure p() begin select 1; end", "select 2"]
        );
    }

    #[test]
    fn never_panics() {
        const PIECES: &[&str] = &[
            "'",
            "\"",
            "`",
            "[",
            "]",
            "$",
            "$a$",
            "$1",
            "--",
            "#",
            "/*",
            "*/",
            ";",
            "\n",
            "E",
            "begin",
            "atomic",
            "end",
            "case",
            "create",
            "DELIMITER ",
            "if",
            "é",
            "\\",
            "x",
            " ",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for round in 0..5000 {
            let mut sql = String::new();
            for _ in 0..(round % 24) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                sql.push_str(PIECES[(seed % PIECES.len() as u64) as usize]);
            }
            for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
                for range in split_statements(&sql, dialect) {
                    assert!(range.start <= range.end && range.end <= sql.len());
                    assert!(sql.is_char_boundary(range.start) && sql.is_char_boundary(range.end));
                }
            }
        }
    }
}
