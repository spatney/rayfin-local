//! The functions package's `package.json` and `tsconfig.json`.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::{BuildError, normalize};

/// `package.json` of the functions package.
pub(super) struct Package {
    path: PathBuf,
    json: Value,
}

impl Package {
    pub(super) fn read(functions_dir: &Path) -> Result<Self, BuildError> {
        let path = functions_dir.join("package.json");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(BuildError::MissingPackageJson { path });
            }
            Err(source) => return Err(BuildError::Read { path, source }),
        };
        match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
            Ok(json) => Ok(Self { path, json }),
            Err(source) => Err(BuildError::InvalidPackageJson { path, source }),
        }
    }

    /// `main`, relative to the package.
    pub(super) fn main(&self) -> Result<&str, BuildError> {
        let main = self
            .json
            .get("main")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|main| !main.is_empty())
            .ok_or_else(|| BuildError::MissingMain {
                path: self.path.clone(),
            })?;
        // Azure Functions accepts globs in `main`; the function host imports a single module.
        if main.contains(['*', '?', '{', '[']) {
            return Err(BuildError::GlobMain {
                path: self.path.clone(),
                main: main.to_owned(),
            });
        }
        Ok(main)
    }

    /// A script, when the package defines it as a non-blank command.
    pub(super) fn script(&self, name: &str) -> Option<&str> {
        self.json
            .get("scripts")?
            .get(name)?
            .as_str()
            .map(str::trim)
            .filter(|script| !script.is_empty())
    }
}

/// `compilerOptions.outDir` of `<functions_dir>/tsconfig.json`, following relative `extends`.
pub(super) fn tsconfig_out_dir(functions_dir: &Path) -> Option<PathBuf> {
    let mut config = functions_dir.join("tsconfig.json");
    // A bound on `extends` chains, which may be cyclic.
    for _ in 0..8 {
        let json = parse_jsonc(&fs::read_to_string(&config).ok()?)?;
        let base = config.parent()?.to_owned();
        if let Some(out_dir) = json
            .pointer("/compilerOptions/outDir")
            .and_then(Value::as_str)
        {
            return Some(normalize(&base.join(out_dir)));
        }
        // Shared configs from packages (`@tsconfig/node20`) do not set `outDir`.
        let extends = json.get("extends")?.as_str()?;
        if !extends.starts_with('.') {
            return None;
        }
        config = base.join(extends);
        if !config.is_file() && !extends.ends_with(".json") {
            config.as_mut_os_string().push(".json");
        }
    }
    None
}

/// Parses JSON with comments and trailing commas, as TypeScript reads `tsconfig.json`.
fn parse_jsonc(text: &str) -> Option<Value> {
    let text = text.trim_start_matches('\u{feff}');
    let mut json = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                json.push(ch);
                while let Some(ch) = chars.next() {
                    json.push(ch);
                    match ch {
                        '\\' => json.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                while chars.next_if(|&next| next != '\n').is_some() {}
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
                json.push(' ');
            }
            ',' => {
                let rest = chars.clone().find(|next| !next.is_whitespace());
                if !matches!(rest, Some('}' | ']')) {
                    json.push(ch);
                }
            }
            _ => json.push(ch),
        }
    }
    serde_json::from_str(&json).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn jsonc_keeps_strings_intact() {
        let text = "\u{feff}{\n  // line\n  \"a\": \"http://x/*y*/\", /* block */\n  \"b\": [1, 2,],\n  \"c\": \"q\\\",//\",\n}\n";
        assert_eq!(
            parse_jsonc(text),
            Some(json!({"a": "http://x/*y*/", "b": [1, 2], "c": "q\",//"}))
        );
        assert_eq!(parse_jsonc("{\"a\": }"), None);
    }

    #[test]
    fn scripts_must_be_non_blank_commands() {
        let package = Package {
            path: "package.json".into(),
            json: json!({"scripts": {"build:watch": " ", "build": "tsc", "n": 1}}),
        };
        assert_eq!(package.script("build"), Some("tsc"));
        assert_eq!(package.script("build:watch"), None);
        assert_eq!(package.script("n"), None);
        assert_eq!(package.script("missing"), None);
    }
}
