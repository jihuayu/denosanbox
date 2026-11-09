//! Immutable plugin source graph and import visibility validation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::config::EngineLimits;
use crate::Error;

/// Immutable sources supplied by the host.
///
/// `entrypoint` must be a key in `modules`. Paths are root-relative POSIX
/// paths such as `main.js` and `lib/util.js`. JavaScript ESM (`.js`/`.mjs`) and TypeScript (`.ts`/`.mts`) are
/// parsed offline; TypeScript is transpiled before creating a runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSource {
    /// Root-relative entry module path.
    pub entrypoint: String,
    /// Root-relative path to immutable source text.
    pub modules: BTreeMap<String, String>,
}

impl PluginSource {
    /// Mount a verified, host-prepared ESM package into this graph.
    /// Returns its relative entry path (import it from the application module).
    /// No Node builtin or package installation is enabled by this operation.
    pub fn add_package(
        &mut self,
        package: &denosanbox_modules::PreparedPackage,
    ) -> Result<String, Error> {
        if !package.profile.allows_package_graph() {
            return Err(Error::InvalidPlugin {
                reason: "package requires the pure-js-npm profile".into(),
            });
        }
        if package.content_digest != package.digest() {
            return Err(Error::InvalidPlugin {
                reason: "prepared package was modified".into(),
            });
        }
        let prefix = format!("vendor/{}/", package.name);
        let mut sources = BTreeMap::new();
        for (path, source) in &package.sources {
            let mounted = format!("{prefix}{path}");
            validate_path(&mounted)?;
            if self.modules.contains_key(&mounted) {
                return Err(Error::InvalidPlugin {
                    reason: "package module collides with existing graph".into(),
                });
            }
            sources.insert(mounted, source.clone());
        }
        let entrypoint = format!("{prefix}{}", package.entrypoint);
        if !sources.contains_key(&entrypoint) {
            return Err(Error::ModuleNotFound {
                specifier: entrypoint,
                referrer: None,
            });
        }
        self.modules.extend(sources);
        Ok(entrypoint)
    }

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

    /// Select a visible entrypoint from this immutable prepared graph.
    pub fn with_entrypoint(&self, entrypoint: &str) -> Result<Self, Error> {
        validate_path(entrypoint)?;
        if !self.graph.modules.contains_key(entrypoint) {
            return Err(Error::ModuleNotFound {
                specifier: entrypoint.into(),
                referrer: None,
            });
        }
        Ok(Self {
            graph: Arc::new(PreparedGraph {
                entrypoint: entrypoint.into(),
                modules: self.graph.modules.clone(),
                total_bytes: self.graph.total_bytes,
            }),
        })
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
        let prepared = denosanbox_modules::transpile_module(&path, &text).map_err(|e| {
            Error::InvalidPlugin {
                reason: e.to_string(),
            }
        })?;
        if prepared.len() > limits.max_module_bytes {
            return Err(Error::LimitExceeded {
                limit: "max_module_bytes".into(),
                detail: "transpiled output exceeds module limit".into(),
            });
        }
        modules.insert(path, prepared);
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

    let emitted_bytes: usize = modules.values().map(String::len).sum();
    if emitted_bytes > limits.max_graph_bytes {
        return Err(Error::LimitExceeded {
            limit: "max_graph_bytes".into(),
            detail: "transpiled graph exceeds limit".into(),
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
    denosanbox_modules::validate_path(path).map_err(|e| Error::InvalidPlugin {
        reason: e.to_string(),
    })?;
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
        "js" | "mjs" | "ts" | "mts" => Ok(()),
        "tsx" | "cts" | "jsx" => Err(Error::UnsupportedLanguage {
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

/// Returns imports found by the JavaScript parser. Preparation has already
/// parsed the source; the runtime still checks computed imports on every load.
pub fn scan_import_specifiers(source: &str) -> Vec<String> {
    denosanbox_modules::import_specifiers(source).unwrap_or_default()
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
    fn prepare_rejects_module_escapes_and_transpiles_ts() {
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

        let ts = prepare(
            PluginSource::single("main.ts", "export const n: number = 1;"),
            &limits(),
        )
        .unwrap();
        assert!(!ts.source("main.ts").unwrap().contains(": number"));
    }
}
