//! A forgiving SQL tokenizer for the editor. It knows each dialect's strings, quoted
//! names and comments (the same set as the statement splitter in `deltos-drivers`), never
//! fails, and runs an unterminated string or comment to the end of the text, marking it so
//! the completion popup stays quiet inside it.

use std::ops::Range;

use super::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Space,
    LineComment,
    BlockComment,
    /// `'...'`, `E'...'` and `$tag$...$tag$`.
    String,
    /// `"..."`, `` `...` `` and SQLite's `[...]`.
    Quoted,
    Number,
    /// A keyword or a plain name.
    Word,
    /// `$1`, `?`, `:name`, `@var`.
    Param,
    /// An operator or punctuation: `(`, `,`, `::`, `<=`, `->>`.
    Punct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: Kind,
    pub range: Range<usize>,
    /// A string, quoted name or block comment that reached the end of the text.
    pub open: bool,
}

impl Token {
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        source.get(self.range.clone()).unwrap_or("")
    }

    /// Whether the token carries meaning, as opposed to space and comments.
    pub fn is_code(&self) -> bool {
        !matches!(
            self.kind,
            Kind::Space | Kind::LineComment | Kind::BlockComment
        )
    }
}

/// Multi-character operators, longest first.
const OPERATORS: &[&str] = &[
    "->>", "#>>", "<=>", "::", "<=", ">=", "<>", "!=", "||", "->", "#>", "@>", "<@", ":=", "<<",
    ">>", "&&", "=>",
];

pub fn tokens(text: &str, dialect: Dialect) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        let mut open = false;
        let kind = match b {
            b if b.is_ascii_whitespace() => {
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                Kind::Space
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = line_end(bytes, i);
                Kind::LineComment
            }
            b'#' if dialect == Dialect::Mysql => {
                i = line_end(bytes, i);
                Kind::LineComment
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let (end, closed) = block_comment_end(bytes, i, dialect == Dialect::Postgres);
                i = end;
                open = !closed;
                Kind::BlockComment
            }
            b'\'' => {
                let backslash = dialect == Dialect::Mysql;
                let (end, closed) = quoted_end(bytes, i, b'\'', backslash);
                i = end;
                open = !closed;
                Kind::String
            }
            b'e' | b'E'
                if dialect == Dialect::Postgres
                    && bytes.get(i + 1) == Some(&b'\'')
                    && !prev_is_word(bytes, i) =>
            {
                let (end, closed) = quoted_end(bytes, i + 1, b'\'', true);
                i = end;
                open = !closed;
                Kind::String
            }
            // `X'ff'`, `B'01'`, `N'text'`: one literal, which a space would split.
            b'x' | b'X' | b'b' | b'B' | b'n' | b'N'
                if bytes.get(i + 1) == Some(&b'\'') && !prev_is_word(bytes, i) =>
            {
                let (end, closed) = quoted_end(bytes, i + 1, b'\'', dialect == Dialect::Mysql);
                i = end;
                open = !closed;
                Kind::String
            }
            // PostgreSQL `U&'d\0061t'` and `U&"d\0061t"`.
            b'u' | b'U'
                if dialect == Dialect::Postgres
                    && bytes.get(i + 1) == Some(&b'&')
                    && matches!(bytes.get(i + 2), Some(b'\'' | b'"'))
                    && !prev_is_word(bytes, i) =>
            {
                let quote = bytes[i + 2];
                let (end, closed) = quoted_end(bytes, i + 2, quote, false);
                i = end;
                open = !closed;
                if quote == b'"' {
                    Kind::Quoted
                } else {
                    Kind::String
                }
            }
            b'"' => {
                let (end, closed) = quoted_end(bytes, i, b'"', dialect == Dialect::Mysql);
                i = end;
                open = !closed;
                if dialect == Dialect::Mysql {
                    Kind::String
                } else {
                    Kind::Quoted
                }
            }
            b'`' if dialect != Dialect::Postgres => {
                let (end, closed) = quoted_end(bytes, i, b'`', false);
                i = end;
                open = !closed;
                Kind::Quoted
            }
            b'[' if dialect == Dialect::Sqlite => {
                match bytes[i + 1..].iter().position(|c| *c == b']') {
                    Some(p) => i += p + 2,
                    None => {
                        i = bytes.len();
                        open = true;
                    }
                }
                Kind::Quoted
            }
            b'$' if dialect == Dialect::Postgres => match dollar_tag(bytes, i) {
                Some(tag_end) => {
                    let tag = &bytes[i..tag_end];
                    match find(bytes, tag, tag_end) {
                        Some(p) => i = p + tag.len(),
                        None => {
                            i = bytes.len();
                            open = true;
                        }
                    }
                    Kind::String
                }
                None => {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                    Kind::Param
                }
            },
            b'?' => {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                Kind::Param
            }
            // PostgreSQL has no `@name`; there `@` is an operator, as in `@e'..'`.
            b'@' | b':'
                if bytes.get(i + 1).is_some_and(|c| is_word_start(*c))
                    && !(b == b'@' && dialect == Dialect::Postgres)
                    && !(b == b':' && bytes.get(i.wrapping_sub(1)) == Some(&b':')) =>
            {
                i += 1;
                while i < bytes.len() && is_word_byte(bytes[i]) {
                    i += 1;
                }
                Kind::Param
            }
            b if b.is_ascii_digit()
                || (b == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                i += 1;
                while i < bytes.len() {
                    let c = bytes[i];
                    let exponent_sign =
                        (c == b'+' || c == b'-') && matches!(bytes[i - 1], b'e' | b'E');
                    // Letters and `$` run on, as in a name: `1set$$` is no dollar quote.
                    if is_word_byte(c) || c == b'.' || exponent_sign {
                        i += 1;
                    } else {
                        break;
                    }
                }
                Kind::Number
            }
            b if is_word_start(b) => {
                i += char_len(text, i);
                while i < bytes.len() && is_word_byte(bytes[i]) {
                    i += char_len(text, i);
                }
                Kind::Word
            }
            _ => {
                let rest = &text[i..];
                let len = OPERATORS
                    .iter()
                    .find(|op| rest.starts_with(**op))
                    .map_or_else(|| char_len(text, i), |op| op.len());
                i += len;
                Kind::Punct
            }
        };
        let end = i.min(bytes.len());
        out.push(Token {
            kind,
            range: start..end,
            open,
        });
        i = end;
    }
    out
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|c| *c == b'\n')
        .map_or(bytes.len(), |p| from + p)
}

/// The end of a block comment that starts at `from`, and whether it was closed.
fn block_comment_end(bytes: &[u8], from: usize, nested: bool) -> (usize, bool) {
    let mut depth = 0u32;
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            if depth == 0 || nested {
                depth += 1;
            }
            i += 2;
        } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth = depth.saturating_sub(1);
            i += 2;
            if depth == 0 {
                return (i, true);
            }
        } else {
            i += 1;
        }
    }
    (bytes.len(), false)
}

/// The end of a quoted run starting with `quote` at `from`. A doubled quote stays inside;
/// with `backslash`, so does an escaped one.
fn quoted_end(bytes: &[u8], from: usize, quote: u8, backslash: bool) -> (usize, bool) {
    let mut i = from + 1;
    while i < bytes.len() {
        let c = bytes[i];
        if backslash && c == b'\\' {
            i += 2;
            continue;
        }
        if c == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return (i + 1, true);
        }
        i += 1;
    }
    (bytes.len(), false)
}

/// The end of a `$tag$` opening at `from`, if there is one there.
fn dollar_tag(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from + 1;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'$' {
            return Some(i + 1);
        }
        let ok = if i == from + 1 {
            c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
        } else {
            is_word_byte(c)
        };
        if !ok {
            return None;
        }
        i += 1;
    }
    None
}

fn find(bytes: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > bytes.len() {
        return None;
    }
    bytes[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| from + p)
}

fn prev_is_word(bytes: &[u8], at: usize) -> bool {
    at > 0 && is_word_byte(bytes[at - 1])
}

pub fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

pub fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn char_len(text: &str, at: usize) -> usize {
    text[at..].chars().next().map_or(1, char::len_utf8)
}

/// A quoted name without its quotes, with doubled quotes made single.
pub fn unquote(text: &str) -> String {
    let mut chars = text.chars();
    let (open, close) = match chars.next() {
        Some('"') => ('"', '"'),
        Some('`') => ('`', '`'),
        Some('[') => ('[', ']'),
        _ => return text.to_owned(),
    };
    let inner = text
        .strip_prefix(open)
        .map(|t| t.strip_suffix(close).unwrap_or(t))
        .unwrap_or(text);
    let doubled: String = [close, close].iter().collect();
    inner.replace(&doubled, &close.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str, dialect: Dialect) -> Vec<(Kind, &str)> {
        tokens(text, dialect)
            .into_iter()
            .filter(|t| t.kind != Kind::Space)
            .map(|t| (t.kind, &text[t.range]))
            .collect()
    }

    #[test]
    fn reads_every_kind_of_token() {
        let text =
            "SELECT a.\"B c\", 'it''s', 1.5e-3, $1 FROM t -- note\n/* x */ WHERE a::text <= ?";
        assert_eq!(
            kinds(text, Dialect::Postgres),
            vec![
                (Kind::Word, "SELECT"),
                (Kind::Word, "a"),
                (Kind::Punct, "."),
                (Kind::Quoted, "\"B c\""),
                (Kind::Punct, ","),
                (Kind::String, "'it''s'"),
                (Kind::Punct, ","),
                (Kind::Number, "1.5e-3"),
                (Kind::Punct, ","),
                (Kind::Param, "$1"),
                (Kind::Word, "FROM"),
                (Kind::Word, "t"),
                (Kind::LineComment, "-- note"),
                (Kind::BlockComment, "/* x */"),
                (Kind::Word, "WHERE"),
                (Kind::Word, "a"),
                (Kind::Punct, "::"),
                (Kind::Word, "text"),
                (Kind::Punct, "<="),
                (Kind::Param, "?"),
            ]
        );
    }

    #[test]
    fn knows_dialect_quotes() {
        assert_eq!(
            kinds("$fn$ a ; b $fn$ `x`", Dialect::Postgres)[0],
            (Kind::String, "$fn$ a ; b $fn$")
        );
        assert_eq!(
            kinds("select `a b`, 'x\\'y' # c", Dialect::Mysql),
            vec![
                (Kind::Word, "select"),
                (Kind::Quoted, "`a b`"),
                (Kind::Punct, ","),
                (Kind::String, "'x\\'y'"),
                (Kind::LineComment, "# c"),
            ]
        );
        assert_eq!(
            kinds("[my table]", Dialect::Sqlite),
            vec![(Kind::Quoted, "[my table]")]
        );
        assert_eq!(
            kinds(
                "x'ff' B'01' n'é' U&'d\\0061' U&\"x\" max'",
                Dialect::Postgres
            ),
            vec![
                (Kind::String, "x'ff'"),
                (Kind::String, "B'01'"),
                (Kind::String, "n'é'"),
                (Kind::String, "U&'d\\0061'"),
                (Kind::Quoted, "U&\"x\""),
                (Kind::Word, "max"),
                (Kind::String, "'"),
            ]
        );
        assert_eq!(unquote("\"a\"\"b\""), "a\"b");
        assert_eq!(unquote("[x y]"), "x y");
    }

    #[test]
    fn unterminated_runs_to_the_end() {
        let t = tokens("select 'abc", Dialect::Postgres);
        let last = t.last().unwrap();
        assert_eq!(last.kind, Kind::String);
        assert!(last.open);
        let t = tokens("/* /* nested */ still", Dialect::Postgres);
        assert!(t[0].open);
    }

    #[test]
    fn never_panics() {
        for text in [
            "", "'", "\"", "$", "$$", "$a", "e'", "/*", "-", "[", "`", "é", "1e", "::", "@",
        ] {
            for d in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
                let t = tokens(text, d);
                let covered: usize = t.iter().map(|t| t.range.len()).sum();
                assert_eq!(covered, text.len(), "{text:?}");
            }
        }
    }
}
