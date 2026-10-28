//! The GraphQL schema DAB generates for an app model (one per model snapshot).
//!
//! Naming and shapes follow DAB's schema builder (`QueryBuilder`, `SchemaConverter`,
//! `InputTypeBuilder`, `StandardQueryInputs`); `E` is the entity name and `P` its plural:
//!
//! - `Query.lowerFirst(P)(first: Int, after: String, filter: EFilterInput, orderBy:
//!   EOrderByInput): EConnection!` and `Query.lowerFirst(E)_by_pk(id: UUID!): E`, for entities
//!   some role may read.
//! - `type E`: the columns some role may read (exposed names, `!` when NOT NULL), then the
//!   relationships: `@one` as `Target` (`Target!` when its FK column is NOT NULL and the
//!   relationship is not self-referencing), `@many` as `TargetConnection` with its own
//!   `first`/`after`/`filter`/`orderBy`. Relationships to the system `User` are not exposed.
//! - `EConnection { items: [E!]!, endCursor: String, hasNextPage: Boolean! }`, plus
//!   `groupBy(fields: [EScalarFields!]): [EGroupBy!]!` and its types (`crate::aggregate`).
//! - `EFilterInput`: a `<Scalar>FilterInput` per column, a `TargetFilterInput` per relationship,
//!   then `and`/`or: [EFilterInput]` (no `not`). `EOrderByInput`: the columns, as `OrderBy`.
//! - Two documented supersets (micro-fabric compatibility.md §6.4): `gt`/`gte`/`lt`/`lte` on
//!   `StringFilterInput`, and `isNull` on a to-one relationship's filter.

use std::collections::HashMap;

use indexmap::IndexMap;
use rl_model::{AppModel, Cardinality, Entity};

use crate::access;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeRef {
    Named(String),
    List(Box<TypeRef>),
    NonNull(Box<TypeRef>),
}

impl TypeRef {
    pub fn named(name: impl Into<String>) -> Self {
        Self::Named(name.into())
    }

    pub fn non_null(self) -> Self {
        Self::NonNull(Box::new(self))
    }

    pub fn list(self) -> Self {
        Self::List(Box::new(self))
    }

    /// The innermost named type.
    pub fn name(&self) -> &str {
        match self {
            Self::Named(name) => name,
            Self::List(inner) | Self::NonNull(inner) => inner.name(),
        }
    }

    pub fn is_non_null(&self) -> bool {
        matches!(self, Self::NonNull(_))
    }

    /// The type without its outer `!`.
    pub fn nullable(&self) -> &TypeRef {
        match self {
            Self::NonNull(inner) => inner,
            other => other,
        }
    }

    /// Converts the parser's type of a variable definition.
    pub fn from_parsed(ty: &async_graphql_parser::types::Type) -> Self {
        use async_graphql_parser::types::BaseType;
        let base = match &ty.base {
            BaseType::Named(name) => Self::Named(name.to_string()),
            BaseType::List(inner) => Self::List(Box::new(Self::from_parsed(inner))),
        };
        if ty.nullable { base } else { base.non_null() }
    }
}

impl std::fmt::Display for TypeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Named(name) => f.write_str(name),
            Self::List(inner) => write!(f, "[{inner}]"),
            Self::NonNull(inner) => write!(f, "{inner}!"),
        }
    }
}

/// What a field of an object type resolves to; the planner dispatches on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolver {
    /// `Query.<list>`: entity index into `AppModel::entities`.
    List(usize),
    /// `Query.<e>_by_pk`.
    ByPk(usize),
    /// A column of entity `.0` (index into `Entity::columns`).
    Column(usize, usize),
    /// A relationship of entity `.0` (index into `Entity::relationships`) to entity `.2`.
    ToOne(usize, usize, usize),
    ToMany(usize, usize, usize),
    Items,
    EndCursor,
    HasNextPage,
    /// `Mutation.create{E}`, `update{E}` and `delete{E}` of entity `.0` (`crate::mutation`).
    Create(usize),
    Update(usize),
    Delete(usize),
    /// Resolved by the module that added the field (mutations, aggregations).
    Extension,
}

#[derive(Clone, Debug)]
pub struct FieldDef {
    pub name: String,
    pub description: Option<String>,
    pub args: IndexMap<String, InputValueDef>,
    pub ty: TypeRef,
    pub resolver: Resolver,
}

#[derive(Clone, Debug)]
pub struct InputValueDef {
    pub name: String,
    pub description: Option<String>,
    pub ty: TypeRef,
    /// A GraphQL literal, e.g. `false`.
    pub default: Option<String>,
    /// A to-one relationship filter: also accepts `isNull: Boolean` (see the module docs).
    pub accepts_is_null: bool,
}

#[derive(Clone, Debug)]
pub struct ObjectDef {
    pub name: String,
    pub description: Option<String>,
    pub fields: IndexMap<String, FieldDef>,
}

#[derive(Clone, Debug)]
pub struct InputObjectDef {
    pub name: String,
    pub description: Option<String>,
    pub fields: IndexMap<String, InputValueDef>,
}

#[derive(Clone, Debug)]
pub struct EnumDef {
    pub name: String,
    pub description: Option<String>,
    pub values: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum TypeDef {
    Scalar(String, Option<String>),
    Object(ObjectDef),
    InputObject(InputObjectDef),
    Enum(EnumDef),
}

impl TypeDef {
    pub fn name(&self) -> &str {
        match self {
            Self::Scalar(name, _) => name,
            Self::Object(def) => &def.name,
            Self::InputObject(def) => &def.name,
            Self::Enum(def) => &def.name,
        }
    }

    pub fn is_input(&self) -> bool {
        !matches!(self, Self::Object(_))
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self, Self::Scalar(..) | Self::Enum(_))
    }
}

/// The schema of one model snapshot.
#[derive(Clone, Debug)]
pub struct Schema {
    pub types: IndexMap<String, TypeDef>,
    pub query_type: String,
    /// Set by the mutations extension.
    pub mutation_type: Option<String>,
    /// Object type name → entity index.
    pub entity_types: HashMap<String, usize>,
}

pub const QUERY: &str = "Query";
pub const ORDER_BY_ENUM: &str = "OrderBy";

impl Schema {
    /// DAB's schema for `model` (queries; mutations and aggregations extend it).
    pub fn build(model: &AppModel) -> Self {
        let mut schema = Schema {
            types: IndexMap::new(),
            query_type: QUERY.into(),
            mutation_type: None,
            entity_types: HashMap::new(),
        };
        for builtin in ["String", "Boolean", "Int", "Float", "ID"] {
            schema.add(TypeDef::Scalar(builtin.into(), None));
        }
        schema.add(TypeDef::Enum(EnumDef {
            name: ORDER_BY_ENUM.into(),
            description: None,
            values: vec!["ASC".into(), "DESC".into()],
        }));
        let exposed: Vec<bool> = model
            .entities
            .iter()
            .map(|e| !e.permissions.is_empty())
            .collect();
        let mut query_fields = IndexMap::new();
        for (index, entity) in model.entities.iter().enumerate() {
            if !exposed[index] {
                continue;
            }
            schema
                .entity_types
                .insert(entity.graphql.type_name.clone(), index);
            let object = entity_object(model, index, &exposed);
            schema.add_filter_inputs(&object);
            schema.add(TypeDef::InputObject(order_by_input(entity, &object)));
            schema.add(TypeDef::InputObject(filter_input(model, entity, &object)));
            schema.add(TypeDef::Object(connection(entity)));
            schema.add(TypeDef::Object(object));
            crate::aggregate::add_types(&mut schema, entity);
            if access::readable_by_anyone(entity) {
                let (list, by_pk) = query_fields_of(index, entity);
                query_fields.insert(list.name.clone(), list);
                query_fields.insert(by_pk.name.clone(), by_pk);
            }
        }
        schema.add(TypeDef::Object(ObjectDef {
            name: QUERY.into(),
            description: None,
            fields: query_fields,
        }));
        crate::mutation::extend_schema(&mut schema, model);
        schema
    }

    fn add(&mut self, def: TypeDef) {
        self.types.insert(def.name().to_owned(), def);
    }

    pub fn object(&self, name: &str) -> Option<&ObjectDef> {
        match self.types.get(name)? {
            TypeDef::Object(def) => Some(def),
            _ => None,
        }
    }

    pub fn input_object(&self, name: &str) -> Option<&InputObjectDef> {
        match self.types.get(name)? {
            TypeDef::InputObject(def) => Some(def),
            _ => None,
        }
    }

    /// The scalar filter inputs the object's columns use, as DAB adds them on first use.
    fn add_filter_inputs(&mut self, object: &ObjectDef) {
        for field in object.fields.values() {
            if let Resolver::Column(..) = field.resolver {
                let scalar = field.ty.name().to_owned();
                if !self.types.contains_key(&scalar) {
                    let description = match scalar.as_str() {
                        "UUID" => "A UUID scalar.",
                        "Decimal" => "The built-in `Decimal` scalar type.",
                        _ => {
                            "The `DateTime` scalar represents an ISO-8601 compliant date time type."
                        }
                    };
                    self.add(TypeDef::Scalar(scalar.clone(), Some(description.into())));
                }
                let filter = scalar_filter(&scalar);
                if !self.types.contains_key(&filter.name) {
                    self.add(TypeDef::InputObject(filter));
                }
            }
        }
    }
}

pub(crate) fn input(name: &str, description: &str, ty: TypeRef) -> InputValueDef {
    InputValueDef {
        name: name.into(),
        description: Some(description.into()),
        ty,
        default: None,
        accepts_is_null: false,
    }
}

pub(crate) fn field(
    name: &str,
    description: Option<&str>,
    ty: TypeRef,
    resolver: Resolver,
) -> FieldDef {
    FieldDef {
        name: name.into(),
        description: description.map(str::to_owned),
        args: IndexMap::new(),
        ty,
        resolver,
    }
}

/// `first`, `after`, `filter`, `orderBy` (DAB `QueryArgumentsForField`).
fn list_arguments(type_name: &str) -> IndexMap<String, InputValueDef> {
    [
        input(
            "first",
            "The number of items to return from the page start point",
            TypeRef::named("Int"),
        ),
        input(
            "after",
            "A pagination token from a previous query to continue through a paginated list",
            TypeRef::named("String"),
        ),
        input(
            "filter",
            "Filter options for query",
            TypeRef::named(format!("{type_name}FilterInput")),
        ),
        input(
            "orderBy",
            "Ordering options for query",
            TypeRef::named(format!("{type_name}OrderByInput")),
        ),
    ]
    .into_iter()
    .map(|arg| (arg.name.clone(), arg))
    .collect()
}

fn query_fields_of(index: usize, entity: &Entity) -> (FieldDef, FieldDef) {
    let names = &entity.graphql;
    let mut list = field(
        &names.list_query,
        Some(&format!(
            "Get a list of all the {} items from the database",
            names.type_name
        )),
        TypeRef::named(names.connection_type()).non_null(),
        Resolver::List(index),
    );
    list.args = list_arguments(&names.type_name);
    let mut by_pk = field(
        &names.by_pk_query,
        Some(&format!(
            "Get a {} from the database by its ID/primary key",
            names.type_name
        )),
        TypeRef::named(&names.type_name),
        Resolver::ByPk(index),
    );
    let pk = entity.primary_key_column();
    let pk_type = TypeRef::named(pk.kind.graphql_scalar()).non_null();
    by_pk.args.insert(
        pk.field.clone(),
        InputValueDef {
            description: None,
            ..input(&pk.field, "", pk_type)
        },
    );
    (list, by_pk)
}

/// The scalar fields some role may read, then the exposed relationships.
fn entity_object(model: &AppModel, index: usize, exposed: &[bool]) -> ObjectDef {
    let entity = &model.entities[index];
    let mut fields = IndexMap::new();
    for (column_index, column) in entity.columns.iter().enumerate() {
        if !access::field_readable_by_anyone(entity, &column.field) {
            continue;
        }
        let scalar = TypeRef::named(column.kind.graphql_scalar());
        let ty = if column.nullable {
            scalar
        } else {
            scalar.non_null()
        };
        let def = field(
            &column.field,
            None,
            ty,
            Resolver::Column(index, column_index),
        );
        fields.insert(column.field.clone(), def);
    }
    for (rel_index, rel) in entity.relationships.iter().enumerate() {
        let Some(target) = relationship_target(model, rel, exposed) else {
            continue;
        };
        let target_name = &model.entities[target].graphql.type_name;
        let def = match rel.cardinality {
            Cardinality::One => {
                let self_referencing = target == index;
                let ty = TypeRef::named(target_name);
                let ty = if rel.nullable || self_referencing {
                    ty
                } else {
                    ty.non_null()
                };
                field(
                    &rel.name,
                    None,
                    ty,
                    Resolver::ToOne(index, rel_index, target),
                )
            }
            Cardinality::Many => {
                let ty = TypeRef::named(format!("{target_name}Connection"));
                let mut def = field(
                    &rel.name,
                    None,
                    ty,
                    Resolver::ToMany(index, rel_index, target),
                );
                def.args = list_arguments(target_name);
                def
            }
        };
        fields.insert(rel.name.clone(), def);
    }
    ObjectDef {
        name: entity.graphql.type_name.clone(),
        description: None,
        fields,
    }
}

/// The target entity of an exposed relationship (not the system `User`, and with a type).
pub fn relationship_target(
    model: &AppModel,
    rel: &rl_model::Relationship,
    exposed: &[bool],
) -> Option<usize> {
    if rel.target_is_system {
        return None;
    }
    let target = model.entities.iter().position(|e| e.name == rel.target)?;
    exposed[target].then_some(target)
}

fn connection(entity: &Entity) -> ObjectDef {
    let name = &entity.graphql.type_name;
    let fields = [
        field(
            "items",
            Some("The list of items that matched the filter"),
            TypeRef::named(name).non_null().list().non_null(),
            Resolver::Items,
        ),
        field(
            "endCursor",
            Some("A pagination token to provide to subsequent pages of a query"),
            TypeRef::named("String"),
            Resolver::EndCursor,
        ),
        field(
            "hasNextPage",
            Some("Indicates if there are more pages of items to return"),
            TypeRef::named("Boolean").non_null(),
            Resolver::HasNextPage,
        ),
    ];
    ObjectDef {
        name: entity.graphql.connection_type(),
        description: Some(
            "The return object from a filter query that supports a pagination token for paging \
             through results"
                .into(),
        ),
        fields: fields.into_iter().map(|f| (f.name.clone(), f)).collect(),
    }
}

fn order_by_input(entity: &Entity, object: &ObjectDef) -> InputObjectDef {
    let fields = object
        .fields
        .values()
        .filter(|f| matches!(f.resolver, Resolver::Column(..)))
        .map(|f| {
            let description = format!("Order by options for {}", f.name);
            let def = input(&f.name, &description, TypeRef::named(ORDER_BY_ENUM));
            (f.name.clone(), def)
        })
        .collect();
    InputObjectDef {
        name: entity.graphql.order_by_input(),
        description: Some(format!("Order by input for {} GraphQL type", object.name)),
        fields,
    }
}

fn filter_input(model: &AppModel, entity: &Entity, object: &ObjectDef) -> InputObjectDef {
    let mut fields = IndexMap::new();
    for f in object.fields.values() {
        let description = format!("Filter options for {}", f.name);
        let def = match f.resolver {
            Resolver::Column(..) => {
                let ty = TypeRef::named(scalar_filter_name(f.ty.name()));
                input(&f.name, &description, ty)
            }
            Resolver::ToOne(_, _, target) | Resolver::ToMany(_, _, target) => {
                let ty = TypeRef::named(model.entities[target].graphql.filter_input());
                InputValueDef {
                    accepts_is_null: matches!(f.resolver, Resolver::ToOne(..)),
                    ..input(&f.name, &description, ty)
                }
            }
            _ => continue,
        };
        fields.insert(f.name.clone(), def);
    }
    let own = entity.graphql.filter_input();
    let list = TypeRef::named(&own).list();
    fields.insert(
        "and".into(),
        input(
            "and",
            "Conditions to be treated as AND operations",
            list.clone(),
        ),
    );
    fields.insert(
        "or".into(),
        input("or", "Conditions to be treated as OR operations", list),
    );
    InputObjectDef {
        name: own,
        description: Some(format!("Filter input for {} GraphQL type", object.name)),
        fields,
    }
}

/// `StringFilterInput`, `UuidFilterInput`, ... for a scalar.
pub fn scalar_filter_name(scalar: &str) -> String {
    match scalar {
        "UUID" => "UuidFilterInput".into(),
        other => format!("{other}FilterInput"),
    }
}

/// The operators of each scalar's filter input, in DAB's order.
pub fn scalar_filter_operators(scalar: &str) -> &'static [&'static str] {
    match scalar {
        "String" => &[
            "eq",
            "contains",
            "notContains",
            "startsWith",
            "endsWith",
            "neq",
            "isNull",
            "in",
            "gt",
            "gte",
            "lt",
            "lte",
        ],
        "UUID" => &[
            "eq",
            "contains",
            "notContains",
            "startsWith",
            "endsWith",
            "neq",
            "isNull",
            "in",
        ],
        "Boolean" | "ID" => &["eq", "neq", "isNull", "in"],
        _ => &["eq", "gt", "gte", "lt", "lte", "neq", "isNull", "in"],
    }
}

fn scalar_filter(scalar: &str) -> InputObjectDef {
    let fields = scalar_filter_operators(scalar)
        .iter()
        .map(|op| {
            let (description, ty) = match *op {
                "eq" => ("Equals", TypeRef::named(scalar)),
                "neq" => ("Not Equals", TypeRef::named(scalar)),
                "gt" => ("Greater Than", TypeRef::named(scalar)),
                "gte" => ("Greater Than or Equal To", TypeRef::named(scalar)),
                "lt" => ("Less Than", TypeRef::named(scalar)),
                "lte" => ("Less Than or Equal To", TypeRef::named(scalar)),
                "contains" => ("Contains", TypeRef::named(scalar)),
                "notContains" => ("Not Contains", TypeRef::named(scalar)),
                "startsWith" => ("Starts With", TypeRef::named(scalar)),
                "endsWith" => ("Ends With", TypeRef::named(scalar)),
                "isNull" => ("Not null test", TypeRef::named("Boolean")),
                _ => ("In", TypeRef::named(scalar).list()),
            };
            (op.to_string(), input(op, description, ty))
        })
        .collect();
    let label = if scalar == "UUID" { "Uuid" } else { scalar };
    InputObjectDef {
        name: scalar_filter_name(scalar),
        description: Some(format!("Input type for adding {label} filters")),
        fields,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic() -> AppModel {
        let json = include_str!("../../rl-model/tests/fixtures/basic.analysis.json");
        AppModel::from_analysis(&serde_json::from_str(json).unwrap()).unwrap()
    }

    #[test]
    fn types_follow_dab_naming_and_nullability() {
        let schema = Schema::build(&basic());
        let query = schema.object(QUERY).unwrap();
        let todos = &query.fields["todos"];
        assert_eq!(todos.ty.to_string(), "TodoConnection!");
        let args: Vec<_> = todos.args.keys().map(String::as_str).collect();
        assert_eq!(args, ["first", "after", "filter", "orderBy"]);
        assert_eq!(
            query.fields["person_by_pk"].args["id"].ty.to_string(),
            "UUID!"
        );
        assert_eq!(query.fields["people"].ty.to_string(), "PersonConnection!");

        let todo = schema.object("Todo").unwrap();
        assert_eq!(todo.fields["title"].ty.to_string(), "String!");
        assert_eq!(todo.fields["cost"].ty.to_string(), "Decimal!");
        assert_eq!(todo.fields["dueDate"].ty.to_string(), "DateTime");
        assert_eq!(todo.fields["category"].ty.to_string(), "Category");
        assert_eq!(todo.fields["comments"].ty.to_string(), "CommentConnection");
        // Readable by no role: not in the schema.
        assert!(!todo.fields.contains_key("secretNote"));
        let comment = schema.object("Comment").unwrap();
        assert_eq!(comment.fields["todo"].ty.to_string(), "Todo!");
        let category = schema.object("Category").unwrap();
        assert_eq!(category.fields["label"].ty.to_string(), "String");
        assert_eq!(category.fields["parent"].ty.to_string(), "Category");
        // The system User relationship is not exposed.
        assert!(
            !schema
                .object("Person")
                .unwrap()
                .fields
                .contains_key("account")
        );

        let connection = schema.object("TodoConnection").unwrap();
        assert_eq!(connection.fields["items"].ty.to_string(), "[Todo!]!");
        assert_eq!(connection.fields["hasNextPage"].ty.to_string(), "Boolean!");

        let filter = schema.input_object("TodoFilterInput").unwrap();
        assert_eq!(filter.fields["id"].ty.to_string(), "UuidFilterInput");
        assert_eq!(
            filter.fields["category"].ty.to_string(),
            "CategoryFilterInput"
        );
        assert!(filter.fields["category"].accepts_is_null);
        assert!(!filter.fields["comments"].accepts_is_null);
        let last: Vec<_> = filter
            .fields
            .keys()
            .rev()
            .take(2)
            .map(String::as_str)
            .collect();
        assert_eq!(last, ["or", "and"]);
        let order = schema.input_object("TodoOrderByInput").unwrap();
        assert!(order.fields.contains_key("title") && !order.fields.contains_key("category"));
        let ops: Vec<_> = schema
            .input_object("BooleanFilterInput")
            .unwrap()
            .fields
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(ops, ["eq", "neq", "isNull", "in"]);
        assert_eq!(
            schema.input_object("IntFilterInput").unwrap().fields["in"]
                .ty
                .to_string(),
            "[Int]"
        );
    }
}
