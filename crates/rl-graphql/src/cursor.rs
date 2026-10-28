//! DAB pagination cursors (`SqlPaginationUtil.MakeCursorFromJsonElement` and
//! `ParseAfterFromJsonString`), byte for byte in structure:
//!
//! `base64(JSON [{"EntityName":"Todo","FieldName":"title","FieldValue":"x","Direction":1}, ...])`
//!
//! with one entry per `orderBy` column (in `orderBy` order), then the primary key columns not
//! already listed (ascending). The JSON is System.Text.Json's: properties in declaration order,
//! `FieldValue` omitted when `null`, `Direction` the `OrderBy` enum as a number (`ASC` 0,
//! `DESC` 1), strings escaped with its default encoder, and standard padded base64, so the token
//! only uses `[A-Za-z0-9+/=]` (the SDK pastes it into the next query unescaped).
//!
//! Values are the row's values as rayfin-local reads them (DAB uses the database's JSON); a
//! cursor is opaque to the SDK, which only passes it back.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value as Json;

/// One entry of a cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct CursorField {
    /// The exposed field name.
    pub field: String,
    pub value: Json,
    /// `Direction`: 0 (`ASC`), 1 (`DESC`) or anything else a client wrote.
    pub direction: i64,
}

/// Encodes a cursor for a row of `entity`.
pub fn encode(entity: &str, fields: &[CursorField]) -> String {
    let mut json = String::from("[");
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str("{\"EntityName\":");
        write_string(&mut json, entity);
        json.push_str(",\"FieldName\":");
        write_string(&mut json, &field.field);
        if !field.value.is_null() {
            json.push_str(",\"FieldValue\":");
            write_value(&mut json, &field.value);
        }
        json.push_str(",\"Direction\":");
        json.push_str(&field.direction.to_string());
        json.push('}');
    }
    json.push(']');
    STANDARD.encode(json.as_bytes())
}

/// Why a token is not a cursor: DAB's `{text} is not a valid pagination token.`, where `text`
/// is the decoded JSON when the base64 decoded, else the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidCursor(pub String);

impl InvalidCursor {
    pub fn message(&self) -> String {
        format!("{} is not a valid pagination token.", self.0)
    }
}

/// Decodes a cursor (structure only; the planner matches it against the query's order).
pub fn decode(token: &str) -> Result<(String, Vec<CursorField>), InvalidCursor> {
    let compact: String = token.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = STANDARD
        .decode(compact.as_bytes())
        .map_err(|_| InvalidCursor(token.to_owned()))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let invalid = || InvalidCursor(text.clone());
    let Ok(Json::Array(entries)) = serde_json::from_str::<Json>(&text) else {
        return Err(invalid());
    };
    let mut fields = Vec::with_capacity(entries.len());
    for entry in entries {
        let Json::Object(entry) = entry else {
            return Err(invalid());
        };
        let field = match entry.get("FieldName") {
            Some(Json::String(name)) => name.clone(),
            _ => return Err(invalid()),
        };
        let direction = match entry.get("Direction") {
            None => 0,
            Some(Json::Number(n)) => n.as_i64().ok_or_else(invalid)?,
            Some(_) => return Err(invalid()),
        };
        let value = entry.get("FieldValue").cloned().unwrap_or(Json::Null);
        fields.push(CursorField {
            field,
            value,
            direction,
        });
    }
    Ok((text, fields))
}

/// A JSON value as System.Text.Json writes it (numbers as `double`).
fn write_value(out: &mut String, value: &Json) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => out.push_str(&(f as i64).to_string()),
            Some(f) => out.push_str(&f.to_string()),
            None => out.push_str(&n.to_string()),
        },
        Json::String(s) => write_string(out, s),
        other => out.push_str(&other.to_string()),
    }
}

/// A string as System.Text.Json's default encoder escapes it: control characters, `"`, `\`, the
/// HTML-sensitive `& ' + < > \``, and everything outside printable ASCII (as UTF-16 `\uXXXX`).
fn write_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' if !matches!(ch, '"' | '&' | '\'' | '+' | '<' | '>' | '`') => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04X}"));
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn encodes_dab_cursors() {
        let token = encode(
            "Todo",
            &[
                CursorField {
                    field: "title".into(),
                    value: json!("a\"<é😀"),
                    direction: 1,
                },
                CursorField {
                    field: "estimate".into(),
                    value: Json::Null,
                    direction: 0,
                },
                CursorField {
                    field: "cost".into(),
                    value: json!(10.0),
                    direction: 0,
                },
            ],
        );
        let text = String::from_utf8(STANDARD.decode(&token).unwrap()).unwrap();
        assert_eq!(
            text,
            "[{\"EntityName\":\"Todo\",\"FieldName\":\"title\",\
             \"FieldValue\":\"a\\u0022\\u003C\\u00E9\\uD83D\\uDE00\",\"Direction\":1},\
             {\"EntityName\":\"Todo\",\"FieldName\":\"estimate\",\"Direction\":0},\
             {\"EntityName\":\"Todo\",\"FieldName\":\"cost\",\"FieldValue\":10,\"Direction\":0}]"
        );
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c))
        );
        let (_, fields) = decode(&token).unwrap();
        assert_eq!(fields[0].value, json!("a\"<é😀"));
        assert_eq!(fields[1].value, Json::Null);
        assert_eq!(fields[2].direction, 0);
    }

    #[test]
    fn invalid_tokens_name_what_dab_names() {
        assert_eq!(
            decode("%%%").unwrap_err().message(),
            "%%% is not a valid pagination token."
        );
        let token = STANDARD.encode("{\"a\":1}");
        assert_eq!(
            decode(&token).unwrap_err().message(),
            "{\"a\":1} is not a valid pagination token."
        );
    }
}
