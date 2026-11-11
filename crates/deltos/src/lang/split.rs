//! Where the statements of an editor's text are, and which one the cursor means.

use std::ops::Range;

use super::Language;

/// Byte ranges of the statements in `text`, trimmed and without their terminators. SQL
/// uses the drivers' own splitter, so the editor and the server agree on where each
/// statement ends.
pub fn statement_ranges(text: &str, language: Language) -> Vec<Range<usize>> {
    match language {
        Language::Sql(dialect) => deltos_drivers::sql::split_statements(text, dialect),
        Language::Mongo => mongo_statements(text),
    }
}

/// The statement the cursor at `at` belongs to: the one around it, else the one that
/// ends just before it on the same or the previous line, else the one that starts just
/// after it. A blank line between the cursor and every statement means none.
pub fn statement_at(ranges: &[Range<usize>], text: &str, at: usize) -> Option<Range<usize>> {
    if let Some(r) = ranges.iter().find(|r| r.start <= at && at <= r.end) {
        return Some(r.clone());
    }
    let gap = |a: usize, b: usize| {
        text.get(a.min(b)..a.max(b).min(text.len()))
            .map_or(usize::MAX, |g| g.matches('\n').count())
    };
    let before = ranges.iter().rev().find(|r| r.end <= at);
    if let Some(r) = before
        && gap(r.end, at) <= 1
    {
        return Some(r.clone());
    }
    let after = ranges.iter().find(|r| r.start >= at);
    if let Some(r) = after
        && gap(at, r.start) <= 1
    {
        return Some(r.clone());
    }
    None
}

/// Splits shell statements at semicolons and at a line that starts a new `db.` statement
/// or command document, outside strings, comments and brackets. A line starting with `.`
/// continues the chain above it.
fn mongo_statements(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth: i32 = 0;
    let mut i = 0;
    let push = |out: &mut Vec<Range<usize>>, from: usize, to: usize| {
        if let Some(r) = trimmed(text, from..to) {
            out.push(r);
        }
    };
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = text[i + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |p| i + 2 + p + 2);
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = (depth - 1).max(0),
            b';' if depth == 0 => {
                push(&mut out, start, i);
                start = i + 1;
            }
            b'\n' if depth == 0 => {
                let rest = text[i..].trim_start();
                let next_starts = rest.starts_with('{')
                    || (rest.starts_with("db") && rest[2..].trim_start().starts_with(['.', '[']));
                if next_starts && has_code(&text[start..i]) {
                    push(&mut out, start, i);
                    start = i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    push(&mut out, start, bytes.len().max(start));
    out
}

fn has_code(piece: &str) -> bool {
    piece
        .lines()
        .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with("//"))
}

fn trimmed(text: &str, r: Range<usize>) -> Option<Range<usize>> {
    let piece = text.get(r.clone())?;
    if !has_code(piece) {
        return None;
    }
    let lead = piece.len() - piece.trim_start().len();
    let trail = piece.len() - piece.trim_end().len();
    Some(r.start + lead..r.end - trail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Dialect;

    const PG: Language = Language::Sql(Dialect::Postgres);

    fn at(text: &str, cursor: usize) -> Option<&str> {
        let ranges = statement_ranges(text, PG);
        statement_at(&ranges, text, cursor).map(|r| &text[r])
    }

    #[test]
    fn statements_split_on_semicolons_outside_quotes() {
        let text = "select 1; select ';' as x;\n-- a; comment\nselect 3";
        assert_eq!(at(text, 3), Some("select 1"));
        assert_eq!(at(text, 12), Some("select ';' as x"));
        assert_eq!(at(text, text.len()), Some("-- a; comment\nselect 3"));
    }

    #[test]
    fn cursor_after_a_statement_picks_it() {
        let text = "select 1;\n\n\n\nselect 2;\n";
        // Right after the semicolon, and on the line below it.
        assert_eq!(at(text, 9), Some("select 1"));
        assert_eq!(at(text, 10), Some("select 1"));
        // In the middle of the blank lines: nothing is near enough.
        assert_eq!(at(text, 11), None);
        // On the line just above the second statement.
        assert_eq!(at(text, 12), Some("select 2"));
        assert_eq!(at(text, 13), Some("select 2"));
        assert_eq!(at(text, text.len()), Some("select 2"));
        assert_eq!(at("", 0), None);
    }

    #[test]
    fn mongo_statements_split_on_lines_and_semicolons() {
        let text = "db.a.find({x: ';'})\n  .sort({_id: -1})\ndb.b.count(); db.c.drop()\n{ping: 1}";
        let parts: Vec<&str> = mongo_statements(text)
            .into_iter()
            .map(|r| &text[r])
            .collect();
        assert_eq!(
            parts,
            vec![
                "db.a.find({x: ';'})\n  .sort({_id: -1})",
                "db.b.count()",
                "db.c.drop()",
                "{ping: 1}"
            ]
        );
        assert!(mongo_statements("// only a note\n").is_empty());
        assert_eq!(mongo_statements("db.x.find({\ndb: 1})").len(), 1);
    }

    #[test]
    fn never_panics() {
        for text in ["'", "db", "db\n", "/*", "{", "}", ";;", "é;é", "db.\ndb"] {
            for language in [PG, Language::Mongo] {
                let ranges = statement_ranges(text, language);
                for at in 0..=text.len() {
                    let _ = statement_at(&ranges, text, at);
                }
                for r in ranges {
                    assert!(text.get(r).is_some());
                }
            }
        }
    }
}
