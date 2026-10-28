//! Logical object paths and content types.
//!
//! Logical paths only ever live in the database; files on disk are named by random ids, so a
//! name can never address the file system. The rules still reject names that would be unsafe or
//! ambiguous anywhere (`..`, separators, control characters).

use crate::error::ApiError;

const MAX_SEGMENT_CHARS: usize = 255;
const MAX_PATH_CHARS: usize = 1024;

/// The object's path in its folder: `prefix/fileName`, or `fileName` (as the client synthesizes
/// it).
pub(crate) fn logical_path(
    file_name: Option<&str>,
    prefix: Option<&str>,
) -> Result<String, ApiError> {
    let file_name = file_name.unwrap_or_default();
    if !segment_ok(file_name) {
        return Err(ApiError::InvalidName(format!(
            "`fileName` must be one path segment of 1-{MAX_SEGMENT_CHARS} characters, not `.` or \
             `..`, without `/`, `\\` or control characters; got {file_name:?}."
        )));
    }
    let path = match prefix.filter(|prefix| !prefix.is_empty()) {
        None => file_name.to_owned(),
        Some(prefix) => {
            check_prefix(prefix)?;
            format!("{prefix}/{file_name}")
        }
    };
    if path.chars().count() > MAX_PATH_CHARS {
        return Err(ApiError::InvalidName(format!(
            "The object path is longer than {MAX_PATH_CHARS} characters; shorten the prefix or \
             the file name."
        )));
    }
    Ok(path)
}

/// Checks a `prefix`: `/`-separated segments under the file name rules.
pub(crate) fn check_prefix(prefix: &str) -> Result<(), ApiError> {
    if prefix.split('/').all(segment_ok) {
        Ok(())
    } else {
        Err(ApiError::InvalidPrefix(format!(
            "`prefix` must be `/`-separated segments (no leading, trailing or doubled `/`), each \
             1-{MAX_SEGMENT_CHARS} characters, not `.` or `..`, without `\\` or control \
             characters; got {prefix:?}."
        )))
    }
}

fn segment_ok(segment: &str) -> bool {
    let chars = segment.chars().count();
    (1..=MAX_SEGMENT_CHARS).contains(&chars)
        && segment != "."
        && segment != ".."
        && !segment
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

/// The media type of a `Content-Type` value: lowercase, without parameters.
pub(crate) fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Whether `content_type` matches one of a folder's `allowedContentTypes` globs (any type when
/// there are none).
pub(crate) fn content_type_allowed(globs: &[String], content_type: &str) -> bool {
    let media = media_type(content_type);
    globs.is_empty()
        || globs
            .iter()
            .any(|glob| glob_match(&glob.trim().to_ascii_lowercase(), &media))
}

/// `*` matches any run of characters (`/` included); every other character matches itself.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && pattern[p] == '*' {
            backtrack = Some((p, t));
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some((star, start)) = backtrack {
            p = star + 1;
            t = start + 1;
            backtrack = Some((star, start + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_join_prefix_and_reject_unsafe_segments() {
        assert_eq!(logical_path(Some("a.png"), None).unwrap(), "a.png");
        assert_eq!(logical_path(Some("a.png"), Some("")).unwrap(), "a.png");
        assert_eq!(
            logical_path(Some("a.png"), Some("x/y")).unwrap(),
            "x/y/a.png"
        );
        for name in ["", ".", "..", "a/b", "a\\b", "a\0b", "a\nb"] {
            let error = logical_path(Some(name), None).unwrap_err();
            assert_eq!(error.code(), "InvalidName", "{name:?}");
        }
        assert_eq!(logical_path(None, None).unwrap_err().code(), "InvalidName");
        for prefix in ["/x", "x/", "x//y", "..", "x/../y", "C:\\x", "x/\u{7}"] {
            let error = logical_path(Some("a"), Some(prefix)).unwrap_err();
            assert_eq!(error.code(), "InvalidPrefix", "{prefix:?}");
        }
        let long = "a/".repeat(600) + "a";
        assert_eq!(
            logical_path(Some("b"), Some(&long)).unwrap_err().code(),
            "InvalidName"
        );
    }

    #[test]
    fn content_types_match_mime_globs() {
        let globs = ["image/*".to_owned(), "application/pdf".to_owned()];
        assert!(content_type_allowed(&globs, "image/png"));
        assert!(content_type_allowed(&globs, "IMAGE/SVG+XML; charset=utf-8"));
        assert!(content_type_allowed(&globs, "application/pdf"));
        assert!(!content_type_allowed(&globs, "application/pdfx"));
        assert!(!content_type_allowed(&globs, "text/plain"));
        assert!(content_type_allowed(&[], "text/plain"));
        assert!(glob_match("*/*", "text/plain"));
        assert!(glob_match("a*b*c", "aXbYbZc"));
        assert!(!glob_match("a*b", "aXbY"));
    }
}
