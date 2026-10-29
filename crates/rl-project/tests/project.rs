//! Loading whole projects from temporary directories.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rl_project::{LoadOptions, LocalState, Project, ProjectError};

/// A temporary project; `files` are (relative path, content) pairs.
fn project_dir(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    for (path, content) in files {
        let path = temp.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let root = dunce::canonicalize(temp.path()).unwrap();
    (temp, root)
}

/// Loads without reading the process environment.
fn load(root: &Path, process_env: &[(&str, &str)]) -> Result<Project, ProjectError> {
    let process_env: BTreeMap<String, String> = process_env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Project::load_with(
        root,
        &LoadOptions {
            process_env: Some(process_env),
            ..Default::default()
        },
    )
}

fn package(name: &str, version: &str) -> String {
    format!("{{\"name\":\"{name}\",\"version\":\"{version}\"}}")
}

const MINIMAL: &str = "id: app\nname: App\nversion: 1.0.0\n";

#[test]
fn discovers_the_root_from_nested_directories() {
    let (_temp, root) = project_dir(&[
        ("rayfin/rayfin.yml", MINIMAL),
        ("src/components/Button.tsx", ""),
        ("rayfin/functions/src/index.ts", ""),
    ]);
    assert_eq!(
        Project::find_root(&root.join("src").join("components")).unwrap(),
        root
    );
    assert_eq!(
        Project::find_root(&root.join("src/components/Button.tsx")).unwrap(),
        root
    );
    let project = Project::discover(&root.join("rayfin").join("functions")).unwrap();
    assert_eq!(project.root(), root);
    assert_eq!(project.display_name(), "App");
}

#[test]
fn reports_a_missing_project() {
    let (_temp, root) = project_dir(&[("src/main.ts", "")]);
    let err = Project::discover(&root.join("src")).unwrap_err();
    let message = err.to_string();
    let expected = format!(
        "No Rayfin project found in {} or its parents",
        root.join("src").display()
    );
    assert!(message.starts_with(&expected), "{message}");
    assert!(message.contains("expected rayfin/rayfin.yml"), "{message}");
    assert!(!message.contains(r"\\?\"), "{message}");
}

#[test]
fn defaults_service_paths_to_the_conventional_folders() {
    let (_temp, root) = project_dir(&[("rayfin/rayfin.yml", MINIMAL)]);
    let project = load(&root, &[]).unwrap();
    assert_eq!(project.data_service_root(), root);
    assert_eq!(project.data_dir(), root.join("rayfin").join("data"));
    assert_eq!(
        project.storage_schema_dir(),
        root.join("rayfin").join("storage")
    );
    // Byte-for-byte: native separators only, since other tools display and spawn in it.
    let functions = root.join("rayfin").join("functions");
    assert_eq!(project.functions_dir().as_os_str(), functions.as_os_str());
    assert_eq!(project.static_build_dir(), root);
    assert_eq!(project.frontend_dir(), root);
    assert_eq!(project.static_output_dir(), root.join("dist"));
    assert!(project.warnings().is_empty(), "{:?}", project.warnings());
}

#[test]
fn resolves_service_paths_of_a_workspace_layout() {
    let yml = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace.yml"),
    )
    .unwrap();
    let (_temp, root) = project_dir(&[("rayfin/rayfin.yml", &yml)]);
    let project = load(&root, &[]).unwrap();
    let packages = root.join("packages");
    assert_eq!(project.data_service_root(), packages.join("data"));
    assert_eq!(
        project.data_dir(),
        packages.join("data").join("rayfin").join("data")
    );
    assert_eq!(project.storage_service_root(), packages.join("storage"));
    let functions = packages.join("functions");
    assert_eq!(project.functions_dir().as_os_str(), functions.as_os_str());
    assert_eq!(project.static_build_dir(), packages.join("frontend"));
    assert_eq!(project.frontend_dir(), packages.join("frontend"));
    assert_eq!(
        project.static_output_dir(),
        packages.join("frontend").join("dist")
    );
}

#[test]
fn applies_static_hosting_root_and_folder() {
    let yml = "services:\n  staticHosting:\n    enabled: true\n    path: web\n    root: app\n    \
               folder: out/site\n";
    let (_temp, root) = project_dir(&[("rayfin/rayfin.yml", yml)]);
    let project = load(&root, &[]).unwrap();
    assert_eq!(project.static_build_dir(), root.join("web"));
    assert_eq!(project.frontend_dir(), root.join("web").join("app"));
    assert_eq!(
        project.static_output_dir(),
        root.join("web").join("app").join("out").join("site")
    );
}

#[test]
fn rejects_paths_outside_the_project_for_enabled_services_only() {
    let yml = "services:\n  data:\n    enabled: true\n    path: ../shared/data\n";
    let (_temp, root) = project_dir(&[("rayfin/rayfin.yml", yml)]);
    let message = load(&root, &[]).unwrap_err().to_string();
    assert!(
        message.contains("services.data.path '../shared/data'"),
        "{message}"
    );
    assert!(
        message.contains("it points outside the project"),
        "{message}"
    );

    let yml = "services:\n  functions:\n    enabled: false\n    path: /opt/functions\n";
    let (_temp, root) = project_dir(&[("rayfin/rayfin.yml", yml)]);
    let project = load(&root, &[]).unwrap();
    assert_eq!(
        project.functions_dir(),
        root.join("rayfin").join("functions")
    );
    assert!(project.warnings()[0].contains("services.functions.path '/opt/functions'"));
}

#[test]
fn interpolates_from_the_env_file_overlaid_with_the_process_environment() {
    let yml = "services:\n  auth:\n    enabled: true\n    expiryInMinutes: ${MINUTES}\n\
               connectors:\n  - name: c\n    type: kusto\n    config:\n      itemId: ${ITEM}\n";
    let (_temp, root) = project_dir(&[
        ("rayfin/rayfin.yml", yml),
        ("rayfin/.env", "MINUTES=30\nITEM=from-file\n"),
    ]);
    let project = load(&root, &[("ITEM", "from-shell")]).unwrap();
    assert_eq!(project.config().services.auth.expiry_in_minutes, Some(30));
    assert_eq!(project.config().connectors[0].item_id(), Some("from-shell"));
    assert_eq!(project.env().path(), root.join("rayfin").join(".env"));

    let err = load(&root, &[("RAYFIN_ENV_FILE", "missing.env")]);
    assert!(matches!(err, Err(ProjectError::MissingVariable { .. })));
}

#[test]
fn honors_rayfin_env_file_and_explicit_env_files() {
    let (_temp, root) = project_dir(&[
        ("rayfin/rayfin.yml", MINIMAL),
        ("config/dev.env", "RAYFIN_PUBLIC_FRONTEND_PORT=5190\n"),
    ]);
    let project = load(&root, &[("RAYFIN_ENV_FILE", "config/dev.env")]).unwrap();
    assert_eq!(project.env().frontend_port(), Some(5190));

    let options = LoadOptions {
        env_file: Some(PathBuf::from("config/missing.env")),
        process_env: Some(BTreeMap::new()),
    };
    let err = Project::load_with(&root, &options).unwrap_err();
    assert!(matches!(err, ProjectError::EnvFileNotFound { .. }), "{err}");
}

#[test]
fn installed_packages_beat_declared_ones() {
    let manifest = r#"{
        "name": "app",
        "scripts": { "dev": "rayfin up && vite" },
        "dependencies": { "@microsoft/rayfin-client": "1.36.2", "@microsoft/rayfin-core": "^1.36.0" },
        "devDependencies": { "@microsoft/rayfin-cli": "1.36.2", "vite": "^7" }
    }"#;
    let (_temp, root) = project_dir(&[
        ("rayfin/rayfin.yml", MINIMAL),
        ("package.json", manifest),
        (
            "node_modules/@microsoft/rayfin-client/package.json",
            &package("x", "1.37.0"),
        ),
        (
            "node_modules/@microsoft/rayfin-core/package.json",
            &package("x", "1.37.0"),
        ),
        (
            "node_modules/@microsoft/rayfin-auth/package.json",
            &package("x", "1.37.0"),
        ),
        ("node_modules/vite/package.json", &package("vite", "7.3.2")),
    ]);
    let project = load(&root, &[]).unwrap();
    let names: Vec<_> = project
        .rayfin_packages()
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "@microsoft/rayfin-auth",
            "@microsoft/rayfin-cli",
            "@microsoft/rayfin-client",
            "@microsoft/rayfin-core"
        ]
    );
    let client = project.rayfin_package("rayfin-client").unwrap();
    assert_eq!(
        (client.declared.as_deref(), client.installed.as_deref()),
        (Some("1.36.2"), Some("1.37.0"))
    );
    assert_eq!(
        client.installed_dir.as_deref(),
        Some(root.join("node_modules/@microsoft/rayfin-client").as_path())
    );
    let cli = project.rayfin_package("@microsoft/rayfin-cli").unwrap();
    assert_eq!(
        (cli.version(), cli.installed.as_deref()),
        (Some("1.36.2"), None)
    );
    assert_eq!(project.sdk_version(), Some("1.37.0"));
    let warning = project.sdk_version_warning().unwrap();
    assert!(
        warning.starts_with("@microsoft/rayfin-client 1.37.0 is not the 1.36 SDK"),
        "{warning}"
    );
    project.check_installed().unwrap();
    assert_eq!(
        project.frontend_package().unwrap().script("dev"),
        Some("rayfin up && vite")
    );
    assert_eq!(
        rl_project::resolve_node_package(project.frontend_dir(), "vite"),
        Some(root.join("node_modules").join("vite"))
    );
}

#[test]
fn workspace_frontends_use_hoisted_packages() {
    let yml = "services:\n  staticHosting:\n    enabled: true\n    path: packages/frontend\n";
    let (_temp, root) = project_dir(&[
        ("rayfin/rayfin.yml", yml),
        (
            "package.json",
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        ),
        (
            "packages/frontend/package.json",
            r#"{"name":"web","dependencies":{"@microsoft/rayfin-client":"1.36.2"}}"#,
        ),
        (
            "node_modules/@microsoft/rayfin-client/package.json",
            &package("x", "1.36.2"),
        ),
        (
            "node_modules/@microsoft/rayfin-core/package.json",
            &package("x", "1.36.2"),
        ),
    ]);
    let project = load(&root, &[]).unwrap();
    assert_eq!(
        project.frontend_package().unwrap().name.as_deref(),
        Some("web")
    );
    assert_eq!(
        project.root_package().unwrap().name.as_deref(),
        Some("root")
    );
    assert_eq!(project.sdk_version(), Some("1.36.2"));
    assert_eq!(project.sdk_version_warning(), None);
    project.check_installed().unwrap();
}

#[test]
fn missing_node_modules_ask_for_npm_install() {
    let (_temp, root) = project_dir(&[
        ("rayfin/rayfin.yml", MINIMAL),
        (
            "package.json",
            r#"{"dependencies":{"@microsoft/rayfin-core":"1.36.2"}}"#,
        ),
    ]);
    let project = load(&root, &[]).unwrap();
    assert_eq!(project.sdk_version(), Some("1.36.2"));
    let message = project.check_installed().unwrap_err().to_string();
    assert!(
        message.contains(&format!("Run `npm install` in {} first", root.display())),
        "{message}"
    );
}

#[test]
fn reports_warnings_without_failing() {
    let yml = "id: app\ndeployment:\n  itemId: x\nservices:\n  storage:\n    enabled: true\n  \
               functions:\n    enabled: true\n";
    let (_temp, root) = project_dir(&[
        ("rayfin/rayfin.yml", yml),
        ("rayfin/.env", "RAYFIN_PUBLIC_FRONTEND_PORT=fast\n"),
    ]);
    let project = load(&root, &[]).unwrap();
    let warnings = project.warnings().join("\n");
    for expected in [
        "unknown setting `deployment`",
        "RAYFIN_PUBLIC_FRONTEND_PORT 'fast' is not a valid port",
        "services.functions.auth.type should be `application`",
        "storage requires the data service",
    ] {
        assert!(
            warnings.contains(expected),
            "missing {expected:?} in:\n{warnings}"
        );
    }
    assert_eq!(
        project.preferred_frontend_port(),
        rl_project::DEFAULT_FRONTEND_PORT
    );
}

#[test]
fn rayfin_local_toml_overrides_the_frontend_port() {
    let (_temp, root) = project_dir(&[
        ("rayfin/rayfin.yml", MINIMAL),
        ("rayfin/.env", "RAYFIN_PUBLIC_FRONTEND_PORT=5175\n"),
    ]);
    assert_eq!(load(&root, &[]).unwrap().preferred_frontend_port(), 5175);
    std::fs::write(root.join("rayfin-local.toml"), "[frontend]\nport = 5200\n").unwrap();
    assert_eq!(load(&root, &[]).unwrap().preferred_frontend_port(), 5200);
    std::fs::write(root.join("rayfin-local.toml"), "[frontend]\nprot = 5200\n").unwrap();
    let message = load(&root, &[]).unwrap_err().to_string();
    assert!(
        message.contains("rayfin-local.toml (line 2, column 1): unknown field `prot`"),
        "{message}"
    );
}

#[test]
fn state_folder_lives_in_the_project_and_resets() {
    let (_temp, root) = project_dir(&[
        ("rayfin/rayfin.yml", MINIMAL),
        ("rayfin/.env", "RAYFIN_PUBLIC_ITEM_ID=item-1\n"),
    ]);
    let project = load(&root, &[]).unwrap();
    let state = LocalState::load_or_create(&project).unwrap();
    assert_eq!(state.dir(), root.join(".rayfin-local"));
    assert_eq!(
        state.db_path(),
        root.join(".rayfin-local").join("db.sqlite")
    );
    assert_eq!(state.data().item_id, "item-1");
    assert!(!root.join(".gitignore").exists());

    assert!(LocalState::reset(&project).unwrap());
    assert!(!root.join(".rayfin-local").exists());
    assert!(!LocalState::reset(&project).unwrap());

    // A database another program holds open, as SQLite does (without delete sharing), stops the
    // reset before anything else is deleted.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let state = LocalState::load_or_create(&project).unwrap();
        state
            .save_secrets(&[("KEY".to_owned(), "value".to_owned())].into())
            .unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0x1 | 0x2)
            .open(state.db_path())
            .unwrap();
        assert!(matches!(
            LocalState::reset(&project),
            Err(rl_project::ProjectError::Reset { .. })
        ));
        assert!(root.join(".rayfin-local").join(".gitignore").is_file());
        assert!(state.secrets_path().is_file());
        drop(held);
        assert!(LocalState::reset(&project).unwrap());
    }
}
