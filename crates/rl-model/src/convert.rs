//! [`AppModel::from_analysis`]: the `js/analyze.mjs` result → the IR.
//!
//! The DAB config (`config`) is the ground truth for tables, columns, constraints, relationships
//! and permissions, as on Fabric. The analyzer's entities add the decorator options the config
//! does not carry (`@set` values, lengths, precision, patterns), and `policies` the policy trees.

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::model::*;
use crate::policy::PolicyExpr;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    core_version: Option<String>,
    typescript_version: Option<String>,
    dialect: String,
    feature_flags: Option<String>,
    entities: Vec<AnalyzedEntity>,
    config: DabConfig,
    #[serde(default)]
    policies: Vec<PolicyRecord>,
    storage: Option<StorageOutput>,
    #[serde(default)]
    sources: Sources,
    #[serde(default)]
    diagnostics: Vec<Diagnostic>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyzedEntity {
    name: String,
    table_name: String,
    fields: Vec<AnalyzedField>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnalyzedField {
    name: String,
    #[serde(default)]
    primary_key: bool,
    #[serde(default)]
    is_relationship: bool,
    generated_foreign_key_column: Option<String>,
    column_name: Option<String>,
    original_field_metadata: Option<FieldMetadata>,
}

#[derive(Deserialize)]
struct FieldMetadata {
    format: Option<String>,
    #[serde(rename = "enum")]
    values: Option<Vec<String>>,
    min: Option<f64>,
    max: Option<f64>,
    precision: Option<f64>,
    scale: Option<f64>,
    regex: Option<RegexJson>,
}

#[derive(Deserialize)]
struct RegexJson {
    #[serde(rename = "$regex")]
    source: String,
    #[serde(default)]
    flags: String,
}

#[derive(Deserialize)]
struct DabConfig {
    entities: IndexMap<String, DabEntity>,
}

#[derive(Deserialize)]
struct DabEntity {
    source: String,
    permissions: Vec<DabPermission>,
    #[serde(default)]
    relationships: IndexMap<String, DabRelationship>,
    #[serde(default)]
    mappings: IndexMap<String, String>,
    #[serde(rename = "x-schema")]
    x_schema: XSchema,
}

#[derive(Deserialize)]
struct DabPermission {
    role: String,
    actions: Vec<Value>,
}

#[derive(Deserialize)]
struct DabRelationship {
    cardinality: String,
    #[serde(rename = "target.entity")]
    target_entity: String,
    #[serde(rename = "source.fields", default)]
    source_fields: Vec<String>,
    #[serde(rename = "target.fields", default)]
    target_fields: Vec<String>,
    linking: Option<Value>,
}

#[derive(Deserialize)]
struct XSchema {
    fields: IndexMap<String, XField>,
    constraints: XConstraints,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct XField {
    db_type: String,
    nullable: bool,
    default_value: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct XConstraints {
    primary_key: Option<Key>,
    #[serde(default)]
    foreign_keys: Vec<XForeignKey>,
    #[serde(default)]
    unique_constraints: Vec<Key>,
    #[serde(default)]
    check_constraints: Vec<CheckConstraint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct XForeignKey {
    name: String,
    columns: Vec<String>,
    referenced_table: String,
    referenced_columns: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PolicyRecord {
    entity: String,
    role: String,
    index: usize,
    text: String,
    check: Option<Value>,
    check_error: Option<String>,
}

#[derive(Deserialize)]
struct StorageOutput {
    config: StorageConfig,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageConfig {
    schema_version: u32,
    folders: Vec<FolderJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderJson {
    name: String,
    display_name: String,
    on_conflict: String,
    max_size: Option<u64>,
    #[serde(default)]
    allowed_content_types: Vec<String>,
    #[serde(default)]
    fields: Vec<FolderFieldJson>,
    rules: Vec<RuleJson>,
}

#[derive(Deserialize)]
struct FolderFieldJson {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    nullable: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleJson {
    path_pattern: String,
    role: String,
    actions: Vec<String>,
    check: Option<Value>,
}

/// The system entity every app may reference but no app database contains.
const SYSTEM_USER: &str = "User";

impl AppModel {
    /// Builds the model from the `result` of `js/analyze.mjs`.
    pub fn from_analysis(analysis: &Value) -> Result<AppModel, ModelError> {
        let output =
            Output::deserialize(analysis).map_err(|e| ModelError::Malformed(e.to_string()))?;
        let dialect = Dialect::parse(&output.dialect).ok_or_else(|| {
            ModelError::Malformed(format!("unknown dialect `{}`", output.dialect))
        })?;
        let entities = output
            .config
            .entities
            .iter()
            .map(|(name, dab)| entity(&output, name, dab))
            .collect::<Result<Vec<_>, _>>()?;
        let storage = output.storage.as_ref().map(storage).transpose()?;
        Ok(AppModel {
            core_version: output.core_version,
            typescript_version: output.typescript_version,
            dialect,
            feature_flags: output.feature_flags,
            entities,
            storage,
            sources: output.sources,
            diagnostics: output.diagnostics,
        })
    }
}

fn entity(output: &Output, name: &str, dab: &DabEntity) -> Result<Entity, ModelError> {
    let error = |message: String| ModelError::Entity {
        entity: name.to_owned(),
        message,
    };
    let analyzed = output
        .entities
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| error("missing from the analyzer's entities".into()))?;
    let constraints = &dab.x_schema.constraints;
    let primary_key = constraints
        .primary_key
        .clone()
        .ok_or_else(|| error("has no primary key".into()))?;

    let mut columns = Vec::with_capacity(dab.x_schema.fields.len());
    for (column, x) in &dab.x_schema.fields {
        let (kind, origin) = column_source(output, name, analyzed, column).map_err(&error)?;
        columns.push(Column {
            name: column.clone(),
            field: dab.mappings.get(column).unwrap_or(column).clone(),
            kind,
            db_type: x.db_type.clone(),
            nullable: x.nullable,
            default: x.default_value.clone(),
            primary_key: primary_key.columns.contains(column),
            unique: constraints
                .unique_constraints
                .iter()
                .any(|key| key.columns.len() == 1 && &key.columns[0] == column),
            origin,
        });
    }
    if !columns.iter().any(|c| c.primary_key) {
        return Err(error(format!(
            "primary key column {:?} is not a column",
            primary_key.columns
        )));
    }

    let foreign_keys = constraints
        .foreign_keys
        .iter()
        .map(|fk| {
            let referenced_entity = output
                .config
                .entities
                .iter()
                .find(|(_, e)| e.source == fk.referenced_table)
                .map(|(n, _)| n.clone());
            if referenced_entity.is_none() && fk.referenced_table != "Users" {
                return Err(error(format!(
                    "foreign key `{}` references unknown table `{}`",
                    fk.name, fk.referenced_table
                )));
            }
            Ok(ForeignKey {
                name: fk.name.clone(),
                columns: fk.columns.clone(),
                referenced_table: fk.referenced_table.clone(),
                referenced_columns: fk.referenced_columns.clone(),
                referenced_entity,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let relationships = dab
        .relationships
        .iter()
        .map(|(rel_name, rel)| relationship(output, &columns, rel_name, rel).map_err(&error))
        .collect::<Result<Vec<_>, _>>()?;

    let permissions = dab
        .permissions
        .iter()
        .map(|perm| role_permission(output, name, perm, &columns))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Entity {
        name: name.to_owned(),
        table: dab.source.clone(),
        graphql: GraphQlNames::new(name, &analyzed.table_name),
        columns,
        primary_key,
        foreign_keys,
        unique_constraints: constraints.unique_constraints.clone(),
        check_constraints: constraints.check_constraints.clone(),
        relationships,
        permissions,
    })
}

/// The decorator (typed) and origin of an `x-schema` column.
fn column_source(
    output: &Output,
    entity: &str,
    analyzed: &AnalyzedEntity,
    column: &str,
) -> Result<(FieldKind, ColumnOrigin), String> {
    let declared = analyzed
        .fields
        .iter()
        .find(|f| !f.is_relationship && f.column_name.as_deref().unwrap_or(&f.name) == column);
    if let Some(field) = declared {
        return match &field.original_field_metadata {
            Some(meta) => Ok((
                field_kind(meta).map_err(|e| format!("field `{}`: {e}", field.name))?,
                ColumnOrigin::Declared,
            )),
            None if field.primary_key => Ok((FieldKind::Uuid, ColumnOrigin::ImplicitPrimaryKey)),
            None => Err(format!("field `{}` has no decorator metadata", field.name)),
        };
    }
    let generated = analyzed
        .fields
        .iter()
        .find(|f| f.is_relationship && f.generated_foreign_key_column.as_deref() == Some(column));
    if let Some(field) = generated {
        let origin = ColumnOrigin::Relationship {
            relationship: field.name.clone(),
        };
        return Ok((FieldKind::Uuid, origin));
    }
    for (source, dab) in &output.config.entities {
        for (rel_name, rel) in &dab.relationships {
            if rel.cardinality == "many"
                && rel.target_entity == entity
                && rel.target_fields.len() == 1
                && rel.target_fields[0] == column
            {
                let origin = ColumnOrigin::InverseRelationship {
                    entity: source.clone(),
                    relationship: rel_name.clone(),
                };
                return Ok((FieldKind::Uuid, origin));
            }
        }
    }
    Err(format!(
        "column `{column}` matches no field or relationship"
    ))
}

fn field_kind(meta: &FieldMetadata) -> Result<FieldKind, String> {
    if let Some(values) = &meta.values {
        // As rayfin-core's TypeInferenceEngine writes the CHECK constraint.
        let allowed = values
            .iter()
            .map(|v| v.replace(['\'', '"'], ""))
            .filter(|v| v != "undefined")
            .collect();
        return Ok(FieldKind::Set {
            values: values.clone(),
            allowed,
        });
    }
    let length = |value: Option<f64>, what: &str| -> Result<Option<u32>, String> {
        match value {
            None => Ok(None),
            Some(v) if v >= 0.0 && v.fract() == 0.0 && v <= f64::from(u32::MAX) => {
                Ok(Some(v as u32).filter(|n| *n > 0))
            }
            Some(v) => Err(format!("{what} {v} is not a whole, non-negative length")),
        }
    };
    match meta.format.as_deref() {
        Some("uuid") => Ok(FieldKind::Uuid),
        Some("text") => Ok(FieldKind::Text {
            min_length: length(meta.min, "min")?,
            max_length: length(meta.max, "max")?,
            pattern: meta.regex.as_ref().map(|r| Pattern {
                source: r.source.clone(),
                flags: r.flags.clone(),
            }),
        }),
        Some("email") => Ok(FieldKind::Email {
            min_length: length(meta.min, "min")?,
            max_length: length(meta.max, "max")?,
        }),
        Some("int") => Ok(FieldKind::Int {
            min: meta.min,
            max: meta.max,
        }),
        Some("decimal") => {
            let (precision, scale) = match (meta.precision, meta.scale) {
                (Some(p), Some(s)) => (
                    length(Some(p), "precision")?.ok_or("precision 0")?,
                    length(Some(s), "scale")?.unwrap_or(0),
                ),
                _ => (18, 2),
            };
            Ok(FieldKind::Decimal {
                precision,
                scale,
                min: meta.min,
                max: meta.max,
            })
        }
        Some("boolean") => Ok(FieldKind::Boolean),
        Some("date") => Ok(FieldKind::Date),
        Some(other) => Err(format!("unknown field format `{other}`")),
        None => Err("no field format".into()),
    }
}

fn relationship(
    output: &Output,
    columns: &[Column],
    name: &str,
    rel: &DabRelationship,
) -> Result<Relationship, String> {
    if rel.linking.is_some() {
        return Err(format!(
            "relationship `{name}` uses a linking table (many-to-many)"
        ));
    }
    let cardinality = match rel.cardinality.as_str() {
        "one" => Cardinality::One,
        "many" => Cardinality::Many,
        other => return Err(format!("relationship `{name}` has cardinality `{other}`")),
    };
    let target_is_system = !output.config.entities.contains_key(&rel.target_entity);
    if target_is_system && rel.target_entity != SYSTEM_USER {
        return Err(format!(
            "relationship `{name}` targets `{}`, which is not an entity",
            rel.target_entity
        ));
    }
    let nullable = cardinality == Cardinality::One
        && rel
            .source_fields
            .iter()
            .any(|f| columns.iter().any(|c| &c.name == f && c.nullable));
    Ok(Relationship {
        name: name.to_owned(),
        cardinality,
        target: rel.target_entity.clone(),
        target_is_system,
        source_columns: rel.source_fields.clone(),
        target_columns: rel.target_fields.clone(),
        nullable,
    })
}

fn role_permission(
    output: &Output,
    entity: &str,
    perm: &DabPermission,
    columns: &[Column],
) -> Result<RolePermission, ModelError> {
    let malformed = |message: String| ModelError::Entity {
        entity: entity.to_owned(),
        message: format!("role `{}`: {message}", perm.role),
    };
    let mut actions = Vec::with_capacity(perm.actions.len());
    for (index, value) in perm.actions.iter().enumerate() {
        let grant = match value {
            Value::String(action) => ActionPermission {
                action: parse_action(action).map_err(&malformed)?,
                fields: None,
                policy: None,
            },
            Value::Object(object) => {
                expect_keys(object, &["action", "fields", "policy"]).map_err(&malformed)?;
                let action = object
                    .get("action")
                    .and_then(Value::as_str)
                    .ok_or_else(|| malformed(format!("action without a name: {value}")))?;
                let action = parse_action(action).map_err(&malformed)?;
                let fields = object
                    .get("fields")
                    .map(field_access)
                    .transpose()
                    .map_err(&malformed)?;
                let policy = match object.get("policy") {
                    None => None,
                    Some(policy) => Some(entity_policy(
                        output, entity, &perm.role, index, action, policy, columns,
                    )?),
                };
                ActionPermission {
                    action,
                    fields,
                    policy,
                }
            }
            other => return Err(malformed(format!("unexpected action {other}"))),
        };
        actions.push(grant);
    }
    Ok(RolePermission {
        role: perm.role.clone(),
        actions,
    })
}

fn parse_action(action: &str) -> Result<Action, String> {
    Action::parse(action).ok_or_else(|| format!("unknown action `{action}`"))
}

fn expect_keys(object: &Map<String, Value>, keys: &[&str]) -> Result<(), String> {
    match object.keys().find(|key| !keys.contains(&key.as_str())) {
        Some(key) => Err(format!("unsupported permission property `{key}`")),
        None => Ok(()),
    }
}

fn field_access(value: &Value) -> Result<FieldAccess, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("`fields` is not an object: {value}"))?;
    expect_keys(object, &["include", "exclude"])?;
    let list = |key: &str| -> Result<Option<Vec<String>>, String> {
        object
            .get(key)
            .map(|v| Vec::<String>::deserialize(v).map_err(|e| format!("`fields.{key}`: {e}")))
            .transpose()
    };
    Ok(FieldAccess {
        include: list("include")?.unwrap_or_else(|| vec!["*".to_owned()]),
        exclude: list("exclude")?.unwrap_or_default(),
    })
}

fn entity_policy(
    output: &Output,
    entity: &str,
    role: &str,
    index: usize,
    action: Action,
    policy: &Value,
    columns: &[Column],
) -> Result<Policy, ModelError> {
    let text = policy
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("database"))
        .and_then(Value::as_str);
    let fail = |policy: String, reason: String| ModelError::Policy {
        entity: entity.to_owned(),
        role: role.to_owned(),
        action: action.as_str().to_owned(),
        policy,
        reason,
    };
    let Some(text) = text else {
        return Err(fail(
            policy.to_string(),
            "only `policy.database` is supported".into(),
        ));
    };
    let record = output
        .policies
        .iter()
        .find(|p| p.entity == entity && p.role == role && p.index == index)
        .filter(|p| p.text == text)
        .ok_or_else(|| {
            fail(
                text.to_owned(),
                "the analysis has no expression tree for it".into(),
            )
        })?;
    let check = match (&record.check, &record.check_error) {
        (Some(check), _) => check,
        (None, Some(reason)) => return Err(fail(text.to_owned(), reason.clone())),
        (None, None) => return Err(fail(text.to_owned(), "it has no expression tree".into())),
    };
    let expr = PolicyExpr::from_check(check).map_err(|e| fail(text.to_owned(), e.reason))?;
    if let Some(unknown) = expr
        .fields()
        .into_iter()
        .find(|field| !columns.iter().any(|c| c.field == *field))
    {
        return Err(fail(
            text.to_owned(),
            format!("`item.{unknown}` is not a field of `{entity}`"),
        ));
    }
    Ok(Policy {
        text: text.to_owned(),
        expr,
    })
}

fn storage(output: &StorageOutput) -> Result<Storage, ModelError> {
    let folders = output
        .config
        .folders
        .iter()
        .map(folder)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Storage {
        schema_version: output.config.schema_version,
        folders,
    })
}

fn folder(folder: &FolderJson) -> Result<StorageFolder, ModelError> {
    let error = |message: String| ModelError::Storage {
        folder: folder.display_name.clone(),
        message,
    };
    let on_conflict = match folder.on_conflict.as_str() {
        "error" => OnConflict::Error,
        "overwrite" => OnConflict::Overwrite,
        other => return Err(error(format!("unknown onConflict `{other}`"))),
    };
    let fields = folder
        .fields
        .iter()
        .map(|field| {
            let kind = match field.kind.as_str() {
                "string" => StorageFieldType::String,
                "number" => StorageFieldType::Number,
                "boolean" => StorageFieldType::Boolean,
                "Date" => StorageFieldType::Date,
                other => return Err(error(format!("field `{}` has type `{other}`", field.name))),
            };
            Ok(StorageField {
                name: field.name.clone(),
                kind,
                nullable: field.nullable,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let rules = folder
        .rules
        .iter()
        .map(|rule| {
            let actions = rule
                .actions
                .iter()
                .map(|a| parse_action(a).map_err(&error))
                .collect::<Result<Vec<_>, _>>()?;
            let check = rule
                .check
                .as_ref()
                .map(PolicyExpr::from_check)
                .transpose()
                .map_err(|e| error(format!("rule for `{}`: {}", rule.role, e.reason)))?;
            Ok(StorageRule {
                path_pattern: rule.path_pattern.clone(),
                role: rule.role.clone(),
                actions,
                check,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(StorageFolder {
        name: folder.name.clone(),
        display_name: folder.display_name.clone(),
        on_conflict,
        max_size: folder.max_size,
        allowed_content_types: folder.allowed_content_types.clone(),
        fields,
        rules,
    })
}
