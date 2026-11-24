//! Where plugins come from: what the workspace says this device should have,
//! and the bytes of a package.
//!
//! The store that will serve signed packages is the Whity ID store module
//! (owner decision of 2026-11-11: store.whity.dev, its own Ed25519 key, packages
//! addressed by sha256). It does not exist yet and signs nothing, so there is
//! no implementation of this trait for it here and none of its endpoints are
//! guessed. Until it lands, a source can only come from whity-core's own
//! installed-plugin list and package endpoints, wired in with device
//! enrolment (ADR 0001 build plan, PR 6).

use serde::{Deserialize, Serialize};

/// One plugin the workspace wants on this device, exactly: version, package
/// hash, and Whity's attestation for that package. Without the attestation
/// the device installs nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesiredPlugin {
    pub name: String,
    pub version: String,
    /// Lower-case hex sha256 of the package zip.
    pub sha256: String,
    /// The signed `whity.attestation/1` envelope, when the source has one.
    pub attestation: Option<String>,
}

pub trait PluginSource {
    /// What should be installed, from the workspace's point of view.
    fn desired(&self) -> Result<Vec<DesiredPlugin>, String>;
    /// The package zip for one version, at most `max_bytes`.
    fn download(&self, name: &str, version: &str, max_bytes: u64) -> Result<Vec<u8>, String>;
}

/// A source with fixed answers, for tests and for a device that was handed
/// packages by other means.
#[derive(Default)]
pub struct StaticSource {
    pub desired: Vec<DesiredPlugin>,
    pub packages: Vec<((String, String), Vec<u8>)>,
}

impl PluginSource for StaticSource {
    fn desired(&self) -> Result<Vec<DesiredPlugin>, String> {
        Ok(self.desired.clone())
    }

    fn download(&self, name: &str, version: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        let bytes = self
            .packages
            .iter()
            .find(|((n, v), _)| n == name && v == version)
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| format!("no package for {name} {version}"))?;
        if bytes.len() as u64 > max_bytes {
            return Err(format!(
                "the plugin package exceeds the maximum allowed size ({max_bytes} bytes)"
            ));
        }
        Ok(bytes)
    }
}
