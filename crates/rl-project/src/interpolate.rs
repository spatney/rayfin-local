//! `${VAR}` / `${VAR:-default}` interpolation in `rayfin.yml`, as the Rayfin CLI does it
//! (`interpolation.ts` in `@microsoft/rayfin-tools-common`).
//!
//! - Only string values are interpolated (keys are not).
//! - A variable that is unset or empty uses the default; without a default it is an error.
//! - In a default, `\}` stands for `}`.
//! - When a whole value is one reference (`enabled: ${AUTH_ENABLED:-true}`), the result is
//!   coerced to a boolean, null or number like YAML would; partial interpolation stays a string.

use std::collections::BTreeMap;
use std::path::Path;

use serde_yaml::{Mapping, Value};

use crate::ProjectError;

/// Interpolates every string in a parsed `rayfin.yml` document.
pub(crate) fn interpolate_document(
    document: Value,
    vars: &BTreeMap<String, String>,
    env_file: &Path,
) -> Result<Value, ProjectError> {
    interpolate_value(document, vars, "", env_file)
}

fn interpolate_value(
    value: Value,
    vars: &BTreeMap<String, String>,
    context: &str,
    env_file: &Path,
) -> Result<Value, ProjectError> {
    Ok(match value {
        Value::Mapping(mapping) => {
            let mut out = Mapping::with_capacity(mapping.len());
            for (key, value) in mapping {
                let name = key_name(&key);
                let child = if context.is_empty() {
                    name
                } else {
                    format!("{context}.{name}")
                };
                out.insert(key, interpolate_value(value, vars, &child, env_file)?);
            }
            Value::Mapping(out)
        }
        Value::Sequence(items) => Value::Sequence(
            items
                .into_iter()
                .enumerate()
                .map(|(i, item)| {
                    interpolate_value(item, vars, &format!("{context}[{i}]"), env_file)
                })
                .collect::<Result<_, _>>()?,
        ),
        Value::String(text) => {
            let interpolated =
                interpolate_str(&text, vars).map_err(|var| ProjectError::MissingVariable {
                    var,
                    context: if context.is_empty() {
                        "root".into()
                    } else {
                        context.into()
                    },
                    env_file: env_file.to_path_buf(),
                })?;
            if is_single_reference(&text) {
                coerce(interpolated)
            } else {
                Value::String(interpolated)
            }
        }
        Value::Tagged(mut tagged) => {
            tagged.value = interpolate_value(tagged.value, vars, context, env_file)?;
            Value::Tagged(tagged)
        }
        scalar => scalar,
    })
}

/// A mapping key as it appears in a dotted path.
pub(crate) fn key_name(key: &Value) -> String {
    match key {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "null".into(),
        Value::Tagged(tagged) => key_name(&tagged.value),
        Value::Sequence(_) | Value::Mapping(_) => "?".into(),
    }
}

/// Replaces every `${NAME}` and `${NAME:-default}` in `text`. On failure, returns the name of the
/// variable that is unset (or empty) and has no default.
fn interpolate_str(text: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let candidate = &rest[start..];
        match parse_reference(candidate) {
            Some(reference) => {
                match vars.get(reference.name).filter(|value| !value.is_empty()) {
                    Some(value) => out.push_str(value),
                    None => match reference.default {
                        Some(default) => out.push_str(&default.replace("\\}", "}")),
                        None => return Err(reference.name.to_string()),
                    },
                }
                rest = &candidate[reference.len..];
            }
            None => {
                out.push('$');
                rest = &candidate[1..];
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

struct Reference<'a> {
    name: &'a str,
    default: Option<&'a str>,
    /// Bytes consumed, including `${` and `}`.
    len: usize,
}

/// Matches `\$\{(\w+)(?::-((?:[^}\\]*(?:\\.[^}\\]*)*)))?\}` at the start of `text`.
fn parse_reference(text: &str) -> Option<Reference<'_>> {
    let body = text.strip_prefix("${")?;
    let name_len = body
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    if name_len == 0 {
        return None;
    }
    let name = &body[..name_len];
    let after = &body[name_len..];
    if after.starts_with('}') {
        return Some(Reference {
            name,
            default: None,
            len: 2 + name_len + 1,
        });
    }
    let default_text = after.strip_prefix(":-")?;
    let mut chars = default_text.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '}' => {
                return Some(Reference {
                    name,
                    default: Some(&default_text[..i]),
                    len: 2 + name_len + 2 + i + 1,
                });
            }
            // `\.` in a JavaScript regex: any character but a line terminator.
            '\\' => match chars.next() {
                Some((_, '\n' | '\r' | '\u{2028}' | '\u{2029}')) | None => return None,
                Some(_) => {}
            },
            _ => {}
        }
    }
    None
}

/// The CLI coerces when the raw value starts with `${`, ends with `}` and has no other `$`.
fn is_single_reference(raw: &str) -> bool {
    raw.starts_with("${") && raw.ends_with('}') && !raw[2..].contains('$')
}

/// `coerceType` from the CLI: `true`/`false`, `null`/`~`, and numbers in their canonical form
/// (`5432`, `-1`, `1.5`; not `05`, `1.0` or `1e3`) become YAML scalars.
fn coerce(value: String) -> Value {
    if value.is_empty() {
        return Value::String(value);
    }
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" | "~" => return Value::Null,
        _ => {}
    }
    canonical_number(value.trim()).unwrap_or(Value::String(value))
}

fn canonical_number(text: &str) -> Option<Value> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let int_ok = int == "0" || (!int.is_empty() && !int.starts_with('0'));
    if !int_ok
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    if digits.contains('.') {
        let number: f64 = text.parse().ok()?;
        (number.is_finite() && number.to_string() == text).then(|| Value::Number(number.into()))
    } else {
        // JavaScript numbers are exact up to 2^53.
        const MAX_SAFE: i64 = (1 << 53) - 1;
        let number: i64 = text.parse().ok()?;
        (number.abs() <= MAX_SAFE && number.to_string() == text)
            .then(|| Value::Number(number.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_variables_and_defaults() {
        let vars = vars(&[("HOST", "db.local"), ("EMPTY", "")]);
        assert_eq!(interpolate_str("${HOST}", &vars).unwrap(), "db.local");
        assert_eq!(
            interpolate_str("https://${HOST}/x", &vars).unwrap(),
            "https://db.local/x"
        );
        assert_eq!(
            interpolate_str("${EMPTY:-fallback}", &vars).unwrap(),
            "fallback"
        );
        assert_eq!(interpolate_str("${UNSET:-a\\}b}", &vars).unwrap(), "a}b");
        assert_eq!(
            interpolate_str("${HOST:-unused}", &vars).unwrap(),
            "db.local"
        );
        assert_eq!(interpolate_str("${UNSET:-}", &vars).unwrap(), "");
        assert_eq!(interpolate_str("${EMPTY}", &vars), Err("EMPTY".to_string()));
        assert_eq!(
            interpolate_str("x ${UNSET} y", &vars),
            Err("UNSET".to_string())
        );
    }

    #[test]
    fn leaves_text_that_is_not_a_reference() {
        let vars = vars(&[("A", "1")]);
        for text in [
            "$A",
            "${}",
            "${A",
            "${A:default}",
            "${A-x}",
            "price: $5",
            "${-}",
        ] {
            assert_eq!(interpolate_str(text, &vars).unwrap(), text);
        }
        assert_eq!(interpolate_str("$${A}", &vars).unwrap(), "$1");
    }

    #[test]
    fn coerces_whole_value_references_only() {
        let vars = vars(&[
            ("ON", "TRUE"),
            ("PORT", "5432"),
            ("ZERO_PADDED", "05"),
            ("NUL", "~"),
        ]);
        let interpolate = |text: &str| {
            interpolate_document(Value::String(text.into()), &vars, Path::new("rayfin/.env"))
                .unwrap()
        };
        assert_eq!(interpolate("${ON}"), Value::Bool(true));
        assert_eq!(interpolate("${PORT}"), Value::Number(5432.into()));
        assert_eq!(interpolate("${UNSET:-1.5}"), Value::Number(1.5.into()));
        assert_eq!(interpolate("${UNSET:--3}"), Value::Number((-3).into()));
        assert_eq!(interpolate("${NUL}"), Value::Null);
        assert_eq!(interpolate("${ZERO_PADDED}"), Value::String("05".into()));
        assert_eq!(interpolate("${UNSET:-1.0}"), Value::String("1.0".into()));
        assert_eq!(interpolate("${UNSET:-1e3}"), Value::String("1e3".into()));
        assert_eq!(
            interpolate("port ${PORT}"),
            Value::String("port 5432".into())
        );
        assert_eq!(
            interpolate("${PORT}${PORT}"),
            Value::String("54325432".into())
        );
    }

    #[test]
    fn missing_variable_error_names_the_setting() {
        let document: Value =
            serde_yaml::from_str("connectors:\n  - config:\n      itemId: ${ITEM}\n").unwrap();
        let err =
            interpolate_document(document, &BTreeMap::new(), Path::new("rayfin/.env")).unwrap_err();
        match err {
            ProjectError::MissingVariable { var, context, .. } => {
                assert_eq!(var, "ITEM");
                assert_eq!(context, "connectors[0].config.itemId");
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
