//! How the configuration of a running `serve` changed: what applies in place, and what needs a
//! restart.

use rl_project::Project;

use crate::commands::serve::ServeOptions;

/// How `rayfin.yml` and `rayfin-local.toml` differ between the project being served and a new
/// reading of it. Settings rayfin-local doesn't use, comments and formatting are no change.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ConfigChange {
    /// What changed, as `config-changed` names it. `auth`, `name`, `data`, `functions`,
    /// `storage`, `connectors`, `secrets` and `fabric` apply while serving; `frontend`, `server`
    /// and `tokens` need a restart.
    pub what: Vec<&'static str>,
    /// Why a restart is needed: empty when everything applies while serving.
    pub restart: Vec<String>,
}

impl ConfigChange {
    /// Compares the configurations of `served` and `new`, for a `serve` run with `options`;
    /// `dev_server`: whether it runs the app's Vite dev server. Settings a flag overrides don't
    /// count.
    pub fn between(
        served: &Project,
        new: &Project,
        options: &ServeOptions,
        dev_server: bool,
    ) -> Self {
        let (before, after) = (served.config(), new.config());
        let (local_before, local_after) = (served.local_config(), new.local_config());
        let mut change = Self::default();
        change.add(
            "auth",
            before.services.auth != after.services.auth || local_before.auth != local_after.auth,
        );
        change.add("name", served.display_name() != new.display_name());
        change.add("data", before.services.data != after.services.data);
        let inspector =
            options.inspect.is_none() && local_before.functions != local_after.functions;
        change.add(
            "functions",
            !options.no_functions
                && (before.services.functions != after.services.functions || inspector),
        );
        change.add("storage", before.services.storage != after.services.storage);
        change.add(
            "connectors",
            before.connectors != after.connectors
                || local_before.connectors != local_after.connectors,
        );
        change.add("secrets", before.secrets != after.secrets);
        change.add("fabric", local_before.fabric != local_after.fabric);

        let (hosting, next_hosting) = (
            &before.services.static_hosting,
            &after.services.static_hosting,
        );
        if options.static_build {
            let rebuilt = served.static_build_dir() != new.static_build_dir()
                || served.static_output_dir() != new.static_output_dir()
                || hosting.build_command != next_hosting.build_command
                || hosting.index_document() != next_hosting.index_document();
            if rebuilt {
                change.restart(
                    "frontend",
                    "services.staticHosting in rayfin.yml changed how the app is built: restart \
                     rayfin-local to build it again.",
                );
            }
        } else if dev_server && served.frontend_dir() != new.frontend_dir() {
            change.restart(
                "frontend",
                "services.staticHosting in rayfin.yml moved the app's folder: restart rayfin-local \
                 to run its Vite dev server from there.",
            );
        }
        if dev_server
            && options.frontend_port.is_none()
            && local_before.frontend != local_after.frontend
        {
            change.restart(
                "frontend",
                "[frontend] port in rayfin-local.toml changed: restart rayfin-local to run Vite on \
                 that port.",
            );
        }
        let (server, next_server) = (&local_before.server, &local_after.server);
        if (options.port.is_none() && server.port != next_server.port)
            || (options.host.is_none() && server.host != next_server.host)
        {
            change.restart(
                "server",
                "[server] in rayfin-local.toml changed: restart rayfin-local to listen on that \
                 port or host.",
            );
        }
        let (tokens, next_tokens) = (&local_before.tokens, &local_after.tokens);
        if (options.token_source.is_none() && tokens.source != next_tokens.source)
            || (options.tenant.is_none() && tokens.tenant != next_tokens.tenant)
            || tokens.scopes != next_tokens.scopes
        {
            change.restart(
                "tokens",
                "[tokens] in rayfin-local.toml changed: restart rayfin-local to get Azure tokens \
                 that way.",
            );
        }
        change
    }

    pub fn has(&self, what: &str) -> bool {
        self.what.contains(&what)
    }

    fn add(&mut self, what: &'static str, changed: bool) {
        if changed && !self.has(what) {
            self.what.push(what);
        }
    }

    fn restart(&mut self, what: &'static str, reason: &str) {
        self.add(what, true);
        self.restart.push(reason.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const SERVED: &str = "name: notes\nservices:\n  auth:\n    enabled: true\n  data:\n    \
                          enabled: true\n  functions:\n    enabled: true\n  storage:\n    \
                          enabled: true\n";

    /// The project in `dir` with `rayfin.yml` and, when given, `rayfin-local.toml`.
    fn project(dir: &Path, yaml: &str, toml: Option<&str>) -> Project {
        std::fs::create_dir_all(dir.join("rayfin")).unwrap();
        std::fs::write(dir.join("rayfin").join("rayfin.yml"), yaml).unwrap();
        let toml_path = dir.join("rayfin-local.toml");
        match toml {
            Some(toml) => std::fs::write(&toml_path, toml).unwrap(),
            None => {
                let _ = std::fs::remove_file(&toml_path);
            }
        }
        Project::load(dir).unwrap()
    }

    #[test]
    fn service_settings_apply_while_serving() {
        let dir = tempfile::tempdir().unwrap();
        let served = project(dir.path(), SERVED, None);
        let options = ServeOptions::default();
        let between = |yaml: &str, toml: Option<&str>| {
            ConfigChange::between(&served, &project(dir.path(), yaml, toml), &options, true)
        };
        let commented = format!("# Edited by hand.\n{SERVED}version: 1.0.1\n");
        assert_eq!(between(&commented, None), ConfigChange::default());
        let storage_off = SERVED.replace(
            "storage:\n    enabled: true",
            "storage:\n    enabled: false",
        );
        assert_eq!(between(&storage_off, None).what, ["storage"]);
        let redirects = SERVED.replace(
            "auth:\n    enabled: true",
            "auth:\n    enabled: true\n    allowedRedirectUris:\n      - https://notes.example",
        );
        let renamed = redirects.replace("name: notes", "name: Notes");
        let functions_off = renamed.replace(
            "functions:\n    enabled: true",
            "functions:\n    enabled: false",
        );
        let secrets = format!("{functions_off}secrets:\n  - name: API_KEY\n");
        let change = between(&secrets, None);
        assert_eq!(change.what, ["auth", "name", "functions", "secrets"]);
        assert!(change.restart.is_empty());
        let local = "[auth]\nauto_create_users = false\n[fabric]\nanonymous_data_access = false\n\
                     [connectors]\npowerbi_base_url = \"http://127.0.0.1:9000\"\n";
        assert_eq!(
            between(SERVED, Some(local)).what,
            ["auth", "connectors", "fabric"]
        );

        let without_functions = ServeOptions {
            no_functions: true,
            ..ServeOptions::default()
        };
        let only_functions_off = SERVED.replace(
            "functions:\n    enabled: true",
            "functions:\n    enabled: false",
        );
        let change = ConfigChange::between(
            &served,
            &project(dir.path(), &only_functions_off, None),
            &without_functions,
            true,
        );
        assert_eq!(change, ConfigChange::default());
    }

    #[test]
    fn moving_the_frontend_or_changing_ports_and_tokens_needs_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("web")).unwrap();
        let served = project(dir.path(), SERVED, None);
        let options = ServeOptions::default();
        let moved = format!("{SERVED}  staticHosting:\n    enabled: true\n    path: web\n");
        let change =
            ConfigChange::between(&served, &project(dir.path(), &moved, None), &options, true);
        assert_eq!(change.what, ["frontend"]);
        assert!(
            change.restart[0].contains("moved the app's folder"),
            "{change:?}"
        );
        // Without a dev server, nothing runs from there.
        let change =
            ConfigChange::between(&served, &project(dir.path(), &moved, None), &options, false);
        assert_eq!(change, ConfigChange::default());

        let local = Some("[server]\nport = 7300\n[tokens]\nsource = \"az\"\n");
        let change =
            ConfigChange::between(&served, &project(dir.path(), SERVED, local), &options, true);
        assert_eq!(change.what, ["server", "tokens"]);
        assert_eq!(change.restart.len(), 2);
        assert!(change.restart[0].contains("[server]"), "{change:?}");
        // `--port` and `--token-source` win over the file.
        let flags = ServeOptions {
            port: Some(0),
            token_source: Some(rl_tokens::TokenMode::Az),
            ..ServeOptions::default()
        };
        let change =
            ConfigChange::between(&served, &project(dir.path(), SERVED, local), &flags, true);
        assert_eq!(change, ConfigChange::default());
    }
}
