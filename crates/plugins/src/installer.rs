//! Verify, extract and commit a plugin package on the device. The guards are
//! core's `PluginInstaller`: a filesystem-safe name, exactly one top-level
//! directory named like the plugin, zip-slip and zip-bomb limits, and an
//! atomic stage-then-rename commit. Before any of that, the bytes must match
//! the sha256 the source named and carry a verified attestation for exactly
//! that hash (`trust.rs`): the device has no "allow unsigned" override.

use crate::trust::{Check, Reason, Trust};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use zip::ZipArchive;

/// Same cap as core's `PluginInstaller::MAX_UPLOAD_BYTES`.
pub const MAX_PACKAGE_BYTES: u64 = 33_554_432; // 32 MiB
const MAX_ZIP_ENTRIES: usize = 2_000;
const MAX_ENTRY_UNCOMPRESSED_BYTES: u64 = 16_777_216; // 16 MiB
const MAX_TOTAL_UNCOMPRESSED_BYTES: u64 = 67_108_864; // 64 MiB
const MAX_COMPRESSION_RATIO: u64 = 200;
const RATIO_MIN_COMPRESSED_BYTES: u64 = 256;

/// Written inside the plugin's directory with the commit: what is installed
/// and the attestation it was installed under, so a later revocation list can
/// be checked against it. The PHP loader skips entries starting with `.`.
const PACKAGE_MARKER_FILE: &str = ".whity-package.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledPlugin {
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub attestation: String,
}

/// Why a package was not installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InstallError {
    /// The bytes are not the package the source named.
    Checksum,
    /// The attestation did not pass, with core's reason.
    NotVerified { reason: Reason },
    /// The package itself is unsafe or malformed.
    Package { message: String },
    /// The disk.
    Io { message: String },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Checksum => write!(f, "the package failed checksum verification"),
            InstallError::NotVerified { reason } => {
                write!(f, "the package is not verified: {reason:?}")
            }
            InstallError::Package { message } | InstallError::Io { message } => {
                write!(f, "{message}")
            }
        }
    }
}

pub struct Installer<'a> {
    root: PathBuf,
    trust: &'a Trust,
}

impl<'a> Installer<'a> {
    /// `root` is the writable plugins directory the PHP host scans
    /// (`WHITY_DOWNLOADED_PLUGINS_ROOT`).
    pub fn new(root: &Path, trust: &'a Trust) -> Self {
        Self {
            root: root.to_path_buf(),
            trust,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn trust(&self) -> &'a Trust {
        self.trust
    }

    /// Install or replace `name` with this package. The commit is atomic: a
    /// current install is moved aside and put back if the new one fails to
    /// land, so a failed update never leaves the plugin absent.
    pub fn install(
        &self,
        name: &str,
        version: &str,
        bytes: &[u8],
        expected_sha256: &str,
        attestation: &str,
    ) -> Result<InstalledPlugin, InstallError> {
        validate_name(name)?;
        validate_segment(version).map_err(|_| InstallError::Package {
            message: "invalid plugin version".into(),
        })?;
        let actual = sha256_hex(bytes);
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            return Err(InstallError::Checksum);
        }
        let Check { ok, reason } =
            self.trust
                .check_attestation(attestation, name, version, &actual);
        if !ok {
            return Err(InstallError::NotVerified { reason });
        }

        std::fs::create_dir_all(&self.root).map_err(io)?;
        let stage_root = self.root.join(format!(".tmp-{}", random_suffix()));
        std::fs::create_dir_all(&stage_root).map_err(io)?;
        let staged = (|| -> Result<PathBuf, InstallError> {
            safe_extract(bytes, &stage_root)?;
            let top = single_top_level_dir(&stage_root)?;
            if top != name {
                return Err(InstallError::Package {
                    message: format!("the package's top-level directory ('{top}') is not the plugin name ('{name}')"),
                });
            }
            let staged = stage_root.join(&top);
            let record = InstalledPlugin {
                name: name.into(),
                version: version.into(),
                sha256: actual.clone(),
                attestation: attestation.into(),
            };
            std::fs::write(
                staged.join(PACKAGE_MARKER_FILE),
                serde_json::to_vec_pretty(&record).map_err(|e| io_msg(e.to_string()))?,
            )
            .map_err(io)?;
            Ok(staged)
        })();
        let staged = match staged {
            Ok(staged) => staged,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&stage_root);
                return Err(e);
            }
        };

        let dest = self.root.join(name);
        let result = (|| -> Result<(), InstallError> {
            if !dest.exists() {
                return std::fs::rename(&staged, &dest).map_err(io);
            }
            let trash = self.root.join(format!(".trash-{}", random_suffix()));
            std::fs::rename(&dest, &trash).map_err(io)?;
            match std::fs::rename(&staged, &dest) {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(&trash);
                    Ok(())
                }
                Err(e) => {
                    let _ = std::fs::rename(&trash, &dest);
                    Err(io(e))
                }
            }
        })();
        let _ = std::fs::remove_dir_all(&stage_root);
        result?;
        Ok(InstalledPlugin {
            name: name.into(),
            version: version.into(),
            sha256: actual,
            attestation: attestation.into(),
        })
    }

    /// Remove `name`. A no-op when it is not there.
    pub fn uninstall(&self, name: &str) -> Result<(), InstallError> {
        validate_name(name)?;
        let dest = self.root.join(name);
        if !dest.exists() {
            return Ok(());
        }
        std::fs::remove_dir_all(&dest).map_err(io)
    }

    /// What is installed, from each plugin's marker. A directory without a
    /// readable marker is listed with empty version and hash, so the reconcile
    /// replaces it with a known package.
    pub fn installed(&self) -> Result<Vec<InstalledPlugin>, InstallError> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(e)),
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let record = std::fs::read(entry.path().join(PACKAGE_MARKER_FILE))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<InstalledPlugin>(&bytes).ok())
                .filter(|r| r.name == name);
            out.push(record.unwrap_or(InstalledPlugin {
                name,
                version: String::new(),
                sha256: String::new(),
                attestation: String::new(),
            }));
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn io(e: std::io::Error) -> InstallError {
    InstallError::Io {
        message: e.to_string(),
    }
}

fn io_msg(message: String) -> InstallError {
    InstallError::Io { message }
}

fn random_suffix() -> String {
    use rand::RngCore as _;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Core's `PluginInstaller::NAME_PATTERN`: the name becomes a directory.
fn validate_name(name: &str) -> Result<(), InstallError> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(InstallError::Package {
            message: "invalid plugin name: must match ^[A-Za-z0-9_-]+$".into(),
        })
    }
}

fn validate_segment(s: &str) -> Result<(), ()> {
    let safe = !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains(['/', '\\'])
        && s.chars().all(|c| c.is_ascii_graphic());
    if safe {
        Ok(())
    } else {
        Err(())
    }
}

fn package(message: impl Into<String>) -> InstallError {
    InstallError::Package {
        message: message.into(),
    }
}

/// Two passes, as core does: inspect every entry and write nothing, then
/// write each under the canonical stage root.
fn safe_extract(bytes: &[u8], stage_root: &Path) -> Result<(), InstallError> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| package(format!("the package could not be opened as a zip: {e}")))?;
    let count = archive.len();
    if count == 0 {
        return Err(package("the package is empty"));
    }
    if count > MAX_ZIP_ENTRIES {
        return Err(package("the package contains too many entries"));
    }
    let mut total_uncompressed = 0u64;
    let mut total_compressed = 0u64;
    for i in 0..count {
        let entry = archive
            .by_index(i)
            .map_err(|e| package(format!("a package entry could not be read: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| package(format!("a package entry has an unreadable name: {e}")))?
            .to_string();
        assert_safe_entry_name(&name)?;
        if name.ends_with('/') {
            continue;
        }
        if entry.size() > MAX_ENTRY_UNCOMPRESSED_BYTES {
            return Err(package("a package entry is too large when uncompressed"));
        }
        total_uncompressed += entry.size();
        total_compressed += entry.compressed_size();
        if total_uncompressed > MAX_TOTAL_UNCOMPRESSED_BYTES {
            return Err(package("the package is too large when fully uncompressed"));
        }
    }
    if total_compressed >= RATIO_MIN_COMPRESSED_BYTES
        && total_uncompressed / total_compressed > MAX_COMPRESSION_RATIO
    {
        return Err(package(
            "the package's compression ratio exceeds the safe limit",
        ));
    }

    let anchor = std::fs::canonicalize(stage_root).map_err(io)?;
    for i in 0..count {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| package(format!("a package entry could not be read: {e}")))?;
        let name = entry
            .name()
            .map_err(|e| package(format!("a package entry has an unreadable name: {e}")))?
            .to_string();
        let target = resolve_entry_target(&anchor, &name)?;
        if name.ends_with('/') {
            std::fs::create_dir_all(&target).map_err(io)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let mut contents = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut contents)
            .map_err(|e| package(format!("a package entry could not be extracted: {e}")))?;
        std::fs::write(&target, contents).map_err(io)?;
    }
    Ok(())
}

fn assert_safe_entry_name(name: &str) -> Result<(), InstallError> {
    if name.is_empty() {
        return Err(package("the package contains an entry with an empty name"));
    }
    if name.starts_with('/') || name.contains('\\') {
        return Err(package(
            "the package contains an unsafe (absolute) entry path",
        ));
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(package(
            "the package contains an unsafe (drive-letter) entry path",
        ));
    }
    if name.split('/').any(|segment| segment == "..") {
        return Err(package(
            "the package contains an unsafe (traversal) entry path",
        ));
    }
    Ok(())
}

fn resolve_entry_target(anchor: &Path, name: &str) -> Result<PathBuf, InstallError> {
    let mut resolved = anchor.to_path_buf();
    for segment in name.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        resolved.push(segment);
    }
    if !resolved.starts_with(anchor) {
        return Err(package(
            "a package entry resolves outside the extraction root",
        ));
    }
    Ok(resolved)
}

fn single_top_level_dir(stage_root: &Path) -> Result<String, InstallError> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(stage_root).map_err(io)?.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            dirs.push(entry.file_name().to_string_lossy().into_owned());
        } else {
            return Err(package(
                "the package has a file outside its top-level directory",
            ));
        }
    }
    match dirs.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(package("the package has no top-level directory")),
        _ => Err(package("the package has more than one top-level directory")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::trust::signing::{public_key_base64, sign};
    use crate::trust::{TrustedKeys, ATTESTATION, REVOCATIONS};
    use ed25519_dalek::SigningKey;
    use serde_json::json;
    use std::io::Write as _;

    pub const NOW: i64 = 1_800_000_000;

    pub fn signer() -> SigningKey {
        SigningKey::from_bytes(&[3; 32])
    }

    pub fn trust_with_fresh_list(signer: &SigningKey) -> Trust {
        let trust =
            Trust::new(TrustedKeys::with_keys(vec![public_key_base64(signer)])).with_clock(|| NOW);
        let list = sign(
            REVOCATIONS,
            json!({ "sequence": 1, "issued_at": NOW - 60, "expires_at": NOW + 86_400, "revoked": [] }).as_object().unwrap(),
            signer,
        );
        trust.apply_revocation_list(&list).unwrap();
        trust
    }

    pub fn attest(signer: &SigningKey, name: &str, version: &str, sha: &str) -> String {
        sign(
            ATTESTATION,
            json!({ "plugin": name, "version": version, "sha256": sha, "expires_at": NOW + 86_400 }).as_object().unwrap(),
            signer,
        )
    }

    /// A zip with the given `path -> contents` entries.
    pub fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (path, contents) in entries {
                if path.ends_with('/') {
                    writer.add_directory(*path, options).unwrap();
                } else {
                    writer.start_file(*path, options).unwrap();
                    writer.write_all(contents).unwrap();
                }
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    pub fn plugin_zip(name: &str, version: &str) -> Vec<u8> {
        let php = format!("<?php\nnamespace {name};\nfinal class Plugin {{ public const VERSION = '{version}'; }}\n");
        zip_of(&[
            (&format!("{name}/"), b""),
            (&format!("{name}/Plugin.php"), php.as_bytes()),
        ])
    }

    #[test]
    fn a_verified_package_installs_and_is_listed_with_its_attestation() {
        let signer = signer();
        let trust = trust_with_fresh_list(&signer);
        let dir = tempfile::tempdir().unwrap();
        let installer = Installer::new(dir.path(), &trust);
        let bytes = plugin_zip("Probe", "1.0.0");
        let sha = sha256_hex(&bytes);
        let attestation = attest(&signer, "Probe", "1.0.0", &sha);
        let installed = installer
            .install("Probe", "1.0.0", &bytes, &sha, &attestation)
            .unwrap();
        assert_eq!(installed.version, "1.0.0");
        assert!(dir.path().join("Probe/Plugin.php").is_file());
        assert_eq!(installer.installed().unwrap(), vec![installed]);
        assert!(std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with(".tmp")));
    }

    #[test]
    fn an_update_replaces_the_old_version_and_nothing_is_lost_on_the_way() {
        let signer = signer();
        let trust = trust_with_fresh_list(&signer);
        let dir = tempfile::tempdir().unwrap();
        let installer = Installer::new(dir.path(), &trust);
        for version in ["1.0.0", "1.1.0"] {
            let bytes = plugin_zip("Probe", version);
            let sha = sha256_hex(&bytes);
            installer
                .install(
                    "Probe",
                    version,
                    &bytes,
                    &sha,
                    &attest(&signer, "Probe", version, &sha),
                )
                .unwrap();
        }
        let php = std::fs::read_to_string(dir.path().join("Probe/Plugin.php")).unwrap();
        assert!(php.contains("1.1.0"));
        assert_eq!(installer.installed().unwrap()[0].version, "1.1.0");
        installer.uninstall("Probe").unwrap();
        installer.uninstall("Probe").unwrap();
        assert!(installer.installed().unwrap().is_empty());
    }

    #[test]
    fn the_wrong_bytes_or_no_attestation_install_nothing() {
        let signer = signer();
        let trust = trust_with_fresh_list(&signer);
        let dir = tempfile::tempdir().unwrap();
        let installer = Installer::new(dir.path(), &trust);
        let bytes = plugin_zip("Probe", "1.0.0");
        let sha = sha256_hex(&bytes);
        let attestation = attest(&signer, "Probe", "1.0.0", &sha);

        assert_eq!(
            installer.install("Probe", "1.0.0", &bytes, &"0".repeat(64), &attestation),
            Err(InstallError::Checksum)
        );
        let other = attest(&signer, "Probe", "1.0.1", &sha);
        assert_eq!(
            installer.install("Probe", "1.0.0", &bytes, &sha, &other),
            Err(InstallError::NotVerified {
                reason: Reason::Mismatch
            })
        );
        assert_eq!(
            installer.install("Probe", "1.0.0", &bytes, &sha, "not an envelope"),
            Err(InstallError::NotVerified {
                reason: Reason::BadSignature
            })
        );
        let no_keys = Trust::new(TrustedKeys::pinned()).with_clock(|| NOW);
        let locked = Installer::new(dir.path(), &no_keys);
        assert_eq!(
            locked.install("Probe", "1.0.0", &bytes, &sha, &attestation),
            Err(InstallError::NotVerified {
                reason: Reason::NoKeys
            })
        );
        assert!(!dir.path().join("Probe").exists());
    }

    #[test]
    fn unsafe_packages_are_refused_before_anything_lands() {
        let signer = signer();
        let trust = trust_with_fresh_list(&signer);
        let dir = tempfile::tempdir().unwrap();
        let installer = Installer::new(dir.path(), &trust);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            (
                "traversal",
                zip_of(&[("Probe/", b""), ("Probe/../evil.php", b"x")]),
            ),
            ("absolute", zip_of(&[("/Probe/a.php", b"x")])),
            (
                "two tops",
                zip_of(&[("Probe/a.php", b"x"), ("Other/b.php", b"y")]),
            ),
            ("wrong top", zip_of(&[("Other/a.php", b"x")])),
            (
                "loose file",
                zip_of(&[("Probe/a.php", b"x"), ("README", b"y")]),
            ),
            ("not a zip", b"hello".to_vec()),
        ];
        for (label, bytes) in cases {
            let sha = sha256_hex(&bytes);
            let attestation = attest(&signer, "Probe", "1.0.0", &sha);
            let result = installer.install("Probe", "1.0.0", &bytes, &sha, &attestation);
            assert!(
                matches!(result, Err(InstallError::Package { .. })),
                "{label}: {result:?}"
            );
            assert!(
                !dir.path().join("Probe").exists(),
                "{label} left files behind"
            );
        }
        assert!(matches!(
            installer.install("../x", "1", b"", "", ""),
            Err(InstallError::Package { .. })
        ));
    }
}
