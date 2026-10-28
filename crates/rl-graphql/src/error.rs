//! GraphQL errors, serialized as Hot Chocolate (the server DAB runs on) writes them:
//! `{"message", "locations", "path", "extensions"}` in that order, with DAB's messages and codes.

use async_graphql_parser::Pos;
use serde_json::{Map, Value, json};

/// DAB `DataApiBuilderException.SubStatusCodes` names, which DAB's error filter writes as
/// `extensions.code`.
pub mod codes {
    pub const BAD_REQUEST: &str = "BadRequest";
    pub const AUTHORIZATION_CHECK_FAILED: &str = "AuthorizationCheckFailed";
    pub const DATABASE_OPERATION_FAILED: &str = "DatabaseOperationFailed";
    pub const DATABASE_POLICY_FAILURE: &str = "DatabasePolicyFailure";
    pub const DATABASE_INPUT_ERROR: &str = "DatabaseInputError";
    pub const ITEM_NOT_FOUND: &str = "ItemNotFound";
    pub const UNEXPECTED_ERROR: &str = "UnexpectedError";
    /// Hot Chocolate's `@authorize` failure.
    pub const AUTH_NOT_AUTHORIZED: &str = "AUTH_NOT_AUTHORIZED";
    pub const AUTH_NOT_AUTHENTICATED: &str = "AUTH_NOT_AUTHENTICATED";
    /// Hot Chocolate: a non-null field resolved to null.
    pub const NON_NULL_VIOLATION: &str = "HC0018";
    /// Hot Chocolate: introspection is disabled.
    pub const INTROSPECTION_NOT_ALLOWED: &str = "HC0046";
    /// Hot Chocolate: the request body could not be parsed (HTTP 400).
    pub const SYNTAX_ERROR: &str = "HC0011";
    pub const REQUEST_INVALID: &str = "HC0009";
    pub const QUERY_MISSING: &str = "HC0013";
}

/// DAB's error messages (`DataApiBuilderException` and the resolvers), verbatim.
pub mod messages {
    pub const NOT_AUTHORIZED: &str = "The current user is not authorized to access this resource.";
    pub const MISSING_CLAIMS: &str =
        "User does not possess all the claims required to perform this operation.";
    pub const FILTER_ENTITY_FORBIDDEN: &str =
        "Access forbidden to the target entity described in the filter.";
    pub const FILTER_FIELD_FORBIDDEN: &str =
        "Access forbidden to a field referenced in the filter.";
    pub const ORDER_BY_FIELD_FORBIDDEN: &str =
        "Access forbidden to a field referenced in the orderBy argument.";
    pub const IN_TOO_MANY_VALUES: &str =
        "IN operator filter object cannot process more than 100 values at a time.";
    pub const DATABASE_ERROR: &str =
        "While processing your request the database ran into an error.";
    pub const NON_NULL: &str = "Cannot return null for non-nullable field.";
}

/// One segment of an error `path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

impl PathSegment {
    fn to_json(&self) -> Value {
        match self {
            Self::Key(key) => Value::String(key.clone()),
            Self::Index(index) => Value::from(*index),
        }
    }
}

/// A GraphQL error.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphQlError {
    pub message: String,
    pub locations: Vec<Pos>,
    pub path: Option<Vec<PathSegment>>,
    /// Boxed to keep Result<_, GraphQlError> small.
    pub extensions: Box<Map<String, Value>>,
}

impl GraphQlError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            locations: Vec::new(),
            path: None,
            extensions: Box::default(),
        }
    }

    /// A DAB execution error (`message`, `extensions.code` = the sub-status code).
    pub fn dab(message: impl Into<String>, code: &str) -> Self {
        Self::new(message).code(code)
    }

    /// Hot Chocolate's authorization error for a field the role may not read.
    pub fn not_authorized() -> Self {
        Self::dab(messages::NOT_AUTHORIZED, codes::AUTH_NOT_AUTHORIZED)
    }

    pub fn at(mut self, pos: Pos) -> Self {
        if pos.line > 0 {
            self.locations.push(pos);
        }
        self
    }

    pub fn with_path(mut self, path: Vec<PathSegment>) -> Self {
        self.path = Some(path);
        self
    }

    pub fn code(self, code: &str) -> Self {
        self.ext("code", code)
    }

    pub fn ext(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extensions.insert(key.to_owned(), value.into());
        self
    }

    /// `extensions.specifiedBy` of Hot Chocolate's validation errors.
    pub fn spec(self, section: &str) -> Self {
        self.ext(
            "specifiedBy",
            format!("https://spec.graphql.org/October2021/#{section}"),
        )
    }

    pub fn to_json(&self) -> Value {
        let mut error = Map::new();
        error.insert("message".into(), Value::String(self.message.clone()));
        if !self.locations.is_empty() {
            let locations = self
                .locations
                .iter()
                .map(|pos| json!({"line": pos.line, "column": pos.column}))
                .collect();
            error.insert("locations".into(), Value::Array(locations));
        }
        if let Some(path) = &self.path {
            let path = path.iter().map(PathSegment::to_json).collect();
            error.insert("path".into(), Value::Array(path));
        }
        if !self.extensions.is_empty() {
            error.insert(
                "extensions".into(),
                Value::Object((*self.extensions).clone()),
            );
        }
        Value::Object(error)
    }
}

/// `{"errors": [...]}`.
pub fn errors_body(errors: &[GraphQlError]) -> Value {
    json!({ "errors": errors.iter().map(GraphQlError::to_json).collect::<Vec<_>>() })
}
