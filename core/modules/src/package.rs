//! Host-prepared, vendored package graphs.
//!
//! A [`Package`] is supplied entirely in memory by the host after vendoring an
//! approved pure-JavaScript package. Nothing in this module downloads, installs
//! or resolves packages. [`Package::prepare`] verifies invariants, checks
//! declared sha256 integrity against the raw vendored sources, transpiles each
//! module into loader-ready JavaScript, and returns an owned
//! [`PreparedPackage`] snapshot.
//!
//! [`CompatibilityProfile`] intentionally denies guest native code, process
//! access, network imports and Node builtins in *both* profiles. `PureJsNpm`
//! only means "prepared pure-JS dependency graph is allowed"; it is not a claim
//! of unrestricted npm compatibility.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::error::ModuleError;
use crate::path::validate_path;
use crate::transpile::transpile_module;

/// Version of the preparation backend. It is part of every content digest so
/// cached loader output is invalidated when the preparation backend changes.
pub const BACKEND_VERSION: &str = "denosanbox-modules/0.1.0+deno_ast/0.53.3";

/// Guest-visible capability profile.
///
/// Both variants deny native code, process access, network access and Node
/// builtins. The distinction is only whether the guest may load modules from a
/// host-prepared pure-JS dependency graph (`PureJsNpm`) or not (`JavaScriptOnly`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CompatibilityProfile {
    /// Application JavaScript/ES modules only; no package graph imports.
    #[default]
    JavaScriptOnly,
    /// Application modules plus a host-prepared, vendored pure-JS graph.
    PureJsNpm,
}

impl CompatibilityProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            CompatibilityProfile::JavaScriptOnly => "javascript-only",
            CompatibilityProfile::PureJsNpm => "pure-js-npm",
        }
    }

    pub const fn allows_node_builtins(self) -> bool {
        false
    }

    pub const fn allows_process(self) -> bool {
        false
    }

    pub const fn allows_network(self) -> bool {
        false
    }

    pub const fn allows_native(self) -> bool {
        false
    }

    pub const fn allows_package_graph(self) -> bool {
        matches!(self, CompatibilityProfile::PureJsNpm)
    }

    /// Policy hook for the runtime loader (the loader remains authoritative for
    /// the actual graph). Denies Node builtins, process/ffi, network URLs and
    /// package-manager specifiers for every profile.
    pub fn ensure_import_allowed(self, specifier: &str) -> Result<(), ModuleError> {
        let policy = if specifier.starts_with("node:") {
            Some("node-builtin")
        } else if specifier == "process" || specifier.starts_with("process/") {
            Some("process")
        } else if specifier == "ffi" || specifier.starts_with("ffi/") {
            Some("ffi")
        } else if specifier.starts_with("http:")
            || specifier.starts_with("https:")
            || specifier.starts_with("ws:")
            || specifier.starts_with("wss:")
            || specifier.starts_with("ftp:")
        {
            Some("network")
        } else if specifier.starts_with("npm:") {
            Some("package-manager")
        } else {
            None
        };
        match policy {
            Some(policy) => Err(ModuleError::DeniedImport {
                module: specifier.to_string(),
                policy,
            }),
            None => Ok(()),
        }
    }
}

/// Resource limits applied during host package preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_modules: usize,
    pub max_module_bytes: usize,
    pub max_graph_bytes: usize,
}

/// 128 modules, 1 MiB per module, 8 MiB total raw source.
pub const DEFAULT_LIMITS: Limits = Limits {
    max_modules: 128,
    max_module_bytes: 1024 * 1024,
    max_graph_bytes: 8 * 1024 * 1024,
};

impl Default for Limits {
    fn default() -> Self {
        DEFAULT_LIMITS
    }
}

/// Host-vendored package description. All fields are supplied by the host; no
/// package registry or network is consulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// Raw vendored sources keyed by relative prepared path.
    pub modules: BTreeMap<String, String>,
    /// Declared sha256 digests (64 hex chars, optionally `sha256-`/`sha256:`
    /// prefixed) keyed by the same paths as `modules`.
    pub integrity: BTreeMap<String, String>,
    pub entrypoint: String,
    pub install_scripts: bool,
    pub native_dependencies: bool,
}

impl Package {
    /// Prepare with the `JavaScriptOnly` profile and [`DEFAULT_LIMITS`].
    pub fn prepare(&self) -> Result<PreparedPackage, ModuleError> {
        self.prepare_with_profile(CompatibilityProfile::JavaScriptOnly)
    }

    /// Prepare with the default profile and explicit limits.
    pub fn prepare_with_limits(&self, limits: &Limits) -> Result<PreparedPackage, ModuleError> {
        self.prepare_with_profile_and_limits(CompatibilityProfile::JavaScriptOnly, limits)
    }

    /// Prepare with an explicit profile and default limits.
    pub fn prepare_with_profile(
        &self,
        profile: CompatibilityProfile,
    ) -> Result<PreparedPackage, ModuleError> {
        self.prepare_with_profile_and_limits(profile, &DEFAULT_LIMITS)
    }

    /// Prepare with explicit profile and limits.
    pub fn prepare_with_profile_and_limits(
        &self,
        profile: CompatibilityProfile,
        limits: &Limits,
    ) -> Result<PreparedPackage, ModuleError> {
        prepare_package(self, profile, limits)
    }
}

/// Validated, loader-ready package source snapshot.
///
/// The host owns the plain [`BTreeMap`] and may edit it; callers must re-prepare
/// after edits. The sandbox facade freezes its own copy on preparation. Paths keep their original extension, while the contents are always
/// prepared JavaScript (JavaScript inputs are unchanged; TypeScript inputs are
/// transpiled). The runtime loader must treat these prepared entries as
/// authoritative and must not re-derive modules by scanning imports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedPackage {
    pub name: String,
    pub version: String,
    pub entrypoint: String,
    pub profile: CompatibilityProfile,
    pub sources: BTreeMap<String, String>,
    /// Normalized lowercase sha256 hex digests of the raw vendored sources.
    pub integrity: BTreeMap<String, String>,
    /// Backend-version + profile + content keyed digest for memoisation.
    pub content_digest: String,
    pub backend_version: &'static str,
}

impl PreparedPackage {
    /// Recompute the content digest from the immutable prepared data.
    pub fn digest(&self) -> String {
        content_digest(self)
    }
}

/// Recompute a [`PreparedPackage`]'s content digest.
///
/// The digest covers [`BACKEND_VERSION`], the compatibility profile, package
/// identity/entrypoint and every prepared module path + source hash.
pub fn content_digest(package: &PreparedPackage) -> String {
    compute_content_digest(
        &package.name,
        &package.version,
        &package.entrypoint,
        package.profile,
        &package.sources,
    )
}

/// Prepare a host-vendored package. See [`Package::prepare`].
pub fn prepare_package(
    package: &Package,
    profile: CompatibilityProfile,
    limits: &Limits,
) -> Result<PreparedPackage, ModuleError> {
    if package.install_scripts {
        return Err(ModuleError::InstallScriptsRejected);
    }
    if package.native_dependencies {
        return Err(ModuleError::NativeDependenciesRejected);
    }
    validate_package_name(&package.name)?;
    validate_version(&package.version)?;

    if package.modules.is_empty() {
        return Err(ModuleError::Config {
            message: "package has no modules".to_string(),
        });
    }
    if package.modules.len() > limits.max_modules {
        return Err(ModuleError::LimitExceeded {
            limit: "max_modules",
            actual: package.modules.len(),
        });
    }
    if package.entrypoint.is_empty() {
        return Err(ModuleError::Config {
            message: "package entrypoint is empty".to_string(),
        });
    }
    validate_package_module_path(&package.entrypoint)?;
    if !package.modules.contains_key(&package.entrypoint) {
        return Err(ModuleError::Config {
            message: format!(
                "entrypoint {:?} is not present in package modules",
                package.entrypoint
            ),
        });
    }

    let mut raw_sources: BTreeMap<String, String> = BTreeMap::new();
    let mut normalized_integrity: BTreeMap<String, String> = BTreeMap::new();
    let mut graph_bytes = 0usize;

    for (path, source) in &package.modules {
        validate_package_module_path(path)?;
        if source.len() > limits.max_module_bytes {
            return Err(ModuleError::LimitExceeded {
                limit: "max_module_bytes",
                actual: source.len(),
            });
        }
        graph_bytes = graph_bytes
            .checked_add(source.len())
            .ok_or(ModuleError::LimitExceeded {
                limit: "max_graph_bytes",
                actual: usize::MAX,
            })?;
        if graph_bytes > limits.max_graph_bytes {
            return Err(ModuleError::LimitExceeded {
                limit: "max_graph_bytes",
                actual: graph_bytes,
            });
        }

        let declared =
            package
                .integrity
                .get(path)
                .ok_or_else(|| ModuleError::InvalidIntegrity {
                    path: path.clone(),
                    reason: "missing declared sha256",
                })?;
        let declared = normalize_sha256(path, declared)?;
        let actual = sha256_hex(source.as_bytes());
        if declared != actual {
            return Err(ModuleError::IntegrityMismatch { path: path.clone() });
        }
        normalized_integrity.insert(path.clone(), actual);
        raw_sources.insert(path.clone(), source.clone());
    }

    for path in package.integrity.keys() {
        if !package.modules.contains_key(path) {
            return Err(ModuleError::InvalidIntegrity {
                path: path.clone(),
                reason: "integrity entry has no matching module",
            });
        }
    }

    let mut prepared_sources = BTreeMap::new();
    for (path, source) in &raw_sources {
        // `transpile_module` re-validates the path, parses every module (JS
        // included) and emits loader-ready JavaScript without source maps.
        let prepared = transpile_module(path, source)?;
        prepared_sources.insert(path.clone(), prepared);
    }

    let content_digest = compute_content_digest(
        &package.name,
        &package.version,
        &package.entrypoint,
        profile,
        &prepared_sources,
    );

    Ok(PreparedPackage {
        name: package.name.clone(),
        version: package.version.clone(),
        entrypoint: package.entrypoint.clone(),
        profile,
        sources: prepared_sources,
        integrity: normalized_integrity,
        content_digest,
        backend_version: BACKEND_VERSION,
    })
}

fn compute_content_digest(
    name: &str,
    version: &str,
    entrypoint: &str,
    profile: CompatibilityProfile,
    sources: &BTreeMap<String, String>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"denosanbox-modules/content-digest/v1\0");
    hasher.update(BACKEND_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(profile.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(version.as_bytes());
    hasher.update([0]);
    hasher.update(entrypoint.as_bytes());
    hasher.update([0]);
    for (path, source) in sources {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(sha256_hex(source.as_bytes()).as_bytes());
        hasher.update([0]);
    }
    hex(hasher.finalize().as_slice())
}

fn validate_package_name(name: &str) -> Result<(), ModuleError> {
    let invalid = |reason| ModuleError::InvalidPackageName {
        name: name.to_string(),
        reason,
    };
    if name.is_empty() {
        return Err(invalid("empty name"));
    }
    if name.len() > 214 {
        return Err(invalid("name too long"));
    }
    let rest = match name.strip_prefix('@') {
        Some(rest) => {
            let (scope, package) = rest
                .split_once('/')
                .ok_or_else(|| invalid("scoped name must contain exactly one slash"))?;
            validate_name_part(scope).map_err(invalid)?;
            if package.contains('/') {
                return Err(invalid("scoped name must contain exactly one slash"));
            }
            package
        }
        None => {
            if name.contains('/') {
                return Err(invalid("unscoped name must not contain a slash"));
            }
            name
        }
    };
    validate_name_part(rest).map_err(invalid)?;
    Ok(())
}

fn validate_name_part(part: &str) -> Result<(), &'static str> {
    if part.is_empty() {
        return Err("empty name segment");
    }
    if part.starts_with('.') || part.starts_with('_') {
        return Err("name segment must not start with '.' or '_'");
    }
    if part.contains("..") {
        return Err("name segment must not contain '..'");
    }
    if !part
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err("name contains unsupported characters");
    }
    Ok(())
}

fn validate_version(version: &str) -> Result<(), ModuleError> {
    let invalid = |reason| ModuleError::Config {
        message: format!("invalid package version {version:?}: {reason}"),
    };
    if version.is_empty() {
        return Err(invalid("empty version"));
    }
    if version.len() > 128 {
        return Err(invalid("version too long"));
    }
    if !version
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
    {
        return Err(invalid("unsupported characters"));
    }
    Ok(())
}

fn validate_package_module_path(path: &str) -> Result<(), ModuleError> {
    validate_path(path).map_err(|err| match err {
        ModuleError::InvalidPath { reason, .. } => ModuleError::invalid_module_path(path, reason),
        other => other,
    })
}

fn normalize_sha256(path: &str, value: &str) -> Result<String, ModuleError> {
    let value = value
        .strip_prefix("sha256-")
        .or_else(|| value.strip_prefix("sha256:"))
        .unwrap_or(value);
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ModuleError::InvalidIntegrity {
            path: path.to_string(),
            reason: "expected 64 hex characters",
        });
    }
    Ok(value.to_ascii_lowercase())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(hasher.finalize().as_slice())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(source: &str) -> String {
        sha256_hex(source.as_bytes())
    }

    fn package_one(source: &str) -> Package {
        let mut modules = BTreeMap::new();
        modules.insert("index.js".to_string(), source.to_string());
        let mut integrity = BTreeMap::new();
        integrity.insert("index.js".to_string(), digest(source));
        Package {
            name: "demo".to_string(),
            version: "1.0.0".to_string(),
            modules,
            integrity,
            entrypoint: "index.js".to_string(),
            install_scripts: false,
            native_dependencies: false,
        }
    }

    #[test]
    fn prepares_pure_js_graph_and_exposes_sources() {
        let package = package_one("export const value = 1;\n");
        let prepared = package.prepare().expect("prepare");
        assert_eq!(
            prepared.sources.get("index.js").map(String::as_str),
            Some("export const value = 1;\n")
        );
        assert_eq!(prepared.entrypoint, "index.js");
        assert_eq!(prepared.content_digest.len(), 64);
        assert!(prepared
            .content_digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit()));
        assert_eq!(prepared.content_digest, prepared.digest());
        assert_eq!(prepared.backend_version, BACKEND_VERSION);
    }

    #[test]
    fn integrity_changes_are_detected() {
        let package = package_one("export const value = 1;\n");
        let mut bad_digest = package.clone();
        bad_digest
            .integrity
            .insert("index.js".to_string(), "0".repeat(64));
        assert!(matches!(
            bad_digest.prepare(),
            Err(ModuleError::IntegrityMismatch { .. })
        ));

        let mut changed_graph = package.clone();
        changed_graph.modules.insert(
            "index.js".to_string(),
            "export const value = 2;\n".to_string(),
        );
        assert!(matches!(
            changed_graph.prepare(),
            Err(ModuleError::IntegrityMismatch { .. })
        ));

        let mut missing = package.clone();
        missing.integrity.clear();
        assert!(matches!(
            missing.prepare(),
            Err(ModuleError::InvalidIntegrity { .. })
        ));
    }

    #[test]
    fn rejects_install_scripts_and_native_dependencies() {
        let mut package = package_one("export {};\n");
        package.install_scripts = true;
        assert!(matches!(
            package.prepare(),
            Err(ModuleError::InstallScriptsRejected)
        ));
        package.install_scripts = false;
        package.native_dependencies = true;
        assert!(matches!(
            package.prepare(),
            Err(ModuleError::NativeDependenciesRejected)
        ));
    }

    #[test]
    fn rejects_module_path_escapes_and_bad_names() {
        let mut package = package_one("export {};\n");
        package
            .modules
            .insert("../escape.js".to_string(), "export {};".to_string());
        package
            .integrity
            .insert("../escape.js".to_string(), digest("export {};"));
        assert!(matches!(
            package.prepare(),
            Err(ModuleError::InvalidModulePath { .. })
        ));

        let mut bad_name = package_one("export {};\n");
        bad_name.name = "../demo".to_string();
        assert!(matches!(
            bad_name.prepare(),
            Err(ModuleError::InvalidPackageName { .. })
        ));

        let mut bad_entry = package_one("export {};\n");
        bad_entry.entrypoint = "missing.js".to_string();
        assert!(matches!(
            bad_entry.prepare(),
            Err(ModuleError::Config { .. })
        ));
    }

    #[test]
    fn enforces_limits() {
        let package = package_one("export const value = 1;\n");
        let limits = Limits {
            max_modules: 0,
            ..DEFAULT_LIMITS
        };
        assert!(matches!(
            package.prepare_with_limits(&limits),
            Err(ModuleError::LimitExceeded {
                limit: "max_modules",
                ..
            })
        ));

        let limits = Limits {
            max_module_bytes: 4,
            ..DEFAULT_LIMITS
        };
        assert!(matches!(
            package.prepare_with_limits(&limits),
            Err(ModuleError::LimitExceeded {
                limit: "max_module_bytes",
                ..
            })
        ));

        let limits = Limits {
            max_graph_bytes: 4,
            ..DEFAULT_LIMITS
        };
        assert!(matches!(
            package.prepare_with_limits(&limits),
            Err(ModuleError::LimitExceeded {
                limit: "max_graph_bytes",
                ..
            })
        ));
    }

    #[test]
    fn transpiles_typescript_package_modules_keeping_extension() {
        let source =
            "interface Point { x: number }\nexport const px = (p: Point): number => p.x;\n";
        let mut package = package_one(source);
        package.modules = BTreeMap::from([("index.ts".to_string(), source.to_string())]);
        package.integrity = BTreeMap::from([("index.ts".to_string(), digest(source))]);
        package.entrypoint = "index.ts".to_string();
        let prepared = package.prepare().expect("prepare ts");
        let output = prepared.sources.get("index.ts").expect("module");
        assert!(!output.contains("interface Point"));
        assert!(!output.contains(": number"));
        assert!(output.contains("px"));
    }

    #[test]
    fn digest_depends_on_profile_and_backend_version() {
        let package = package_one("export {};\n");
        let js = package.prepare().expect("js profile");
        let npm = package
            .prepare_with_profile(CompatibilityProfile::PureJsNpm)
            .expect("npm profile");
        assert_ne!(js.content_digest, npm.content_digest);
        assert!(BACKEND_VERSION.contains("deno_ast/0.53.3"));
    }

    #[test]
    fn profiles_deny_node_process_network_and_package_manager() {
        for profile in [
            CompatibilityProfile::JavaScriptOnly,
            CompatibilityProfile::PureJsNpm,
        ] {
            assert!(!profile.allows_node_builtins());
            assert!(!profile.allows_process());
            assert!(!profile.allows_network());
            assert!(!profile.allows_native());
            for denied in [
                "node:fs",
                "node:fs/promises",
                "process",
                "ffi",
                "https://example.com/x.js",
                "npm:left-pad",
            ] {
                assert!(profile.ensure_import_allowed(denied).is_err(), "{denied}");
            }
            assert!(profile.ensure_import_allowed("./dep.js").is_ok());
        }
    }
}
