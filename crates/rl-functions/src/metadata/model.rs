//! The `runtimemetadata.json` document (schema 2.0), checked the way
//! `@microsoft/fabric-user-data-functions` 1.36.2 checks it (`validateRuntimeMetadata`).

use std::{
    collections::{BTreeSet, HashSet},
    sync::{Mutex, PoisonError},
};

use rl_tokens::Audience;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The type name of the context parameter (`RayfinContext<Schema, Audiences>`).
const CONTEXT_TYPE_NAME: &str = "RayfinContext";

/// `runtimemetadata.json`: every `udf.func(...)` registration of the functions package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeMetadata {
    pub schema_version: String,
    pub functions: Vec<FunctionMetadata>,
    /// Fields this version does not model, kept so that a load/save round trip loses nothing.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One registered function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionMetadata {
    pub function_name: String,
    /// The handler's parameters, in order.
    pub delegate_parameters: Vec<ParameterMetadata>,
    /// The audiences declared on the handler's `RayfinContext<Schema, Audiences>` annotation, as
    /// `AudienceType` member names (`ADO`, `AzureAI`, ...). Absent only in files written by CLIs
    /// older than 1.36.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_audiences: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// One handler parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParameterMetadata {
    /// The parameter's source text (`{ a, b }` for a destructured parameter).
    pub name: String,
    /// The annotation's source text; absent when the parameter has none.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    pub optional: bool,
    pub has_default: bool,
    pub position: u32,
    pub is_fabric_parameter: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fabric_parameter_type: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl RuntimeMetadata {
    /// The function registered as `name`.
    pub fn function(&self, name: &str) -> Option<&FunctionMetadata> {
        self.functions
            .iter()
            .find(|function| function.function_name == name)
    }

    /// The registered function names, in file order.
    pub fn function_names(&self) -> impl Iterator<Item = &str> {
        self.functions
            .iter()
            .map(|function| function.function_name.as_str())
    }

    /// The audiences function `name` gets tokens for; empty for an unknown function. See
    /// [`FunctionMetadata::audiences`].
    pub fn audiences(&self, name: &str) -> Vec<Audience> {
        self.function(name)
            .map(FunctionMetadata::audiences)
            .unwrap_or_default()
    }

    /// Checks the document the way the UDF package does before using it; the package fails every
    /// invocation when this fails. The error says what is wrong.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(key) = self.extra.keys().next() {
            return Err(format!("unknown field `{key}`"));
        }
        if !is_supported_schema_version(&self.schema_version) {
            return Err(format!(
                "schema version `{}` is not supported (the functions runtime reads 2.x)",
                self.schema_version
            ));
        }
        let mut names = HashSet::new();
        for (index, function) in self.functions.iter().enumerate() {
            let at = format!("functions[{index}]");
            if let Some(key) = function.extra.keys().next() {
                return Err(format!("{at} has an unknown field `{key}`"));
            }
            if function.function_name.trim().is_empty() {
                return Err(format!("{at}.functionName is empty"));
            }
            if !names.insert(function.function_name.as_str()) {
                return Err(format!(
                    "{at}.functionName duplicates `{}`",
                    function.function_name
                ));
            }
            match &function.context_audiences {
                Some(audiences) if audiences.iter().any(String::is_empty) => {
                    return Err(format!("{at}.contextAudiences has an empty name"));
                }
                None if function
                    .delegate_parameters
                    .iter()
                    .any(|parameter| parameter.ty.as_deref().is_some_and(declares_audiences)) =>
                {
                    return Err(format!(
                        "function `{}` declares context audiences but the file has no \
                         contextAudiences (it was written by a Rayfin CLI older than 1.36)",
                        function.function_name
                    ));
                }
                _ => {}
            }
            for (position, parameter) in function.delegate_parameters.iter().enumerate() {
                parameter.validate(&format!("{at}.delegateParameters[{position}]"), position)?;
            }
        }
        Ok(())
    }
}

impl FunctionMetadata {
    /// The audiences this function gets tokens for: its `contextAudiences` that are
    /// `AudienceType` members, matched exactly as the UDF package matches them. Other names are
    /// skipped with a warning (once per function and name), since the package binds no token
    /// for them either.
    pub fn audiences(&self) -> Vec<Audience> {
        let mut audiences = Vec::new();
        for name in self.context_audiences.iter().flatten() {
            match Audience::UDF
                .into_iter()
                .find(|audience| audience.udf_name() == name)
            {
                Some(audience) if !audiences.contains(&audience) => audiences.push(audience),
                Some(_) => {}
                None => warn_unknown_audience(&self.function_name, name),
            }
        }
        audiences
    }
}

impl ParameterMetadata {
    /// Whether this is the context parameter, which the runtime supplies (never the caller).
    pub fn is_context(&self) -> bool {
        self.ty.as_deref().is_some_and(is_context_type)
    }

    fn validate(&self, at: &str, position: usize) -> Result<(), String> {
        if let Some(key) = self.extra.keys().next() {
            return Err(format!("{at} has an unknown field `{key}`"));
        }
        if self.name.is_empty() {
            return Err(format!("{at}.name is empty"));
        }
        if self.ty.as_deref() == Some("") {
            return Err(format!("{at}.type is empty"));
        }
        if usize::try_from(self.position).ok() != Some(position) {
            return Err(format!(
                "{at}.position is {}, not {position}",
                self.position
            ));
        }
        if self.fabric_parameter_type.as_deref() == Some("") {
            return Err(format!("{at}.fabricParameterType is empty"));
        }
        if !self.is_fabric_parameter && self.fabric_parameter_type.is_some() {
            return Err(format!(
                "{at}.fabricParameterType is set but isFabricParameter is false"
            ));
        }
        if self.is_fabric_parameter && self.ty.is_some() && self.fabric_parameter_type != self.ty {
            return Err(format!("{at}.fabricParameterType does not match its type"));
        }
        Ok(())
    }
}

/// `major.minor` with major 2: the UDF package rejects other majors and older files.
fn is_supported_schema_version(version: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    version.split_once('.').is_some_and(|(major, minor)| {
        digits(major) && digits(minor) && major.trim_start_matches('0') == "2"
    })
}

/// The UDF package's `isRayfinContextType`.
fn is_context_type(ty: &str) -> bool {
    ty == CONTEXT_TYPE_NAME
        || ty
            .strip_prefix(CONTEXT_TYPE_NAME)
            .is_some_and(|rest| rest.starts_with('<'))
}

/// Whether a context annotation names audiences (a second type argument other than `never`).
fn declares_audiences(ty: &str) -> bool {
    is_context_type(ty)
        && split_type_arguments(ty)
            .get(1)
            .map(|argument| argument.trim())
            .is_some_and(|argument| !argument.is_empty() && argument != "never")
}

/// The top-level arguments of `Name<A, B, ...>`, without a parser: nested brackets, `=>` and
/// string literals are skipped. An unterminated list keeps what is there.
fn split_type_arguments(ty: &str) -> Vec<&str> {
    let Some(open) = ty.find('<') else {
        return Vec::new();
    };
    let bytes = ty.as_bytes();
    let mut arguments = Vec::new();
    let mut depth = 0usize;
    let mut start = open + 1;
    let mut quote = None;
    for i in open + 1..bytes.len() {
        let ch = bytes[i];
        if let Some(q) = quote {
            if ch == q && bytes[i - 1] != b'\\' {
                quote = None;
            }
        } else if matches!(ch, b'\'' | b'"' | b'`') {
            quote = Some(ch);
        } else if matches!(ch, b'<' | b'(' | b'[' | b'{') {
            depth += 1;
        } else if ch == b'>' && bytes[i - 1] == b'=' {
            // The arrow of a function type, not a closing bracket.
        } else if matches!(ch, b'>' | b')' | b']' | b'}') {
            if depth == 0 {
                arguments.push(&ty[start..i]);
                return arguments;
            }
            depth -= 1;
        } else if ch == b',' && depth == 0 {
            arguments.push(&ty[start..i]);
            start = i + 1;
        }
    }
    arguments.push(&ty[start..]);
    arguments
}

fn warn_unknown_audience(function: &str, audience: &str) {
    static WARNED: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());
    let first = WARNED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert((function.to_owned(), audience.to_owned()));
    if first {
        tracing::warn!(
            "function `{function}`: `{audience}` in its RayfinContext annotation is not an \
             AudienceType member, so it gets no token for it. Declare audiences literally, e.g. \
             RayfinContext<Schema, AudienceType.Sql>."
        );
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parameter(name: &str, ty: Option<&str>, position: u32) -> ParameterMetadata {
        ParameterMetadata {
            name: name.into(),
            ty: ty.map(Into::into),
            optional: false,
            has_default: false,
            position,
            is_fabric_parameter: false,
            fabric_parameter_type: None,
            extra: Map::new(),
        }
    }

    fn function(name: &str, audiences: Option<&[&str]>) -> FunctionMetadata {
        FunctionMetadata {
            function_name: name.into(),
            delegate_parameters: vec![
                parameter("id", Some("number"), 0),
                parameter("ctx", Some("RayfinContext<Schema, AudienceType.ADO>"), 1),
            ],
            context_audiences: audiences
                .map(|names| names.iter().map(|name| name.to_string()).collect()),
            extra: Map::new(),
        }
    }

    fn metadata(functions: Vec<FunctionMetadata>) -> RuntimeMetadata {
        RuntimeMetadata {
            schema_version: "2.0".into(),
            functions,
            extra: Map::new(),
        }
    }

    #[test]
    fn unknown_fields_round_trip() {
        let document = json!({
            "schemaVersion": "2.1",
            "functions": [{
                "functionName": "f",
                "delegateParameters": [{
                    "name": "x",
                    "optional": true,
                    "hasDefault": false,
                    "position": 0,
                    "isFabricParameter": false,
                    "future": [1, 2]
                }],
                "contextAudiences": [],
                "capabilities": ["sql"]
            }],
            "generator": {"name": "cli"}
        });
        let parsed: RuntimeMetadata = serde_json::from_value(document.clone()).unwrap();
        assert_eq!(parsed.functions[0].delegate_parameters[0].ty, None);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), document);
        // The 1.36 runtime rejects fields it does not know.
        assert_eq!(parsed.validate().unwrap_err(), "unknown field `generator`");
    }

    #[test]
    fn validation_follows_the_runtime() {
        assert_eq!(
            metadata(vec![function("f", Some(&["ADO"]))]).validate(),
            Ok(())
        );

        let mut versioned = metadata(vec![]);
        versioned.schema_version = "1.0".into();
        assert!(
            versioned
                .validate()
                .unwrap_err()
                .contains("schema version `1.0`")
        );
        versioned.schema_version = "2.15".into();
        assert_eq!(versioned.validate(), Ok(()));
        versioned.schema_version = "3.0".into();
        assert!(versioned.validate().is_err());

        let duplicate = metadata(vec![function("f", Some(&[])), function("f", Some(&[]))]);
        assert!(duplicate.validate().unwrap_err().contains("duplicates `f`"));

        let legacy = metadata(vec![function("f", None)]);
        assert!(legacy.validate().unwrap_err().contains("older than 1.36"));
        let mut no_audiences = function("g", None);
        no_audiences.delegate_parameters[1].ty = Some("RayfinContext<Schema, never>".into());
        assert_eq!(metadata(vec![no_audiences]).validate(), Ok(()));

        let mut misplaced = function("f", Some(&["ADO"]));
        misplaced.delegate_parameters[1].position = 3;
        assert!(
            metadata(vec![misplaced])
                .validate()
                .unwrap_err()
                .contains("position is 3")
        );

        let mut fabric = function("f", Some(&["ADO"]));
        fabric.delegate_parameters[1].is_fabric_parameter = true;
        assert!(
            metadata(vec![fabric.clone()])
                .validate()
                .unwrap_err()
                .contains("does not match its type")
        );
        fabric.delegate_parameters[1].fabric_parameter_type =
            fabric.delegate_parameters[1].ty.clone();
        assert_eq!(metadata(vec![fabric]).validate(), Ok(()));
    }

    #[test]
    fn audiences_are_exact_audience_type_members() {
        let document = metadata(vec![
            function(
                "ask",
                Some(&["ADO", "AzureAI", "ADO", "ado", "Kusto", "PowerBI"]),
            ),
            function("plain", Some(&[])),
        ]);
        assert_eq!(
            document.audiences("ask"),
            vec![Audience::Ado, Audience::AzureAi]
        );
        assert_eq!(document.audiences("plain"), vec![]);
        assert_eq!(document.audiences("missing"), vec![]);
        assert_eq!(document.function("plain").unwrap().function_name, "plain");
        assert_eq!(
            document.function_names().collect::<Vec<_>>(),
            ["ask", "plain"]
        );
    }

    #[test]
    fn context_parameters_are_recognised_by_their_annotation() {
        assert!(parameter("ctx", Some("RayfinContext"), 0).is_context());
        assert!(parameter("c", Some("RayfinContext<{ a: 1 }, never>"), 0).is_context());
        assert!(!parameter("ctx", Some("udf.RayfinContext<S>"), 0).is_context());
        assert!(!parameter("ctx", Some("RayfinContextual"), 0).is_context());
        assert!(!parameter("ctx", None, 0).is_context());
    }

    #[test]
    fn type_arguments_split_at_the_top_level() {
        assert_eq!(
            split_type_arguments("RayfinContext<Record<string, unknown>, A.B | A.C>"),
            ["Record<string, unknown>", " A.B | A.C"]
        );
        assert_eq!(
            split_type_arguments("RayfinContext<{ f: (a: number) => void; s: '>,' }, never>"),
            ["{ f: (a: number) => void; s: '>,' }", " never"]
        );
        assert_eq!(split_type_arguments("RayfinContext<S, X"), ["S", " X"]);
        assert!(declares_audiences("RayfinContext<S, AudienceType.Sql>"));
        assert!(!declares_audiences("RayfinContext<S, never>"));
        assert!(!declares_audiences("RayfinContext<S>"));
    }
}
