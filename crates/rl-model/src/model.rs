//! The app model (IR): entities with their columns, keys, relationships and permissions, and the
//! storage folders, as `@microsoft/rayfin-core`'s analyzer and DAB config generator define them.
//!
//! Naming (compatibility.md §6.2): `E` is the entity name (the class name, or `@entity('E')`),
//! `P` the analyzer's plural of `E` (rayfin-lib `EntityNameResolver.getPlural`, i.e. pluralize 8,
//! the same function the SDK uses for list queries; also the table name). GraphQL names follow the
//! SDK: list query `lowerFirst(P)`, by-key query `lowerFirst(E)_by_pk`, mutations `createE`,
//! `updateE`, `deleteE`, types `E`, `EConnection`, `EFilterInput`, ...
//!
//! Field names: a column has a SQL name ([`Column::name`]) and an exposed name
//! ([`Column::field`]); they differ only for `@text({ column: 'x' })` renames (DAB `mappings`).
//! GraphQL, policies (`@item.<field>`) and field permissions use exposed names.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::policy::PolicyExpr;

/// Why an analysis result cannot become an [`AppModel`].
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ModelError {
    #[error(
        "the schema analysis output is malformed ({0}); this @microsoft/rayfin-core version may \
         not be supported by rayfin-local (it targets 1.36.x)"
    )]
    Malformed(String),
    #[error("entity `{entity}`: {message}")]
    Entity { entity: String, message: String },
    #[error(
        "the `{role}` `{action}` policy of entity `{entity}` cannot be enforced by rayfin-local: \
         {reason} (policy: {policy})"
    )]
    Policy {
        entity: String,
        role: String,
        action: String,
        policy: String,
        reason: String,
    },
    #[error("storage folder `{folder}`: {message}")]
    Storage { folder: String, message: String },
}

/// The SQL dialect of `services.data.dialect`. It decides the DAB `x-schema` column types
/// ([`Column::db_type`]) and constraint expressions; SQLite stands in for both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Mssql,
    Postgresql,
}

impl Dialect {
    /// The `rayfin.yml` spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mssql => "mssql",
            Self::Postgresql => "postgresql",
        }
    }

    /// Parses the `rayfin.yml` spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "mssql" => Some(Self::Mssql),
            "postgresql" => Some(Self::Postgresql),
            _ => None,
        }
    }
}

/// Everything the data, GraphQL and storage services need to know about the app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppModel {
    /// The project's `@microsoft/rayfin-core` version that produced the analysis.
    pub core_version: Option<String>,
    /// The project's TypeScript version that compiled `rayfin/`.
    pub typescript_version: Option<String>,
    pub dialect: Dialect,
    /// `RAYFIN_FEATURE_FLAGS` during the analysis (`cli-minor-fixes` adds range CHECKs and
    /// case-sensitive enum CHECKs, as it does for `rayfin up`).
    pub feature_flags: Option<String>,
    /// In the DAB config's order: entity modules sorted by file name, then their exports.
    pub entities: Vec<Entity>,
    /// `None` when storage was not analyzed (`services.storage` disabled).
    pub storage: Option<Storage>,
    /// The modules the model came from, relative to the project.
    pub sources: Sources,
    /// Non-fatal findings of the analysis.
    pub diagnostics: Vec<Diagnostic>,
}

/// Project-relative paths (with `/`) of the analysed modules.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sources {
    /// The entity modules imported (top-level files of `rayfin/data`).
    pub data: Vec<String>,
    /// The `@blob` modules imported (top-level files of `rayfin/storage`).
    pub storage: Vec<String>,
    /// Every project file the TypeScript program read (to watch for changes).
    pub inputs: Vec<String>,
}

/// A non-fatal finding, e.g. a data file the tsconfig does not include.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// `warning` today.
    pub level: String,
    pub message: String,
}

/// A data entity: one table and one GraphQL type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    /// `E`: the DAB entity and GraphQL type name.
    pub name: String,
    /// The table (DAB `source`), `P` for data entities.
    pub table: String,
    pub graphql: GraphQlNames,
    /// Every column of the table in DDL order (DAB `x-schema.fields`), including relationship
    /// foreign keys. Each one is also a GraphQL scalar field named [`Column::field`].
    pub columns: Vec<Column>,
    /// Always the single `id` column for data entities (`PK_<table>`).
    pub primary_key: Key,
    /// `FK_<table>_<column>`; none cascade.
    pub foreign_keys: Vec<ForeignKey>,
    /// `UQ_<table>_<column>`.
    pub unique_constraints: Vec<Key>,
    /// The dialect's CHECK expressions (`CK_<table>_<column>[suffix]`), for reference: they are
    /// T-SQL/PostgreSQL. [`FieldKind`] carries the same rules in typed form.
    pub check_constraints: Vec<CheckConstraint>,
    /// The `@one`/`@many` fields, in declaration order.
    pub relationships: Vec<Relationship>,
    /// The DAB permissions: roles sorted by name. With no `@role` at all the analyzer grants
    /// `authenticated: *`.
    pub permissions: Vec<RolePermission>,
}

/// The GraphQL names DAB serves for an entity and the SDK queries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphQlNames {
    /// `E`.
    pub type_name: String,
    /// `P`.
    pub plural: String,
    /// `lowerFirst(P)`, e.g. `todos`, `people`.
    pub list_query: String,
    /// `lowerFirst(E)_by_pk`, argument `id`.
    pub by_pk_query: String,
    /// `createE(item:)`.
    pub create_mutation: String,
    /// `updateE(id:, item:)`.
    pub update_mutation: String,
    /// `deleteE(id:)`.
    pub delete_mutation: String,
}

impl GraphQlNames {
    /// The names for entity `name` with plural `plural`.
    pub fn new(name: &str, plural: &str) -> Self {
        Self {
            type_name: name.to_owned(),
            plural: plural.to_owned(),
            list_query: lower_first(plural),
            by_pk_query: format!("{}_by_pk", lower_first(name)),
            create_mutation: format!("create{name}"),
            update_mutation: format!("update{name}"),
            delete_mutation: format!("delete{name}"),
        }
    }

    /// `EConnection { items, endCursor, hasNextPage, groupBy }`.
    pub fn connection_type(&self) -> String {
        format!("{}Connection", self.type_name)
    }

    /// `EFilterInput`.
    pub fn filter_input(&self) -> String {
        format!("{}FilterInput", self.type_name)
    }

    /// `EOrderByInput`.
    pub fn order_by_input(&self) -> String {
        format!("{}OrderByInput", self.type_name)
    }

    /// `CreateEInput`.
    pub fn create_input(&self) -> String {
        format!("Create{}Input", self.type_name)
    }

    /// `UpdateEInput`.
    pub fn update_input(&self) -> String {
        format!("Update{}Input", self.type_name)
    }

    /// `EScalarFields` (groupBy field enum).
    pub fn scalar_fields_enum(&self) -> String {
        format!("{}ScalarFields", self.type_name)
    }

    /// `ENumericAggregateFields`.
    pub fn numeric_aggregate_fields_enum(&self) -> String {
        format!("{}NumericAggregateFields", self.type_name)
    }

    /// `EGroupBy { fields, aggregations }`.
    pub fn group_by_type(&self) -> String {
        format!("{}GroupBy", self.type_name)
    }

    /// `EAggregations`.
    pub fn aggregations_type(&self) -> String {
        format!("{}Aggregations", self.type_name)
    }
}

/// Lowercases only the first character, as the SDK does.
fn lower_first(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A table column.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// The SQL column name.
    pub name: String,
    /// The exposed (GraphQL, policy, field-permission) name.
    pub field: String,
    pub kind: FieldKind,
    /// The dialect type from the DAB `x-schema`, e.g. `NVARCHAR(200)`, `DECIMAL(10,4)`.
    pub db_type: String,
    pub nullable: bool,
    /// The decorator's `default` (DAB `defaultValue`): a string, number or boolean; dates are
    /// ISO-8601 strings.
    pub default: Option<Value>,
    pub primary_key: bool,
    /// A single-column unique constraint (`unique: true`).
    pub unique: bool,
    pub origin: ColumnOrigin,
    /// What the database says about the column of a Category A connector's source table
    /// (`rayfin/connectors/<name>/metadata.json`); `None` for data entities and unknown columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceColumn>,
}

/// A connector source column as `rayfin connector add` discovered it (`metadata.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceColumn {
    /// The SQL type, lowercase, e.g. `int`, `nvarchar`, `datetime2`, `date`, `float`.
    pub data_type: String,
    /// The database fills it in and refuses writes: an identity, a computed column or a
    /// server-managed one (`rowversion`, temporal period columns).
    pub generated: bool,
    /// It has a `DEFAULT`, so a create may leave it out.
    pub has_default: bool,
}

/// Where a column comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ColumnOrigin {
    /// A decorated property (including an explicitly declared FK such as `thread_id`).
    Declared,
    /// The `id` the analyzer adds to an entity that declares none. The SDK never sends `id` on
    /// create, so the database generates it, like any `id`.
    ImplicitPrimaryKey,
    /// The generated FK column `<relationship>_id` of this entity's `@one`.
    Relationship { relationship: String },
    /// The FK column `<Entity>_id` (NOT NULL) the analyzer adds for another entity's `@many`
    /// that has no `@one` back.
    InverseRelationship {
        entity: String,
        relationship: String,
    },
}

/// The typed form of a column's decorator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum FieldKind {
    /// `@uuid()`, the primary key and every FK column.
    Uuid,
    /// `@text()`: `min_length` only when it adds a CHECK (`> 0`); `max_length` `None` means
    /// unbounded (`NVARCHAR(MAX)`/`TEXT`). `pattern` is a JavaScript RegExp the SDK validates;
    /// the database does not.
    Text {
        min_length: Option<u32>,
        max_length: Option<u32>,
        pattern: Option<Pattern>,
    },
    /// `@email()`: bounded at 320 unless `max` says otherwise; the database checks
    /// `LIKE '_%@_%._%'` and no spaces.
    Email {
        min_length: Option<u32>,
        max_length: Option<u32>,
    },
    /// `@set(...)`: `values` as declared; `allowed` what the database CHECK accepts, which
    /// rayfin-core derives by deleting `'` and `"` from each value (so a declared `it's` is
    /// stored only as `its`, on Fabric too).
    Set {
        values: Vec<String>,
        allowed: Vec<String>,
    },
    /// `@int()`; `min`/`max` become a CHECK only under the `cli-minor-fixes` flag.
    Int { min: Option<f64>, max: Option<f64> },
    /// `@decimal()`: `precision`/`scale` default to 18 and 2; `min`/`max` as for `Int`.
    Decimal {
        precision: u32,
        scale: u32,
        min: Option<f64>,
        max: Option<f64>,
    },
    /// `@boolean()`.
    Boolean,
    /// `@date()`: a date-time (`DATETIME2`, `TIMESTAMP WITH TIME ZONE`).
    Date,
}

impl FieldKind {
    /// The DAB GraphQL scalar of the column.
    pub fn graphql_scalar(&self) -> &'static str {
        match self {
            Self::Uuid => "UUID",
            Self::Text { .. } | Self::Email { .. } | Self::Set { .. } => "String",
            Self::Int { .. } => "Int",
            Self::Decimal { .. } => "Decimal",
            Self::Boolean => "Boolean",
            Self::Date => "DateTime",
        }
    }

    /// Whether DAB offers the column to `sum`/`avg` (`ENumericAggregateFields`).
    pub fn is_numeric(&self) -> bool {
        matches!(self, Self::Int { .. } | Self::Decimal { .. })
    }
}

/// A JavaScript regular expression (`@text({ regex })`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pattern {
    pub source: String,
    pub flags: String,
}

/// A named key over columns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub name: String,
    pub columns: Vec<String>,
}

/// A foreign key constraint (never cascading).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
    /// `None` for the system `User` entity (`Users(Id)`), which lives in Fabric's control
    /// plane, not in the app database.
    pub referenced_entity: Option<String>,
}

/// A CHECK constraint in the dialect's SQL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckConstraint {
    pub name: String,
    pub expression: String,
}

/// A relationship field (DAB `relationships`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relationship {
    /// The GraphQL field name (the TypeScript property).
    pub name: String,
    pub cardinality: Cardinality,
    /// The target entity `E`.
    pub target: String,
    /// The target is the system `User` entity, which the app database does not have.
    pub target_is_system: bool,
    /// Columns of this entity (`[rel_id]` for `One`, `[id]` for `Many`).
    pub source_columns: Vec<String>,
    /// Columns of the target (`[id]` for `One`, `[<FK>]` for `Many`).
    pub target_columns: Vec<String>,
    /// `One` only: the FK column is nullable, so the related object may be `null`.
    pub nullable: bool,
}

/// How many rows a relationship field yields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    /// `@one`: an object or `null`.
    One,
    /// `@many`: a connection `{ items }`.
    Many,
}

/// A role's grants on an entity or storage folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RolePermission {
    /// `anonymous` or `authenticated`.
    pub role: String,
    /// In the analyzer's order (per `@role` declaration, CRUD order within one); an action may
    /// appear more than once (e.g. `read` and `*`).
    pub actions: Vec<ActionPermission>,
}

/// One granted action, with its field access and row policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionPermission {
    pub action: Action,
    /// `None`: every field.
    pub fields: Option<FieldAccess>,
    /// `None`: every row.
    pub policy: Option<Policy>,
}

/// A permission action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Read,
    Update,
    Delete,
    /// Stored procedures only; DAB has no meaning for it on tables.
    Execute,
    /// `*`: every action.
    #[serde(rename = "*")]
    All,
}

impl Action {
    /// Parses the DAB spelling.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "create" => Self::Create,
            "read" => Self::Read,
            "update" => Self::Update,
            "delete" => Self::Delete,
            "execute" => Self::Execute,
            "*" => Self::All,
            _ => return None,
        })
    }

    /// The DAB spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Read => "read",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Execute => "execute",
            Self::All => "*",
        }
    }

    /// Whether a grant of `self` covers `action`.
    pub fn covers(self, action: Action) -> bool {
        self == Self::All || self == action
    }
}

/// DAB `fields`: allowed = `include − exclude`, `*` meaning every field (exclude wins).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldAccess {
    /// Exposed names, or `*`. The analyzer writes `["*"]` for exclude-only rules.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl FieldAccess {
    /// Whether the exposed field `field` is allowed.
    pub fn allows(&self, field: &str) -> bool {
        let listed = |list: &[String]| list.iter().any(|f| f == "*" || f == field);
        listed(&self.include) && !listed(&self.exclude)
    }
}

/// A row policy: the DAB string rayfin-core generates and its typed tree.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// The DAB `policy.database` string, e.g. `@claims.sub eq @item.user_id`.
    pub text: String,
    pub expr: PolicyExpr,
}

/// The `@blob` storage folders (the storage JSON DSL rayfin-core generates).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Storage {
    pub schema_version: u32,
    /// Sorted by name.
    pub folders: Vec<StorageFolder>,
}

/// A `@blob` folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StorageFolder {
    /// The normalized container name (`[a-z0-9-]{3,63}`), used in URLs and on disk.
    pub name: String,
    /// The `@blob` class name.
    pub display_name: String,
    pub on_conflict: OnConflict,
    /// Bytes; `None` means the service limit.
    pub max_size: Option<u64>,
    /// MIME globs (`image/*`); empty allows any type.
    pub allowed_content_types: Vec<String>,
    /// App fields stored in the object's `user_metadata`.
    pub fields: Vec<StorageField>,
    /// Each rule grants `actions` to `role` when its check (if any) holds.
    pub rules: Vec<StorageRule>,
}

/// What an upload does when the path exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Answer `Conflict`.
    Error,
    /// Replace, when the role is granted `update`.
    Overwrite,
}

/// An app field of a `@blob` folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageField {
    pub name: String,
    pub kind: StorageFieldType,
    pub nullable: bool,
}

/// The JavaScript type of a [`StorageField`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageFieldType {
    String,
    Number,
    Boolean,
    Date,
}

/// A storage authorization rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StorageRule {
    /// `**` in 1.36.2.
    pub path_pattern: String,
    pub role: String,
    pub actions: Vec<Action>,
    /// Over `claims.*` and `item.<field>` (intrinsic object fields such as `owner_id`, or the
    /// folder's app fields).
    pub check: Option<PolicyExpr>,
}

/// A GraphQL root field of an entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootField {
    List,
    ByPk,
    Create,
    Update,
    Delete,
}

impl AppModel {
    /// The entity named `name` (`E`, case-sensitive).
    pub fn entity(&self, name: &str) -> Option<&Entity> {
        self.entities.iter().find(|entity| entity.name == name)
    }

    /// The entity whose table is `table`.
    pub fn entity_by_table(&self, table: &str) -> Option<&Entity> {
        self.entities.iter().find(|entity| entity.table == table)
    }

    /// The entity and kind of a GraphQL root field (`todos`, `todo_by_pk`, `createTodo`, ...).
    pub fn root_field(&self, name: &str) -> Option<(&Entity, RootField)> {
        self.entities.iter().find_map(|entity| {
            let names = &entity.graphql;
            let kind = if name == names.list_query {
                RootField::List
            } else if name == names.by_pk_query {
                RootField::ByPk
            } else if name == names.create_mutation {
                RootField::Create
            } else if name == names.update_mutation {
                RootField::Update
            } else if name == names.delete_mutation {
                RootField::Delete
            } else {
                return None;
            };
            Some((entity, kind))
        })
    }

    /// The storage folder with normalized name `name`.
    pub fn storage_folder(&self, name: &str) -> Option<&StorageFolder> {
        self.storage
            .as_ref()?
            .folders
            .iter()
            .find(|folder| folder.name == name)
    }
}

impl Entity {
    /// The column with SQL name `name`.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }

    /// The column exposed as `field`.
    pub fn field(&self, field: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.field == field)
    }

    /// The relationship field `name`.
    pub fn relationship(&self, name: &str) -> Option<&Relationship> {
        self.relationships.iter().find(|rel| rel.name == name)
    }

    /// The primary key column (`id`).
    pub fn primary_key_column(&self) -> &Column {
        self.columns
            .iter()
            .find(|column| column.primary_key)
            .expect("from_analysis checks every entity has a primary key column")
    }

    /// The primary key's columns, in key order: `id` for data entities; for connector entities
    /// the `Source({ primaryKey })` columns, which may be several or none (a keyless entity).
    pub fn key_columns(&self) -> Vec<&Column> {
        self.primary_key
            .columns
            .iter()
            .filter_map(|name| self.column(name))
            .collect()
    }

    /// The grants of `role` (`anonymous`/`authenticated`), if it has any.
    pub fn role(&self, role: &str) -> Option<&RolePermission> {
        self.permissions.iter().find(|perm| perm.role == role)
    }

    /// The grants of `role` that cover `action` (its own entries and `*` entries).
    pub fn grants(&self, role: &str, action: Action) -> impl Iterator<Item = &ActionPermission> {
        self.role(role)
            .into_iter()
            .flat_map(|perm| perm.actions.iter())
            .filter(move |grant| grant.action.covers(action))
    }
}
