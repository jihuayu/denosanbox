//! Immutable plugin source graph and import visibility validation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::config::EngineLimits;
use crate::Error;

/// Immutable sources supplied by the host.
///
/// `entrypoint` must be a key in `modules`. Paths are root-relative POSIX
/// paths such as `main.js` and `lib/util.js`. Only JavaScript ESM (`.js` or
/// `.mjs`) is accepted by the foundation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSource {
    /// Root-relative entry module path.
    pub entrypoint: String,
    /// Root-relative path to immutable source text.
    pub modules: BTreeMap<String, String>,
}

impl PluginSource {
    /// Creates a one-module plugin source.
    pub fn single(name: impl Into<String>, source: impl Into<String>) -> Self {
        let name = name.into();
        let mut modules = BTreeMap::new();
        modules.insert(name.clone(), source.into());
        Self {
            entrypoint: name,
            modules,
        }
    }
}

/// A validated, immutable module source graph.
#[derive(Clone)]
pub struct PreparedPlugin {
    pub(crate) graph: Arc<PreparedGraph>,
}

impl std::fmt::Debug for PreparedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedPlugin")
            .field("entrypoint", &self.graph.entrypoint)
            .field("module_count", &self.graph.modules.len())
            .field("total_bytes", &self.graph.total_bytes)
            .finish()
    }
}

impl PreparedPlugin {
    /// Root-relative entrypoint path.
    pub fn entrypoint(&self) -> &str {
        &self.graph.entrypoint
    }

    /// Number of modules in the immutable graph.
    pub fn module_count(&self) -> usize {
        self.graph.modules.len()
    }

    /// Total source bytes in the graph.
    pub fn total_bytes(&self) -> usize {
        self.graph.total_bytes
    }

    /// Returns an immutable module source by root-relative path.
    pub fn source(&self, path: &str) -> Option<&str> {
        self.graph.modules.get(path).map(|s| s.as_str())
    }

    /// Iterates immutable `(path, source)` pairs in deterministic path order.
    pub fn iter_modules(&self) -> impl Iterator<Item = (&str, &str)> {
        self.graph
            .modules
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
    }
}

#[derive(Debug)]
pub(crate) struct PreparedGraph {
    pub(crate) entrypoint: String,
    pub(crate) modules: BTreeMap<String, String>,
    pub(crate) total_bytes: usize,
}

/// Validates and freezes a host-supplied source graph.
///
/// The prepared graph is the only visibility source used by runtime module
/// loaders. Literal static imports and literal dynamic imports are checked
/// eagerly; every later load is rechecked by the backend loader against this
/// same immutable graph.
pub fn prepare(source: PluginSource, limits: &EngineLimits) -> Result<PreparedPlugin, Error> {
    limits.validate()?;
    validate_path(&source.entrypoint)?;

    if source.modules.is_empty() {
        return Err(Error::InvalidPlugin {
            reason: "plugin must contain at least one module".to_string(),
        });
    }
    if source.modules.len() > limits.max_modules {
        return Err(Error::LimitExceeded {
            limit: "max_modules".to_string(),
            detail: format!(
                "plugin has {} modules but the limit is {}",
                source.modules.len(),
                limits.max_modules
            ),
        });
    }
    if !source.modules.contains_key(&source.entrypoint) {
        return Err(Error::InvalidPlugin {
            reason: format!(
                "entrypoint '{}' is not present in modules",
                source.entrypoint
            ),
        });
    }

    let mut total_bytes = 0usize;
    let mut names = BTreeSet::new();
    let mut modules = BTreeMap::new();
    for (path, text) in source.modules {
        validate_path(&path)?;
        if !names.insert(path.clone()) {
            return Err(Error::InvalidPlugin {
                reason: format!("duplicate module path '{path}'"),
            });
        }
        if text.len() > limits.max_module_bytes {
            return Err(Error::LimitExceeded {
                limit: "max_module_bytes".to_string(),
                detail: format!(
                    "module '{path}' is {} bytes but the limit is {}",
                    text.len(),
                    limits.max_module_bytes
                ),
            });
        }
        total_bytes = total_bytes.saturating_add(text.len());
        modules.insert(path, text);
    }
    if total_bytes > limits.max_graph_bytes {
        return Err(Error::LimitExceeded {
            limit: "max_graph_bytes".to_string(),
            detail: format!(
                "plugin graph is {total_bytes} bytes but the limit is {}",
                limits.max_graph_bytes
            ),
        });
    }

    // Eagerly validate literal imports so a bad graph fails at prepare time.
    // The runtime loader still rechecks every resolution, including computed
    // dynamic imports, against this immutable map.
    for (path, text) in &modules {
        for specifier in scan_import_specifiers(text) {
            validate_import(&specifier, path, &modules)?;
        }
    }

    Ok(PreparedPlugin {
        graph: Arc::new(PreparedGraph {
            entrypoint: source.entrypoint,
            modules,
            total_bytes,
        }),
    })
}

/// Validates a normalized, root-relative module path.
pub fn validate_path(path: &str) -> Result<(), Error> {
    if path.is_empty() {
        return Err(Error::InvalidPlugin {
            reason: "module path must not be empty".to_string(),
        });
    }
    if path.len() > 512 {
        return Err(Error::InvalidPlugin {
            reason: format!("module path is too long: {path}"),
        });
    }
    if path.starts_with('/') || path.starts_with('\\') || path.starts_with("//") {
        return Err(Error::ModuleDenied {
            specifier: path.to_string(),
            reason: "absolute paths are not permitted".to_string(),
        });
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(Error::InvalidPlugin {
            reason: format!("invalid character in module path '{path}'"),
        });
    }
    if path.contains('?') || path.contains('#') {
        return Err(Error::ModuleDenied {
            specifier: path.to_string(),
            reason: "query strings and fragments are not permitted".to_string(),
        });
    }
    if let Some(colon) = path.find(':') {
        if !path[..colon].contains('/') {
            return Err(Error::ModuleDenied {
                specifier: path.to_string(),
                reason: "URL schemes and bare specifiers are not permitted".to_string(),
            });
        }
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(Error::InvalidPlugin {
                reason: format!("module path must be normalized: '{path}'"),
            });
        }
    }
    let extension = path.rsplit('.').next().unwrap_or("");
    match extension {
        "js" | "mjs" => Ok(()),
        "ts" | "tsx" | "mts" | "cts" | "jsx" => Err(Error::UnsupportedLanguage {
            reason: format!(
                "TypeScript/JSX module '{path}' was supplied but this build has no transpiler; \
         provide JavaScript ESM instead"
            ),
        }),
        _ => Err(Error::InvalidPlugin {
            reason: format!("module '{path}' must use the .js or .mjs extension"),
        }),
    }
}

fn validate_import(
    specifier: &str,
    referrer: &str,
    modules: &BTreeMap<String, String>,
) -> Result<(), Error> {
    if specifier.is_empty() {
        return Err(Error::ModuleDenied {
            specifier: specifier.to_string(),
            reason: "empty import specifier".to_string(),
        });
    }
    if specifier.contains('\\') || specifier.contains('\0') {
        return Err(Error::ModuleDenied {
            specifier: specifier.to_string(),
            reason: "invalid characters in import specifier".to_string(),
        });
    }
    if specifier.contains('?') || specifier.contains('#') {
        return Err(Error::ModuleDenied {
            specifier: specifier.to_string(),
            reason: "query strings and fragments are not permitted".to_string(),
        });
    }
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return Err(Error::ModuleDenied {
            specifier: specifier.to_string(),
            reason: "only relative imports (./ or ../) are permitted".to_string(),
        });
    }
    let resolved = resolve_relative(referrer, specifier)?;
    validate_path(&resolved)?;
    if !modules.contains_key(&resolved) {
        return Err(Error::ModuleNotFound {
            specifier: specifier.to_string(),
            referrer: Some(referrer.to_string()),
        });
    }
    Ok(())
}

/// Resolves a relative specifier against a root-relative referrer path.
pub fn resolve_relative(referrer: &str, specifier: &str) -> Result<String, Error> {
    let mut parts: Vec<&str> = referrer.split('/').collect();
    parts.pop(); // remove the referrer file name
    for segment in specifier.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err(Error::ModuleDenied {
                        specifier: specifier.to_string(),
                        reason: "relative import escapes the prepared graph root".to_string(),
                    });
                }
            }
            other => parts.push(other),
        }
    }
    let resolved = parts.join("/");
    if resolved.is_empty() {
        return Err(Error::ModuleDenied {
            specifier: specifier.to_string(),
            reason: "import resolves to the graph root".to_string(),
        });
    }
    Ok(resolved)
}

/// Small conservative ES-module import scanner.
///
/// It skips comments and ordinary strings, recognizes literal static imports,
/// `export ... from` clauses, and literal `import("...")` calls. It is not a
/// JavaScript parser: computed dynamic imports are deliberately left for the
/// runtime loader, which performs the authoritative visibility check.
pub fn scan_import_specifiers(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b'\'' | b'"' | b'`' => {
                let (_, next) =
                    read_string(bytes, i).unwrap_or_else(|| (String::new(), bytes.len()));
                i = next;
            }
            b if is_ident_start(b) => {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                let word = &source[start..i];
                if word == "import" {
                    let mut j = skip_trivia(bytes, i);
                    if j < bytes.len() && bytes[j] == b'(' {
                        j = skip_trivia(bytes, j + 1);
                        if j < bytes.len() && matches!(bytes[j], b'\'' | b'"') {
                            if let Some((spec, _)) = read_string(bytes, j) {
                                out.push(spec);
                            }
                        }
                    } else if j < bytes.len() && bytes[j] == b'.' {
                        // import.meta
                    } else if j < bytes.len() && !matches!(bytes[j], b';' | b'\n' | b'\r') {
                        // Static import: the first string literal before the statement
                        // terminator is the module specifier (side-effect imports have no
                        // `from` token).
                        if let Some((spec, _)) = first_string_in_statement(bytes, j) {
                            out.push(spec);
                        }
                    }
                } else if word == "export" {
                    if let Some((spec, _)) = from_string_in_statement(bytes, i) {
                        out.push(spec);
                    }
                }
            }
            _ => i += 1,
        }
    }
    out
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn skip_trivia(bytes: &[u8], mut i: usize) -> usize {
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        return i;
    }
}

fn first_string_in_statement(bytes: &[u8], mut i: usize) -> Option<(String, usize)> {
    while i < bytes.len() {
        match bytes[i] {
            b'\n' | b'\r' | b';' => return None,
            b'\'' | b'"' => return read_string(bytes, i),
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i = skip_trivia(bytes, i);
            }
            b'`' => {
                let (_, next) =
                    read_string(bytes, i).unwrap_or_else(|| (String::new(), bytes.len()));
                i = next;
            }
            _ => i += 1,
        }
    }
    None
}

fn from_string_in_statement(bytes: &[u8], mut i: usize) -> Option<(String, usize)> {
    let mut saw_from = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' | b'\r' | b';' => return None,
            b'\'' | b'"' => {
                let (spec, next) = read_string(bytes, i)?;
                return if saw_from { Some((spec, next)) } else { None };
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i = skip_trivia(bytes, i);
            }
            b'`' => {
                let (_, next) =
                    read_string(bytes, i).unwrap_or_else(|| (String::new(), bytes.len()));
                i = next;
            }
            b if is_ident_start(b) => {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_continue(bytes[i]) {
                    i += 1;
                }
                if &bytes[start..i] == b"from" {
                    saw_from = true;
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn read_string(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    let quote = *bytes.get(start)?;
    if !matches!(quote, b'\'' | b'"' | b'`') {
        return None;
    }
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        let b = bytes[i];
        if b == quote {
            return Some((out, i + 1));
        }
        if quote != b'`' && (b == b'\n' || b == b'\r') {
            return None;
        }
        if b == b'\\' {
            i += 1;
            let escaped = *bytes.get(i)?;
            let ch = match escaped {
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'0' => '\0',
                other => other as char,
            };
            out.push(ch);
            i += 1;
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    #[test]
    fn scanner_finds_static_dynamic_and_export_imports() {
        let source = r#"
      // import "./ignored.js";
      import { a } from "./a.js";
      import "./side.js";
      export * from "./b.js";
      const x = import("./dyn.js");
      const s = "import './string.js'";
      const m = import.meta.url;
      const t = `import "./template.js"`;
    "#;
        let mut found = scan_import_specifiers(source);
        found.sort();
        assert_eq!(found, vec!["./a.js", "./b.js", "./dyn.js", "./side.js"]);
    }

    #[test]
    fn prepare_accepts_visible_relative_graph() {
        let mut source = PluginSource::single(
            "main.js",
            "import { n } from './lib.js'; export async function run(i) { return n(i); }",
        );
        source.modules.insert(
            "lib.js".to_string(),
            "export function n(i) { return i + 1; }".to_string(),
        );
        let prepared = prepare(source, &limits()).unwrap();
        assert_eq!(prepared.entrypoint(), "main.js");
        assert_eq!(prepared.module_count(), 2);
    }

    #[test]
    fn prepare_rejects_missing_escaped_scheme_and_ts() {
        let missing = prepare(
            PluginSource::single("main.js", "import './missing.js';"),
            &limits(),
        )
        .unwrap_err();
        assert!(matches!(missing, Error::ModuleNotFound { .. }));

        let escape = prepare(
            PluginSource::single("main.js", "import '../outside.js';"),
            &limits(),
        )
        .unwrap_err();
        assert!(matches!(escape, Error::ModuleDenied { .. }));

        let scheme = prepare(
            PluginSource::single("main.js", "import 'https://example.com/x.js';"),
            &limits(),
        )
        .unwrap_err();
        assert!(matches!(scheme, Error::ModuleDenied { .. }));

        let ts = prepare(PluginSource::single("main.ts", "export {};"), &limits()).unwrap_err();
        assert!(matches!(ts, Error::UnsupportedLanguage { .. }));
    }
}
