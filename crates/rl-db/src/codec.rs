//! Value codecs: how each [`FieldKind`] is stored in SQLite, and the conversions between GraphQL
//! input (JSON), SQLite values and the JSON DAB emits on Fabric (compatibility.md §8).
//!
//! | Kind | Column | Stored as | JSON out (§8.1) |
//! | --- | --- | --- | --- |
//! | `uuid` | `TEXT COLLATE NOCASE` | lowercase `8-4-4-4-12` | the same string |
//! | `text`, `email`, `set` | `TEXT COLLATE NOCASE` | as given | the same string |
//! | `int` | `INTEGER` | a 32-bit integer | a number |
//! | `decimal(p,s)` | `REAL` | rounded half away from zero to `s` digits | a number with at most `s` decimals |
//! | `boolean` | `INTEGER` | `0` / `1` | `false` / `true` |
//! | `date` | `TEXT` | `YYYY-MM-DDTHH:MM:SS.fffffffZ` (UTC, 100 ns, fixed width) | `YYYY-MM-DDTHH:MM:SS.fffZ` |
//!
//! Ordering and comparison in SQL follow from the storage: fixed-width UTC dates sort
//! chronologically as text, uuids are compared lowercase, and text uses SQLite's built-in `NOCASE`
//! collation, standing in for Fabric SQL's case-insensitive default collation
//! (`SQL_Latin1_General_CP1_CI_AS`). `NOCASE` folds ASCII letters only, so `'É' = 'é'` is false
//! here but true on Fabric. Apps on the `postgresql` dialect compare text case-sensitively.
//!
//! Decimals are IEEE doubles: about 15 significant digits are exact (a `DECIMAL(18,2)` above
//! ~10^13 loses cents), where Fabric's `DECIMAL(p,s)` is exact to `p` digits.
//!
//! Who validates what (1.36.2): Hot Chocolate's scalar coercion rejects wrong types ([`encode`]
//! reports it with Hot Chocolate's message); the database enforces `NOT NULL`, lengths
//! (`NVARCHAR(n)`), `@set` membership, the email `LIKE` shape, minimum lengths, ranges (only under
//! `cli-minor-fixes`), uniqueness and foreign keys (the DDL of [`crate::schema`]; violations are
//! DAB's [`DATABASE_ERROR_MESSAGE`]). Patterns, the email regex and the other `min`/`max` options
//! are only checked by the SDK's form validation (`toStandardSchema`), never by the server, so
//! rayfin-local accepts what Fabric accepts.

use rl_model::{Column, FieldKind};
use rusqlite::types::{Value as SqlValue, ValueRef};
use serde_json::{Number, Value as Json};
use time::{Date, Month, PrimitiveDateTime, Time, UtcOffset};

/// DAB's message for any database error, constraint violations included (compatibility.md §7.5).
pub const DATABASE_ERROR_MESSAGE: &str =
    "While processing your request the database ran into an error.";
/// The `extensions.code` of [`DATABASE_ERROR_MESSAGE`].
pub const DATABASE_ERROR_CODE: &str = "DatabaseOperationFailed";

/// A value that cannot be stored or read as its column's kind.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// The GraphQL scalar rejects the input, e.g. a string for an `Int` or a malformed UUID
    /// (Hot Chocolate's literal-coercion message).
    #[error("{scalar} cannot parse the given literal of type `{node}`.")]
    InvalidLiteral {
        /// The GraphQL scalar (`UUID`, `String`, `Int`, `Decimal`, `Boolean`, `DateTime`).
        scalar: &'static str,
        /// Hot Chocolate's syntax node of the input (`StringValueNode`, `IntValueNode`, ...).
        node: &'static str,
    },
    /// A stored value that does not have its column's storage format (written by another tool).
    #[error("the stored value {found} is not a valid {expected}")]
    Stored {
        expected: &'static str,
        found: String,
    },
}

/// The SQLite storage class of a kind's column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    Text,
    Integer,
    Real,
}

impl Storage {
    /// The declared column type (which gives the column its affinity).
    pub fn sql_type(self) -> &'static str {
        match self {
            Self::Text => "TEXT",
            Self::Integer => "INTEGER",
            Self::Real => "REAL",
        }
    }
}

/// How `kind` is stored.
pub fn storage(kind: &FieldKind) -> Storage {
    match kind {
        FieldKind::Uuid
        | FieldKind::Text { .. }
        | FieldKind::Email { .. }
        | FieldKind::Set { .. }
        | FieldKind::Date => Storage::Text,
        FieldKind::Int { .. } | FieldKind::Boolean => Storage::Integer,
        FieldKind::Decimal { .. } => Storage::Real,
    }
}

/// Whether the kind's values are text that the app compares (case-insensitively on Fabric SQL).
pub fn is_textual(kind: &FieldKind) -> bool {
    matches!(
        kind,
        FieldKind::Uuid | FieldKind::Text { .. } | FieldKind::Email { .. } | FieldKind::Set { .. }
    )
}

/// Converts a GraphQL input value (a literal or a variable, as JSON) to the value stored in a
/// column of `kind`: uuids lowercased, dates normalized to UTC, booleans to `0`/`1`, decimals
/// rounded to the column's scale as `DECIMAL(p,s)` does. `null` stays `NULL` (nullability is the
/// schema's and the database's business).
pub fn encode(kind: &FieldKind, value: &Json) -> Result<SqlValue, CodecError> {
    encode_value(kind, value, true)
}

/// Like [`encode`], for the right-hand side of a comparison (filters, policies): a decimal keeps
/// all its digits, as SQL compares a `DECIMAL(p,s)` column with an unrounded literal.
pub fn encode_operand(kind: &FieldKind, value: &Json) -> Result<SqlValue, CodecError> {
    encode_value(kind, value, false)
}

fn encode_value(kind: &FieldKind, value: &Json, round: bool) -> Result<SqlValue, CodecError> {
    let invalid = || CodecError::InvalidLiteral {
        scalar: kind.graphql_scalar(),
        node: node_kind(value),
    };
    match (kind, value) {
        (_, Json::Null) => Ok(SqlValue::Null),
        (FieldKind::Uuid, Json::String(text)) => {
            parse_uuid(text).map(SqlValue::Text).ok_or_else(invalid)
        }
        (
            FieldKind::Text { .. } | FieldKind::Email { .. } | FieldKind::Set { .. },
            Json::String(text),
        ) => Ok(SqlValue::Text(text.clone())),
        (FieldKind::Int { .. }, Json::Number(number)) => number
            .as_i64()
            .filter(|n| i32::try_from(*n).is_ok())
            .map(SqlValue::Integer)
            .ok_or_else(invalid),
        (FieldKind::Decimal { scale, .. }, Json::Number(number)) => {
            let scale = round.then_some(*scale);
            decimal_from_number(number, scale)
                .map(SqlValue::Real)
                .ok_or_else(invalid)
        }
        (FieldKind::Boolean, Json::Bool(flag)) => Ok(SqlValue::Integer(i64::from(*flag))),
        (FieldKind::Date, Json::String(text)) => {
            parse_datetime(text).map(SqlValue::Text).ok_or_else(invalid)
        }
        _ => Err(invalid()),
    }
}

/// Converts text to a value of `kind`, as SQL Server converts a string (claims compared with a
/// typed field, data migrated from a text column): uuids in any .NET format, integers, decimal
/// numbers, `true`/`false`/`1`/`0`, and the date formats [`encode`] accepts.
pub fn parse_text(kind: &FieldKind, text: &str) -> Result<SqlValue, CodecError> {
    let invalid = || CodecError::InvalidLiteral {
        scalar: kind.graphql_scalar(),
        node: "StringValueNode",
    };
    let trimmed = text.trim();
    match kind {
        FieldKind::Text { .. } | FieldKind::Email { .. } | FieldKind::Set { .. } => {
            Ok(SqlValue::Text(text.to_owned()))
        }
        FieldKind::Uuid => parse_uuid(trimmed).map(SqlValue::Text).ok_or_else(invalid),
        FieldKind::Int { .. } => trimmed
            .parse::<i32>()
            .map(|n| SqlValue::Integer(n.into()))
            .map_err(|_| invalid()),
        FieldKind::Decimal { scale, .. } => trimmed
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && is_decimal_text(trimmed))
            .map(|n| SqlValue::Real(round_decimal(n, *scale)))
            .ok_or_else(invalid),
        FieldKind::Boolean => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(SqlValue::Integer(1)),
            "false" | "0" => Ok(SqlValue::Integer(0)),
            _ => Err(invalid()),
        },
        FieldKind::Date => parse_datetime(trimmed)
            .map(SqlValue::Text)
            .ok_or_else(invalid),
    }
}

/// Reads a stored value of `kind` as the JSON DAB emits for it (compatibility.md §8.1).
pub fn decode(kind: &FieldKind, value: ValueRef<'_>) -> Result<Json, CodecError> {
    let stored = |expected: &'static str| CodecError::Stored {
        expected,
        found: describe(value),
    };
    match (kind, value) {
        (_, ValueRef::Null) => Ok(Json::Null),
        (FieldKind::Uuid, ValueRef::Text(bytes)) => std::str::from_utf8(bytes)
            .ok()
            .and_then(parse_uuid)
            .map(Json::String)
            .ok_or_else(|| stored("uuid")),
        (
            FieldKind::Text { .. } | FieldKind::Email { .. } | FieldKind::Set { .. },
            ValueRef::Text(bytes),
        ) => Ok(Json::String(String::from_utf8_lossy(bytes).into_owned())),
        (
            FieldKind::Text { .. } | FieldKind::Email { .. } | FieldKind::Set { .. },
            ValueRef::Integer(n),
        ) => Ok(Json::String(n.to_string())),
        (FieldKind::Int { .. }, ValueRef::Integer(n)) => Ok(Json::Number(n.into())),
        (FieldKind::Decimal { scale, .. }, ValueRef::Real(n)) => {
            decimal_json(round_decimal(n, *scale)).ok_or_else(|| stored("decimal"))
        }
        (FieldKind::Decimal { scale, .. }, ValueRef::Integer(n)) => {
            decimal_json(round_decimal(n as f64, *scale)).ok_or_else(|| stored("decimal"))
        }
        (FieldKind::Boolean, ValueRef::Integer(0)) => Ok(Json::Bool(false)),
        (FieldKind::Boolean, ValueRef::Integer(1)) => Ok(Json::Bool(true)),
        (FieldKind::Date, ValueRef::Text(bytes)) => std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| {
                if is_stored_datetime(text) {
                    Some(output_datetime(text))
                } else {
                    parse_datetime(text).map(|normalized| output_datetime(&normalized))
                }
            })
            .map(Json::String)
            .ok_or_else(|| stored("date")),
        (FieldKind::Int { .. }, _) => Err(stored("int")),
        (FieldKind::Decimal { .. }, _) => Err(stored("decimal")),
        (FieldKind::Boolean, _) => Err(stored("boolean (0 or 1)")),
        (FieldKind::Date, _) => Err(stored("date")),
        (FieldKind::Uuid, _) => Err(stored("uuid")),
        (_, _) => Err(stored("text")),
    }
}

/// A SQL expression giving the value of stored column `column_sql` of `kind` as
/// [`decode`] would, for building results inside SQL (`json_object`, `json_group_array`): a
/// boolean becomes JSON `true`/`false`, a decimal a number rounded to its scale, a date the
/// millisecond ISO string. `NULL` stays `NULL`.
///
/// The boolean form carries SQLite's JSON subtype, which survives function arguments but not a
/// subquery boundary: wrap the result of a scalar subquery that returns JSON in `json(...)`.
pub fn json_expr(kind: &FieldKind, column_sql: &str) -> String {
    match kind {
        FieldKind::Boolean => {
            format!("json(CASE {column_sql} WHEN 1 THEN 'true' WHEN 0 THEN 'false' END)")
        }
        FieldKind::Decimal { scale, .. } => format!("round({column_sql}, {scale})"),
        FieldKind::Date => format!("(substr({column_sql}, 1, 23) || 'Z')"),
        FieldKind::Uuid
        | FieldKind::Text { .. }
        | FieldKind::Email { .. }
        | FieldKind::Set { .. }
        | FieldKind::Int { .. } => column_sql.to_owned(),
    }
}

/// The value the database fills in when an insert omits `column`, for code that needs the row
/// before inserting it (a create policy checked on the candidate row): the decorator's `default`,
/// or a fresh uuid for the `id` key (Fabric's `NEWID()`). `None` means `NULL`.
///
/// The DDL declares the same defaults, so an insert that omits the column gets an equivalent
/// value either way.
pub fn generated_default(column: &Column) -> Option<SqlValue> {
    match column.default.as_ref().filter(|value| !value.is_null()) {
        Some(default) => encode(&column.kind, default).ok(),
        None if is_generated_key(column) => Some(SqlValue::Text(new_uuid())),
        None => None,
    }
}

/// Whether the database generates `column` (the `id` key: `DEFAULT NEWID()` on Fabric).
pub fn is_generated_key(column: &Column) -> bool {
    column.name.eq_ignore_ascii_case("id") && column.kind == FieldKind::Uuid
}

/// A random (version 4) uuid in the stored format, as `NEWID()` generates.
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

/// The SQLite expression of [`new_uuid`], for `DEFAULT`.
pub(crate) const UUID_V4_SQL: &str = "(lower(hex(randomblob(4))) || '-' || \
     lower(hex(randomblob(2))) || '-4' || substr(lower(hex(randomblob(2))), 2) || '-' || \
     substr('89ab', 1 + (abs(random()) % 4), 1) || substr(lower(hex(randomblob(2))), 2) || '-' || \
     lower(hex(randomblob(6))))";

/// The kind of constraint an insert or update broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintKind {
    NotNull,
    Unique,
    PrimaryKey,
    Check,
    ForeignKey,
    Other,
}

/// A constraint violation reported by SQLite, for logs; DAB answers every one of them with
/// [`DATABASE_ERROR_MESSAGE`] (`DatabaseOperationFailed`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConstraintViolation {
    pub kind: ConstraintKind,
    /// SQLite's message, e.g. `CHECK constraint failed: CK_Todos_status`.
    pub message: String,
}

/// The constraint violation `error` reports, if it is one.
pub fn constraint_violation(error: &rusqlite::Error) -> Option<ConstraintViolation> {
    let rusqlite::Error::SqliteFailure(failure, message) = error else {
        return None;
    };
    if failure.code != rusqlite::ErrorCode::ConstraintViolation {
        return None;
    }
    use rusqlite::ffi;
    let kind = match failure.extended_code {
        ffi::SQLITE_CONSTRAINT_NOTNULL => ConstraintKind::NotNull,
        ffi::SQLITE_CONSTRAINT_UNIQUE => ConstraintKind::Unique,
        ffi::SQLITE_CONSTRAINT_PRIMARYKEY => ConstraintKind::PrimaryKey,
        ffi::SQLITE_CONSTRAINT_CHECK => ConstraintKind::Check,
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => ConstraintKind::ForeignKey,
        _ => ConstraintKind::Other,
    };
    Some(ConstraintViolation {
        kind,
        message: message.clone().unwrap_or_else(|| error.to_string()),
    })
}

/// Hot Chocolate's syntax node for a JSON input value.
fn node_kind(value: &Json) -> &'static str {
    match value {
        Json::Null => "NullValueNode",
        Json::Bool(_) => "BooleanValueNode",
        Json::Number(number) if number.is_f64() => "FloatValueNode",
        Json::Number(_) => "IntValueNode",
        Json::String(_) => "StringValueNode",
        Json::Array(_) => "ListValueNode",
        Json::Object(_) => "ObjectValueNode",
    }
}

fn describe(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "NULL".into(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(n) => n.to_string(),
        ValueRef::Text(bytes) => format!("'{}'", String::from_utf8_lossy(bytes)),
        ValueRef::Blob(bytes) => format!("a {}-byte blob", bytes.len()),
    }
}

/// A uuid in the .NET `D`, `N`, `B` or `P` format (as `Guid.TryParse`), any case, in the stored
/// lowercase `D` format.
pub(crate) fn parse_uuid(text: &str) -> Option<String> {
    let inner = match text.as_bytes() {
        [b'{', .., b'}'] | [b'(', .., b')'] => &text[1..text.len() - 1],
        _ => text,
    };
    let hex: String = match inner.len() {
        32 => inner.to_owned(),
        36 => {
            let bytes = inner.as_bytes();
            if [8, 13, 18, 23].iter().any(|&i| bytes[i] != b'-') {
                return None;
            }
            inner.replace('-', "")
        }
        _ => return None,
    };
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if inner.len() == 32 && text.len() != 32 {
        // `{...}` and `(...)` wrap the hyphenated form only.
        return None;
    }
    let hex = hex.to_ascii_lowercase();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// An RFC 3339 date-time with `Z` or an offset and any fractional precision, or a date
/// (`YYYY-MM-DD`, UTC midnight), in the stored format; `None` outside years 0001–9999 (UTC).
pub(crate) fn parse_datetime(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        let slice = bytes.get(range)?;
        if slice.is_empty() || !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(slice).ok()?.parse().ok()
    };
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = digits(0..4)? as i32;
    let month = Month::try_from(u8::try_from(digits(5..7)?).ok()?).ok()?;
    let day = u8::try_from(digits(8..10)?).ok()?;
    let date = Date::from_calendar_date(year, month, day).ok()?;
    if bytes.len() == 10 {
        return format_stored(PrimitiveDateTime::new(date, Time::MIDNIGHT).assume_utc());
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') || bytes.len() < 20 {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let hour = u8::try_from(digits(11..13)?).ok()?;
    let minute = u8::try_from(digits(14..16)?).ok()?;
    let second = u8::try_from(digits(17..19)?).ok()?;
    let mut position = 19;
    let mut nanos = 0u32;
    if bytes[position] == b'.' {
        let start = position + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == start {
            return None;
        }
        // 100 ns resolution (DATETIME2(7)); further digits are dropped.
        let fraction = &text[start..end.min(start + 9)];
        nanos = format!("{fraction:0<9}").parse().ok()?;
        nanos -= nanos % 100;
        position = end;
    }
    let offset = match &bytes[position..] {
        [b'Z' | b'z'] => UtcOffset::UTC,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let parse = |a: u8, b: u8| -> Option<i8> {
                (a.is_ascii_digit() && b.is_ascii_digit())
                    .then(|| ((a - b'0') * 10 + (b - b'0')) as i8)
            };
            let (hours, minutes) = (parse(*h1, *h2)?, parse(*m1, *m2)?);
            let sign = if *sign == b'-' { -1 } else { 1 };
            UtcOffset::from_hms(sign * hours, sign * minutes, 0).ok()?
        }
        _ => return None,
    };
    let time = Time::from_hms_nano(hour, minute, second, nanos).ok()?;
    let instant = PrimitiveDateTime::new(date, time)
        .assume_offset(offset)
        .checked_to_offset(UtcOffset::UTC)?;
    format_stored(instant)
}

fn format_stored(instant: time::OffsetDateTime) -> Option<String> {
    let year = instant.year();
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some(format!(
        "{year:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}Z",
        u8::from(instant.month()),
        instant.day(),
        instant.hour(),
        instant.minute(),
        instant.second(),
        instant.nanosecond() / 100
    ))
}

/// Whether `text` is exactly in the stored date format.
fn is_stored_datetime(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 28
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            10 => *b == b'T',
            13 | 16 => *b == b':',
            19 => *b == b'.',
            27 => *b == b'Z',
            _ => b.is_ascii_digit(),
        })
}

/// The stored date format as DAB emits it: milliseconds (truncated, like .NET's `fff`) and `Z`.
pub(crate) fn output_datetime(stored: &str) -> String {
    format!("{}Z", &stored[..23])
}

/// A JSON number as a `DECIMAL`: `None` beyond .NET's `decimal` range (Hot Chocolate's
/// `Decimal` rejects it). With `scale`, rounded as SQL Server stores it in `DECIMAL(p,scale)`.
fn decimal_from_number(number: &Number, scale: Option<u32>) -> Option<f64> {
    let value = if let Some(n) = number.as_i64() {
        n as f64
    } else if let Some(n) = number.as_u64() {
        n as f64
    } else {
        number.as_f64()?
    };
    if !value.is_finite() || value.abs() >= 7.922_816_251_426_434e28 {
        return None;
    }
    Some(match scale {
        Some(scale) if number.is_f64() => round_decimal(value, scale),
        _ => value,
    })
}

/// Whether `text` is a plain decimal number (no `inf`/`nan` spellings).
fn is_decimal_text(text: &str) -> bool {
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    !body.is_empty()
        && body.bytes().any(|b| b.is_ascii_digit())
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
}

/// Rounds `value` to `scale` decimals, half away from zero, on its shortest decimal
/// representation (the digits a client wrote), so `2.345` rounds to `2.35` although the nearest
/// double is `2.34499…`.
pub(crate) fn round_decimal(value: f64, scale: u32) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    let text = format!("{:e}", value.abs());
    let Some((mantissa, exponent)) = text.split_once('e') else {
        return value;
    };
    let Ok(exponent) = exponent.parse::<i64>() else {
        return value;
    };
    let digits: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(|b| b - b'0')
        .collect();
    // value = d0.d1d2... × 10^exponent; keep the digits down to 10^-scale.
    let keep = exponent + 1 + i64::from(scale);
    if keep >= digits.len() as i64 {
        return value;
    }
    if keep < 0 {
        return 0.0;
    }
    let keep = keep as usize;
    let mut kept = digits[..keep].to_vec();
    if digits[keep] >= 5 {
        let mut index = kept.len();
        loop {
            if index == 0 {
                kept.insert(0, 1);
                break;
            }
            index -= 1;
            if kept[index] == 9 {
                kept[index] = 0;
            } else {
                kept[index] += 1;
                break;
            }
        }
    }
    if kept.iter().all(|d| *d == 0) {
        return 0.0;
    }
    let mantissa: String = kept.iter().map(|d| char::from(b'0' + d)).collect();
    let sign = if value < 0.0 { "-" } else { "" };
    // The last kept digit is at 10^-scale, carry or not.
    format!("{sign}{mantissa}e-{scale}")
        .parse()
        .unwrap_or(value)
}

fn decimal_json(value: f64) -> Option<Json> {
    Number::from_f64(value).map(Json::Number)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn text() -> FieldKind {
        FieldKind::Text {
            min_length: None,
            max_length: None,
            pattern: None,
        }
    }

    fn decimal(scale: u32) -> FieldKind {
        FieldKind::Decimal {
            precision: 18,
            scale,
            min: None,
            max: None,
        }
    }

    fn int() -> FieldKind {
        FieldKind::Int {
            min: None,
            max: None,
        }
    }

    fn round_trip(kind: &FieldKind, input: Json) -> Json {
        let stored = encode(kind, &input).unwrap();
        decode(kind, (&stored).into()).unwrap()
    }

    #[test]
    fn uuids_accept_dotnet_formats_and_come_back_lowercase() {
        let canonical = "0f8fad5b-d9cb-469f-a165-70867728950e";
        for input in [
            "0F8FAD5B-D9CB-469F-A165-70867728950E",
            "0f8fad5bd9cb469fa16570867728950e",
            "{0f8fad5b-d9cb-469f-a165-70867728950e}",
            "(0F8FAD5B-D9CB-469F-A165-70867728950E)",
        ] {
            assert_eq!(round_trip(&FieldKind::Uuid, json!(input)), json!(canonical));
        }
        for bad in [
            json!("0f8fad5b-d9cb-469f-a165-70867728950"),
            json!("0f8fad5b_d9cb_469f_a165_70867728950e"),
            json!("{0f8fad5bd9cb469fa16570867728950e}"),
            json!("zf8fad5b-d9cb-469f-a165-70867728950e"),
            json!(42),
        ] {
            let error = encode(&FieldKind::Uuid, &bad).unwrap_err();
            assert!(error.to_string().starts_with("UUID cannot parse"), "{bad}");
        }
        assert_eq!(
            encode(&FieldKind::Uuid, &json!("nope"))
                .unwrap_err()
                .to_string(),
            "UUID cannot parse the given literal of type `StringValueNode`."
        );
    }

    #[test]
    fn dates_normalize_to_fixed_width_utc() {
        let cases = [
            (
                "2024-01-01T00:00:00.000Z",
                "2024-01-01T00:00:00.0000000Z",
                "2024-01-01T00:00:00.000Z",
            ),
            (
                "2024-01-01T00:00:00Z",
                "2024-01-01T00:00:00.0000000Z",
                "2024-01-01T00:00:00.000Z",
            ),
            (
                "2024-01-01",
                "2024-01-01T00:00:00.0000000Z",
                "2024-01-01T00:00:00.000Z",
            ),
            (
                "2024-03-10T01:30:00.5+02:00",
                "2024-03-09T23:30:00.5000000Z",
                "2024-03-09T23:30:00.500Z",
            ),
            (
                "2024-12-31T23:00:00-05:30",
                "2025-01-01T04:30:00.0000000Z",
                "2025-01-01T04:30:00.000Z",
            ),
            (
                "2024-06-01t10:00:00.123456789z",
                "2024-06-01T10:00:00.1234567Z",
                "2024-06-01T10:00:00.123Z",
            ),
            (
                "2024-06-01 10:00:00.9999Z",
                "2024-06-01T10:00:00.9999000Z",
                "2024-06-01T10:00:00.999Z",
            ),
        ];
        for (input, stored, output) in cases {
            assert_eq!(
                encode(&FieldKind::Date, &json!(input)).unwrap(),
                SqlValue::Text(stored.into()),
                "{input}"
            );
            assert_eq!(
                round_trip(&FieldKind::Date, json!(input)),
                json!(output),
                "{input}"
            );
        }
        for bad in [
            "2024-01-01T10:00:00",
            "2024-02-30T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T10:00Z",
            "2024-01-01T10:00:00+0200",
            "0001-01-01T00:00:00+01:00",
            "9999-12-31T23:00:00-02:00",
            "yesterday",
            "",
        ] {
            assert_eq!(
                encode(&FieldKind::Date, &json!(bad)).unwrap_err(),
                CodecError::InvalidLiteral {
                    scalar: "DateTime",
                    node: "StringValueNode"
                },
                "{bad}"
            );
        }
        assert!(encode(&FieldKind::Date, &json!(1_700_000_000)).is_err());
    }

    #[test]
    fn dates_written_by_other_tools_are_read_leniently() {
        let read = |text: &str| decode(&FieldKind::Date, ValueRef::Text(text.as_bytes()));
        assert_eq!(
            read("2024-01-01T10:00:00Z").unwrap(),
            json!("2024-01-01T10:00:00.000Z")
        );
        assert!(read("not a date").is_err());
        assert!(decode(&FieldKind::Date, ValueRef::Integer(5)).is_err());
    }

    #[test]
    fn booleans_are_zero_and_one() {
        assert_eq!(
            encode(&FieldKind::Boolean, &json!(true)).unwrap(),
            SqlValue::Integer(1)
        );
        assert_eq!(round_trip(&FieldKind::Boolean, json!(false)), json!(false));
        assert_eq!(round_trip(&FieldKind::Boolean, json!(true)), json!(true));
        for bad in [json!("true"), json!(1), json!(0)] {
            assert!(encode(&FieldKind::Boolean, &bad).is_err(), "{bad}");
        }
        assert!(decode(&FieldKind::Boolean, ValueRef::Integer(2)).is_err());
    }

    #[test]
    fn ints_are_32_bit_and_reject_floats() {
        assert_eq!(
            round_trip(&int(), json!(-2147483648i64)),
            json!(-2147483648i64)
        );
        assert_eq!(
            encode(&int(), &json!(2147483648i64))
                .unwrap_err()
                .to_string(),
            "Int cannot parse the given literal of type `IntValueNode`."
        );
        assert_eq!(
            encode(&int(), &json!(5.0)).unwrap_err().to_string(),
            "Int cannot parse the given literal of type `FloatValueNode`."
        );
        assert_eq!(
            encode(&int(), &json!("5")).unwrap_err().to_string(),
            "Int cannot parse the given literal of type `StringValueNode`."
        );
    }

    #[test]
    fn decimals_round_half_away_from_zero_to_their_scale() {
        let cases = [
            (json!(2.345), json!(2.35)),
            (json!(-2.345), json!(-2.35)),
            (json!(2.344), json!(2.34)),
            (json!(9.999), json!(10.0)),
            (json!(0.005), json!(0.01)),
            (json!(0.004), json!(0.0)),
            (json!(1e-7), json!(0.0)),
            (json!(12), json!(12.0)),
            (json!(1.5e10), json!(1.5e10)),
            (json!(99.995), json!(100.0)),
        ];
        for (input, output) in cases {
            assert_eq!(round_trip(&decimal(2), input.clone()), output, "{input}");
        }
        assert_eq!(round_trip(&decimal(0), json!(2.5)), json!(3.0));
        assert_eq!(round_trip(&decimal(4), json!(1.23456)), json!(1.2346));
        // Comparison operands keep their digits.
        assert_eq!(
            encode_operand(&decimal(2), &json!(2.345)).unwrap(),
            SqlValue::Real(2.345)
        );
        assert!(encode(&decimal(2), &json!("9.99")).is_err());
        assert!(encode(&decimal(2), &json!(1e29)).is_err());
        // Values written by other tools are rounded on the way out.
        assert_eq!(
            decode(&decimal(2), ValueRef::Real(1.005_000_1)).unwrap(),
            json!(1.01)
        );
    }

    #[test]
    fn text_kinds_take_strings_only() {
        assert_eq!(round_trip(&text(), json!("Héllo")), json!("Héllo"));
        assert_eq!(round_trip(&text(), json!("")), json!(""));
        assert_eq!(
            encode(&text(), &json!(5)).unwrap_err().to_string(),
            "String cannot parse the given literal of type `IntValueNode`."
        );
        assert_eq!(encode(&text(), &json!(null)).unwrap(), SqlValue::Null);
        assert_eq!(
            encode(&text(), &json!({})).unwrap_err().to_string(),
            "String cannot parse the given literal of type `ObjectValueNode`."
        );
    }

    #[test]
    fn text_converts_like_sql_server() {
        assert_eq!(parse_text(&int(), " 42 ").unwrap(), SqlValue::Integer(42));
        assert!(parse_text(&int(), "4.2").is_err());
        assert_eq!(
            parse_text(&decimal(1), "2.25").unwrap(),
            SqlValue::Real(2.3)
        );
        assert!(parse_text(&decimal(1), "inf").is_err());
        assert_eq!(
            parse_text(&FieldKind::Boolean, "TRUE").unwrap(),
            SqlValue::Integer(1)
        );
        assert_eq!(
            parse_text(&FieldKind::Boolean, "0").unwrap(),
            SqlValue::Integer(0)
        );
        assert!(parse_text(&FieldKind::Boolean, "yes").is_err());
        assert!(parse_text(&FieldKind::Uuid, "abc").is_err());
    }

    #[test]
    fn defaults_fill_literals_and_generate_ids() {
        let column = |name: &str, kind: FieldKind, default: Option<Json>| Column {
            name: name.into(),
            field: name.into(),
            kind,
            db_type: String::new(),
            nullable: false,
            default,
            primary_key: name == "id",
            unique: false,
            origin: rl_model::ColumnOrigin::Declared,
        };
        let Some(SqlValue::Text(id)) = generated_default(&column("id", FieldKind::Uuid, None))
        else {
            panic!("expected a uuid")
        };
        assert_eq!(parse_uuid(&id).as_deref(), Some(id.as_str()));
        assert_eq!(&id[14..15], "4");
        assert_eq!(
            generated_default(&column("rate", decimal(2), Some(json!(9.999)))),
            Some(SqlValue::Real(10.0))
        );
        assert_eq!(
            generated_default(&column(
                "since",
                FieldKind::Date,
                Some(json!("2024-01-01T00:00:00.000Z"))
            )),
            Some(SqlValue::Text("2024-01-01T00:00:00.0000000Z".into()))
        );
        assert_eq!(generated_default(&column("title", text(), None)), None);
    }

    #[test]
    fn json_expressions_match_decode() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let cases = [
            (FieldKind::Boolean, SqlValue::Integer(1)),
            (FieldKind::Boolean, SqlValue::Integer(0)),
            (FieldKind::Boolean, SqlValue::Null),
            (decimal(2), SqlValue::Real(9.99)),
            (decimal(2), SqlValue::Real(5.0)),
            (decimal(2), SqlValue::Real(1.005_000_1)),
            (decimal(4), SqlValue::Real(-0.1235)),
            (
                FieldKind::Date,
                SqlValue::Text("2024-03-09T23:30:00.5000000Z".into()),
            ),
            (FieldKind::Uuid, SqlValue::Text(new_uuid())),
            (int(), SqlValue::Integer(-7)),
            (text(), SqlValue::Text("a \"quoted\" ünïcode".into())),
        ];
        for (kind, stored) in cases {
            let sql = format!(
                "SELECT json_object('v', {}), json_array({})",
                json_expr(&kind, "?1"),
                json_expr(&kind, "?1")
            );
            let (object, array): (String, String) = conn
                .query_row(&sql, [&stored], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap();
            let expected = decode(&kind, ValueRef::from(&stored)).unwrap();
            let object: Json = serde_json::from_str(&object).unwrap();
            let array: Json = serde_json::from_str(&array).unwrap();
            assert_eq!(object["v"], expected, "{kind:?} {stored:?}");
            assert_eq!(array[0], expected, "{kind:?} {stored:?}");
        }
    }

    #[test]
    fn constraint_errors_are_classified() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE t (a TEXT NOT NULL UNIQUE, b INTEGER CONSTRAINT CK_t_b CHECK (b > 0))",
        )
        .unwrap();
        let kind = |sql: &str| {
            let error = conn.execute(sql, []).unwrap_err();
            constraint_violation(&error).unwrap()
        };
        conn.execute("INSERT INTO t (a, b) VALUES ('x', 1)", [])
            .unwrap();
        assert_eq!(
            kind("INSERT INTO t (a) VALUES (NULL)").kind,
            ConstraintKind::NotNull
        );
        assert_eq!(
            kind("INSERT INTO t (a) VALUES ('x')").kind,
            ConstraintKind::Unique
        );
        let check = kind("INSERT INTO t (a, b) VALUES ('y', 0)");
        assert_eq!(check.kind, ConstraintKind::Check);
        assert!(check.message.contains("CK_t_b"), "{}", check.message);
        let syntax = conn.execute("INSERT INTO nope VALUES (1)", []).unwrap_err();
        assert!(constraint_violation(&syntax).is_none());
    }
}
