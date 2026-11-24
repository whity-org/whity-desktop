//! Plugins on the device (ADR 0001, decisions 2b and 3a).
//!
//! Only signed plugins run on the edge. [`trust`] checks Whity's attestation
//! for an exact package and the signed revocation list, against public keys
//! pinned in this crate: no setting, file, server or store can add a trust
//! root, and the pinned list is empty until the owner supplies a key in a
//! reviewed change. The format is whity-core's
//! (`src/Core/Plugin/Trust/PluginTrust.php`), so one signed document verifies
//! on the server and on the device alike.
//!
//! [`installer`] takes a package's bytes, checks them against the attestation,
//! extracts them with the same guards as core's `PluginInstaller`, and commits
//! atomically. [`reconcile`] makes what is installed match what a
//! [`source::PluginSource`] says the workspace has. The source that downloads
//! from the Whity ID store is not here yet: that store does not sign packages
//! today, and its endpoints are not invented here.

pub mod installer;
pub mod reconcile;
pub mod source;
pub mod trust;

pub use installer::{InstalledPlugin, Installer};
pub use reconcile::{Outcome, Plan, Reconciler};
pub use source::{DesiredPlugin, PluginSource};
pub use trust::{Check, Reason, Trust, TrustedKeys};
