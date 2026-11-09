//! Truthful feature negotiation before creating a guest runtime.
//!
//! A capability is advertised only when its mediation layer is actually
//! installed. The matrix here is deliberately conservative: the Deno backend
//! claims JavaScript execution, JSON calls, prepared packages, independent
//! isolates and deadline/cancellation because each is enforced by the native
//! core. Mediated features (host callbacks, mounts, fetch, workers) are added
//! by the facade per sandbox after the mediator is wired. Native code and
//! shared memory are never claimed.

use crate::{BackendKind, Error, Result};
use std::collections::BTreeSet;

/// Stable guest-visible feature names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    /// Guest JavaScript ESM executes in a V8 isolate.
    JavaScriptEsm,
    /// TypeScript sources are transpiled during preparation.
    TypeScript,
    /// JSON input/output crosses the call boundary.
    JsonCalls,
    /// Host-prepared module graphs can be mounted.
    PreparedPackages,
    /// Each sandbox has an independent isolate.
    IndependentIsolates,
    /// Absolute deadlines and cancellation terminate execution.
    DeadlineCancellation,
    /// Scoped host callbacks are registered and mediated.
    HostCallbacks,
    /// Virtual filesystem access is mediated by the host.
    VirtualFilesystem,
    /// Virtual process environment is mediated by the host.
    VirtualEnvironment,
    /// Children are created only through the trusted broker.
    ManagedChildren,
    /// Browser `Worker` profile is installed.
    WebWorkers,
    /// Node `worker_threads` alias is installed under an approved profile.
    NodeWorkerThreads,
    /// Nofollow host directories are mounted into the virtual view.
    MountedDirectories,
    /// `fetch` is mediated by the host connector.
    Fetch,
    /// Guest native code (never advertised).
    NativeCode,
    /// Shared memory / `SharedArrayBuffer` (never advertised).
    SharedMemory,
}

impl Feature {
    /// Stable lowercase name used in typed errors and bindings.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JavaScriptEsm => "javascript-esm",
            Self::TypeScript => "typescript",
            Self::JsonCalls => "json-calls",
            Self::PreparedPackages => "prepared-packages",
            Self::IndependentIsolates => "independent-isolates",
            Self::DeadlineCancellation => "deadline-cancellation",
            Self::HostCallbacks => "host-callbacks",
            Self::VirtualFilesystem => "virtual-filesystem",
            Self::VirtualEnvironment => "virtual-environment",
            Self::ManagedChildren => "managed-children",
            Self::WebWorkers => "web-workers",
            Self::NodeWorkerThreads => "node-worker-threads",
            Self::MountedDirectories => "mounted-directories",
            Self::Fetch => "fetch",
            Self::NativeCode => "native-code",
            Self::SharedMemory => "shared-memory",
        }
    }

    /// Every feature in stable order.
    pub const ALL: [Feature; 16] = [
        Self::JavaScriptEsm,
        Self::TypeScript,
        Self::JsonCalls,
        Self::PreparedPackages,
        Self::IndependentIsolates,
        Self::DeadlineCancellation,
        Self::HostCallbacks,
        Self::VirtualFilesystem,
        Self::VirtualEnvironment,
        Self::ManagedChildren,
        Self::WebWorkers,
        Self::NodeWorkerThreads,
        Self::MountedDirectories,
        Self::Fetch,
        Self::NativeCode,
        Self::SharedMemory,
    ];
}

impl std::fmt::Display for Feature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Truthful capability set of one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// Backend whose contract this set describes.
    pub backend: BackendKind,
    /// Backend capability protocol version.
    pub protocol_version: u16,
    features: BTreeSet<Feature>,
}

impl Capabilities {
    /// Baseline matrix for `backend`. Only installed mediators are listed.
    pub fn for_backend(backend: BackendKind) -> Self {
        let features = match backend {
            // The fake backend never executes JavaScript and mediates nothing.
            BackendKind::Fake => BTreeSet::new(),
            BackendKind::Deno => [
                Feature::JavaScriptEsm,
                Feature::TypeScript,
                Feature::JsonCalls,
                Feature::PreparedPackages,
                Feature::IndependentIsolates,
                Feature::DeadlineCancellation,
                Feature::VirtualEnvironment,
            ]
            .into_iter()
            .collect(),
        };
        Self {
            backend,
            protocol_version: 1,
            features,
        }
    }

    /// Returns whether `feature` is advertised.
    pub fn supports(&self, feature: Feature) -> bool {
        self.features.contains(&feature)
    }

    /// Advertised features in stable order.
    pub fn features(&self) -> impl Iterator<Item = Feature> + '_ {
        self.features.iter().copied()
    }

    /// The facade adds only features whose mediation layer is actually installed.
    pub fn with_feature(mut self, feature: Feature) -> Self {
        self.features.insert(feature);
        self
    }

    /// Removes a feature that is not installed for a particular sandbox.
    pub fn without_feature(mut self, feature: Feature) -> Self {
        self.features.remove(&feature);
        self
    }

    /// Returns a typed [`Error::UnsupportedFeature`] for the first missing feature.
    pub fn require(&self, requirements: &PluginRequirements) -> Result<()> {
        for feature in &requirements.features {
            if !self.supports(*feature) {
                return Err(Error::UnsupportedFeature {
                    feature: feature.as_str().into(),
                });
            }
        }
        Ok(())
    }
}

/// Features a plugin requires before it may be parsed or executed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginRequirements {
    /// Required features.
    pub features: BTreeSet<Feature>,
}

impl PluginRequirements {
    /// Builds requirements from a feature set.
    pub fn new(features: impl IntoIterator<Item = Feature>) -> Self {
        Self {
            features: features.into_iter().collect(),
        }
    }

    /// Adds one requirement.
    pub fn with(mut self, feature: Feature) -> Self {
        self.features.insert(feature);
        self
    }

    /// Returns whether no feature is required.
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }
}

/// Backend metric snapshot. Unknown quantities remain `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeMetrics {
    /// Successful invocations observed by this backend.
    pub completed_invocations: Option<u64>,
    /// Cancelled/deadline-terminated invocations observed.
    pub cancelled_invocations: Option<u64>,
    /// Current V8 heap usage in bytes, when measured.
    pub heap_used_bytes: Option<usize>,
    /// Configured V8 heap limit in bytes, when enforced.
    pub heap_limit_bytes: Option<usize>,
}

impl RuntimeMetrics {
    /// Metrics with an accurate invocation count and no heap measurement.
    pub fn invocations(completed: u64, cancelled: u64) -> Self {
        Self {
            completed_invocations: Some(completed),
            cancelled_invocations: Some(cancelled),
            heap_used_bytes: None,
            heap_limit_bytes: None,
        }
    }

    /// Merges `update`, preferring measured values.
    pub fn merge(&mut self, update: &RuntimeMetrics) {
        if update.completed_invocations.is_some() {
            self.completed_invocations = update.completed_invocations;
        }
        if update.cancelled_invocations.is_some() {
            self.cancelled_invocations = update.cancelled_invocations;
        }
        if update.heap_used_bytes.is_some() {
            self.heap_used_bytes = update.heap_used_bytes;
        }
        if update.heap_limit_bytes.is_some() {
            self.heap_limit_bytes = update.heap_limit_bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    //! The advertised matrix must stay truthful: uninstalled features fail
    //! negotiation with a typed error instead of being claimed and failing at
    //! runtime.

    use super::*;

    #[test]
    fn require_native_code_returns_typed_unsupported_feature() {
        for backend in [BackendKind::Fake, BackendKind::Deno] {
            let capabilities = Capabilities::for_backend(backend);
            assert!(!capabilities.supports(Feature::NativeCode));
            assert!(!capabilities.supports(Feature::SharedMemory));
            let err = capabilities
                .require(&PluginRequirements::new([Feature::NativeCode]))
                .expect_err("native-code must not be advertised");
            match err {
                Error::UnsupportedFeature { feature } => {
                    assert_eq!(feature, Feature::NativeCode.as_str());
                }
                other => panic!("expected typed UnsupportedFeature, got {other:?}"),
            }
        }
    }

    #[test]
    fn workers_are_not_advertised_by_baseline() {
        for backend in [BackendKind::Fake, BackendKind::Deno] {
            let capabilities = Capabilities::for_backend(backend);
            for feature in [Feature::WebWorkers, Feature::NodeWorkerThreads] {
                assert!(!capabilities.supports(feature));
                assert!(capabilities
                    .require(&PluginRequirements::new([feature]))
                    .is_err());
            }
        }
    }

    #[test]
    fn fake_backend_claims_no_javascript() {
        let capabilities = Capabilities::for_backend(BackendKind::Fake);
        assert_eq!(capabilities.features().count(), 0);
        for feature in [
            Feature::JavaScriptEsm,
            Feature::JsonCalls,
            Feature::IndependentIsolates,
            Feature::TypeScript,
        ] {
            assert!(
                !capabilities.supports(feature),
                "{feature} must not be claimed by fake"
            );
        }
        assert!(capabilities
            .require(&PluginRequirements::new([Feature::JsonCalls]))
            .is_err());
    }

    #[test]
    fn mediation_gated_features_require_explicit_opt_in() {
        let capabilities = Capabilities::for_backend(BackendKind::Deno);
        assert!(capabilities
            .require(&PluginRequirements::new([
                Feature::JavaScriptEsm,
                Feature::TypeScript,
            ]))
            .is_ok());
        for gated in [
            Feature::HostCallbacks,
            Feature::VirtualFilesystem,
            Feature::Fetch,
            Feature::MountedDirectories,
            Feature::ManagedChildren,
            Feature::WebWorkers,
            Feature::SharedMemory,
            Feature::NativeCode,
        ] {
            assert!(!capabilities.supports(gated), "{gated:?} must be opt-in");
        }
        // `with_feature` is the only path that adds a mediated feature.
        let wired = capabilities.with_feature(Feature::VirtualFilesystem);
        assert!(wired.supports(Feature::VirtualFilesystem));
        assert!(!wired
            .without_feature(Feature::VirtualFilesystem)
            .supports(Feature::VirtualFilesystem));
    }

    #[test]
    fn feature_names_are_unique() {
        let names: BTreeSet<_> = Feature::ALL.iter().map(|f| f.as_str()).collect();
        assert_eq!(names.len(), Feature::ALL.len());
    }
}
