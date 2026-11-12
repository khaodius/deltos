//! Checks on a raw `WHERE` condition from the filter bar. The condition is SQL used as
//! written, in parentheses after the filter bar's own conditions and before the keyset
//! seek, so it must stay one condition: a semicolon outside its strings and comments would
//! end the statement and start another, and a parenthesis without its pair would pull the
//! conditions around it, the seek among them, into its own `OR`.
//!
//! The reading is cautious. Where a server setting changes how strings are read (MySQL's
//! `NO_BACKSLASH_ESCAPES`, PostgreSQL's `standard_conforming_strings`), a semicolon passes
//! only if it is inside a string either way. MySQL runs the text of `/*! ... */` comments,
//! so that text is read as code.

use deltos_data::{Error, Result};

use super::Dialect;

/// Refuses a raw condition that could end the statement or leave its parentheses.
pub fn check_condition(dialect: Dialect, raw: &str) -> Result<()> {
    // The default reading: MySQL escapes with backslashes, PostgreSQL does not outside
    // `E'...'` strings.
    let usual = scan(raw, dialect, dialect == Dialect::Mysql);
    let other = match dialect {
        Dialect::Sqlite => usual,
        Dialect::Mysql => scan(raw, dialect, false),
        Dialect::Postgres => scan(raw, dialect, true),
    };
    if usual.semicolon || other.semicolon {
        return Err(Error::Query(
            "The condition has a semicolon outside quotes. It must be one condition, not \
             statements."
                .to_string(),
        ));
    }
    if !usual.balanced {
        return Err(Error::Query(
            "The condition's parentheses do not pair up.".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct Scan {
    semicolon: bool,
    balanced: bool,
}

/// Reads `sql` for semicolons and parentheses outside strings, quoted names and comments.
/// `backslash` says whether a backslash escapes the next character in plain strings.
fn scan(sql: &str, dialect: Dialect, backslash: bool) -> Scan {
    let b = sql.as_bytes();
    let mut i = 0;
    let mut depth: i64 = 0;
    let mut balanced = true;
    while i < b.len() {
        let next = b.get(i + 1).copied();
        match b[i] {
            b';' => {
                return Scan {
                    semicolon: true,
                    balanced,
                };
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    balanced = false;
                }
            }
            b'\'' => {
                let escapes = backslash || (dialect == Dialect::Postgres && e_prefix(b, i));
                i = skip_quoted(b, i, b'\'', escapes);
                continue;
            }
            b'"' => {
                i = skip_quoted(b, i, b'"', dialect == Dialect::Mysql && backslash);
                continue;
            }
            b'`' if dialect != Dialect::Postgres => {
                i = skip_quoted(b, i, b'`', false);
                continue;
            }
            b'[' if dialect == Dialect::Sqlite => {
                i = find(b, i + 1, b']').map_or(b.len(), |p| p + 1);
                continue;
            }
            // MySQL takes `--` as a comment only before a space or a control character.
            b'-' if next == Some(b'-')
                && (dialect != Dialect::Mysql
                    || b.get(i + 2)
                        .is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control())) =>
            {
                i = find(b, i, b'\n').map_or(b.len(), |p| p + 1);
                continue;
            }
            b'#' if dialect == Dialect::Mysql => {
                i = find(b, i, b'\n').map_or(b.len(), |p| p + 1);
                continue;
            }
            b'/' if next == Some(b'*') => {
                if dialect == Dialect::Mysql && matches!(b.get(i + 2), Some(b'!' | b'+')) {
                    // Run by the server: read on as code.
                    i += 3;
                    continue;
                }
                i = skip_block_comment(b, i, dialect == Dialect::Postgres);
                continue;
            }
            b'$' if dialect == Dialect::Postgres && (i == 0 || !is_word(b[i - 1])) => {
                if let Some(end) = skip_dollar(b, i) {
                    i = end;
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
    Scan {
        semicolon: false,
        balanced: balanced && depth == 0,
    }
}

fn find(b: &[u8], from: usize, byte: u8) -> Option<usize> {
    b.get(from..)?
        .iter()
        .position(|c| *c == byte)
        .map(|p| from + p)
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// Whether the quote at `at` opens a PostgreSQL `E'...'` string.
fn e_prefix(b: &[u8], at: usize) -> bool {
    at > 0 && matches!(b[at - 1], b'e' | b'E') && (at < 2 || !is_word(b[at - 2]))
}

/// The position after the string or name opened at `at`. A doubled quote stays inside.
fn skip_quoted(b: &[u8], at: usize, quote: u8, escapes: bool) -> usize {
    let mut i = at + 1;
    while i < b.len() {
        let c = b[i];
        if escapes && c == b'\\' {
            i += 2;
            continue;
        }
        i += 1;
        if c == quote {
            if b.get(i) == Some(&quote) {
                i += 1;
                continue;
            }
            return i;
        }
    }
    b.len()
}

/// The position after the `/* */` comment opened at `at`, nested where `nests`.
fn skip_block_comment(b: &[u8], at: usize, nests: bool) -> usize {
    let mut depth = 0u32;
    let mut i = at;
    while i + 1 < b.len() {
        if b[i] == b'/' && b[i + 1] == b'*' && (nests || depth == 0) {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && b[i + 1] == b'/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// The position after the PostgreSQL `$tag$...$tag$` string opened at `at`, or `None`
/// when no tag starts there, as in a `$1` parameter.
fn skip_dollar(b: &[u8], at: usize) -> Option<usize> {
    let mut end = at + 1;
    while let Some(&c) = b.get(end) {
        if c == b'$' {
            break;
        }
        let first = end == at + 1;
        if !(c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 || (!first && c.is_ascii_digit())) {
            return None;
        }
        end += 1;
    }
    b.get(end)?;
    let tag = &b[at..=end];
    let body = end + 1;
    Some(
        b.get(body..)
            .and_then(|rest| rest.windows(tag.len()).position(|w| w == tag))
            .map_or(b.len(), |p| body + p + tag.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(dialect: Dialect, raw: &str) -> bool {
        check_condition(dialect, raw).is_ok()
    }

    #[test]
    fn one_condition_passes() {
        for d in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
            assert!(ok(d, "status = 'open' AND total > 100"), "{d:?}");
            assert!(ok(d, "name = 'a;b' AND (x IN (1, 2))"), "{d:?}");
            assert!(ok(d, "x = ')' -- trailing; comment"), "{d:?}");
            assert!(ok(d, "x = 1 /* ; ( */"), "{d:?}");
            assert!(ok(d, "\"odd;name\" = 1"), "{d:?}");
            assert!(ok(d, ""), "{d:?}");
        }
        assert!(ok(Dialect::Postgres, "body = $$a;b)$$ AND n = $1"));
        assert!(ok(Dialect::Postgres, "body = $t$ $$ ; $t$"));
        assert!(ok(Dialect::Postgres, "x = E'it\\'s; fine'"));
        assert!(ok(Dialect::Postgres, "x = 1 /* a /* b */ ; */"));
        assert!(ok(Dialect::Mysql, "`a;b` = 1 # note; here"));
        assert!(ok(Dialect::Sqlite, "[a;b] = 1"));
    }

    #[test]
    fn statements_are_refused() {
        for d in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
            assert!(!ok(d, "1 = 1; DROP TABLE t"), "{d:?}");
            assert!(!ok(d, "x = 1;"), "{d:?}");
            assert!(!ok(d, "x = 'a'; DELETE FROM t WHERE 'b' = 'b'"), "{d:?}");
        }
        // Read either way the server might, the semicolon is outside the string once.
        assert!(!ok(
            Dialect::Mysql,
            "name = 'x\\' OR 1 = 1; DROP TABLE t; -- '"
        ));
        assert!(!ok(Dialect::Postgres, "name = 'x\\'; SELECT 1; --'"));
        // MySQL runs what is in `/*! */`, and `--` needs a space after it there.
        assert!(!ok(Dialect::Mysql, "x = 1 /*! ; DROP TABLE t */"));
        assert!(!ok(Dialect::Mysql, "x = 1 --; DROP TABLE t"));
        assert!(ok(Dialect::Postgres, "x = 1 --; not a statement"));
    }

    #[test]
    fn parentheses_must_pair() {
        for d in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
            assert!(!ok(d, "a = 1) OR (b = 2"), "{d:?}");
            assert!(!ok(d, "(a = 1"), "{d:?}");
            assert!(!ok(d, "a = 1)"), "{d:?}");
            assert!(ok(d, "(a = 1) OR (b = '(')"), "{d:?}");
        }
    }

    #[test]
    fn never_panics() {
        const PIECES: &[&str] = &[
            "'", "\"", "`", "[", "]", "\\", "$", "$a", "E", "--", "-", "#", "/*", "*/", "/*!", "(",
            ")", ";", " ", "\n", "é", "1",
        ];
        let mut seed = 11_u64;
        for _ in 0..5000 {
            let mut text = String::new();
            for _ in 0..(seed % 9) {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                text.push_str(PIECES[(seed >> 33) as usize % PIECES.len()]);
            }
            for d in [Dialect::Postgres, Dialect::Mysql, Dialect::Sqlite] {
                let _ = check_condition(d, &text);
            }
            seed = seed.wrapping_add(17);
        }
    }
}
