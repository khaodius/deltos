//! Just enough XML for the connection files other tools write: elements, attributes,
//! text, CDATA and the five named entities plus numeric ones. Comments, processing
//! instructions and the doctype are skipped. It never panics; malformed input is an
//! error that says where.

/// An element with its attributes and children.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Element(Element),
    Text(String),
}

/// Nesting deeper than this is refused rather than risking the stack.
const MAX_DEPTH: usize = 200;

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Child elements, in order.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.elements().find(|e| e.name == name)
    }

    /// Every element named `name` at any depth below this one, in document order.
    pub fn descendants<'a>(&'a self, name: &'a str) -> Vec<&'a Element> {
        let mut out = Vec::new();
        let mut stack: Vec<&Element> = vec![self];
        while let Some(e) = stack.pop() {
            for child in e.elements().collect::<Vec<_>>().into_iter().rev() {
                stack.push(child);
            }
            if !std::ptr::eq(e, self) && e.name == name {
                out.push(e);
            }
        }
        out
    }

    /// The element's text, trimmed.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for n in &self.children {
            if let Node::Text(t) = n {
                out.push_str(t);
            }
        }
        out.trim().to_owned()
    }

    /// The trimmed text of the child `name`, when it has any.
    pub fn child_text(&self, name: &str) -> Option<String> {
        self.child(name)
            .map(Element::text)
            .filter(|t| !t.is_empty())
    }
}

/// Parses a document and returns its root element.
pub fn parse(text: &str) -> Result<Element, String> {
    let mut p = Parser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.skip_misc()?;
    let root = p.element(0)?;
    p.skip_misc()?;
    if p.pos < p.bytes.len() {
        return Err(p.error("text after the root element"));
    }
    Ok(root)
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, what: &str) -> String {
        let line = self
            .text
            .get(..self.pos)
            .map_or(1, |t| t.matches('\n').count() + 1);
        format!("not valid XML: {what} (line {line})")
    }

    fn starts_with(&self, s: &str) -> bool {
        self.bytes
            .get(self.pos..)
            .is_some_and(|b| b.starts_with(s.as_bytes()))
    }

    fn skip_until(&mut self, end: &str) -> Result<(), String> {
        match self.text.get(self.pos..).and_then(|t| t.find(end)) {
            Some(at) => {
                self.pos += at + end.len();
                Ok(())
            }
            None => Err(self.error(&format!("missing {end}"))),
        }
    }

    fn skip_space(&mut self) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }

    /// Whitespace, comments, processing instructions and the doctype between elements.
    fn skip_misc(&mut self) -> Result<(), String> {
        loop {
            self.skip_space();
            if self.starts_with("\u{feff}") {
                self.pos += 3;
            } else if self.starts_with("<?") {
                self.skip_until("?>")?;
            } else if self.starts_with("<!--") {
                self.skip_until("-->")?;
            } else if self.starts_with("<!DOCTYPE") || self.starts_with("<!doctype") {
                self.skip_until(">")?;
            } else {
                return Ok(());
            }
        }
    }

    fn name(&mut self) -> Result<String, String> {
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':') || *b >= 0x80
        }) {
            self.pos += 1;
        }
        match self.text.get(start..self.pos) {
            Some(n) if !n.is_empty() => Ok(n.to_owned()),
            _ => Err(self.error("expected a name")),
        }
    }

    fn element(&mut self, depth: usize) -> Result<Element, String> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        if self.bytes.get(self.pos) != Some(&b'<') {
            return Err(self.error("expected an element"));
        }
        self.pos += 1;
        let name = self.name()?;
        let mut element = Element {
            name,
            ..Element::default()
        };
        loop {
            self.skip_space();
            match self.bytes.get(self.pos) {
                Some(b'/') if self.bytes.get(self.pos + 1) == Some(&b'>') => {
                    self.pos += 2;
                    return Ok(element);
                }
                Some(b'>') => {
                    self.pos += 1;
                    break;
                }
                Some(_) => {
                    let key = self.name()?;
                    self.skip_space();
                    if self.bytes.get(self.pos) != Some(&b'=') {
                        return Err(self.error("expected = after an attribute name"));
                    }
                    self.pos += 1;
                    self.skip_space();
                    let quote = match self.bytes.get(self.pos) {
                        Some(q @ (b'"' | b'\'')) => *q,
                        _ => return Err(self.error("expected a quoted attribute value")),
                    };
                    self.pos += 1;
                    let start = self.pos;
                    while self.bytes.get(self.pos).is_some_and(|b| *b != quote) {
                        self.pos += 1;
                    }
                    let raw = self.text.get(start..self.pos).unwrap_or("");
                    if self.pos >= self.bytes.len() {
                        return Err(self.error("unterminated attribute value"));
                    }
                    self.pos += 1;
                    element.attrs.push((key, unescape(raw)));
                }
                None => return Err(self.error("unterminated tag")),
            }
        }
        // Content up to the closing tag.
        loop {
            if self.starts_with("</") {
                self.pos += 2;
                let close = self.name()?;
                if close != element.name {
                    return Err(self.error(&format!("</{close}> closes <{}>", element.name)));
                }
                self.skip_space();
                if self.bytes.get(self.pos) != Some(&b'>') {
                    return Err(self.error("unterminated closing tag"));
                }
                self.pos += 1;
                return Ok(element);
            } else if self.starts_with("<!--") {
                self.skip_until("-->")?;
            } else if self.starts_with("<![CDATA[") {
                self.pos += "<![CDATA[".len();
                let start = self.pos;
                self.skip_until("]]>")?;
                let text = self.text.get(start..self.pos - 3).unwrap_or("");
                element.children.push(Node::Text(text.to_owned()));
            } else if self.starts_with("<?") {
                self.skip_until("?>")?;
            } else if self.starts_with("<") {
                let child = self.element(depth + 1)?;
                element.children.push(Node::Element(child));
            } else if self.pos >= self.bytes.len() {
                return Err(self.error(&format!("<{}> is never closed", element.name)));
            } else {
                let start = self.pos;
                while self.bytes.get(self.pos).is_some_and(|b| *b != b'<') {
                    self.pos += 1;
                }
                let raw = self.text.get(start..self.pos).unwrap_or("");
                element.children.push(Node::Text(unescape(raw)));
            }
        }
    }
}

/// Replaces entity references; an unknown one is kept as it is.
fn unescape(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|e| *e <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_elements_attributes_and_text() {
        let doc = parse(
            "\u{feff}<?xml version=\"1.0\"?>\n<!DOCTYPE x>\n<!-- c -->\n<a k='1' b=\"x &amp; y\"><b>  hi &lt;there&gt; &#65;&#x42;</b><c/><![CDATA[<raw>]]><!-- d --></a>\n",
        )
        .expect("parses");
        assert_eq!(doc.name, "a");
        assert_eq!(doc.attr("k"), Some("1"));
        assert_eq!(doc.attr("b"), Some("x & y"));
        assert_eq!(doc.child_text("b").as_deref(), Some("hi <there> AB"));
        assert!(doc.child("c").is_some());
        assert_eq!(doc.text(), "<raw>");
        assert_eq!(doc.descendants("c").len(), 1);
    }

    #[test]
    fn refuses_broken_documents() {
        for bad in [
            "",
            "<a>",
            "<a></b>",
            "<a b></a>",
            "<a b='1></a>",
            "<a><b></a>",
            "text",
            "<a/><b/>",
            "<a><!-- </a>",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let deep = "<a>".repeat(500);
        assert!(parse(&deep).is_err());
        assert_eq!(unescape("a &bogus; & b &#xZZ;"), "a &bogus; & b &#xZZ;");
    }
}
