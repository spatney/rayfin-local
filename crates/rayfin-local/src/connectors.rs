//! Category A connectors' entities: the classes in `rayfin/connectors/<name>/` analyzed with the
//! project's own `@microsoft/rayfin-core` (`js/analyze.mjs`, connectors mode), as `rayfin up`
//! analyzes them, and cached in `.rayfin-local/cache/connectors-analysis.json` while their
//! sources are unchanged. Each model is completed with the connector's `metadata.json` and
//! limited to its declared operations.
//!
//! A connector whose analysis fails doesn't stop `serve`: its requests answer why.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use rl_connectors::sql::restrict_to_operations;
use rl_connectors::{SqlConnectorModel, SqlKind};
use rl_model::connector::ConnectorsAnalysis;
use rl_model::{ConnectorSource, ConnectorsAnalyzeInput};
use rl_node::{Helper, NodeRuntime};
use rl_project::{CORE_PACKAGE, LocalState, Project, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::analysis::{cache_key, inputs_fingerprint, tsconfig_chain};
use crate::integrations::connector_defs;
use crate::ui;

const HELPER: Helper = Helper::new("analyze", rl_model::ANALYZE_JS);
const CACHE_FILE: &str = "connectors-analysis.json";
/// Bump when the cache's meaning changes.
const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    version: u32,
    key: String,
    inputs: String,
    analysis: Value,
}

/// The models of the project's Category A connectors, by name; empty when it has none. Prints
/// what was analyzed and the warnings.
pub async fn analyze(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    verbose: bool,
) -> HashMap<String, SqlConnectorModel> {
    let defs = connector_defs(&project.config().connectors);
    let sql: Vec<_> = defs
        .iter()
        .filter_map(|def| SqlKind::parse(&def.kind).map(|kind| (def, kind)))
        .collect();
    if sql.is_empty() {
        return HashMap::new();
    }
    let connectors_dir = project.rayfin_dir().join("connectors");
    let sources = sql
        .iter()
        .map(|(def, kind)| ConnectorSource {
            name: def.name.clone(),
            dir: connectors_dir.join(&def.name),
            dialect: kind.analyzer_dialect().to_owned(),
        })
        .collect();
    let mut input = ConnectorsAnalyzeInput::for_project(project.root(), sources);
    input.out_dir = state.cache_dir().join("compiled-connectors");
    input.verbose = verbose;

    let analysis = match load(project, state, node, &input).await {
        Ok(analysis) => analysis,
        Err(problem) => {
            ui::warn(format_args!(
                "Could not analyze the connector entities in rayfin/connectors: {problem} Fix it \
                 and save; until then those connectors answer this error."
            ));
            return sql
                .iter()
                .map(|(def, _)| (def.name.clone(), Err(problem.clone())))
                .collect();
        }
    };
    for diagnostic in &analysis.diagnostics {
        ui::warn(&diagnostic.message);
    }
    let mut models = HashMap::new();
    for (def, kind) in sql {
        let found = analysis
            .connectors
            .iter()
            .find(|connector| connector.name == def.name);
        let model = match found.map(|connector| connector.model.clone()) {
            Some(Ok(mut model)) => {
                let metadata = connectors_dir.join(&def.name).join("metadata.json");
                match fs::read(&metadata)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                {
                    Some(metadata) => {
                        for entity in model.apply_source_metadata(&metadata) {
                            ui::warn(format_args!(
                                "Connector '{}': entity {entity} maps to a table its \
                                 metadata.json doesn't list; its columns keep their decorators' \
                                 types.",
                                def.name
                            ));
                        }
                    }
                    None => tracing::debug!(
                        "connector {} has no readable metadata.json; using the decorators' types",
                        def.name
                    ),
                }
                for warning in restrict_to_operations(&mut model, def, kind) {
                    ui::warn(warning);
                }
                ui::step(format_args!(
                    "Analyzed connector {} ({}, {} {})",
                    def.name,
                    def.kind,
                    model.entities.len(),
                    if model.entities.len() == 1 {
                        "entity"
                    } else {
                        "entities"
                    }
                ));
                Ok(Arc::new(model))
            }
            Some(Err(problem)) => {
                ui::warn(format_args!(
                    "Connector '{}' can't run locally: {problem}",
                    def.name
                ));
                Err(problem)
            }
            None => Err(format!(
                "The analysis of the connector entities returned nothing for '{}'.",
                def.name
            )),
        };
        models.insert(def.name.clone(), model);
    }
    models
}

/// The analysis, from the cache or the helper; `Err` says why there is none.
async fn load(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    input: &ConnectorsAnalyzeInput,
) -> Result<ConnectorsAnalysis, String> {
    let key = key(project, input);
    let cache_path = state.cache_dir().join(CACHE_FILE);
    if let Some(analysis) = read_cache(project, &cache_path, &key) {
        return Ok(analysis);
    }
    let result: Value = rl_node::run_helper(
        node,
        &HELPER,
        &state.cache_dir(),
        project.root(),
        &input.to_json(),
        ui::child_sink(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let analysis = ConnectorsAnalysis::from_result(&result).map_err(|error| error.to_string())?;
    let entry = CacheEntry {
        version: CACHE_VERSION,
        key,
        inputs: inputs_fingerprint(project.root(), &analysis.inputs),
        analysis: result,
    };
    let written = serde_json::to_vec(&entry)
        .map_err(std::io::Error::other)
        .and_then(|bytes| write_atomic(&cache_path, &bytes));
    if let Err(error) = written {
        tracing::debug!("could not write {}: {error}", cache_path.display());
    }
    Ok(analysis)
}

fn read_cache(project: &Project, path: &std::path::Path, key: &str) -> Option<ConnectorsAnalysis> {
    let entry: CacheEntry = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    if entry.version != CACHE_VERSION || entry.key != key {
        return None;
    }
    let analysis = ConnectorsAnalysis::from_result(&entry.analysis).ok()?;
    (inputs_fingerprint(project.root(), &analysis.inputs) == entry.inputs).then_some(analysis)
}

/// Everything the analysis depends on besides the files the TypeScript program reads.
fn key(project: &Project, input: &ConnectorsAnalyzeInput) -> String {
    let mut input_json = input.to_json();
    if let Some(fields) = input_json.as_object_mut() {
        fields.remove("verbose");
    }
    let files: Vec<_> = input
        .tsconfig_path
        .iter()
        .flat_map(|path| tsconfig_chain(path))
        .collect();
    let core_version = project
        .rayfin_package(CORE_PACKAGE)
        .and_then(|package| package.version())
        .unwrap_or_default();
    let feature_flags = std::env::var("RAYFIN_FEATURE_FLAGS").unwrap_or_default();
    cache_key(
        &input_json,
        &[project.rayfin_dir().join("connectors")],
        &files,
        &[rl_model::ANALYZE_JS, core_version, &feature_flags],
    )
}
