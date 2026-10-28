//! `rayfin/.env`: parsing, the `RAYFIN_PUBLIC_*` accessors, and the `rayfin env` key mapping.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::ProjectError;

/// Prefix of the variables that are exposed to the frontend.
pub const RAYFIN_PUBLIC_PREFIX: &str = "RAYFIN_PUBLIC_";
/// The local dev-server port `rayfin dev` assigns to the frontend.
pub const FRONTEND_PORT_VAR: &str = "RAYFIN_PUBLIC_FRONTEND_PORT";
pub const API_URL_VAR: &str = "RAYFIN_PUBLIC_API_URL";
pub const PUBLISHABLE_KEY_VAR: &str = "RAYFIN_PUBLIC_PUBLISHABLE_KEY";
pub const FUNCTIONS_URL_VAR: &str = "RAYFIN_PUBLIC_FUNCTIONS_URL";
pub const WORKSPACE_ID_VAR: &str = "RAYFIN_PUBLIC_WORKSPACE_ID";
pub const ITEM_ID_VAR: &str = "RAYFIN_PUBLIC_ITEM_ID";
pub const TENANT_ID_VAR: &str = "RAYFIN_PUBLIC_TENANT_ID";
pub const PORTAL_URL_VAR: &str = "RAYFIN_PUBLIC_PORTAL_URL";

/// Parses dotenv content the way the Rayfin CLI reads `rayfin/.env` (`parseEnvContent`), plus
/// the multi-line quoted values and quoted values with a trailing comment that `dotenv` accepts.
///
/// - Blank lines and lines starting with `#` are skipped; `export KEY=value` is accepted.
/// - Keys must match `[A-Za-z_][A-Za-z0-9_]*`; other lines are ignored.
/// - `'…'`, `"…"` and `` `…` `` quotes are stripped. Inside double quotes `\n`, `\r` and `\t`
///   are expanded.
/// - Unquoted values end at ` #` (an inline comment) and are trimmed; `KEY=` is an empty value.
/// - A later assignment of the same key wins.
pub fn parse_env(content: &str) -> BTreeMap<String, String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let lines: Vec<&str> = content
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let mut vars = BTreeMap::new();
    let mut next = 0;
    while next < lines.len() {
        let line = lines[next].trim();
        next += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = match line.strip_prefix("export") {
            Some(rest) if rest.starts_with([' ', '\t']) => rest.trim_start(),
            _ => line,
        };
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        if !is_valid_key(key) {
            continue;
        }
        let raw = line[eq + 1..].trim();
        let value = match raw.chars().next() {
            Some(quote @ ('"' | '\'' | '`')) => match quoted_value(raw, quote, &lines[next..]) {
                Some((inner, extra_lines)) => {
                    next += extra_lines;
                    if quote == '"' {
                        inner
                            .replace("\\n", "\n")
                            .replace("\\r", "\r")
                            .replace("\\t", "\t")
                    } else {
                        inner
                    }
                }
                None => unquoted_value(raw),
            },
            _ => unquoted_value(raw),
        };
        vars.insert(key.to_string(), value);
    }
    vars
}

fn is_valid_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn unquoted_value(raw: &str) -> String {
    let comment = raw
        .match_indices('#')
        .find(|(i, _)| *i > 0 && matches!(raw.as_bytes()[i - 1], b' ' | b'\t'));
    let end = comment.map_or(raw.len(), |(i, _)| i);
    raw[..end].trim_end().to_string()
}

/// The text inside quotes that start `raw`, and how many following lines it spans. `None` when
/// the quote is never closed (the value is then read as unquoted, as the CLI does).
fn quoted_value(raw: &str, quote: char, following: &[&str]) -> Option<(String, usize)> {
    let body = &raw[1..];
    if raw.len() >= 2 && raw.ends_with(quote) {
        return Some((body[..body.len() - 1].to_string(), 0));
    }
    if let Some(end) = closing_quote(body, quote) {
        // `KEY="value" # comment`
        return is_comment_or_blank(&body[end + 1..]).then(|| (body[..end].to_string(), 0));
    }
    let mut value = body.to_string();
    for (index, line) in following.iter().enumerate() {
        value.push('\n');
        if let Some(end) = closing_quote(line, quote) {
            if !is_comment_or_blank(&line[end + 1..]) {
                return None;
            }
            value.push_str(&line[..end]);
            return Some((value, index + 1));
        }
        value.push_str(line);
    }
    None
}

fn closing_quote(text: &str, quote: char) -> Option<usize> {
    let mut chars = text.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == quote {
            return Some(i);
        }
        if c == '\\' && quote == '"' {
            chars.next();
        }
    }
    None
}

fn is_comment_or_blank(rest: &str) -> bool {
    let rest = rest.trim_start();
    rest.is_empty() || rest.starts_with('#')
}

/// A dotenv file (normally `rayfin/.env`). A missing file is empty.
///
/// Its `Debug` output lists the variable names only: the file may hold secrets.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct EnvFile {
    path: PathBuf,
    exists: bool,
    vars: BTreeMap<String, String>,
}

impl std::fmt::Debug for EnvFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvFile")
            .field("path", &self.path)
            .field("exists", &self.exists)
            .field("keys", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl EnvFile {
    /// Reads `path`; a missing file gives an empty [`EnvFile`].
    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(Self::from_content(path, &content)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: path.to_path_buf(),
                exists: false,
                vars: BTreeMap::new(),
            }),
            Err(source) => Err(ProjectError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// An env file with the given content (as if it existed at `path`).
    pub fn from_content(path: &Path, content: &str) -> Self {
        Self {
            path: path.to_path_buf(),
            exists: true,
            vars: parse_env(content),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the file exists.
    pub fn exists(&self) -> bool {
        self.exists
    }

    /// Every variable in the file.
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    /// The raw value of `key` (possibly empty).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// The value of `key`, treating an empty value as unset.
    pub fn non_empty(&self, key: &str) -> Option<&str> {
        self.get(key)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// The `RAYFIN_PUBLIC_*` variables (the ones `rayfin env` exposes to the frontend).
    pub fn public_env(&self) -> BTreeMap<String, String> {
        self.vars
            .iter()
            .filter(|(key, _)| key.starts_with(RAYFIN_PUBLIC_PREFIX))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    /// `RAYFIN_PUBLIC_FRONTEND_PORT` when it is a valid port (decimal, 1–65535).
    pub fn frontend_port(&self) -> Option<u16> {
        self.get(FRONTEND_PORT_VAR).and_then(parse_port)
    }

    /// `RAYFIN_PUBLIC_WORKSPACE_ID`: the Fabric workspace of the deployed app.
    pub fn workspace_id(&self) -> Option<&str> {
        self.non_empty(WORKSPACE_ID_VAR)
    }

    /// `RAYFIN_PUBLIC_ITEM_ID`: the deployed app's Fabric item.
    pub fn item_id(&self) -> Option<&str> {
        self.non_empty(ITEM_ID_VAR)
    }

    /// `RAYFIN_PUBLIC_TENANT_ID`: the Entra tenant of the workspace.
    pub fn tenant_id(&self) -> Option<&str> {
        self.non_empty(TENANT_ID_VAR)
    }

    /// `RAYFIN_PUBLIC_PORTAL_URL`, for example `https://app.fabric.microsoft.com`.
    pub fn portal_url(&self) -> Option<&str> {
        self.non_empty(PORTAL_URL_VAR)
    }

    /// `RAYFIN_PUBLIC_API_URL`: the deployed backend (rayfin-local replaces it with its own).
    pub fn api_url(&self) -> Option<&str> {
        self.non_empty(API_URL_VAR)
    }

    /// `RAYFIN_PUBLIC_PUBLISHABLE_KEY` of the deployed backend.
    pub fn publishable_key(&self) -> Option<&str> {
        self.non_empty(PUBLISHABLE_KEY_VAR)
    }

    /// `RAYFIN_PUBLIC_FUNCTIONS_URL` (set by `rayfin dev` for the local Functions host).
    pub fn functions_url(&self) -> Option<&str> {
        self.non_empty(FUNCTIONS_URL_VAR)
    }
}

/// Parses a port the way the CLI does: decimal digits only, 1–65535.
pub(crate) fn parse_port(value: &str) -> Option<u16> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse::<u16>().ok().filter(|port| *port > 0)
}

/// The frontend frameworks `rayfin env --framework` supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrontendFramework {
    Vite,
    NextJs,
    /// No prefix: `RAYFIN_PUBLIC_` is stripped.
    Plain,
}

impl FrontendFramework {
    fn prefix(self) -> &'static str {
        match self {
            FrontendFramework::Vite => "VITE_",
            FrontendFramework::NextJs => "NEXT_PUBLIC_",
            FrontendFramework::Plain => "",
        }
    }

    /// The canonical frontend name of a well-known key.
    fn alias(self, key: &str) -> Option<&'static str> {
        let (_, vite, next) = FRAMEWORK_ALIASES
            .iter()
            .find(|(public, ..)| *public == key)?;
        match self {
            FrontendFramework::Vite => Some(vite),
            FrontendFramework::NextJs => Some(next),
            FrontendFramework::Plain => None,
        }
    }
}

/// `FRAMEWORK_ALIASES` in the CLI's `env.ts`: (public key, Vite name, Next.js name).
const FRAMEWORK_ALIASES: [(&str, &str, &str); 9] = [
    (
        API_URL_VAR,
        "VITE_RAYFIN_API_URL",
        "NEXT_PUBLIC_RAYFIN_API_URL",
    ),
    (
        PUBLISHABLE_KEY_VAR,
        "VITE_RAYFIN_PUBLISHABLE_KEY",
        "NEXT_PUBLIC_RAYFIN_PUBLISHABLE_KEY",
    ),
    (
        FUNCTIONS_URL_VAR,
        "VITE_RAYFIN_FUNCTIONS_URL",
        "NEXT_PUBLIC_RAYFIN_FUNCTIONS_URL",
    ),
    (
        ITEM_ID_VAR,
        "VITE_FABRIC_ITEM_ID",
        "NEXT_PUBLIC_FABRIC_ITEM_ID",
    ),
    (
        WORKSPACE_ID_VAR,
        "VITE_FABRIC_WORKSPACE_ID",
        "NEXT_PUBLIC_FABRIC_WORKSPACE_ID",
    ),
    (
        TENANT_ID_VAR,
        "VITE_FABRIC_TENANT_ID",
        "NEXT_PUBLIC_FABRIC_TENANT_ID",
    ),
    (
        PORTAL_URL_VAR,
        "VITE_FABRIC_PORTAL_URL",
        "NEXT_PUBLIC_FABRIC_PORTAL_URL",
    ),
    (
        "RAYFIN_PUBLIC_SERVICE_MODE",
        "VITE_SERVICE_MODE",
        "NEXT_PUBLIC_SERVICE_MODE",
    ),
    (FRONTEND_PORT_VAR, "VITE_PORT", "PORT"),
];

/// The frontend variable name `rayfin env` writes for a `RAYFIN_PUBLIC_*` key
/// (`mapPublicEnvKey`): the well-known keys get their canonical names (`RAYFIN_PUBLIC_ITEM_ID` →
/// `VITE_FABRIC_ITEM_ID`, `RAYFIN_PUBLIC_FRONTEND_PORT` → `VITE_PORT`), any other
/// `RAYFIN_PUBLIC_X` becomes `VITE_RAYFIN_X`. `None` for keys without the prefix.
pub fn map_public_env_key(key: &str, framework: FrontendFramework) -> Option<String> {
    let suffix = key.strip_prefix(RAYFIN_PUBLIC_PREFIX)?;
    if framework == FrontendFramework::Plain {
        return Some(suffix.to_string());
    }
    Some(match framework.alias(key) {
        Some(alias) => alias.to_string(),
        None => format!("{}RAYFIN_{suffix}", framework.prefix()),
    })
}

/// Keeps the `RAYFIN_PUBLIC_*` entries of `vars` under their frontend names
/// (`mapPublicEnvForFramework`); everything else (secrets, tooling settings) is dropped.
pub fn map_public_env(
    vars: &BTreeMap<String, String>,
    framework: FrontendFramework,
) -> BTreeMap<String, String> {
    vars.iter()
        .filter_map(|(key, value)| Some((map_public_env_key(key, framework)?, value.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotenv_syntax() {
        let vars = parse_env(
            "\u{feff}# comment\r\n\
             \r\n\
             PLAIN=value\r\n\
             export EXPORTED = spaced value \n\
             EMPTY=\n\
             DOUBLE=\"line1\\nline2\\tend\"\n\
             SINGLE='keep \\n literally'\n\
             BACKTICK=`tick`\n\
             INLINE=value # a comment\n\
             HASH=abc#def\n\
             QUOTED_COMMENT=\"v\" # comment\n\
             URL=https://x.test/a?b=c=d\n\
             MULTI=\"first\n\
             second\"\n\
             UNTERMINATED=\"open\n\
             1BAD=x\n\
             NO_EQUALS\n\
             PLAIN=override\n",
        );
        let expected = [
            ("PLAIN", "override"),
            ("EXPORTED", "spaced value"),
            ("EMPTY", ""),
            ("DOUBLE", "line1\nline2\tend"),
            ("SINGLE", "keep \\n literally"),
            ("BACKTICK", "tick"),
            ("INLINE", "value"),
            ("HASH", "abc#def"),
            ("QUOTED_COMMENT", "v"),
            ("URL", "https://x.test/a?b=c=d"),
            ("MULTI", "first\nsecond"),
            ("UNTERMINATED", "\"open"),
        ];
        let expected: BTreeMap<String, String> = expected
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(vars, expected);
    }

    #[test]
    fn exposes_public_values() {
        let env = EnvFile::from_content(
            Path::new("rayfin/.env"),
            "RAYFIN_PUBLIC_FRONTEND_PORT=5175\n\
             RAYFIN_PUBLIC_WORKSPACE_ID=ws\n\
             RAYFIN_PUBLIC_TENANT_ID=\n\
             RAYFIN_FRONTEND_DEV_PORT_ALIASES=5174\n\
             SECRET=x\n",
        );
        assert_eq!(env.frontend_port(), Some(5175));
        assert_eq!(env.workspace_id(), Some("ws"));
        assert_eq!(env.tenant_id(), None);
        assert_eq!(
            env.public_env().keys().collect::<Vec<_>>(),
            [
                "RAYFIN_PUBLIC_FRONTEND_PORT",
                "RAYFIN_PUBLIC_TENANT_ID",
                "RAYFIN_PUBLIC_WORKSPACE_ID"
            ]
        );
    }

    #[test]
    fn rejects_invalid_ports() {
        for value in ["", "0", "65536", "51 73", "+5173", "5173.0", "abc"] {
            assert_eq!(parse_port(value), None, "{value:?}");
        }
        assert_eq!(parse_port("65535"), Some(65535));
    }

    #[test]
    fn maps_public_keys_like_rayfin_env() {
        let vars: BTreeMap<String, String> = [
            "RAYFIN_PUBLIC_API_URL",
            "RAYFIN_PUBLIC_PUBLISHABLE_KEY",
            "RAYFIN_PUBLIC_FUNCTIONS_URL",
            "RAYFIN_PUBLIC_ITEM_ID",
            "RAYFIN_PUBLIC_WORKSPACE_ID",
            "RAYFIN_PUBLIC_TENANT_ID",
            "RAYFIN_PUBLIC_PORTAL_URL",
            "RAYFIN_PUBLIC_SERVICE_MODE",
            "RAYFIN_PUBLIC_FRONTEND_PORT",
            "RAYFIN_PUBLIC_CUSTOM_FLAG",
            "RAYFIN_FRONTEND_DEV_PORT_ALIASES",
        ]
        .iter()
        .map(|key| (key.to_string(), format!("<{key}>")))
        .collect();

        let vite = map_public_env(&vars, FrontendFramework::Vite);
        let expected = [
            ("VITE_RAYFIN_API_URL", "RAYFIN_PUBLIC_API_URL"),
            (
                "VITE_RAYFIN_PUBLISHABLE_KEY",
                "RAYFIN_PUBLIC_PUBLISHABLE_KEY",
            ),
            ("VITE_RAYFIN_FUNCTIONS_URL", "RAYFIN_PUBLIC_FUNCTIONS_URL"),
            ("VITE_FABRIC_ITEM_ID", "RAYFIN_PUBLIC_ITEM_ID"),
            ("VITE_FABRIC_WORKSPACE_ID", "RAYFIN_PUBLIC_WORKSPACE_ID"),
            ("VITE_FABRIC_TENANT_ID", "RAYFIN_PUBLIC_TENANT_ID"),
            ("VITE_FABRIC_PORTAL_URL", "RAYFIN_PUBLIC_PORTAL_URL"),
            ("VITE_SERVICE_MODE", "RAYFIN_PUBLIC_SERVICE_MODE"),
            ("VITE_PORT", "RAYFIN_PUBLIC_FRONTEND_PORT"),
            ("VITE_RAYFIN_CUSTOM_FLAG", "RAYFIN_PUBLIC_CUSTOM_FLAG"),
        ];
        let expected: BTreeMap<String, String> = expected
            .iter()
            .map(|(k, v)| (k.to_string(), format!("<{v}>")))
            .collect();
        assert_eq!(vite, expected);

        let next = |key| map_public_env_key(key, FrontendFramework::NextJs);
        assert_eq!(
            next("RAYFIN_PUBLIC_ITEM_ID").as_deref(),
            Some("NEXT_PUBLIC_FABRIC_ITEM_ID")
        );
        assert_eq!(next("RAYFIN_PUBLIC_FRONTEND_PORT").as_deref(), Some("PORT"));
        assert_eq!(
            next("RAYFIN_PUBLIC_X").as_deref(),
            Some("NEXT_PUBLIC_RAYFIN_X")
        );
        let plain = map_public_env_key("RAYFIN_PUBLIC_ITEM_ID", FrontendFramework::Plain);
        assert_eq!(plain.as_deref(), Some("ITEM_ID"));
    }
}
