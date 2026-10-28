//! The messages between [`super::FunctionHost`] and `js/function-host.mjs` (see the module docs of
//! [`super`] for the protocol).

use std::borrow::Cow;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::RegisteredFunction;

/// Starts the host's announcement lines on stdout.
const MARKER: &str = "@@rayfin-local:";

/// How the host announced itself.
#[derive(Debug)]
pub(super) enum Announcement {
    Ready(Ready),
    LoadError(ScriptError),
}

#[derive(Debug, Deserialize)]
pub(super) struct Ready {
    pub port: u16,
}

/// A JavaScript error: `name` is the error's class name.
#[derive(Debug, Deserialize)]
pub(super) struct ScriptError {
    #[serde(default)]
    pub name: Option<String>,
    pub message: String,
    #[serde(default)]
    pub stack: Option<String>,
}

/// Finds an announcement in a stdout line. Returns the app output before the marker (a line the
/// app left unterminated) and the parsed announcement.
pub(super) fn split_announcement(line: &str) -> Option<(&str, Result<Announcement, String>)> {
    let at = line.find(MARKER)?;
    let rest = &line[at + MARKER.len()..];
    let (kind, json) = rest.split_once(' ').unwrap_or((rest, ""));
    let announcement = match kind {
        "ready" => serde_json::from_str(json).map(Announcement::Ready),
        "load-error" => serde_json::from_str(json).map(Announcement::LoadError),
        _ => return None,
    };
    Some((
        &line[..at],
        announcement.map_err(|e| format!("invalid `{kind}` announcement: {e}")),
    ))
}

/// How a body travels in a JSON string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum BodyEncoding {
    Utf8,
    /// One char (U+0000 to U+00FF) per byte, for bodies that aren't valid UTF-8.
    Latin1,
}

pub(super) fn encode_body(body: &[u8]) -> (Cow<'_, str>, BodyEncoding) {
    match std::str::from_utf8(body) {
        Ok(text) => (Cow::Borrowed(text), BodyEncoding::Utf8),
        Err(_) => (
            Cow::Owned(body.iter().map(|&b| char::from(b)).collect()),
            BodyEncoding::Latin1,
        ),
    }
}

pub(super) fn decode_body(body: String, encoding: BodyEncoding) -> Result<Bytes, String> {
    match encoding {
        BodyEncoding::Utf8 => Ok(Bytes::from(body)),
        BodyEncoding::Latin1 => body
            .chars()
            .map(|c| u8::try_from(c).map_err(|_| format!("{c:?} in a latin1 body")))
            .collect::<Result<Vec<u8>, _>>()
            .map(Bytes::from),
    }
}

/// Request headers as the worker's `RpcHttpData.headers` record: one entry per name (lower case),
/// repeated headers joined with `, ` as `Headers` would.
pub(super) fn header_record(headers: &[(String, String)]) -> Map<String, Value> {
    let mut record = Map::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        match record.get_mut(&name) {
            Some(Value::String(existing)) => {
                existing.push_str(", ");
                existing.push_str(value);
            }
            _ => {
                record.insert(name, Value::from(value.as_str()));
            }
        }
    }
    record
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InvokeRequest<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub secret: &'a str,
    pub invocation_id: &'a str,
    pub function_name: &'a str,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<&'a str>,
    pub headers: Map<String, Value>,
    pub body: Cow<'a, str>,
    pub body_encoding: BodyEncoding,
    pub bindings: &'a Map<String, Value>,
}

/// `describe`, `shutdown`: requests with no other content than the secret (and `drainMs`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ControlRequest<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub secret: &'a str,
    /// For `shutdown`: how long in-flight invocations may run before the host exits anyway.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drain_ms: Option<u64>,
}

/// The reply to `describe`.
#[derive(Debug, Deserialize)]
pub(super) struct Described {
    pub ok: bool,
    #[serde(default)]
    pub functions: Vec<RegisteredFunction>,
    #[serde(default)]
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Reply {
    pub ok: bool,
    #[serde(default)]
    pub response: Option<WireResponse>,
    #[serde(default)]
    pub error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WireResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub body_encoding: BodyEncoding,
}

#[derive(Debug, Deserialize)]
pub(super) struct WireError {
    /// `notFound`, `notInvocable`, `failed`, `unauthorized` or `protocol`.
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    pub message: String,
    #[serde(default)]
    pub stack: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcements_are_found_after_unterminated_output() {
        let line = r#"partial@@rayfin-local:ready {"port":5}"#;
        let (before, announcement) = split_announcement(line).unwrap();
        assert_eq!(before, "partial");
        assert!(matches!(
            announcement.unwrap(),
            Announcement::Ready(Ready { port: 5 })
        ));

        let (_, error) =
            split_announcement(r#"@@rayfin-local:load-error {"message":"boom"}"#).unwrap();
        assert!(matches!(error.unwrap(), Announcement::LoadError(e) if e.message == "boom"));
        assert!(
            split_announcement("@@rayfin-local:ready {")
                .unwrap()
                .1
                .is_err()
        );
        assert!(split_announcement("console output").is_none());
    }

    #[test]
    fn bodies_round_trip_through_latin1_when_not_utf8() {
        let bytes = [0x7b, 0xff, 0x00, 0xe9];
        let (text, encoding) = encode_body(&bytes);
        assert_eq!(encoding, BodyEncoding::Latin1);
        assert_eq!(
            decode_body(text.into_owned(), encoding).unwrap(),
            Bytes::copy_from_slice(&bytes)
        );
        assert_eq!(encode_body("é".as_bytes()).1, BodyEncoding::Utf8);
        assert!(decode_body("€".into(), BodyEncoding::Latin1).is_err());
    }

    #[test]
    fn repeated_headers_are_joined_under_lower_case_names() {
        let record = header_record(&[
            ("Accept".into(), "a".into()),
            ("accept".into(), "b".into()),
            ("X-One".into(), "1".into()),
        ]);
        assert_eq!(
            Value::Object(record),
            serde_json::json!({"accept": "a, b", "x-one": "1"})
        );
    }
}
