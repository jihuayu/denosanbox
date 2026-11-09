//! Truthful feature negotiation before creating a guest runtime.
use crate::{BackendKind, Error, Result};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Feature {
    JavaScriptEsm, TypeScript, JsonCalls, PreparedPackages, IndependentIsolates,
    DeadlineCancellation, HostCallbacks, VirtualFilesystem, VirtualEnvironment,
    ManagedChildren, WebWorkers, MountedDirectories, Fetch, NativeCode, SharedMemory,
}
impl Feature {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JavaScriptEsm => "javascript-esm", Self::TypeScript => "typescript",
            Self::JsonCalls => "json-calls", Self::PreparedPackages => "prepared-packages",
            Self::IndependentIsolates => "independent-isolates", Self::DeadlineCancellation => "deadline-cancellation",
            Self::HostCallbacks => "host-callbacks", Self::VirtualFilesystem => "virtual-filesystem",
            Self::VirtualEnvironment => "virtual-environment", Self::ManagedChildren => "managed-children",
            Self::WebWorkers => "web-workers", Self::MountedDirectories => "mounted-directories",
            Self::Fetch => "fetch", Self::NativeCode => "native-code", Self::SharedMemory => "shared-memory",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub backend: BackendKind,
    pub protocol_version: u16,
    features: BTreeSet<Feature>,
}
impl Capabilities {
    pub fn for_backend(backend: BackendKind) -> Self {
        let features = match backend {
            BackendKind::Fake => BTreeSet::new(),
            BackendKind::Deno => [Feature::JavaScriptEsm, Feature::TypeScript,
                Feature::JsonCalls, Feature::PreparedPackages, Feature::IndependentIsolates,
                Feature::DeadlineCancellation].into_iter().collect(),
        };
        Self { backend, protocol_version: 1, features }
    }
    pub fn supports(&self, feature: Feature) -> bool { self.features.contains(&feature) }
    pub fn features(&self) -> impl Iterator<Item = Feature> + '_ { self.features.iter().copied() }
    /// The facade adds only features whose mediation layer is actually installed.
    pub fn with_feature(mut self, feature: Feature) -> Self { self.features.insert(feature); self }
    pub fn require(&self, requirements: &PluginRequirements) -> Result<()> {
        for feature in &requirements.features {
            if !self.supports(*feature) {
                return Err(Error::UnsupportedFeature { feature: feature.as_str().into() });
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginRequirements { pub features: BTreeSet<Feature> }
impl PluginRequirements {
    pub fn new(features: impl IntoIterator<Item = Feature>) -> Self { Self { features: features.into_iter().collect() } }
}
/// Backend-private metric snapshot. Unknown quantities remain `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeMetrics {
    pub completed_invocations: Option<u64>,
    pub heap_used_bytes: Option<usize>,
    pub heap_limit_bytes: Option<usize>,
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
    fn web_workers_are_not_enabled_yet() {
        for backend in [BackendKind::Fake, BackendKind::Deno] {
            let capabilities = Capabilities::for_backend(backend);
            assert!(!capabilities.supports(Feature::WebWorkers));
            assert!(capabilities
                .require(&PluginRequirements::new([Feature::WebWorkers]))
                .is_err());
        }
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
            Feature::SharedMemory,
            Feature::NativeCode,
        ] {
            assert!(!capabilities.supports(gated), "{gated:?} must be opt-in");
        }
        // `with_feature` is the only path that adds a mediated feature.
        let wired = capabilities.with_feature(Feature::VirtualEnvironment);
        assert!(wired.supports(Feature::VirtualEnvironment));
    }
}
