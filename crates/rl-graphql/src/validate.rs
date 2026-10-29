//! Document validation against the schema, with Hot Chocolate's messages
//! (`HotChocolate.Validation` resources) so errors read as they do on Fabric.

use std::collections::{HashMap, HashSet};

use async_graphql_parser::Positioned;
use async_graphql_parser::types::{
    Directive, ExecutableDocument, Field, OperationType, Selection, SelectionSet,
};
use serde_json::Map;

use crate::coerce::{self, Problem, Variables};
use crate::error::GraphQlError;
use crate::schema::{FieldDef, InputValueDef, Schema, TypeDef, TypeRef};
use crate::value::Source;

/// Every validation error of `doc` (empty when the document may run).
pub fn validate(
    schema: &Schema,
    doc: &ExecutableDocument,
    source: &Source<'_>,
    allow_introspection: bool,
) -> Vec<GraphQlError> {
    let mut v = Validator {
        schema,
        doc,
        source,
        allow_introspection,
        errors: Vec::new(),
        var_types: HashMap::new(),
        used: Vec::new(),
        visited: HashSet::new(),
        reachable: HashSet::new(),
    };
    for (_, op) in doc.operations.iter() {
        v.operation(op);
    }
    v.fragment_cycles();
    let mut unused: Vec<_> = doc
        .fragments
        .iter()
        .filter(|(name, _)| !v.reachable.contains(name.as_str()))
        .collect();
    unused.sort_by_key(|(_, f)| f.pos);
    for (name, fragment) in unused {
        v.errors.push(
            GraphQlError::new(format!(
                "The specified fragment `{name}` is not used within the current document."
            ))
            .at(fragment.pos)
            .ext("fragment", name.as_str())
            .spec("sec-Fragments-Must-Be-Used"),
        );
    }
    v.errors
}

struct Validator<'a> {
    schema: &'a Schema,
    doc: &'a ExecutableDocument,
    source: &'a Source<'a>,
    allow_introspection: bool,
    errors: Vec<GraphQlError>,
    var_types: HashMap<String, (TypeRef, bool)>,
    /// Variables used by the current operation (with a position each).
    used: Vec<(String, async_graphql_parser::Pos)>,
    /// Fragments already validated for the current operation.
    visited: HashSet<String>,
    /// Fragments some operation spreads.
    reachable: HashSet<String>,
}

impl Validator<'_> {
    fn operation(&mut self, op: &Positioned<async_graphql_parser::types::OperationDefinition>) {
        let root = match op.node.ty {
            OperationType::Query => Some(self.schema.query_type.clone()),
            OperationType::Mutation => self.schema.mutation_type.clone(),
            OperationType::Subscription => None,
        };
        let Some(root) = root else {
            let kind = match op.node.ty {
                OperationType::Mutation => "Mutation",
                _ => "Subscription",
            };
            self.errors.push(GraphQlError::new(format!(
                "This GraphQL schema does not support `{kind}` operations."
            )));
            return;
        };
        self.var_types.clear();
        self.used.clear();
        self.visited.clear();
        for def in &op.node.variable_definitions {
            let name = def.node.name.node.to_string();
            let ty = TypeRef::from_parsed(&def.node.var_type.node);
            if self.var_types.contains_key(&name) {
                self.errors.push(
                    GraphQlError::new(
                        "A document containing operations that define more than one variable \
                         with the same name is invalid for execution.",
                    )
                    .at(def.pos)
                    .ext("variable", name.as_str())
                    .spec("sec-Variable-Uniqueness"),
                );
                continue;
            }
            if !self
                .schema
                .types
                .get(ty.name())
                .is_some_and(TypeDef::is_input)
            {
                self.errors.push(
                    GraphQlError::new(format!(
                        "The type of variable `{name}` is not an input type."
                    ))
                    .at(def.pos)
                    .ext("variable", name.as_str())
                    .ext("variableType", ty.to_string())
                    .spec("sec-Variables-Are-Input-Types"),
                );
            } else if let Some(default) = &def.node.default_value {
                let ok = self.source.value_at(default.pos).ok().is_some_and(|value| {
                    let empty = HashMap::new();
                    let vars = Variables {
                        types: &empty,
                        values: None,
                    };
                    coerce::literal(self.schema, &ty, &value, None, &vars).is_ok()
                });
                if !ok {
                    self.errors.push(
                        GraphQlError::new(format!(
                            "The specified value type of variable `{name}` does not match the \
                             variable type."
                        ))
                        .at(default.pos)
                        .ext("variable", name.as_str())
                        .ext("variableType", ty.to_string())
                        .spec("sec-Values-of-Correct-Type"),
                    );
                }
            }
            let has_default = def.node.default_value.is_some();
            self.var_types.insert(name, (ty, has_default));
        }
        for directive in &op.node.directives {
            self.directive_misplaced(directive);
        }
        let selection_set = &op.node.selection_set;
        if selection_set.node.items.is_empty() {
            return;
        }
        self.selection_set(&root, selection_set, true);

        let used: HashSet<&str> = self.used.iter().map(|(n, _)| n.as_str()).collect();
        let mut unused: Vec<&str> = op
            .node
            .variable_definitions
            .iter()
            .map(|d| d.node.name.node.as_str())
            .filter(|name| !used.contains(name))
            .collect();
        unused.dedup();
        if !unused.is_empty() {
            self.errors.push(
                GraphQlError::new(format!(
                    "The following variables were not used: {}.",
                    unused.join(", ")
                ))
                .at(op.pos)
                .spec("sec-All-Variables-Used"),
            );
        }
        let mut undeclared: Vec<&str> = self
            .used
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|name| !self.var_types.contains_key(*name))
            .collect();
        undeclared.dedup();
        if !undeclared.is_empty() {
            let message = format!(
                "The following variables were not declared: {}.",
                undeclared.join(", ")
            );
            self.errors.push(
                GraphQlError::new(message)
                    .at(op.pos)
                    .spec("sec-All-Variable-Uses-Defined"),
            );
        }
    }

    fn selection_set(&mut self, parent: &str, set: &Positioned<SelectionSet>, root: bool) {
        self.merge_check(parent, &set.node);
        for selection in &set.node.items {
            match &selection.node {
                Selection::Field(field) => self.field(parent, field, root),
                Selection::FragmentSpread(spread) => {
                    self.directives(&spread.node.directives);
                    let name = spread.node.fragment_name.node.to_string();
                    self.reachable.insert(name.clone());
                    let Some(fragment) = self.doc.fragments.get(name.as_str()) else {
                        self.errors.push(
                            GraphQlError::new(format!(
                                "The specified fragment `{name}` does not exist."
                            ))
                            .at(spread.pos)
                            .ext("fragment", name.as_str())
                            .spec("sec-Fragment-Spread-Target-Defined"),
                        );
                        continue;
                    };
                    let on = fragment.node.type_condition.node.on.node.to_string();
                    if !self.type_condition(&on, parent, spread.pos, Some(&name)) {
                        continue;
                    }
                    if self.visited.insert(name.clone()) {
                        for directive in &fragment.node.directives {
                            self.directive_misplaced(directive);
                        }
                        self.selection_set(&on, &fragment.node.selection_set, root);
                    }
                }
                Selection::InlineFragment(inline) => {
                    self.directives(&inline.node.directives);
                    let on = match &inline.node.type_condition {
                        Some(condition) => condition.node.on.node.to_string(),
                        None => parent.to_owned(),
                    };
                    if self.type_condition(&on, parent, inline.pos, None) {
                        self.selection_set(&on, &inline.node.selection_set, root);
                    }
                }
            }
        }
    }

    /// Whether a fragment on `on` may be spread in `parent` (object types only: they must match).
    fn type_condition(
        &mut self,
        on: &str,
        parent: &str,
        pos: async_graphql_parser::Pos,
        fragment: Option<&str>,
    ) -> bool {
        let error = match self.schema.types.get(on) {
            None => GraphQlError::new(format!("Unknown type `{on}`."))
                .ext("typeCondition", on)
                .spec("sec-Fragment-Spread-Type-Existence"),
            Some(TypeDef::Object(_)) if on == parent => return true,
            Some(TypeDef::Object(_)) => GraphQlError::new(
                "The parent type does not match the type condition on the fragment.",
            )
            .ext("typeCondition", on)
            .ext("selectionSetType", parent)
            .spec("sec-Fragment-Spread-Is-Possible"),
            Some(_) => GraphQlError::new(
                "Fragments can only be declared on unions, interfaces, and objects.",
            )
            .ext("typeCondition", on)
            .spec("sec-Fragments-on-Object-Interface-or-Union-Types"),
        };
        let error = match fragment {
            Some(name) => error.ext("fragment", name),
            None => error,
        };
        self.errors.push(error.at(pos));
        false
    }

    fn field(&mut self, parent: &str, field: &Positioned<Field>, root: bool) {
        let name = field.node.name.node.as_str();
        self.directives(&field.node.directives);
        if name == "__typename" {
            self.no_selection(field, parent, "String!");
            return;
        }
        if root && (name == "__schema" || name == "__type") && parent == self.schema.query_type {
            if !self.allow_introspection {
                self.errors.push(
                    GraphQlError::new("Introspection is not allowed for the current request.")
                        .at(field.pos)
                        .ext("field", name)
                        .code(crate::error::codes::INTROSPECTION_NOT_ALLOWED),
                );
            }
            return;
        }
        let Some(def) = self.schema.object(parent).and_then(|o| o.fields.get(name)) else {
            let key = field.node.response_key().node.as_str();
            self.errors.push(
                GraphQlError::new(format!(
                    "The field `{name}` does not exist on the type `{parent}`."
                ))
                .at(field.pos)
                .ext("type", parent)
                .ext("field", name)
                .ext("responseName", key)
                .spec("sec-Field-Selections"),
            );
            return;
        };
        self.arguments(parent, def, field);
        let named = def.ty.name().to_owned();
        match self.schema.types.get(&named) {
            Some(TypeDef::Object(_)) => {
                if field.node.selection_set.node.items.is_empty() {
                    self.errors.push(
                        GraphQlError::new(format!(
                            "Field \"{name}\" of type \"{}\" must have a selection of subfields. \
                             Did you mean \"{name} {{ ... }}\"?",
                            def.ty
                        ))
                        .at(field.pos)
                        .ext("declaringType", parent)
                        .ext("field", name)
                        .ext("type", def.ty.to_string())
                        .ext("responseName", field.node.response_key().node.as_str())
                        .spec("sec-Field-Selections"),
                    );
                } else {
                    self.selection_set(&named, &field.node.selection_set, false);
                }
            }
            _ => self.no_selection(field, parent, &def.ty.to_string()),
        }
    }

    fn no_selection(&mut self, field: &Positioned<Field>, parent: &str, ty: &str) {
        if field.node.selection_set.node.items.is_empty() {
            return;
        }
        let name = field.node.name.node.as_str();
        self.errors.push(
            GraphQlError::new(format!(
                "Field \"{name}\" must not have a selection since type \"{ty}\" has no subfields."
            ))
            .at(field.pos)
            .ext("declaringType", parent)
            .ext("field", name)
            .ext("type", ty)
            .ext("responseName", field.node.response_key().node.as_str())
            .spec("sec-Leaf-Field-Selections"),
        );
    }

    fn arguments(&mut self, parent: &str, def: &FieldDef, field: &Positioned<Field>) {
        let mut seen = HashSet::new();
        for (name, value) in &field.node.arguments {
            if !seen.insert(name.node.as_str()) {
                self.errors.push(
                    GraphQlError::new(
                        "More than one argument with the same name in an argument set is \
                         ambiguous and invalid.",
                    )
                    .at(name.pos)
                    .ext("type", parent)
                    .ext("field", def.name.as_str())
                    .ext("argument", name.node.as_str())
                    .spec("sec-Argument-Uniqueness"),
                );
                continue;
            }
            let Some(arg) = def.args.get(name.node.as_str()) else {
                self.errors.push(
                    GraphQlError::new(format!("The argument `{}` does not exist.", name.node))
                        .at(name.pos)
                        .ext("type", parent)
                        .ext("field", def.name.as_str())
                        .ext("argument", name.node.as_str())
                        .spec("sec-Argument-Names"),
                );
                continue;
            };
            self.value(arg, value.pos);
        }
        for (name, arg) in &def.args {
            if arg.ty.is_non_null() && arg.default.is_none() && !seen.contains(name.as_str()) {
                self.errors.push(
                    GraphQlError::new(format!("The argument `{name}` is required."))
                        .at(field.pos)
                        .ext("type", parent)
                        .ext("field", def.name.as_str())
                        .ext("argument", name.as_str())
                        .spec("sec-Required-Arguments"),
                );
            }
        }
    }

    /// Checks the argument value at `pos` against `arg`'s type.
    fn value(&mut self, arg: &InputValueDef, pos: async_graphql_parser::Pos) {
        let value = match self.source.value_at(pos) {
            Ok(value) => value,
            Err(message) => {
                self.errors.push(GraphQlError::new(message).at(pos));
                return;
            }
        };
        collect_variables(&value, &mut self.used);
        let vars = Variables {
            types: &self.var_types,
            values: None,
        };
        let problem = match coerce::literal(self.schema, &arg.ty, &value, None, &vars) {
            Ok(_) => return,
            Err(problem) => problem,
        };
        let error = match problem {
            Problem::NotCompatible {
                field: None, pos, ..
            } => {
                GraphQlError::new("The specified argument value does not match the argument type.")
                    .at(pos)
                    .ext("argument", arg.name.as_str())
                    .ext("argumentValue", value.to_graphql())
                    .ext("locationType", arg.ty.to_string())
                    .spec("sec-Values-of-Correct-Type")
            }
            Problem::NotCompatible {
                field: Some(field),
                field_type,
                pos,
            } => GraphQlError::new(format!(
                "The specified value type of field `{field}` does not match the field type."
            ))
            .at(pos)
            .ext("fieldName", field.as_str())
            .ext("fieldType", field_type.unwrap_or_default())
            .ext("locationType", arg.ty.to_string())
            .spec("sec-Values-of-Correct-Type"),
            Problem::UnknownField { name, pos } => GraphQlError::new(format!(
                "The specified input object field `{name}` does not exist."
            ))
            .at(pos)
            .ext("field", name.as_str())
            .spec("sec-Input-Object-Field-Names"),
            Problem::DuplicateField { name, pos } => {
                GraphQlError::new(format!("There can be only one input field named `{name}`."))
                    .at(pos)
                    .ext("field", name.as_str())
                    .spec("sec-Input-Object-Field-Uniqueness")
            }
            Problem::MissingField { name, pos } => {
                GraphQlError::new(format!("`{name}` is a required field and cannot be null."))
                    .at(pos)
                    .ext("field", name.as_str())
                    .spec("sec-Input-Object-Required-Fields")
            }
            Problem::Variable { name, pos } => GraphQlError::new(format!(
                "The variable `{name}` is not compatible with the type of the current location."
            ))
            .at(pos)
            .ext("variable", name.as_str())
            .spec("sec-All-Variable-Usages-Are-Allowed"),
            // Reported once per operation, with every undeclared name.
            Problem::UndeclaredVariable { .. } => return,
        };
        self.errors.push(error);
    }

    fn directives(&mut self, directives: &[Positioned<Directive>]) {
        let mut seen = HashSet::new();
        for directive in directives {
            let name = directive.node.name.node.as_str();
            if name != "skip" && name != "include" {
                self.errors.push(
                    GraphQlError::new(format!(
                        "The specified directive `{name}` is not supported by the current schema."
                    ))
                    .at(directive.pos)
                    .spec("sec-Directives-Are-Defined"),
                );
                continue;
            }
            if !seen.insert(name) {
                self.errors.push(
                    GraphQlError::new("Only one of each directive is allowed per location.")
                        .at(directive.pos)
                        .spec("sec-Directives-Are-Unique-per-Location"),
                );
            }
            let arg = InputValueDef {
                name: "if".into(),
                description: None,
                ty: TypeRef::named("Boolean").non_null(),
                default: None,
                accepts_is_null: false,
            };
            let mut has_if = false;
            for (arg_name, value) in &directive.node.arguments {
                if arg_name.node == "if" {
                    has_if = true;
                    self.value(&arg, value.pos);
                } else {
                    self.errors.push(
                        GraphQlError::new(format!(
                            "The argument `{}` does not exist.",
                            arg_name.node
                        ))
                        .at(arg_name.pos)
                        .ext("directive", name)
                        .ext("argument", arg_name.node.as_str())
                        .spec("sec-Argument-Names"),
                    );
                }
            }
            if !has_if {
                self.errors.push(
                    GraphQlError::new("The argument `if` is required.")
                        .at(directive.pos)
                        .ext("directive", name)
                        .ext("argument", "if")
                        .spec("sec-Required-Arguments"),
                );
            }
        }
    }

    fn directive_misplaced(&mut self, directive: &Positioned<Directive>) {
        let name = directive.node.name.node.as_str();
        let error = if name == "skip" || name == "include" {
            GraphQlError::new("The specified directive is not valid in the current location.")
                .spec("sec-Directives-Are-in-Valid-Locations")
        } else {
            GraphQlError::new(format!(
                "The specified directive `{name}` is not supported by the current schema."
            ))
            .spec("sec-Directives-Are-Defined")
        };
        self.errors.push(error.at(directive.pos));
    }

    /// Fields with the same response name must be the same field with the same arguments
    /// (a simplified `OverlappingFieldsCanBeMerged`: the schema has object types only).
    fn merge_check(&mut self, parent: &str, set: &SelectionSet) {
        let mut fields: Vec<&Positioned<Field>> = Vec::new();
        let mut visited = HashSet::new();
        self.flatten(parent, set, &mut fields, &mut visited);
        let mut by_key: HashMap<&str, &Positioned<Field>> = HashMap::new();
        for field in fields {
            let key = field.node.response_key().node.as_str();
            let Some(first) = by_key.get(key) else {
                by_key.insert(key, field);
                continue;
            };
            let args = |f: &Positioned<Field>| -> Vec<(String, String)> {
                f.node
                    .arguments
                    .iter()
                    .map(|(n, v)| (n.node.to_string(), v.node.to_string()))
                    .collect()
            };
            if first.node.name.node != field.node.name.node || args(first) != args(field) {
                let ty = self
                    .schema
                    .object(parent)
                    .and_then(|o| o.fields.get(field.node.name.node.as_str()))
                    .map(|f| f.ty.to_string())
                    .unwrap_or_default();
                self.errors.push(
                    GraphQlError::new(
                        "Encountered fields for the same object that cannot be merged.",
                    )
                    .at(first.pos)
                    .at(field.pos)
                    .ext("declaringTypeA", parent)
                    .ext("declaringTypeB", parent)
                    .ext("fieldA", first.node.name.node.as_str())
                    .ext("fieldB", field.node.name.node.as_str())
                    .ext("typeA", ty.clone())
                    .ext("typeB", ty)
                    .ext("responseNameA", key)
                    .ext("responseNameB", key)
                    .spec("sec-Field-Selection-Merging"),
                );
            }
        }
    }

    fn flatten<'d>(
        &self,
        parent: &str,
        set: &'d SelectionSet,
        out: &mut Vec<&'d Positioned<Field>>,
        visited: &mut HashSet<String>,
    ) where
        Self: 'd,
    {
        for selection in &set.items {
            match &selection.node {
                Selection::Field(field) => out.push(field),
                Selection::InlineFragment(inline) => {
                    let on = inline
                        .node
                        .type_condition
                        .as_ref()
                        .map(|c| c.node.on.node.as_str());
                    if on.is_none_or(|on| on == parent) {
                        self.flatten(parent, &inline.node.selection_set.node, out, visited);
                    }
                }
                Selection::FragmentSpread(spread) => {
                    let name = spread.node.fragment_name.node.as_str();
                    if let Some(fragment) = self.doc.fragments.get(name)
                        && fragment.node.type_condition.node.on.node == parent
                        && visited.insert(name.to_owned())
                    {
                        self.flatten(parent, &fragment.node.selection_set.node, out, visited);
                    }
                }
            }
        }
    }

    fn fragment_cycles(&mut self) {
        fn spreads(set: &SelectionSet, out: &mut Vec<(String, async_graphql_parser::Pos)>) {
            for selection in &set.items {
                match &selection.node {
                    Selection::Field(f) => spreads(&f.node.selection_set.node, out),
                    Selection::InlineFragment(i) => spreads(&i.node.selection_set.node, out),
                    Selection::FragmentSpread(s) => {
                        out.push((s.node.fragment_name.node.to_string(), s.pos));
                    }
                }
            }
        }
        let graph: HashMap<String, Vec<(String, async_graphql_parser::Pos)>> = self
            .doc
            .fragments
            .iter()
            .map(|(name, f)| {
                let mut out = Vec::new();
                spreads(&f.node.selection_set.node, &mut out);
                (name.to_string(), out)
            })
            .collect();
        let mut reported = HashSet::new();
        for start in graph.keys() {
            let mut stack = vec![(start.clone(), 0usize)];
            let mut on_path = vec![start.clone()];
            while let Some((node, next)) = stack.pop() {
                let edges = graph.get(&node).map(Vec::as_slice).unwrap_or_default();
                if next < edges.len() {
                    stack.push((node.clone(), next + 1));
                    let (target, pos) = &edges[next];
                    if target == start {
                        if reported.insert(start.clone()) {
                            self.errors.push(
                                GraphQlError::new(
                                    "The graph of fragment spreads must not form any cycles \
                                     including spreading itself. Otherwise an operation could \
                                     infinitely spread or infinitely execute on cycles in the \
                                     underlying data.",
                                )
                                .at(*pos)
                                .ext("fragment", target.as_str())
                                .spec("sec-Fragment-Spreads-Must-Not-Form-Cycles"),
                            );
                        }
                    } else if !on_path.contains(target) && on_path.len() < 64 {
                        on_path.push(target.clone());
                        stack.push((target.clone(), 0));
                    }
                } else {
                    on_path.pop();
                }
            }
        }
    }
}

fn collect_variables(
    value: &crate::value::PValue,
    out: &mut Vec<(String, async_graphql_parser::Pos)>,
) {
    use crate::value::PKind;
    match &value.kind {
        PKind::Variable(name) => out.push((name.clone(), value.pos)),
        PKind::List(items) => items.iter().for_each(|v| collect_variables(v, out)),
        PKind::Object(fields) => fields.iter().for_each(|f| collect_variables(&f.value, out)),
        _ => {}
    }
}

/// Coerces the request's variables to their declared types (spec `CoerceVariableValues`).
pub fn coerce_variables(
    schema: &Schema,
    op: &async_graphql_parser::types::OperationDefinition,
    source: &Source<'_>,
    provided: &Map<String, serde_json::Value>,
) -> Result<Map<String, serde_json::Value>, GraphQlError> {
    let mut out = Map::new();
    for def in &op.variable_definitions {
        let name = def.node.name.node.to_string();
        let ty = TypeRef::from_parsed(&def.node.var_type.node);
        let invalid = || {
            GraphQlError::new(format!("Variable `{name}` got an invalid value."))
                .at(def.pos)
                .ext("variable", name.as_str())
                .ext("code", "EXEC_INVALID_TYPE")
        };
        match provided.get(&name) {
            Some(value) => {
                coerce::json(schema, &ty, value, None).ok_or_else(invalid)?;
                out.insert(name.clone(), value.clone());
            }
            None => {
                if let Some(default) = &def.node.default_value {
                    let value = source
                        .value_at(default.pos)
                        .ok()
                        .and_then(|v| v.to_json())
                        .ok_or_else(invalid)?;
                    out.insert(name.clone(), value);
                } else if ty.is_non_null() {
                    return Err(GraphQlError::new(format!("Variable `{name}` is required."))
                        .at(def.pos)
                        .ext("variable", name.as_str())
                        .ext("code", "EXEC_NON_NULL_VIOLATION"));
                }
            }
        }
    }
    Ok(out)
}
