//! Path helpers: resolving the service paths from `rayfin.yml`, and Node's package lookup.

use std::path::{Path, PathBuf};

/// Resolves `value` (a relative path from `rayfin.yml`, with either slash style) against `base`,
/// as the CLI's `resolveServiceSubpath` does: `.` and `..` are applied lexically, and absolute
/// paths or paths that leave `base` are rejected.
pub(crate) fn resolve_contained(base: &Path, value: &str) -> Result<PathBuf, PathProblem> {
    let value = value.trim();
    if is_absolute_like(value) {
        return Err(PathProblem::Absolute);
    }
    let mut resolved = base.to_path_buf();
    let mut depth = 0usize;
    for component in value.split(['/', '\\']) {
        match component {
            "" | "." => {}
            ".." => {
                if depth == 0 {
                    return Err(PathProblem::Escapes);
                }
                depth -= 1;
                resolved.pop();
            }
            // `PathBuf::push("C:")` would replace the whole path on Windows.
            name if cfg!(windows) && name.contains(':') => return Err(PathProblem::Absolute),
            name => {
                depth += 1;
                resolved.push(name);
            }
        }
    }
    Ok(resolved)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathProblem {
    Absolute,
    Escapes,
}

/// Absolute in POSIX or Windows terms (`/x`, `\x`, `C:\x`, `C:x`, `\\server\share`).
fn is_absolute_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.starts_with(['/', '\\'])
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// The directory of npm package `name` (for example `vite` or `@microsoft/rayfin-core`) as Node
/// resolves it from `from`: the first `node_modules/<name>/package.json` in `from` or one of its
/// parents.
pub fn resolve_node_package(from: &Path, name: &str) -> Option<PathBuf> {
    from.ancestors().find_map(|dir| {
        let mut candidate = dir.join("node_modules");
        candidate.extend(name.split('/'));
        candidate
            .join("package.json")
            .is_file()
            .then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_paths_inside_the_base() {
        let base = Path::new("/project");
        let ok = |value| resolve_contained(base, value).unwrap();
        assert_eq!(ok("packages/data"), base.join("packages").join("data"));
        assert_eq!(ok("packages\\data\\"), base.join("packages").join("data"));
        assert_eq!(ok("./a/../b"), base.join("b"));
        assert_eq!(ok("."), base);
        assert_eq!(ok(""), base);
    }

    #[test]
    fn rejects_absolute_and_escaping_paths() {
        let base = Path::new("/project");
        for value in ["/etc", "\\x", "C:\\x", "c:x", "\\\\server\\share"] {
            assert_eq!(
                resolve_contained(base, value),
                Err(PathProblem::Absolute),
                "{value:?}"
            );
        }
        for value in ["..", "a/../../b"] {
            assert_eq!(
                resolve_contained(base, value),
                Err(PathProblem::Escapes),
                "{value:?}"
            );
        }
    }
}
