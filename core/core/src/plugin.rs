//! Immutable plugin source graph and import visibility validation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::config::EngineLimits;
use crate::Error;
use denosanbox_modules::CompatibilityProfile;

/// Reserved namespace for trusted, host-injected builtin modules.
///
/// Guest-supplied sources may never occupy this namespace; only the host
/// injects modules here, and only for the profiles that authorize them.
pub const RESERVED_BUILTIN_DIR: &str = "__builtin__";

/// Reserved module path backing the `node:worker_threads` / `worker_threads`
/// aliases.
pub const WORKER_THREADS_BUILTIN: &str = "__builtin__/worker_threads.js";

/// Trusted source of the reserved `worker_threads` builtin.
///
/// The loader treats the presence of this exact module in the private graph as
/// the authorization for the `worker_threads` aliases; the implementation
/// objects themselves are installed by the runtime on the sandbox global.
pub const WORKER_THREADS_SOURCE: &str = concat!(
    "export const Worker=globalThis.__denosanboxNodeWorker;\n",
    "export const parentPort=globalThis.__denosanboxParentPort;\n",
    "export const isMainThread=globalThis.__denosanboxIsMainThread;\n",
    "export const workerData=undefined;\n",
    "export const threadId=isMainThread?0:1;\n",
);

/// Returns whether `path` lives in the reserved `__builtin__/` namespace.
pub fn is_reserved_builtin_path(path: &str) -> bool {
    path == RESERVED_BUILTIN_DIR
        || (path.len() > RESERVED_BUILTIN_DIR.len()
            && path.starts_with(RESERVED_BUILTIN_DIR)
            && path.as_bytes()[RESERVED_BUILTIN_DIR.len()] == b'/')
}

/// Maps a Node-compatible alias to its reserved builtin module path.
///
/// Only `worker_threads` (with or without the `node:` prefix) is aliased and
/// only for profiles that inject the reserved module; every other Node builtin,
/// bare specifier and URL stays denied.
pub fn builtin_alias_target(specifier: &str) -> Option<&'static str> {
    match specifier {
        "node:worker_threads" | "worker_threads" => Some(WORKER_THREADS_BUILTIN),
        _ => None,
    }
}

/// Trusted builtin modules injected for a profile.
fn injected_builtins(profile: CompatibilityProfile) -> &'static [(&'static str, &'static str)] {
    if profile.allows_package_graph() {
        &[(WORKER_THREADS_BUILTIN, WORKER_THREADS_SOURCE)]
    } else {
        &[]
    }
}

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
            if is_reserved_builtin_path(&mounted) {
                return Err(Error::ModuleDenied {
                    specifier: mounted,
                    reason: "the __builtin__/ namespace is reserved for trusted modules".into(),
                });
            }
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
            .field("profile", &self.graph.profile)
            .finish()
    }
}

impl PreparedPlugin {
    /// Root-relative entrypoint path.
    pub fn entrypoint(&self) -> &str {
        &self.graph.entrypoint
    }

    /// Capability profile this graph was prepared under.
    pub fn profile(&self) -> CompatibilityProfile {
        self.graph.profile
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
                profile: self.graph.profile,
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

    /// Returns whether the private graph carries the reserved trusted module.
    pub fn has_reserved_builtin(&self, path: &str) -> bool {
        is_reserved_builtin_path(path)
            && self.graph.modules.get(path).map(|s| s.as_str())
                == Some(builtin_source(path).unwrap_or(""))
    }

    /// Iterates immutable `(path, source)` pairs in deterministic path order.
    pub fn iter_modules(&self) -> impl Iterator<Item = (&str, &str)> {
        self.graph
            .modules
            .iter()
            .map(|(path, source)| (path.as_str(), source.as_str()))
    }
}

/// Trusted source text for a reserved builtin module path.
pub fn builtin_source(path: &str) -> Option<&'static str> {
    match path {
        WORKER_THREADS_BUILTIN => Some(WORKER_THREADS_SOURCE),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) struct PreparedGraph {
    pub(crate) entrypoint: String,
    pub(crate) modules: BTreeMap<String, String>,
    pub(crate) total_bytes: usize,
    pub(crate) profile: CompatibilityProfile,
}

/// Validates and freezes a host-supplied source graph with the default
/// [`CompatibilityProfile::JavaScriptOnly`] profile.
///
/// See [`prepare_with_profile`].
pub fn prepare(source: PluginSource, limits: &EngineLimits) -> Result<PreparedPlugin, Error> {
    prepare_with_profile(source, limits, CompatibilityProfile::JavaScriptOnly)
}

/// Validates and freezes a host-supplied source graph under `profile`.
///
/// The prepared graph is the only visibility source used by runtime module
/// loaders. Literal static imports and literal dynamic imports are checked
/// eagerly; every later load is rechecked by the backend loader against this
/// same immutable graph.
///
/// `CompatibilityProfile::PureJsNpm` additionally injects the trusted
/// `__builtin__/worker_threads.js` module and authorizes only the
/// `worker_threads` / `node:worker_threads` aliases to it. Guest sources may
/// never supply keys under `__builtin__/`, and every other Node builtin, bare
/// specifier and URL import stays denied in every profile.
pub fn prepare_with_profile(
    source: PluginSource,
    limits: &EngineLimits,
    profile: CompatibilityProfile,
) -> Result<PreparedPlugin, Error> {
    limits.validate()?;
    validate_path(&source.entrypoint)?;
    if is_reserved_builtin_path(&source.entrypoint) {
        return Err(Error::ModuleDenied {
            specifier: source.entrypoint,
            reason: "the __builtin__/ namespace is reserved for trusted modules".to_string(),
        });
    }

    if source.modules.is_empty() {
        return Err(Error::InvalidPlugin {
            reason: "plugin must contain at least one module".to_string(),
        });
    }
    // Reject guest-supplied raw sources in the reserved namespace before any
    // trusted module is injected, in every profile.
    for path in source.modules.keys() {
        if is_reserved_builtin_path(path) {
            return Err(Error::ModuleDenied {
                specifier: path.clone(),
                reason: "the __builtin__/ namespace is reserved for trusted modules".to_string(),
            });
        }
    }

    let injected = injected_builtins(profile);
    let module_count = source.modules.len().saturating_add(injected.len());
    if module_count > limits.max_modules {
        return Err(Error::LimitExceeded {
            limit: "max_modules".to_string(),
            detail: format!(
                "plugin has {module_count} modules but the limit is {}",
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

    // Trusted placeholders count against the same module and byte budgets as
    // guest modules, so a budget cannot be escaped by the injected sources.
    for (path, text) in injected {
        if text.len() > limits.max_module_bytes {
            return Err(Error::LimitExceeded {
                limit: "max_module_bytes".to_string(),
                detail: format!(
                    "builtin module '{path}' is {} bytes but the limit is {}",
                    text.len(),
                    limits.max_module_bytes
                ),
            });
        }
        total_bytes = total_bytes.saturating_add(text.len());
        modules.insert((*path).to_string(), (*text).to_string());
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
            validate_import(&specifier, path, &modules, profile)?;
        }
    }

    Ok(PreparedPlugin {
        graph: Arc::new(PreparedGraph {
            entrypoint: source.entrypoint,
            modules,
            total_bytes,
            profile,
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
    profile: CompatibilityProfile,
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
    // Node-compatible aliases resolve only to a reserved builtin module that
    // this profile actually injected into the private graph.
    if let Some(target) = builtin_alias_target(specifier) {
        if profile.allows_package_graph() && modules.contains_key(target) {
            return Ok(());
        }
        return Err(Error::ModuleDenied {
            specifier: specifier.to_string(),
            reason: format!(
                "'{specifier}' requires the {profile_str} profile and an injected trusted builtin",
                profile_str = CompatibilityProfile::PureJsNpm.as_str()
            ),
        });
    }
    if specifier.starts_with("node:") {
        return Err(Error::ModuleDenied {
            specifier: specifier.to_string(),
            reason: "Node builtin modules are not available in this profile".to_string(),
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

    fn pure() -> CompatibilityProfile {
        CompatibilityProfile::PureJsNpm
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

    #[test]
    fn defaultdeniesnodealiases() {
        for specifier in ["node:worker_threads", "worker_threads"] {
            let source = format!(
                "import {{ Worker }} from '{specifier}'; export function run() {{ return Worker; }}"
            );
            let denied = prepare(PluginSource::single("main.js", source), &limits()).unwrap_err();
            assert!(
                matches!(denied, Error::ModuleDenied { .. }),
                "{specifier} must be denied by default, got {denied:?}"
            );
        }
        // The default profile injects no reserved module at all.
        let prepared = prepare(PluginSource::single("main.js", "export const n = 1;"), &limits())
            .unwrap();
        assert_eq!(prepared.profile(), CompatibilityProfile::JavaScriptOnly);
        assert_eq!(prepared.entrypoint(), "main.js");
        assert!(prepared.source(WORKER_THREADS_BUILTIN).is_none());
        assert_eq!(prepared.module_count(), 1);
    }

    #[test]
    #[allow(non_snake_case)]
    fn PureJsNpmpermits() {
        let source = PluginSource::single(
            "main.js",
            "import { Worker, isMainThread } from 'node:worker_threads'; \
             import * as wt from 'worker_threads'; \
             export function run() { return [Worker, isMainThread, wt.threadId]; }",
        );
        let prepared = prepare_with_profile(source, &limits(), pure()).unwrap();
        assert_eq!(prepared.profile(), pure());
        assert_eq!(prepared.module_count(), 2);
        let builtin = prepared
            .source(WORKER_THREADS_BUILTIN)
            .expect("reserved worker_threads module must be injected");
        assert_eq!(builtin, WORKER_THREADS_SOURCE);
        assert!(builtin.contains("export const Worker=globalThis.__denosanboxNodeWorker;"));
        assert!(builtin.contains("export const parentPort=globalThis.__denosanboxParentPort;"));
        assert!(builtin.contains("export const isMainThread=globalThis.__denosanboxIsMainThread;"));
        assert!(builtin.contains("export const workerData=undefined;"));
        assert!(builtin.contains("export const threadId=isMainThread?0:1;"));
        assert!(prepared.has_reserved_builtin(WORKER_THREADS_BUILTIN));
        // TypeScript and entrypoint preservation still hold with the profile.
        let ts = prepare_with_profile(
            PluginSource::single("main.ts", "export const n: number = 1;"),
            &limits(),
            pure(),
        )
        .unwrap();
        assert!(!ts.source("main.ts").unwrap().contains(": number"));
        let alt = ts.with_entrypoint(WORKER_THREADS_BUILTIN).unwrap();
        assert_eq!(alt.entrypoint(), WORKER_THREADS_BUILTIN);
        assert_eq!(alt.profile(), pure());
    }

    #[test]
    fn reservedrawpathrejects() {
        for profile in [CompatibilityProfile::JavaScriptOnly, pure()] {
            let mut source = PluginSource::single("main.js", "export const n = 1;");
            source.modules.insert(
                WORKER_THREADS_BUILTIN.to_string(),
                "export const Worker = () => 'hijacked';".to_string(),
            );
            let rejected = prepare_with_profile(source, &limits(), profile).unwrap_err();
            assert!(
                matches!(rejected, Error::ModuleDenied { .. }),
                "profile {profile:?} must reject raw __builtin__ sources, got {rejected:?}"
            );

            let mut other = PluginSource::single("main.js", "export const n = 1;");
            other
                .modules
                .insert("__builtin__/evil.js".to_string(), "export const x = 1;".to_string());
            let rejected = prepare_with_profile(other, &limits(), profile).unwrap_err();
            assert!(matches!(rejected, Error::ModuleDenied { .. }));

            let entry = PluginSource {
                entrypoint: WORKER_THREADS_BUILTIN.to_string(),
                modules: BTreeMap::from([(
                    WORKER_THREADS_BUILTIN.to_string(),
                    "export const Worker = 1;".to_string(),
                )]),
            };
            let rejected = prepare_with_profile(entry, &limits(), profile).unwrap_err();
            assert!(matches!(rejected, Error::ModuleDenied { .. }));
        }
    }

    #[test]
    fn modulequotascountbuiltin() {
        // Two guest modules plus the injected builtin exceed a budget of two.
        let mut source = PluginSource::single(
            "main.js",
            "import { n } from './lib.js'; export const value = n;",
        );
        source
            .modules
            .insert("lib.js".to_string(), "export const n = 1;".to_string());
        let mut two_modules = limits();
        two_modules.max_modules = 2;
        let over = prepare_with_profile(source.clone(), &two_modules, pure()).unwrap_err();
        assert!(
            matches!(&over, Error::LimitExceeded { limit, .. } if limit == "max_modules"),
            "injected builtin must count against max_modules, got {over:?}"
        );
        // The same graph fits without the injected builtin.
        assert!(prepare_with_profile(source.clone(), &two_modules, CompatibilityProfile::JavaScriptOnly)
            .is_ok());

        // Builtin bytes count against the graph budget too.
        let guest_bytes: usize = source.modules.values().map(String::len).sum();
        let mut tight = limits();
        tight.max_graph_bytes = guest_bytes + WORKER_THREADS_SOURCE.len() - 1;
        let over = prepare_with_profile(source.clone(), &tight, pure()).unwrap_err();
        assert!(
            matches!(&over, Error::LimitExceeded { limit, .. } if limit == "max_graph_bytes"),
            "injected builtin must count against max_graph_bytes, got {over:?}"
        );
        assert!(prepare_with_profile(source.clone(), &tight, CompatibilityProfile::JavaScriptOnly)
            .is_ok());

        // Builtin bytes count against the per-module budget too.
        let mut tiny_module = limits();
        tiny_module.max_module_bytes = WORKER_THREADS_SOURCE.len() - 1;
        tiny_module.max_graph_bytes = usize::MAX / 2;
        let over = prepare_with_profile(source.clone(), &tiny_module, pure()).unwrap_err();
        assert!(
            matches!(&over, Error::LimitExceeded { limit, .. } if limit == "max_module_bytes"),
            "injected builtin must count against max_module_bytes, got {over:?}"
        );

        // Accounting is surfaced in the graph totals.
        let ok = prepare_with_profile(source, &limits(), pure()).unwrap();
        assert_eq!(ok.module_count(), 3);
        assert_eq!(
            ok.total_bytes(),
            guest_bytes + WORKER_THREADS_SOURCE.len()
        );
    }

    #[test]
    fn othernativebuiltinbothdeny() {
        for specifier in ["node:fs", "fs", "node:child_process", "node:vm", "node:ffi"] {
            for profile in [CompatibilityProfile::JavaScriptOnly, pure()] {
                let source = format!(
                    "import * as m from '{specifier}'; export const run = () => m;"
                );
                let denied =
                    prepare_with_profile(PluginSource::single("main.js", source), &limits(), profile)
                        .unwrap_err();
                assert!(
                    matches!(denied, Error::ModuleDenied { .. }),
                    "profile {profile:?} must deny {specifier}, got {denied:?}"
                );
            }
        }
        // URLs and other bare specifiers stay denied even with PureJsNpm.
        for specifier in [
            "https://example.com/x.js",
            "npm:left-pad",
            "node:worker_threads/inner",
        ] {
            let source = format!("import '{specifier}'; export const run = () => 1;");
            let denied =
                prepare_with_profile(PluginSource::single("main.js", source), &limits(), pure())
                    .unwrap_err();
            assert!(matches!(denied, Error::ModuleDenied { .. }));
        }
    }
}
