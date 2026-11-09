use std::fmt;

/// Errors produced by module preparation and validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleError {
    /// A supplied path/specifier was rejected by [`crate::validate_path`].
    InvalidPath { path: String, reason: &'static str },
    /// The media type is not supported (jsx/tsx/cts and unknown extensions).
    UnsupportedMediaType { path: String, media_type: String },
    /// Parsing failed; contains a compact diagnostic.
    ParseError { path: String, message: String },
    /// Transpilation failed; contains a compact diagnostic.
    TranspileError { path: String, message: String },
    /// Specifier could not be represented as a `ModuleSpecifier`.
    InvalidSpecifier { path: String },
    /// Package name failed host-side validation.
    InvalidPackageName { name: String, reason: &'static str },
    /// A package module path failed validation or escaped the package root.
    InvalidModulePath { path: String, reason: &'static str },
    /// Duplicate module path within one package.
    DuplicateModule { path: String },
    /// A limit from [`crate::Limits`] was exceeded.
    LimitExceeded { limit: &'static str, actual: usize },
    /// Declared integrity value is malformed.
    InvalidIntegrity { path: String, reason: &'static str },
    /// Computed sha256 does not match the declared integrity digest.
    IntegrityMismatch { path: String },
    /// Install scripts are not supported.
    InstallScriptsRejected,
    /// Native dependencies are not supported.
    NativeDependenciesRejected,
    /// A guest native/process/network/Node-builtin import was denied.
    DeniedImport {
        module: String,
        policy: &'static str,
    },
    /// Generic configuration error (e.g. missing entrypoint).
    Config { message: String },
}

impl ModuleError {
    pub(crate) fn invalid_path(path: impl Into<String>, reason: &'static str) -> Self {
        ModuleError::InvalidPath {
            path: path.into(),
            reason,
        }
    }

    pub(crate) fn invalid_module_path(path: impl Into<String>, reason: &'static str) -> Self {
        ModuleError::InvalidModulePath {
            path: path.into(),
            reason,
        }
    }
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModuleError::InvalidPath { path, reason } => {
                write!(f, "invalid module path {path:?}: {reason}")
            }
            ModuleError::UnsupportedMediaType { path, media_type } => {
                write!(f, "unsupported media type {media_type} for {path:?}")
            }
            ModuleError::ParseError { path, message } => {
                write!(f, "parse error in {path:?}: {message}")
            }
            ModuleError::TranspileError { path, message } => {
                write!(f, "transpile error in {path:?}: {message}")
            }
            ModuleError::InvalidSpecifier { path } => {
                write!(f, "cannot build module specifier for {path:?}")
            }
            ModuleError::InvalidPackageName { name, reason } => {
                write!(f, "invalid package name {name:?}: {reason}")
            }
            ModuleError::InvalidModulePath { path, reason } => {
                write!(f, "invalid package module path {path:?}: {reason}")
            }
            ModuleError::DuplicateModule { path } => {
                write!(f, "duplicate package module {path:?}")
            }
            ModuleError::LimitExceeded { limit, actual } => {
                write!(f, "limit {limit} exceeded (actual {actual})")
            }
            ModuleError::InvalidIntegrity { path, reason } => {
                write!(f, "invalid integrity for {path:?}: {reason}")
            }
            ModuleError::IntegrityMismatch { path } => {
                write!(f, "integrity mismatch for {path:?}")
            }
            ModuleError::InstallScriptsRejected => {
                write!(f, "install scripts are not supported for prepared packages")
            }
            ModuleError::NativeDependenciesRejected => {
                write!(
                    f,
                    "native dependencies are not supported for prepared packages"
                )
            }
            ModuleError::DeniedImport { module, policy } => {
                write!(f, "import denied for {module:?} by policy {policy}")
            }
            ModuleError::Config { message } => write!(f, "invalid configuration: {message}"),
        }
    }
}

impl std::error::Error for ModuleError {}
