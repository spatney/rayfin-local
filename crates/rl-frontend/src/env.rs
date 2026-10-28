//! The environment that points the app's frontend at the local backend.

/// What the frontend needs to reach the local backend; the `serve` command fills it in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RayfinEnv {
    /// The backend origin, such as `http://localhost:7171`.
    pub api_origin: String,
    /// The base of the function routes, `<api_origin>/.rayfin-local/functions`: `rayfinLocalDev()`
    /// proxies `/functions/<name>/invoke` and `/.rayfin/api/<name>` to `<functions_url>/api/<name>`.
    pub functions_url: String,
    pub publishable_key: String,
    pub workspace_id: String,
    pub item_id: String,
    pub tenant_id: Option<String>,
    /// The sign-in portal, `<api_origin>/.rayfin-local/portal`: the SDK opens
    /// `<portal_url>/secureItemEmbed` in the sign-in popup.
    pub portal_url: String,
    /// The project's other public variables, already mapped the way `rayfin env --framework vite`
    /// maps them (`RAYFIN_PUBLIC_FOO` → `VITE_RAYFIN_FOO`).
    pub extra: Vec<(String, String)>,
}

/// The variables to set on the Vite dev server or the build: `extra`, then the local backend's
/// values under both their `RAYFIN_PUBLIC_*` and their `VITE_*` names. Later entries win.
///
/// Process variables beat the app's `.env*` files in Vite's `loadEnv`, and `rayfinLocalDev()`
/// reads `RAYFIN_PUBLIC_API_URL` and `RAYFIN_PUBLIC_FUNCTIONS_URL` before them, so the values that
/// `rayfin env` wrote for the deployed app never apply. That is why a missing tenant is set to an
/// empty value rather than left out.
pub fn frontend_env(env: &RayfinEnv) -> Vec<(String, String)> {
    let api_url = format!("{}/", env.api_origin.trim().trim_end_matches('/'));
    let functions_url = env.functions_url.trim().trim_end_matches('/');
    let tenant_id = env.tenant_id.as_deref().unwrap_or_default();
    let values = [
        ("RAYFIN_PUBLIC_API_URL", api_url.as_str()),
        ("VITE_RAYFIN_API_URL", &api_url),
        // The backend URL in the connectors packages' own docs (`ConnectorsRayfinClient`).
        ("VITE_RAYFIN_BASE_URL", &api_url),
        ("RAYFIN_PUBLIC_PUBLISHABLE_KEY", &env.publishable_key),
        ("VITE_RAYFIN_PUBLISHABLE_KEY", &env.publishable_key),
        ("RAYFIN_PUBLIC_FUNCTIONS_URL", functions_url),
        ("VITE_RAYFIN_FUNCTIONS_URL", functions_url),
        ("RAYFIN_PUBLIC_WORKSPACE_ID", &env.workspace_id),
        ("VITE_FABRIC_WORKSPACE_ID", &env.workspace_id),
        ("RAYFIN_PUBLIC_ITEM_ID", &env.item_id),
        ("VITE_FABRIC_ITEM_ID", &env.item_id),
        ("RAYFIN_PUBLIC_TENANT_ID", tenant_id),
        ("VITE_FABRIC_TENANT_ID", tenant_id),
        ("RAYFIN_PUBLIC_PORTAL_URL", &env.portal_url),
        ("VITE_FABRIC_PORTAL_URL", &env.portal_url),
    ];
    let mut vars = env.extra.clone();
    vars.extend(
        values
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned())),
    );
    vars
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> RayfinEnv {
        RayfinEnv {
            api_origin: "http://localhost:7171".into(),
            functions_url: "http://localhost:7171/.rayfin-local/functions/".into(),
            publishable_key: "pk-local".into(),
            workspace_id: "ws".into(),
            item_id: "item".into(),
            tenant_id: None,
            portal_url: "http://localhost:7171/.rayfin-local/portal".into(),
            extra: vec![
                ("VITE_SERVICE_MODE".into(), "rayfin".into()),
                (
                    "VITE_RAYFIN_API_URL".into(),
                    "https://deployed.example/".into(),
                ),
            ],
        }
    }

    /// The value a process ends up with: the last entry for the name.
    fn effective<'a>(vars: &'a [(String, String)], name: &str) -> Option<&'a str> {
        vars.iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn local_values_win_over_extra_under_both_names() {
        let vars = frontend_env(&local());
        assert_eq!(vars[0].0, "VITE_SERVICE_MODE", "extra comes first");
        for (name, value) in [
            ("RAYFIN_PUBLIC_API_URL", "http://localhost:7171/"),
            ("VITE_RAYFIN_API_URL", "http://localhost:7171/"),
            ("VITE_RAYFIN_BASE_URL", "http://localhost:7171/"),
            ("RAYFIN_PUBLIC_PUBLISHABLE_KEY", "pk-local"),
            ("VITE_RAYFIN_PUBLISHABLE_KEY", "pk-local"),
            (
                "RAYFIN_PUBLIC_FUNCTIONS_URL",
                "http://localhost:7171/.rayfin-local/functions",
            ),
            (
                "VITE_RAYFIN_FUNCTIONS_URL",
                "http://localhost:7171/.rayfin-local/functions",
            ),
            ("RAYFIN_PUBLIC_WORKSPACE_ID", "ws"),
            ("VITE_FABRIC_WORKSPACE_ID", "ws"),
            ("RAYFIN_PUBLIC_ITEM_ID", "item"),
            ("VITE_FABRIC_ITEM_ID", "item"),
            (
                "RAYFIN_PUBLIC_PORTAL_URL",
                "http://localhost:7171/.rayfin-local/portal",
            ),
            (
                "VITE_FABRIC_PORTAL_URL",
                "http://localhost:7171/.rayfin-local/portal",
            ),
            ("VITE_SERVICE_MODE", "rayfin"),
        ] {
            assert_eq!(effective(&vars, name), Some(value), "{name}");
        }
    }

    #[test]
    fn a_missing_tenant_blanks_the_deployed_one() {
        let vars = frontend_env(&local());
        assert_eq!(effective(&vars, "RAYFIN_PUBLIC_TENANT_ID"), Some(""));
        assert_eq!(effective(&vars, "VITE_FABRIC_TENANT_ID"), Some(""));

        let vars = frontend_env(&RayfinEnv {
            tenant_id: Some("tenant".into()),
            api_origin: "http://127.0.0.1:7171/".into(),
            ..local()
        });
        assert_eq!(effective(&vars, "VITE_FABRIC_TENANT_ID"), Some("tenant"));
        assert_eq!(
            effective(&vars, "VITE_RAYFIN_API_URL"),
            Some("http://127.0.0.1:7171/")
        );
    }
}
