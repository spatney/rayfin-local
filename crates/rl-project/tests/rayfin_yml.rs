//! `rayfin.yml` parsing against the fixtures in `tests/fixtures/`, and (ignored) against the real
//! apps in the fabric-apps repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rl_project::config::{
    AssetAccess, ConnectorAuthType, Dialect, FunctionsAuthType, RayfinConfig,
};
use rl_project::{Project, ProjectError};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn parse(name: &str, vars: &[(&str, &str)]) -> Result<RayfinConfig, ProjectError> {
    let vars: BTreeMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    RayfinConfig::load(&fixture_path(name), &vars, Path::new("rayfin/.env"))
}

#[test]
fn minimal() {
    let config = parse("minimal.yml", &[]).unwrap();
    assert_eq!(config.id.as_deref(), Some("minimal-app"));
    assert_eq!(config.name.as_deref(), Some("Minimal App"));
    assert_eq!(config.version.as_deref(), Some("0.1.0"));
    assert!(!config.services.auth.enabled && !config.services.functions.enabled);
    assert!(config.connectors.is_empty() && config.secrets.is_empty());
    assert!(config.unknown_keys.is_empty());
}

#[test]
fn full() {
    let config = parse("full.yml", &[]).unwrap();
    assert!(config.unknown_keys.is_empty(), "{:?}", config.unknown_keys);

    let auth = &config.services.auth;
    assert!(auth.enabled && auth.password_enabled() && auth.fabric_enabled());
    assert!(auth.external_entra_exchange_enabled() && auth.magic_link_enabled());
    assert_eq!(auth.access_token_lifetime_minutes(), 30);
    assert_eq!(auth.refresh_token_lifetime_days(), 14);
    assert_eq!(auth.refresh_token_absolute_lifetime_minutes(), Some(1440));
    assert_eq!(auth.custom_claims["app_version"], "1.0.0");
    assert_eq!(auth.custom_claims["tier"], "3");
    assert_eq!(auth.scopes, ["read:data", "write:data"]);
    assert_eq!(auth.allowed_redirect_uris.len(), 2);
    assert_eq!(
        auth.dynamic_claims.as_ref().and_then(|d| d.enabled),
        Some(true)
    );
    let magic_link = auth
        .passwordless
        .as_ref()
        .and_then(|p| p.magic_link.as_ref())
        .unwrap();
    assert_eq!(magic_link.expiry_minutes(), 10);
    let smtp = auth.email.as_ref().and_then(|e| e.smtp.as_ref()).unwrap();
    assert_eq!(
        (smtp.host.as_deref(), smtp.port, smtp.web_port),
        (Some("maildev"), Some(1025), Some(1080))
    );

    let data = &config.services.data;
    assert!(data.enabled && data.allow_introspection);
    assert_eq!(data.dialect(), Dialect::PostgreSql);
    assert_eq!(data.build_command.as_deref(), Some("npm run build:data"));

    let functions = &config.services.functions;
    assert_eq!(
        functions.auth.and_then(|a| a.auth_type),
        Some(FunctionsAuthType::Application)
    );

    let hosting = &config.services.static_hosting;
    assert_eq!(
        (hosting.folder(), hosting.index_document()),
        ("build", "app.html")
    );
    assert_eq!(hosting.root.as_deref(), Some("."));
    assert_eq!(hosting.asset_access, Some(AssetAccess::Protected));
    assert_eq!(hosting.embedded.and_then(|e| e.only), Some(true));
    assert_eq!(config.services.telemetry.map(|t| t.enabled), Some(true));
    assert_eq!(
        config.services.connectors_opt_in.map(|t| t.enabled),
        Some(true)
    );
    assert_eq!(
        config
            .frontend
            .as_ref()
            .and_then(|f| f.framework.as_deref()),
        Some("vite")
    );

    let sales = config.connector("sales").unwrap();
    assert_eq!(sales.connector_type, "fabric-semanticmodel");
    assert_eq!(sales.version.as_deref(), Some("1"));
    assert_eq!(
        sales.workspace_id(),
        Some("67e66ea4-5ea2-4f76-98da-6144ba7d52fa")
    );
    assert_eq!(
        sales.item_id(),
        Some("7ee53e06-5065-4c1b-96bb-b8ef12abaad6")
    );
    assert_eq!(sales.auth_type(), Some(ConnectorAuthType::Delegated));
    assert!(sales.allows_operation("executeQuery"));
    let orders = config.connector("orders").unwrap();
    assert!(orders.allows_operation("create") && !orders.allows_operation("delete"));

    let secrets: Vec<_> = config
        .secrets
        .iter()
        .map(|s| (s.name.as_str(), s.description.is_some()))
        .collect();
    assert_eq!(secrets, [("OPENAI_KEY", true), ("NO_DESCRIPTION", false)]);
}

#[test]
fn workspace_layout() {
    let config = parse("workspace.yml", &[]).unwrap();
    assert!(config.unknown_keys.is_empty(), "{:?}", config.unknown_keys);
    assert_eq!(config.services.data.path.as_deref(), Some("packages/data"));
    assert_eq!(
        config.services.static_hosting.path.as_deref(),
        Some("packages/frontend")
    );
    assert_eq!(
        config.services.static_hosting.asset_access,
        Some(AssetAccess::Public)
    );
    assert_eq!(
        config.services.functions.path.as_deref(),
        Some("packages\\functions")
    );
}

#[test]
fn legacy_connectors() {
    let config = parse("legacy-connectors.yml", &[]).unwrap();
    assert!(config.unknown_keys.is_empty(), "{:?}", config.unknown_keys);
    let types: Vec<_> = config
        .connectors
        .iter()
        .map(|c| {
            (
                c.name.as_str(),
                c.connector_type.as_str(),
                c.version.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        types,
        [
            ("inventory", "fabric-sqlanalytics", None),
            ("dataset", "fabric-semanticmodel", Some("2"))
        ]
    );
    assert_eq!(config.connectors[0].item_id(), Some("item-1"));
}

#[test]
fn interpolated() {
    let config = parse(
        "interpolated.yml",
        &[
            ("TOKEN_MINUTES", "45"),
            ("AUTH_ENABLED", "true"),
            ("RAYFIN_PUBLIC_FRONTEND_PORT", "5175"),
            ("TELEMETRY_WORKSPACE_ID", "ws-1"),
            ("TELEMETRY_ITEM_ID", "item-1"),
        ],
    )
    .unwrap();
    assert_eq!(config.id.as_deref(), Some("interpolated-app"));
    assert!(config.services.auth.enabled);
    assert_eq!(config.services.auth.access_token_lifetime_minutes(), 45);
    assert_eq!(
        config.services.auth.allowed_redirect_uris,
        ["http://localhost:5175"]
    );
    assert_eq!(config.services.data.dialect(), Dialect::MsSql);
    assert_eq!(config.connectors[0].workspace_id(), Some("ws-1"));

    let err = parse("interpolated.yml", &[("TOKEN_MINUTES", "45")]).unwrap_err();
    assert!(
        matches!(&err, ProjectError::MissingVariable { var, context, .. }
            if var == "TELEMETRY_WORKSPACE_ID" && context == "connectors[0].config.workspaceId"),
        "{err}"
    );
}

#[test]
fn unknown_keys() {
    let config = parse("unknown-keys.yml", &[]).unwrap();
    assert_eq!(
        config.unknown_keys,
        [
            "publishable_key",
            "deployment",
            "services.auth.issuer",
            "services.auth.password.minLength",
            "services.data.connectionString",
            "services.cache",
            "connectors[0].database",
        ]
    );
    assert!(config.services.auth.password_enabled());
}

/// Parses every `apps/*/rayfin/rayfin.yml` in the fabric-apps repository (read-only).
/// Run with `cargo test -p rl-project --test rayfin_yml -- --ignored --nocapture`; set
/// `RAYFIN_APPS_DIR` to use another checkout.
#[test]
#[ignore = "needs the fabric-apps repository"]
fn parses_every_fabric_app() {
    let apps = std::env::var_os("RAYFIN_APPS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\sapatney\code\fabric-apps\apps"));
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&apps)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", apps.display()))
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| dir.join("rayfin").join("rayfin.yml").is_file())
        .collect();
    dirs.sort();

    let mut failures = Vec::new();
    for dir in &dirs {
        let name = dir.file_name().unwrap().to_string_lossy();
        match Project::load(dir) {
            Ok(project) => {
                let config = project.config();
                let frontend = project.frontend_dir().strip_prefix(project.root()).unwrap();
                println!(
                    "ok   {name}: sdk {}, frontend {}, connectors {}, functions {}",
                    project.sdk_version().unwrap_or("not installed"),
                    Path::new(".").join(frontend).display(),
                    config.connectors.len(),
                    config.services.functions.enabled,
                );
                for warning in project
                    .warnings()
                    .iter()
                    .chain(project.sdk_version_warning().as_ref())
                {
                    println!("     warning: {warning}");
                }
            }
            Err(err) => {
                println!("FAIL {name}: {err}");
                failures.push(name.to_string());
            }
        }
    }
    println!("{} apps parsed, {} failed", dirs.len(), failures.len());
    assert!(!dirs.is_empty(), "no apps found in {}", apps.display());
    assert!(failures.is_empty(), "failed: {failures:?}");
}
