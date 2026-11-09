//! Path validation for guest-visible module paths.
//!
//! The sandbox resolves modules by exact string key from a host-built graph
//! map; the runtime loader is authoritative. These checks exist so a
//! host-prepared path can never be confused with an absolute path, a URL, or
//! something the platform would normalise into a different location.

use crate::error::ModuleError;

/// Validate a *relative, prepared* module path such as `src/index.js`.
///
/// Rejected:
/// * empty strings, absolute paths, `.`/`..` segments, drives/UNC prefixes
/// * backslashes, NUL, other control characters
/// * `%` (percent-encoding / URL normalisation tricks), `:`, `?`, `#`
///
/// Accepted: plain relative POSIX-style paths, e.g. `index.js`,
/// `node_modules/pkg/a.ts`.
pub fn validate_path(path: &str) -> Result<(), ModuleError> {
    if path.is_empty() {
        return Err(ModuleError::invalid_path(path, "empty path"));
    }
    if path.len() > 1024 {
        return Err(ModuleError::invalid_path(path, "path too long"));
    }
    for ch in path.chars() {
        match ch {
            '\\' => return Err(ModuleError::invalid_path(path, "backslash not allowed")),
            '\0' => return Err(ModuleError::invalid_path(path, "NUL byte not allowed")),
            '%' => return Err(ModuleError::invalid_path(path, "percent not allowed")),
            ':' => return Err(ModuleError::invalid_path(path, "colon not allowed")),
            '?' => return Err(ModuleError::invalid_path(path, "query not allowed")),
            '#' => return Err(ModuleError::invalid_path(path, "fragment not allowed")),
            c if c.is_control() => {
                return Err(ModuleError::invalid_path(
                    path,
                    "control character not allowed",
                ))
            }
            _ => {}
        }
    }
    if path.starts_with('/') {
        return Err(ModuleError::invalid_path(path, "absolute path not allowed"));
    }
    if path.ends_with('/') {
        return Err(ModuleError::invalid_path(
            path,
            "trailing slash not allowed",
        ));
    }
    if path.contains("//") {
        return Err(ModuleError::invalid_path(path, "empty path segment"));
    }
    for segment in path.split('/') {
        match segment {
            "" => return Err(ModuleError::invalid_path(path, "empty path segment")),
            "." => return Err(ModuleError::invalid_path(path, "dot segment not allowed")),
            ".." => {
                return Err(ModuleError::invalid_path(
                    path,
                    "dot-dot segment not allowed",
                ))
            }
            // Windows reserved names / device paths add nothing legitimate here.
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_relative_prepared_paths() {
        for ok in [
            "index.js",
            "src/a/b.mjs",
            "node_modules/pkg/dist/index.mts",
            "a b.js",
        ] {
            assert!(validate_path(ok).is_ok(), "expected {ok:?} to be accepted");
        }
    }

    #[test]
    fn rejects_escapes_and_url_tricks() {
        for bad in [
            "",
            "/etc/passwd",
            "../secret.js",
            "a/../b.js",
            "a/./b.js",
            "a//b.js",
            "src\\win.js",
            "a\0b.js",
            "a\tb.js",
            "a%2e%2e/b.js",
            "./index.ts",
            "file:///etc/passwd",
            "https://example.com/x.js",
            "a.js?x=1",
            "a.js#frag",
            "a/",
            ".",
            "..",
        ] {
            assert!(
                validate_path(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }
}
