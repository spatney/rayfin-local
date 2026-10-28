//! Input values with positions, read again from the document text.
//!
//! async-graphql-parser keeps positions only down to an argument's value, and collects input
//! objects into maps, so duplicate input fields vanish. DAB rejects them (the SDK emits one for a
//! repeated `.orderBy()` key) and reports nested positions, so argument values are re-read from
//! the source at the position the parser recorded, keeping every object field, in order, with
//! its position.

use async_graphql_parser::Pos;

/// A literal input value.
#[derive(Clone, Debug, PartialEq)]
pub struct PValue {
    pub pos: Pos,
    pub kind: PKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PKind {
    Variable(String),
    Null,
    /// The literal text, e.g. `-12`.
    Int(String),
    /// The literal text, e.g. `1.5e+21`.
    Float(String),
    String(String),
    Boolean(bool),
    Enum(String),
    List(Vec<PValue>),
    /// Every field in document order, duplicates included.
    Object(Vec<PField>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PField {
    pub name: String,
    pub pos: Pos,
    pub value: PValue,
}

impl PValue {
    /// The literal as JSON (variable defaults); `None` when it contains a variable.
    pub fn to_json(&self) -> Option<serde_json::Value> {
        use serde_json::Value as Json;
        Some(match &self.kind {
            PKind::Variable(_) => return None,
            PKind::Null => Json::Null,
            PKind::Int(text) | PKind::Float(text) => Json::Number(text.parse().ok()?),
            PKind::String(text) | PKind::Enum(text) => Json::String(text.clone()),
            PKind::Boolean(value) => Json::Bool(*value),
            PKind::List(items) => {
                Json::Array(items.iter().map(PValue::to_json).collect::<Option<_>>()?)
            }
            PKind::Object(fields) => Json::Object(
                fields
                    .iter()
                    .map(|f| Some((f.name.clone(), f.value.to_json()?)))
                    .collect::<Option<_>>()?,
            ),
        })
    }

    /// The literal as GraphQL text (Hot Chocolate's `argumentValue` extension).
    pub fn to_graphql(&self) -> String {
        match &self.kind {
            PKind::Variable(name) => format!("${name}"),
            PKind::Null => "null".into(),
            PKind::Int(text) | PKind::Float(text) => text.clone(),
            PKind::String(text) => serde_json::to_string(text).unwrap_or_default(),
            PKind::Boolean(value) => value.to_string(),
            PKind::Enum(name) => name.clone(),
            PKind::List(items) => {
                let items: Vec<_> = items.iter().map(PValue::to_graphql).collect();
                format!("[{}]", items.join(", "))
            }
            PKind::Object(fields) => {
                let fields: Vec<_> = fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, f.value.to_graphql()))
                    .collect();
                format!("{{ {} }}", fields.join(", "))
            }
        }
    }
}

/// A GraphQL document's text, addressable by the parser's positions.
pub struct Source<'a> {
    text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Source<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { text, line_starts }
    }

    /// The byte offset of `pos`, counted as async-graphql-parser counts: lines end at `\n`, and
    /// both `\n` and `\r` restart the column at 1.
    fn offset(&self, pos: Pos) -> Option<usize> {
        let start = *self.line_starts.get(pos.line.checked_sub(1)?)?;
        let mut column = 1;
        for (offset, ch) in self.text[start..].char_indices() {
            if column == pos.column {
                return Some(start + offset);
            }
            match ch {
                '\r' => column = 1,
                '\n' => return None,
                _ => column += 1,
            }
        }
        (column == pos.column).then_some(self.text.len())
    }

    /// The value literal that starts at `pos`.
    pub fn value_at(&self, pos: Pos) -> Result<PValue, String> {
        let offset = self
            .offset(pos)
            .ok_or_else(|| format!("no value at {pos}"))?;
        let mut reader = Reader {
            text: self.text,
            at: offset,
            line: pos.line,
            column: pos.column,
            depth: 0,
        };
        reader.value()
    }
}

/// Values nested deeper than this are refused (the parser does not bound them).
pub const MAX_VALUE_DEPTH: usize = 128;

struct Reader<'a> {
    text: &'a str,
    at: usize,
    line: usize,
    column: usize,
    depth: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.at += ch.len_utf8();
        match ch {
            '\r' => self.column = 1,
            '\n' => {
                self.line += 1;
                self.column = 1;
            }
            _ => self.column += 1,
        }
        Some(ch)
    }

    fn pos(&self) -> Pos {
        Pos {
            line: self.line,
            column: self.column,
        }
    }

    fn rest(&self) -> &str {
        &self.text[self.at..]
    }

    fn skip_ignored(&mut self) {
        while let Some(ch) = self.peek() {
            match ch {
                ' ' | '\t' | '\n' | '\r' | ',' | '\u{feff}' => {
                    self.bump();
                }
                '#' => {
                    while let Some(ch) = self.peek() {
                        if ch == '\n' || ch == '\r' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), String> {
        self.skip_ignored();
        match self.bump() {
            Some(ch) if ch == expected => Ok(()),
            other => Err(format!("expected `{expected}`, found {other:?}")),
        }
    }

    fn name(&mut self) -> Result<String, String> {
        let start = self.at;
        while let Some(ch) = self.peek() {
            let ok =
                ch == '_' || ch.is_ascii_alphabetic() || (self.at > start && ch.is_ascii_digit());
            if !ok {
                break;
            }
            self.bump();
        }
        if self.at == start {
            return Err(format!("expected a name at {}", self.pos()));
        }
        Ok(self.text[start..self.at].to_owned())
    }

    fn value(&mut self) -> Result<PValue, String> {
        self.skip_ignored();
        let pos = self.pos();
        let kind = match self.peek().ok_or("unexpected end of document")? {
            '$' => {
                self.bump();
                PKind::Variable(self.name()?)
            }
            '[' => {
                self.nest()?;
                self.bump();
                let mut items = Vec::new();
                loop {
                    self.skip_ignored();
                    if self.peek() == Some(']') {
                        self.bump();
                        break;
                    }
                    items.push(self.value()?);
                }
                self.depth -= 1;
                PKind::List(items)
            }
            '{' => {
                self.nest()?;
                self.bump();
                let mut fields = Vec::new();
                loop {
                    self.skip_ignored();
                    if self.peek() == Some('}') {
                        self.bump();
                        break;
                    }
                    let pos = self.pos();
                    let name = self.name()?;
                    self.expect(':')?;
                    let value = self.value()?;
                    fields.push(PField { name, pos, value });
                }
                self.depth -= 1;
                PKind::Object(fields)
            }
            '"' => PKind::String(self.string()?),
            '-' | '0'..='9' => self.number()?,
            _ => match self.name()?.as_str() {
                "true" => PKind::Boolean(true),
                "false" => PKind::Boolean(false),
                "null" => PKind::Null,
                other => PKind::Enum(other.to_owned()),
            },
        };
        Ok(PValue { pos, kind })
    }

    fn nest(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_VALUE_DEPTH {
            return Err(format!(
                "values nested deeper than {MAX_VALUE_DEPTH} levels"
            ));
        }
        Ok(())
    }

    fn number(&mut self) -> Result<PKind, String> {
        let start = self.at;
        let mut float = false;
        if self.peek() == Some('-') {
            self.bump();
        }
        self.digits();
        if self.peek() == Some('.') {
            float = true;
            self.bump();
            self.digits();
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            float = true;
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            self.digits();
        }
        let text = self.text[start..self.at].to_owned();
        Ok(if float {
            PKind::Float(text)
        } else {
            PKind::Int(text)
        })
    }

    fn digits(&mut self) {
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
        }
    }

    fn string(&mut self) -> Result<String, String> {
        if self.rest().starts_with("\"\"\"") {
            for _ in 0..3 {
                self.bump();
            }
            let start = self.at;
            loop {
                if self.rest().starts_with("\\\"\"\"") {
                    for _ in 0..4 {
                        self.bump();
                    }
                } else if self.rest().starts_with("\"\"\"") {
                    let raw = self.text[start..self.at].replace("\\\"\"\"", "\"\"\"");
                    for _ in 0..3 {
                        self.bump();
                    }
                    return Ok(block_string_value(&raw));
                } else if self.bump().is_none() {
                    return Err("unterminated block string".into());
                }
            }
        }
        self.bump();
        let mut out = String::new();
        loop {
            match self.bump().ok_or("unterminated string")? {
                '"' => return Ok(out),
                '\\' => match self.bump().ok_or("unterminated string")? {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        let first = self.hex4()?;
                        let ch = if (0xD800..0xDC00).contains(&first)
                            && self.rest().starts_with("\\u")
                        {
                            let saved = (self.at, self.line, self.column);
                            self.bump();
                            self.bump();
                            let second = self.hex4()?;
                            if (0xDC00..0xE000).contains(&second) {
                                char::from_u32(
                                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00),
                                )
                            } else {
                                (self.at, self.line, self.column) = saved;
                                None
                            }
                        } else {
                            char::from_u32(first)
                        };
                        out.push(ch.unwrap_or('\u{fffd}'));
                    }
                    other => return Err(format!("invalid escape `\\{other}`")),
                },
                ch => out.push(ch),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits: String = (0..4).filter_map(|_| self.bump()).collect();
        u32::from_str_radix(&digits, 16).map_err(|_| format!("invalid unicode escape `{digits}`"))
    }
}

/// The spec's `BlockStringValue`: common indentation and blank first/last lines removed.
fn block_string_value(raw: &str) -> String {
    let lines: Vec<&str> = raw
        .split("\r\n")
        .flat_map(|l| l.split(['\n', '\r']))
        .collect();
    let indent_of = |line: &str| line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    let common = lines
        .iter()
        .skip(1)
        .filter(|line| indent_of(line) < line.chars().count())
        .map(|line| indent_of(line))
        .min();
    let mut lines: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| match common {
            Some(n) if i > 0 => line.chars().skip(n).collect(),
            _ => (*line).to_owned(),
        })
        .collect();
    let blank = |line: &String| line.chars().all(|c| c == ' ' || c == '\t');
    while lines.first().is_some_and(blank) {
        lines.remove(0);
    }
    while lines.last().is_some_and(blank) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, line: usize, column: usize) -> PValue {
        Source::new(text).value_at(Pos { line, column }).unwrap()
    }

    #[test]
    fn keeps_duplicate_fields_in_order_with_positions() {
        let doc =
            "query {\r\n  todos(orderBy: { title: ASC, # c\n title: DESC }) { items { id } } }";
        let value = at(doc, 2, 18);
        let PKind::Object(fields) = value.kind else {
            panic!("{value:?}")
        };
        let names: Vec<_> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["title", "title"]);
        assert_eq!(fields[1].pos, Pos { line: 3, column: 2 });
        assert_eq!(fields[1].value.kind, PKind::Enum("DESC".into()));
    }

    #[test]
    fn reads_every_literal_kind() {
        let doc = r#"{ a(x: [1, -2.5e+3, "q\"\\\u00e9\ud83d\ude00", true, null, $v, E, """
            two
              lines
        """]) }"#;
        let value = at(doc, 1, 8);
        let PKind::List(items) = value.kind else {
            panic!("{value:?}")
        };
        let kinds: Vec<_> = items.into_iter().map(|v| v.kind).collect();
        assert_eq!(
            kinds,
            [
                PKind::Int("1".into()),
                PKind::Float("-2.5e+3".into()),
                PKind::String("q\"\\é😀".into()),
                PKind::Boolean(true),
                PKind::Null,
                PKind::Variable("v".into()),
                PKind::Enum("E".into()),
                PKind::String("two\n  lines".into()),
            ]
        );
    }

    #[test]
    fn refuses_values_nested_too_deep() {
        let doc = format!("{{ a(x: {}1{}) }}", "[".repeat(200), "]".repeat(200));
        assert!(
            Source::new(&doc)
                .value_at(Pos { line: 1, column: 8 })
                .is_err()
        );
    }
}
