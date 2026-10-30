//! `rayfin-local secret`: the local values of the secrets `rayfin.yml` declares, kept in
//! `.rayfin-local/secrets.json`. Values are never printed.
//!
//! `serve` gives the functions each declared secret from the environment variable of the same
//! name, else from `secrets.json`, else from the function host's `local.settings.json` `Values`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Read, Write as _};
use std::path::Path;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::Subcommand;
use rl_project::{LocalState, Project};
use serde_json::json;

use super::users::{read_hidden, render_table};
use crate::ui::{self, Painter};

const RESTART_NOTE: &str = "A running `rayfin-local serve` reads secrets when it starts: restart \
                            it to apply this change.";

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// Store a secret's local value in .rayfin-local/secrets.json.
    Set {
        /// The secret's name, as declared under `secrets:` in rayfin/rayfin.yml.
        name: String,
        /// The value (default: read from stdin, hidden when typed). A value given here may be
        /// kept in your shell's history.
        #[arg(allow_hyphen_values = true)]
        value: Option<String>,
    },
    /// List the secrets and where their local values come from (never the values).
    List {
        /// Print one JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Delete a secret's local value from .rayfin-local/secrets.json.
    Delete {
        /// The secret's name.
        name: String,
    },
}

pub fn run(dir: &Path, command: SecretCommand) -> anyhow::Result<ExitCode> {
    let project = Project::discover(dir)?;
    let state = LocalState::load_or_create(&project)?;
    match command {
        SecretCommand::Set { name, value } => {
            let name = validate_name(&name)?;
            let value = match value {
                Some(value) => value,
                None => read_value(name)?,
            };
            set(&project, &state, name, value)?;
        }
        SecretCommand::List { json: false } => list(&project, &state)?,
        SecretCommand::List { json: true } => list_json(&project, &state)?,
        SecretCommand::Delete { name } => delete(&project, &state, &name)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// The name, trimmed. Declared secrets become environment variables of the function host.
fn validate_name(name: &str) -> anyhow::Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        anyhow::bail!("The secret name is empty.");
    }
    if name
        .chars()
        .any(|c| c == '=' || c.is_whitespace() || c.is_control())
    {
        anyhow::bail!(
            "The secret name {name:?} contains a space, `=` or a control character; use the \
             name declared under `secrets:` in rayfin/rayfin.yml."
        );
    }
    Ok(name)
}

/// The value from stdin: typed (hidden) at a terminal, else everything piped in.
fn read_value(name: &str) -> anyhow::Result<String> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return read_hidden(&format!("Value of {name} (hidden): "));
    }
    let mut text = String::new();
    stdin
        .lock()
        .read_to_string(&mut text)
        .context("Could not read the secret's value from stdin")?;
    Ok(piped_value(&text).to_owned())
}

/// A piped value without the byte-order mark and final line break that shells and editors add.
fn piped_value(text: &str) -> &str {
    text.strip_prefix('\u{feff}')
        .unwrap_or(text)
        .trim_end_matches(['\r', '\n'])
}

fn set(project: &Project, state: &LocalState, name: &str, value: String) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!(
            "The value of {name} is empty; nothing was saved. To remove its value, run \
             `rayfin-local secret delete {name}`."
        );
    }
    let mut secrets = state.load_secrets()?;
    let replaced = secrets.insert(name.to_owned(), value).is_some();
    state.save_secrets(&secrets)?;
    ui::step(format_args!(
        "{} the value of {name} in {}",
        if replaced { "Replaced" } else { "Saved" },
        relative(project, &state.secrets_path())
    ));
    let declared = declared_names(project);
    if !declared.contains(&name) {
        ui::warn(undeclared_warning(&declared, name));
    }
    if env_value(name).is_some() {
        ui::warn(format_args!(
            "The environment variable {name} is set: `rayfin-local serve` uses it instead of \
             this value."
        ));
    }
    ui::status(RESTART_NOTE);
    Ok(())
}

fn undeclared_warning(declared: &[&str], name: &str) -> String {
    let hint = match declared
        .iter()
        .find(|declared| declared.eq_ignore_ascii_case(name))
    {
        Some(similar) => format!(" (names are case-sensitive: did you mean {similar}?)"),
        None if declared.is_empty() => String::new(),
        None => format!(" (it declares {})", declared.join(", ")),
    };
    format!(
        "{name} is not declared under `secrets:` in rayfin/rayfin.yml{hint}, so the functions \
         don't get it."
    )
}

fn list(project: &Project, state: &LocalState) -> anyhow::Result<()> {
    let rows = project_rows(project, state, |name| env_value(name).is_some())?;
    if rows.is_empty() {
        ui::status(format_args!(
            "rayfin/rayfin.yml declares no secrets (under `secrets:`), and {} has none.",
            relative(project, &state.secrets_path())
        ));
        return Ok(());
    }
    print!("{}", render(&rows, Painter::stdout()));
    let declared = declared_names(project);
    for row in &rows {
        if !row.declared {
            ui::warn(undeclared_warning(&declared, &row.name));
        } else if row.sources.is_empty() {
            ui::warn(format_args!(
                "{0} has no local value: run `rayfin-local secret set {0}`.",
                row.name
            ));
        }
    }
    Ok(())
}

/// `secret list --json`: `{"secrets": [{"name", "declared", "description", "source"}]}`, where
/// `source` is where `serve` takes the value from: `env`, `local` (`.rayfin-local/secrets.json`),
/// `settings` (`local.settings.json`) or `none`.
fn list_json(project: &Project, state: &LocalState) -> anyhow::Result<()> {
    let secrets = secrets_json(project, state, |name| env_value(name).is_some())?;
    let _ = writeln!(io::stdout(), "{secrets}");
    Ok(())
}

fn secrets_json(
    project: &Project,
    state: &LocalState,
    in_env: impl Fn(&str) -> bool,
) -> anyhow::Result<serde_json::Value> {
    let rows = project_rows(project, state, in_env)?;
    let description = |name: &str| {
        project
            .config()
            .secrets
            .iter()
            .find(|secret| secret.name.trim() == name)
            .and_then(|secret| secret.description.clone())
    };
    let secrets: Vec<_> = rows
        .iter()
        .map(|row| {
            json!({
                "name": row.name,
                "declared": row.declared,
                "description": description(&row.name),
                "source": row.sources.first().map_or("none", |source| source.id()),
            })
        })
        .collect();
    Ok(json!({ "secrets": secrets }))
}

fn delete(project: &Project, state: &LocalState, name: &str) -> anyhow::Result<()> {
    let name = name.trim();
    let path = relative(project, &state.secrets_path());
    let mut secrets = state.load_secrets()?;
    if secrets.remove(name).is_none() {
        anyhow::bail!(
            "{name} has no value in {path}. Run `rayfin-local secret list` to see where the \
             secrets' values come from."
        );
    }
    state.save_secrets(&secrets)?;
    ui::step(format_args!("Deleted the value of {name} from {path}"));
    ui::status(RESTART_NOTE);
    Ok(())
}

/// The environment's value of `name` as `serve` reads it: empty is unset.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn declared_names(project: &Project) -> Vec<&str> {
    project
        .config()
        .secrets
        .iter()
        .map(|secret| secret.name.trim())
        .filter(|name| !name.is_empty())
        .collect()
}

/// `path` relative to the project root, with `/`.
fn relative(project: &Project, path: &Path) -> String {
    path.strip_prefix(project.root())
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Where a secret's local value can come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Env,
    SecretsJson,
    LocalSettings,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::SecretsJson => "secrets.json",
            Source::LocalSettings => "local.settings.json",
        }
    }

    /// The name of `secret list --json`.
    fn id(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::SecretsJson => "local",
            Source::LocalSettings => "settings",
        }
    }
}

struct SecretRow {
    name: String,
    /// Declared under `secrets:` in `rayfin.yml`; the functions get only those.
    declared: bool,
    /// Where it has a value, the one `serve` uses first.
    sources: Vec<Source>,
}

/// [`secret_rows`] of the project: `in_env` says whether the environment sets a name.
fn project_rows(
    project: &Project,
    state: &LocalState,
    in_env: impl Fn(&str) -> bool,
) -> anyhow::Result<Vec<SecretRow>> {
    let stored = state.load_secrets()?;
    let settings = match rl_functions::host::local_settings_values(project.functions_dir()) {
        Ok(values) => values.into_iter().map(|(name, _)| name).collect(),
        Err(err) => {
            ui::warn(format_args!(
                "Could not read the functions' local.settings.json: {err}"
            ));
            BTreeSet::new()
        }
    };
    Ok(secret_rows(
        &declared_names(project),
        in_env,
        &stored,
        &settings,
    ))
}

/// The declared secrets in `rayfin.yml` order, then the undeclared ones `secrets.json` has, with
/// their sources in the order `serve` prefers them.
fn secret_rows(
    declared: &[&str],
    in_env: impl Fn(&str) -> bool,
    stored: &BTreeMap<String, String>,
    settings: &BTreeSet<String>,
) -> Vec<SecretRow> {
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for &name in declared {
        if !seen.insert(name) {
            continue;
        }
        let sources = [
            (in_env(name), Source::Env),
            (stored.contains_key(name), Source::SecretsJson),
            (settings.contains(name), Source::LocalSettings),
        ]
        .into_iter()
        .filter_map(|(has_value, source)| has_value.then_some(source))
        .collect();
        rows.push(SecretRow {
            name: name.to_owned(),
            declared: true,
            sources,
        });
    }
    rows.extend(
        stored
            .keys()
            .filter(|name| !seen.contains(name.as_str()))
            .map(|name| SecretRow {
                name: name.clone(),
                declared: false,
                sources: vec![Source::SecretsJson],
            }),
    );
    rows
}

fn render(rows: &[SecretRow], painter: Painter) -> String {
    let mut table = vec![["NAME", "DECLARED", "SOURCE"].map(str::to_owned)];
    table.extend(rows.iter().map(|row| {
        [
            row.name.clone(),
            if row.declared { "yes" } else { "no" }.to_owned(),
            source_cell(row),
        ]
    }));
    render_table(&table, painter)
}

fn source_cell(row: &SecretRow) -> String {
    let Some((used, shadowed)) = row.sources.split_first() else {
        return "none".to_owned();
    };
    let mut cell = used.label().to_owned();
    if !row.declared {
        cell.push_str(" (unused)");
    } else if !shadowed.is_empty() {
        let shadowed: Vec<&str> = shadowed.iter().map(|source| source.label()).collect();
        let _ = write!(cell, " (overrides {})", shadowed.join(", "));
    }
    cell
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        secret: SecretCommand,
    }

    #[test]
    fn values_may_start_with_a_hyphen_or_come_from_stdin() {
        let cli = Cli::try_parse_from(["secret", "set", "API_KEY", "-abc"]).unwrap();
        assert!(matches!(
            cli.secret,
            SecretCommand::Set { name, value: Some(value) } if name == "API_KEY" && value == "-abc"
        ));
        let cli = Cli::try_parse_from(["secret", "set", "API_KEY"]).unwrap();
        assert!(matches!(cli.secret, SecretCommand::Set { value: None, .. }));
    }

    #[test]
    fn piped_values_lose_the_bom_and_final_line_break_only() {
        assert_eq!(piped_value("\u{feff}abc\r\n"), "abc");
        assert_eq!(piped_value("line 1\nline 2\n"), "line 1\nline 2");
        assert_eq!(piped_value("  spaced  "), "  spaced  ");
    }

    #[test]
    fn names_must_be_usable_as_environment_variables() {
        assert_eq!(validate_name(" API_KEY ").unwrap(), "API_KEY");
        for name in ["", "  ", "A=B", "MY KEY", "A\u{1}"] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
    }

    fn summary(rows: &[SecretRow]) -> Vec<(String, bool, String)> {
        rows.iter()
            .map(|row| (row.name.clone(), row.declared, source_cell(row)))
            .collect()
    }

    fn expected(rows: &[(&str, bool, &str)]) -> Vec<(String, bool, String)> {
        rows.iter()
            .map(|&(name, declared, source)| (name.to_owned(), declared, source.to_owned()))
            .collect()
    }

    #[test]
    fn sources_follow_the_precedence_of_serve() {
        let stored = BTreeMap::from(["A", "B", "Z"].map(|name| (name.to_owned(), "v".to_owned())));
        let settings = BTreeSet::from(["B", "C"].map(str::to_owned));
        let rows = secret_rows(
            &["A", "B", "C", "D", "A"],
            |name| name == "A",
            &stored,
            &settings,
        );
        assert_eq!(
            summary(&rows),
            expected(&[
                ("A", true, "env (overrides secrets.json)"),
                ("B", true, "secrets.json (overrides local.settings.json)"),
                ("C", true, "local.settings.json"),
                ("D", true, "none"),
                ("Z", false, "secrets.json (unused)"),
            ])
        );
    }

    const RAYFIN_YML: &str = "id: app\nname: App\nversion: 1.0.0\nsecrets:\n  - name: API_KEY\n  \
                              - name: GREETING\n    description: in local.settings.json\n";

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let rayfin = dir.path().join("rayfin");
        std::fs::create_dir_all(rayfin.join("functions")).unwrap();
        std::fs::write(rayfin.join("rayfin.yml"), RAYFIN_YML).unwrap();
        std::fs::write(
            rayfin.join("functions").join("local.settings.json"),
            r#"{ "IsEncrypted": false, "Values": { "GREETING": "hello" } }"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn sets_lists_and_deletes_local_values() {
        let dir = project();
        let root = dir.path();
        let set = |name: &str, value: &str| {
            run(
                root,
                SecretCommand::Set {
                    name: name.to_owned(),
                    value: Some(value.to_owned()),
                },
            )
        };
        set(" API_KEY ", "s3cret-value").unwrap();
        // Saved with a warning.
        set("UNDECLARED_KEY", "other").unwrap();
        assert!(set("API_KEY", "").is_err());
        run(root, SecretCommand::List { json: false }).unwrap();

        let project = Project::discover(root).unwrap();
        let state = LocalState::load_or_create(&project).unwrap();
        assert_eq!(
            state.load_secrets().unwrap(),
            BTreeMap::from([
                ("API_KEY".to_owned(), "s3cret-value".to_owned()),
                ("UNDECLARED_KEY".to_owned(), "other".to_owned()),
            ])
        );
        let listed = secrets_json(&project, &state, |name| name == "GREETING").unwrap();
        assert_eq!(
            listed,
            json!({ "secrets": [
                { "name": "API_KEY", "declared": true, "description": null, "source": "local" },
                {
                    "name": "GREETING",
                    "declared": true,
                    "description": "in local.settings.json",
                    "source": "env",
                },
                {
                    "name": "UNDECLARED_KEY",
                    "declared": false,
                    "description": null,
                    "source": "local",
                },
            ] })
        );
        assert!(!listed.to_string().contains("s3cret-value"));
        let rows = project_rows(&project, &state, |_| false).unwrap();
        assert_eq!(
            summary(&rows),
            expected(&[
                ("API_KEY", true, "secrets.json"),
                ("GREETING", true, "local.settings.json"),
                ("UNDECLARED_KEY", false, "secrets.json (unused)"),
            ])
        );
        let table = render(&rows, Painter::stdout());
        assert!(
            !table.contains("s3cret-value") && !table.contains("hello"),
            "{table}"
        );

        let delete = |name: &str| {
            run(
                root,
                SecretCommand::Delete {
                    name: name.to_owned(),
                },
            )
        };
        delete("API_KEY").unwrap();
        let err = delete("API_KEY").unwrap_err();
        assert!(err.to_string().contains("has no value"), "{err:#}");
        let rows = project_rows(&project, &state, |_| false).unwrap();
        assert_eq!(summary(&rows)[0], expected(&[("API_KEY", true, "none")])[0]);
    }
}
