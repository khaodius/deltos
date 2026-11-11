//! Tab-separated text the way spreadsheets put cells on the clipboard: fields split by
//! tabs, rows by line breaks, and a field that holds a tab, a line break or a quote
//! wrapped in double quotes, its quotes doubled.

/// Appends `field` to `out`, quoted when it needs to be.
pub fn push_field(out: &mut String, field: &str) {
    if field.contains(['\t', '\n', '\r', '"']) {
        out.push('"');
        for ch in field.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    } else {
        out.push_str(field);
    }
}

/// Rows of fields as one text, rows on their own lines, without a line break at the end.
#[cfg(test)]
pub fn encode<S: AsRef<str>>(rows: &[Vec<S>]) -> String {
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for (j, field) in row.iter().enumerate() {
            if j > 0 {
                out.push('\t');
            }
            push_field(&mut out, field.as_ref());
        }
    }
    out
}

/// The rows of `text`, at most `max_rows` of them, and whether there were more.
///
/// Rows end at `\n`, `\r\n` or a lone `\r`, and one line break at the very end is not an
/// extra row. A field that starts with a quote runs to the matching quote, with `""` for
/// a quote inside; when the closing quote is missing, or something other than a tab or a
/// line break follows it, the field is read as it stands instead.
pub fn decode(text: &str, max_rows: usize) -> (Vec<Vec<String>>, bool) {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut rest = text;
    if rest.is_empty() {
        return (rows, false);
    }
    loop {
        let (field, after) = field(rest);
        row.push(field);
        match after.chars().next() {
            Some('\t') => rest = &after[1..],
            None => {
                rows.push(row);
                return (rows, false);
            }
            Some(_) => {
                // A line break: \r\n, \n or \r.
                let skip = if after.starts_with("\r\n") { 2 } else { 1 };
                rest = &after[skip..];
                rows.push(std::mem::take(&mut row));
                if rest.is_empty() {
                    return (rows, false);
                }
                if rows.len() == max_rows {
                    return (rows, true);
                }
            }
        }
    }
}

/// One field from the start of `text`, and what follows it (a tab, a line break, or
/// nothing).
fn field(text: &str) -> (String, &str) {
    if let Some(quoted) = text.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = quoted.char_indices();
        while let Some((i, ch)) = chars.next() {
            if ch != '"' {
                out.push(ch);
                continue;
            }
            let after = &quoted[i + 1..];
            if after.starts_with('"') {
                out.push('"');
                chars.next();
                continue;
            }
            if after.is_empty() || after.starts_with(['\t', '\n', '\r']) {
                return (out, after);
            }
            break;
        }
    }
    let end = text.find(['\t', '\n', '\r']).unwrap_or(text.len());
    (text[..end].to_owned(), &text[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<Vec<String>> {
        decode(text, usize::MAX).0
    }

    #[test]
    fn plain_fields_and_rows() {
        assert_eq!(rows("a\tb\nc\td"), [["a", "b"], ["c", "d"]]);
        assert_eq!(rows("a\tb\n"), [["a", "b"]]);
        assert_eq!(rows("a\t\tc"), [["a", "", "c"]]);
        assert_eq!(rows("\t"), [["", ""]]);
        assert_eq!(rows("x\n\ny"), [vec!["x"], vec![""], vec!["y"]]);
        assert!(rows("").is_empty());
        assert_eq!(rows("\n"), [[""]]);
    }

    #[test]
    fn crlf_and_lone_cr_end_rows() {
        assert_eq!(rows("a\tb\r\nc\td\r\n"), [["a", "b"], ["c", "d"]]);
        assert_eq!(rows("a\rb"), [["a"], ["b"]]);
    }

    #[test]
    fn quoted_fields_hold_tabs_line_breaks_and_quotes() {
        assert_eq!(
            rows("\"one\ntwo\"\t\"a\tb\"\n\"say \"\"hi\"\"\"\tz"),
            [["one\ntwo", "a\tb"], ["say \"hi\"", "z"]]
        );
        assert_eq!(rows("\"line\r\nbreak\"\r\n"), [["line\r\nbreak"]]);
        assert_eq!(rows("\"\"\tb"), [["", "b"]]);
    }

    #[test]
    fn stray_quotes_are_read_as_written() {
        // Quotes inside a field that does not start with one are plain text.
        assert_eq!(rows("5\" disk\tb"), [["5\" disk", "b"]]);
        // A closing quote followed by more text: the field as it stands.
        assert_eq!(rows("\"ab\"cd\te"), [["\"ab\"cd", "e"]]);
        // No closing quote: up to the next tab or line break.
        assert_eq!(rows("\"open\tx\ny"), [vec!["\"open", "x"], vec!["y"]]);
    }

    #[test]
    fn encoding_quotes_only_what_needs_it() {
        assert_eq!(encode(&[vec!["a", "b c"], vec!["", "d"]]), "a\tb c\n\td");
        assert_eq!(
            encode(&[vec!["tab\there", "new\nline", "say \"hi\"", "cr\r"]]),
            "\"tab\there\"\t\"new\nline\"\t\"say \"\"hi\"\"\"\t\"cr\r\""
        );
    }

    #[test]
    fn round_trip() {
        let table = vec![
            vec!["id", "note", "json"],
            vec!["1", "plain", "{\"a\": [1, 2]}"],
            vec!["2", "two\nlines", ""],
            vec!["3", "tab\tand \"quote\"", "\r\n"],
            vec!["4", "\"starts quoted\"", "ends \""],
            vec!["5", "ünïcödé ✓", " spaces "],
        ];
        let text = encode(&table);
        assert_eq!(rows(&text), table);
        // A trailing line break, as spreadsheets add, changes nothing.
        assert_eq!(rows(&format!("{text}\n")), table);
        assert_eq!(rows(&text.replace('\n', "\r\n")).len(), table.len());
    }

    #[test]
    fn stops_at_max_rows() {
        let (r, more) = decode("a\nb\nc\n", 2);
        assert_eq!(r, [["a"], ["b"]]);
        assert!(more);
        let (r, more) = decode("a\nb\n", 2);
        assert_eq!(r.len(), 2);
        assert!(!more);
    }
}
