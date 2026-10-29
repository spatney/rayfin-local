//! `rayfin-local schema`: the app model analysed from `rayfin/data`, as compact tables, and the
//! SQLite DDL of its entity tables.

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use rl_model::{
    ActionPermission, AppModel, Cardinality, Column, Entity, FieldKind, RolePermission,
    StorageFolder,
};
use rl_project::LocalState;

use crate::analysis;
use crate::ui::{Color, Painter};

pub async fn run(dir: &Path, verbose: bool) -> anyhow::Result<ExitCode> {
    let project = super::load_project(dir)?;
    let state = LocalState::load_or_create(&project)?;
    let node = super::find_node().await?;
    let (model, _) = analysis::analyze(&project, &state, &node, verbose).await?;
    print!("{}", render(&model, Painter::stdout()));
    if !model.entities.is_empty() {
        let sql = rl_db::schema_sql(&model)?;
        println!("\n{}", sql.trim_end());
    }
    Ok(ExitCode::SUCCESS)
}

fn render(model: &AppModel, painter: Painter) -> String {
    let mut out = String::new();
    if model.entities.is_empty() {
        out.push_str("No entities.\n");
    }
    for entity in &model.entities {
        out.push('\n');
        render_entity(&mut out, entity, painter);
    }
    if let Some(storage) = &model.storage {
        out.push('\n');
        let _ = writeln!(out, "{}", painter.paint(Color::Bold, "Storage folders"));
        if storage.folders.is_empty() {
            out.push_str("  none\n");
        }
        for folder in &storage.folders {
            render_folder(&mut out, folder);
        }
    }
    out
}

fn render_entity(out: &mut String, entity: &Entity, painter: Painter) {
    let _ = writeln!(
        out,
        "{} (table {}; GraphQL {}, {})",
        painter.paint(Color::Bold, &entity.name),
        entity.table,
        entity.graphql.type_name,
        entity.graphql.list_query
    );
    let mut rows = vec![["FIELD", "TYPE", "SQL TYPE", "NULL", "KEY"].map(str::to_owned)];
    rows.extend(entity.columns.iter().map(|column| {
        let field = if column.field == column.name {
            column.field.clone()
        } else {
            format!("{} ({})", column.field, column.name)
        };
        [
            field,
            kind_summary(&column.kind),
            column.db_type.clone(),
            if column.nullable { "null" } else { "" }.to_owned(),
            keys(entity, column),
        ]
    }));
    render_table(out, &rows, painter);

    if !entity.relationships.is_empty() {
        let relationships: Vec<String> = entity
            .relationships
            .iter()
            .map(|rel| {
                let cardinality = match rel.cardinality {
                    Cardinality::One => "one",
                    Cardinality::Many => "many",
                };
                format!("{} → {} ({cardinality})", rel.name, rel.target)
            })
            .collect();
        let _ = writeln!(out, "  relationships: {}", relationships.join(", "));
    }
    let roles: Vec<String> = entity.permissions.iter().map(role_summary).collect();
    let roles = if roles.is_empty() {
        "none (no one can use it)".to_owned()
    } else {
        roles.join("; ")
    };
    let _ = writeln!(out, "  roles: {roles}");
}

fn render_table(out: &mut String, rows: &[[String; 5]], painter: Painter) {
    let mut widths = [0; 5];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    for (index, row) in rows.iter().enumerate() {
        let mut line = String::from(" ");
        for (cell, width) in row.iter().zip(widths) {
            let _ = write!(line, " {cell:<width$}");
        }
        let line = line.trim_end();
        if index == 0 {
            let _ = writeln!(out, "{}", painter.paint(Color::Dim, line));
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
}

fn kind_summary(kind: &FieldKind) -> String {
    match kind {
        FieldKind::Uuid => "uuid".to_owned(),
        FieldKind::Text {
            min_length,
            max_length,
            pattern,
        } => {
            let mut summary = format!(
                "text{}",
                range(min_length.map(f64::from), max_length.map(f64::from))
            );
            if let Some(pattern) = pattern {
                let _ = write!(summary, " /{}/{}", pattern.source, pattern.flags);
            }
            summary
        }
        FieldKind::Email {
            min_length,
            max_length,
        } => format!(
            "email{}",
            range(min_length.map(f64::from), max_length.map(f64::from))
        ),
        FieldKind::Set { values, .. } => format!("set({})", values.join("|")),
        FieldKind::Int { min, max } => format!("int{}", range(*min, *max)),
        FieldKind::Decimal {
            precision,
            scale,
            min,
            max,
        } => format!("decimal({precision},{scale}){}", range(*min, *max)),
        FieldKind::Boolean => "boolean".to_owned(),
        FieldKind::Date => "date".to_owned(),
    }
}

fn range(min: Option<f64>, max: Option<f64>) -> String {
    if min.is_none() && max.is_none() {
        return String::new();
    }
    let bound = |value: Option<f64>| value.map(|v| v.to_string()).unwrap_or_default();
    format!(" {}..{}", bound(min), bound(max))
}

fn keys(entity: &Entity, column: &Column) -> String {
    let mut keys = Vec::new();
    if column.primary_key {
        keys.push("PK".to_owned());
    } else if column.unique {
        keys.push("unique".to_owned());
    }
    for foreign_key in &entity.foreign_keys {
        if foreign_key.columns.contains(&column.name) {
            let target = foreign_key
                .referenced_entity
                .as_deref()
                .unwrap_or(&foreign_key.referenced_table);
            keys.push(format!("FK → {target}"));
        }
    }
    keys.join(", ")
}

fn role_summary(role: &RolePermission) -> String {
    let actions: Vec<String> = role.actions.iter().map(action_summary).collect();
    format!("{}: {}", role.role, actions.join(", "))
}

fn action_summary(grant: &ActionPermission) -> String {
    let mut summary = grant.action.as_str().to_owned();
    if let Some(fields) = &grant.fields {
        let mut access = fields.include.join(" ");
        if !fields.exclude.is_empty() {
            let _ = write!(access, " − {}", fields.exclude.join(" "));
        }
        let _ = write!(summary, " [{access}]");
    }
    if let Some(policy) = &grant.policy {
        let _ = write!(summary, " where {}", policy.text);
    }
    summary
}

fn render_folder(out: &mut String, folder: &StorageFolder) {
    let mut details = Vec::new();
    if folder.display_name != folder.name {
        details.push(format!("\"{}\"", folder.display_name));
    }
    if let Some(max_size) = folder.max_size {
        details.push(format!("max {max_size} bytes"));
    }
    if !folder.allowed_content_types.is_empty() {
        details.push(folder.allowed_content_types.join(" "));
    }
    let rules: Vec<String> = folder
        .rules
        .iter()
        .map(|rule| {
            let actions: Vec<&str> = rule.actions.iter().map(|action| action.as_str()).collect();
            format!(
                "{} {}: {}",
                rule.path_pattern,
                rule.role,
                actions.join(", ")
            )
        })
        .collect();
    if !rules.is_empty() {
        details.push(format!("rules: {}", rules.join("; ")));
    }
    let _ = writeln!(out, "  {} {}", folder.name, details.join(", "));
}
