//! Input coercion: literals (and variables) checked against their schema type and turned into
//! [`Input`] values, as Hot Chocolate coerces them.
//!
//! Scalars stay JSON (the shape `rl_db::codec` takes) once their literal kind and format are
//! checked: `Int` (32-bit) from an integer, `Decimal`/`Float` from an integer or a float,
//! `String`/`UUID`/`DateTime` from a string (UUIDs and date-times must parse), `Boolean` from a
//! boolean, `ID` from a string or an integer.

use std::collections::HashMap;

use async_graphql_parser::Pos;
use rl_db::codec;
use rl_model::FieldKind;
use serde_json::{Map, Number, Value as Json};

use crate::schema::{InputValueDef, Schema, TypeDef, TypeRef};
use crate::value::{PKind, PValue};

/// A coerced input value.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    Null,
    Scalar(Json),
    Enum(String),
    List(Vec<Input>),
    /// Fields in document order (unique), with their positions.
    Object(Vec<(String, Input, Pos)>),
}

impl Input {
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn field(&self, name: &str) -> Option<&Input> {
        match self {
            Self::Object(fields) => fields.iter().find(|f| f.0 == name).map(|f| &f.1),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Input, Pos)]> {
        match self {
            Self::Object(fields) => Some(fields),
            _ => None,
        }
    }
}

/// Why a value does not fit its type.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    /// The value (inside input field `field`, or the argument itself) has the wrong type.
    NotCompatible {
        field: Option<String>,
        field_type: Option<String>,
        pos: Pos,
    },
    UnknownField {
        name: String,
        pos: Pos,
    },
    DuplicateField {
        name: String,
        pos: Pos,
    },
    MissingField {
        name: String,
        pos: Pos,
    },
    /// A variable whose declared type does not fit the location.
    Variable {
        name: String,
        pos: Pos,
    },
    UndeclaredVariable {
        name: String,
        pos: Pos,
    },
}

/// Variable declarations and (when executing) values.
pub struct Variables<'a> {
    /// Declared type and whether a default exists.
    pub types: &'a HashMap<String, (TypeRef, bool)>,
    /// `None` while validating: variables are then only checked for type compatibility.
    pub values: Option<&'a Map<String, Json>>,
}

/// The FieldKind standing for a GraphQL scalar in the codec.
pub fn scalar_kind(scalar: &str) -> Option<FieldKind> {
    Some(match scalar {
        "UUID" => FieldKind::Uuid,
        "String" => FieldKind::Text {
            min_length: None,
            max_length: None,
            pattern: None,
        },
        "Int" => FieldKind::Int {
            min: None,
            max: None,
        },
        "Decimal" => FieldKind::Decimal {
            precision: 38,
            scale: 38,
            min: None,
            max: None,
        },
        "Boolean" => FieldKind::Boolean,
        "DateTime" => FieldKind::Date,
        _ => return None,
    })
}

/// Coerces a literal of type `ty`; `field` names the input field holding it (for messages).
pub fn literal(
    schema: &Schema,
    ty: &TypeRef,
    value: &PValue,
    field: Option<&InputValueDef>,
    vars: &Variables<'_>,
) -> Result<Input, Problem> {
    let incompatible = || Problem::NotCompatible {
        field: field.map(|f| f.name.clone()),
        field_type: field.map(|f| f.ty.to_string()),
        pos: value.pos,
    };
    if let PKind::Variable(name) = &value.kind {
        return variable(schema, ty, name, value.pos, field, vars);
    }
    match ty {
        TypeRef::NonNull(inner) => {
            if value.kind == PKind::Null {
                return Err(incompatible());
            }
            literal(schema, inner, value, field, vars)
        }
        _ if value.kind == PKind::Null => Ok(Input::Null),
        TypeRef::List(inner) => match &value.kind {
            PKind::List(items) => items
                .iter()
                .map(|item| literal(schema, inner, item, field, vars))
                .collect::<Result<_, _>>()
                .map(Input::List),
            _ => Ok(Input::List(vec![literal(
                schema, inner, value, field, vars,
            )?])),
        },
        TypeRef::Named(name) => match schema.types.get(name) {
            Some(TypeDef::Scalar(..)) => scalar_literal(name, &value.kind).ok_or_else(incompatible),
            Some(TypeDef::Enum(def)) => match &value.kind {
                PKind::Enum(v) if def.values.contains(v) => Ok(Input::Enum(v.clone())),
                _ => Err(incompatible()),
            },
            Some(TypeDef::InputObject(def)) => {
                let PKind::Object(fields) = &value.kind else {
                    return Err(incompatible());
                };
                let mut out: Vec<(String, Input, Pos)> = Vec::with_capacity(fields.len());
                for f in fields {
                    if out.iter().any(|o| o.0 == f.name) {
                        return Err(Problem::DuplicateField {
                            name: f.name.clone(),
                            pos: f.pos,
                        });
                    }
                    let Some(def_field) = def.fields.get(&f.name) else {
                        if f.name == "isNull" && field.is_some_and(|d| d.accepts_is_null) {
                            let boolean = TypeRef::named("Boolean");
                            let v =
                                literal(schema, &boolean, &f.value, None, vars).map_err(|_| {
                                    Problem::NotCompatible {
                                        field: Some("isNull".into()),
                                        field_type: Some("Boolean".into()),
                                        pos: f.value.pos,
                                    }
                                })?;
                            out.push((f.name.clone(), v, f.pos));
                            continue;
                        }
                        return Err(Problem::UnknownField {
                            name: f.name.clone(),
                            pos: f.pos,
                        });
                    };
                    let v = literal(schema, &def_field.ty, &f.value, Some(def_field), vars)?;
                    out.push((f.name.clone(), v, f.pos));
                }
                for (name, def_field) in &def.fields {
                    let given = out.iter().any(|o| &o.0 == name);
                    if !given && def_field.ty.is_non_null() && def_field.default.is_none() {
                        return Err(Problem::MissingField {
                            name: name.clone(),
                            pos: value.pos,
                        });
                    }
                }
                Ok(Input::Object(out))
            }
            _ => Err(incompatible()),
        },
    }
}

fn variable(
    schema: &Schema,
    ty: &TypeRef,
    name: &str,
    pos: Pos,
    field: Option<&InputValueDef>,
    vars: &Variables<'_>,
) -> Result<Input, Problem> {
    let Some((declared, has_default)) = vars.types.get(name) else {
        return Err(Problem::UndeclaredVariable {
            name: name.into(),
            pos,
        });
    };
    let location_default = field.is_some_and(|f| f.default.is_some());
    if !usage_allowed(declared, *has_default || location_default, ty) {
        return Err(Problem::Variable {
            name: name.into(),
            pos,
        });
    }
    match vars.values {
        None => Ok(Input::Null),
        Some(values) => match values.get(name) {
            None | Some(Json::Null) => Ok(Input::Null),
            Some(value) => json(schema, ty, value, field).ok_or(Problem::Variable {
                name: name.into(),
                pos,
            }),
        },
    }
}

/// The spec's `IsVariableUsageAllowed`.
fn usage_allowed(variable: &TypeRef, has_default: bool, location: &TypeRef) -> bool {
    if let TypeRef::NonNull(inner) = location
        && !variable.is_non_null()
    {
        return has_default && compatible(variable, inner);
    }
    compatible(variable, location)
}

fn compatible(variable: &TypeRef, location: &TypeRef) -> bool {
    match (variable, location) {
        (TypeRef::NonNull(v), TypeRef::NonNull(l)) => compatible(v, l),
        (TypeRef::NonNull(v), l) => compatible(v, l),
        (_, TypeRef::NonNull(_)) => false,
        (TypeRef::List(v), TypeRef::List(l)) => compatible(v, l),
        (TypeRef::List(_), _) | (_, TypeRef::List(_)) => false,
        (TypeRef::Named(v), TypeRef::Named(l)) => v == l,
    }
}

fn scalar_literal(scalar: &str, kind: &PKind) -> Option<Input> {
    let number = |text: &str| text.parse::<Number>().ok().map(Json::Number);
    let json = match (scalar, kind) {
        ("Int", PKind::Int(text)) => text.parse::<i32>().ok().map(|n| Json::Number(n.into())),
        ("Float" | "Decimal", PKind::Int(text) | PKind::Float(text)) => number(text),
        ("String" | "UUID" | "DateTime" | "ID", PKind::String(text)) => {
            Some(Json::String(text.clone()))
        }
        ("ID", PKind::Int(text)) => Some(Json::String(text.clone())),
        ("Boolean", PKind::Boolean(b)) => Some(Json::Bool(*b)),
        _ => None,
    }?;
    scalar_value(scalar, json)
}

/// Checks a scalar's format (UUIDs, date-times, finite numbers); UUIDs come back canonical.
fn scalar_value(scalar: &str, json: Json) -> Option<Input> {
    if let Json::Number(n) = &json
        && n.as_f64().is_none_or(|f| !f.is_finite())
    {
        return None;
    }
    match scalar_kind(scalar) {
        Some(FieldKind::Uuid) => match codec::encode_operand(&FieldKind::Uuid, &json).ok()? {
            rusqlite::types::Value::Text(text) => Some(Input::Scalar(Json::String(text))),
            _ => None,
        },
        Some(kind @ FieldKind::Date) => {
            codec::encode_operand(&kind, &json).ok()?;
            Some(Input::Scalar(json))
        }
        _ => Some(Input::Scalar(json)),
    }
}

/// Coerces a JSON variable value to `ty` (`None` when it does not fit); `field` is the input
/// field holding it, as for [`literal`].
pub fn json(
    schema: &Schema,
    ty: &TypeRef,
    value: &Json,
    field: Option<&InputValueDef>,
) -> Option<Input> {
    match (ty, value) {
        (TypeRef::NonNull(_), Json::Null) => None,
        (TypeRef::NonNull(inner), value) => json(schema, inner, value, field),
        (_, Json::Null) => Some(Input::Null),
        (TypeRef::List(inner), Json::Array(items)) => items
            .iter()
            .map(|item| json(schema, inner, item, field))
            .collect::<Option<_>>()
            .map(Input::List),
        (TypeRef::List(inner), value) => {
            Some(Input::List(vec![json(schema, inner, value, field)?]))
        }
        (TypeRef::Named(name), value) => match schema.types.get(name)? {
            TypeDef::Scalar(..) => {
                let ok = match (name.as_str(), value) {
                    ("Int", Json::Number(n)) => {
                        n.as_i64().is_some_and(|n| i32::try_from(n).is_ok())
                    }
                    ("Float" | "Decimal", Json::Number(_)) => true,
                    ("String" | "UUID" | "DateTime" | "ID", Json::String(_)) => true,
                    ("ID", Json::Number(n)) => n.is_i64(),
                    ("Boolean", Json::Bool(_)) => true,
                    _ => false,
                };
                ok.then(|| scalar_value(name, value.clone())).flatten()
            }
            TypeDef::Enum(def) => match value {
                Json::String(v) if def.values.contains(v) => Some(Input::Enum(v.clone())),
                _ => None,
            },
            TypeDef::InputObject(def) => {
                let Json::Object(object) = value else {
                    return None;
                };
                let mut out = Vec::with_capacity(object.len());
                for (key, v) in object {
                    let coerced = match def.fields.get(key) {
                        Some(f) => json(schema, &f.ty, v, Some(f))?,
                        None if key == "isNull" && field.is_some_and(|d| d.accepts_is_null) => {
                            json(schema, &TypeRef::named("Boolean"), v, None)?
                        }
                        None => return None,
                    };
                    out.push((key.clone(), coerced, Pos::default()));
                }
                let complete = def.fields.iter().all(|(name, f)| {
                    !f.ty.is_non_null() || f.default.is_some() || object.contains_key(name)
                });
                complete.then_some(Input::Object(out))
            }
            TypeDef::Object(_) => None,
        },
    }
}
