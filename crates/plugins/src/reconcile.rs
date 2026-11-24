//! Make what is installed match what the workspace says, exactly: install
//! what is missing, replace what differs in version or hash, remove what the
//! workspace no longer has, and remove what the revocation list kills. A
//! plugin that cannot be verified is not installed and is reported with the
//! reason; it is never installed unverified.

use crate::installer::{InstallError, InstalledPlugin, Installer, MAX_PACKAGE_BYTES};
use crate::source::{DesiredPlugin, PluginSource};
use serde::Serialize;

/// What the reconcile will do, before it does it. Pure, so a screen can show
/// it and tests can check it.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub install: Vec<DesiredPlugin>,
    pub update: Vec<DesiredPlugin>,
    pub remove: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.install.is_empty() && self.update.is_empty() && self.remove.is_empty()
    }
}

pub fn plan(
    desired: &[DesiredPlugin],
    installed: &[InstalledPlugin],
    killed: impl Fn(&str, &str) -> bool,
) -> Plan {
    let mut out = Plan::default();
    for want in desired {
        match installed.iter().find(|have| have.name == want.name) {
            None => out.install.push(want.clone()),
            Some(have)
                if have.version != want.version
                    || !have.sha256.eq_ignore_ascii_case(&want.sha256) =>
            {
                out.update.push(want.clone())
            }
            Some(_) => {}
        }
    }
    for have in installed {
        let wanted = desired.iter().any(|want| want.name == have.name);
        if !wanted || killed(&have.name, &have.version) {
            // A killed version goes even when the workspace still lists it;
            // the reconcile will not reinstall it, since its attestation is revoked.
            if !out.remove.contains(&have.name) {
                out.remove.push(have.name.clone());
            }
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub name: String,
    pub version: String,
    pub error: InstallError,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub installed: Vec<InstalledPlugin>,
    pub updated: Vec<InstalledPlugin>,
    pub removed: Vec<String>,
    pub failed: Vec<Failure>,
}

impl Outcome {
    /// Whether the PHP host must be reloaded to see the change.
    pub fn changed(&self) -> bool {
        !self.installed.is_empty() || !self.updated.is_empty() || !self.removed.is_empty()
    }
}

pub struct Reconciler<'a> {
    installer: Installer<'a>,
}

impl<'a> Reconciler<'a> {
    pub fn new(installer: Installer<'a>) -> Self {
        Self { installer }
    }

    pub fn plan(&self, source: &dyn PluginSource) -> Result<Plan, String> {
        let desired = source.desired()?;
        let installed = self.installer.installed().map_err(|e| e.to_string())?;
        let trust = self.trust();
        Ok(plan(&desired, &installed, |name, version| {
            trust.is_killed(name, version)
        }))
    }

    /// Run the plan. One plugin's failure never stops the others.
    pub fn run(&self, source: &dyn PluginSource) -> Result<Outcome, String> {
        let plan = self.plan(source)?;
        let mut outcome = Outcome::default();
        for name in &plan.remove {
            match self.installer.uninstall(name) {
                Ok(()) => outcome.removed.push(name.clone()),
                Err(error) => outcome.failed.push(Failure {
                    name: name.clone(),
                    version: String::new(),
                    error,
                }),
            }
        }
        for (want, updating) in plan
            .install
            .iter()
            .map(|w| (w, false))
            .chain(plan.update.iter().map(|w| (w, true)))
        {
            if plan.remove.contains(&want.name) {
                continue; // killed: not reinstalled, its attestation is revoked anyway
            }
            match self.fetch_and_install(source, want) {
                Ok(installed) if updating => outcome.updated.push(installed),
                Ok(installed) => outcome.installed.push(installed),
                Err(error) => outcome.failed.push(Failure {
                    name: want.name.clone(),
                    version: want.version.clone(),
                    error,
                }),
            }
        }
        Ok(outcome)
    }

    fn fetch_and_install(
        &self,
        source: &dyn PluginSource,
        want: &DesiredPlugin,
    ) -> Result<InstalledPlugin, InstallError> {
        let Some(attestation) = want.attestation.as_deref() else {
            return Err(InstallError::NotVerified {
                reason: crate::trust::Reason::BadSignature,
            });
        };
        let bytes = source
            .download(&want.name, &want.version, MAX_PACKAGE_BYTES)
            .map_err(|message| InstallError::Io { message })?;
        self.installer
            .install(&want.name, &want.version, &bytes, &want.sha256, attestation)
    }

    /// The installer verifies with this trust; the reconcile asks the same
    /// one about kills.
    fn trust(&self) -> &crate::trust::Trust {
        self.installer.trust()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::sha256_hex;
    use crate::installer::tests::{attest, plugin_zip, signer, trust_with_fresh_list, NOW};
    use crate::source::StaticSource;
    use crate::trust::signing::sign;
    use crate::trust::{Reason, Trust, TrustedKeys, REVOCATIONS};
    use serde_json::json;

    fn have(name: &str, version: &str, sha: &str) -> InstalledPlugin {
        InstalledPlugin {
            name: name.into(),
            version: version.into(),
            sha256: sha.into(),
            attestation: String::new(),
        }
    }

    fn want(name: &str, version: &str, sha: &str) -> DesiredPlugin {
        DesiredPlugin {
            name: name.into(),
            version: version.into(),
            sha256: sha.into(),
            attestation: Some("env".into()),
        }
    }

    #[test]
    fn the_plan_installs_updates_and_removes_to_match_exactly() {
        let desired = vec![
            want("A", "1", "aa"),
            want("B", "2", "bb"),
            want("C", "1", "cc"),
        ];
        let installed = vec![
            have("B", "1", "bb"),
            have("C", "1", "CC"),
            have("D", "1", "dd"),
            have("K", "1", "kk"),
        ];
        let planned = plan(&desired, &installed, |name, _| name == "K");
        assert_eq!(planned.install, vec![want("A", "1", "aa")]);
        assert_eq!(planned.update, vec![want("B", "2", "bb")]);
        assert_eq!(planned.remove, vec!["D".to_string(), "K".to_string()]);
        assert!(plan(&[], &[], |_, _| false).is_empty());
    }

    #[test]
    fn running_installs_what_verifies_removes_the_rest_and_reports_failures() {
        let signer = signer();
        let trust = trust_with_fresh_list(&signer);
        let dir = tempfile::tempdir().unwrap();
        let installer = Installer::new(dir.path(), &trust);
        std::fs::create_dir_all(dir.path().join("Stale")).unwrap();

        let good = plugin_zip("Good", "1.0.0");
        let good_sha = sha256_hex(&good);
        let unsigned = plugin_zip("Unsigned", "1.0.0");
        let unsigned_sha = sha256_hex(&unsigned);
        let wrong = plugin_zip("Wrong", "1.0.0");
        let source = StaticSource {
            desired: vec![
                DesiredPlugin {
                    name: "Good".into(),
                    version: "1.0.0".into(),
                    sha256: good_sha.clone(),
                    attestation: Some(attest(&signer, "Good", "1.0.0", &good_sha)),
                },
                DesiredPlugin {
                    name: "Unsigned".into(),
                    version: "1.0.0".into(),
                    sha256: unsigned_sha.clone(),
                    attestation: None,
                },
                DesiredPlugin {
                    name: "Wrong".into(),
                    version: "1.0.0".into(),
                    sha256: "00".repeat(32),
                    attestation: Some(attest(&signer, "Wrong", "1.0.0", &"00".repeat(32))),
                },
                DesiredPlugin {
                    name: "Missing".into(),
                    version: "1.0.0".into(),
                    sha256: "11".repeat(32),
                    attestation: Some(attest(&signer, "Missing", "1.0.0", &"11".repeat(32))),
                },
            ],
            packages: vec![
                (("Good".into(), "1.0.0".into()), good),
                (("Unsigned".into(), "1.0.0".into()), unsigned),
                (("Wrong".into(), "1.0.0".into()), wrong),
            ],
        };
        let reconciler = Reconciler::new(installer);
        let outcome = reconciler.run(&source).unwrap();
        assert_eq!(
            outcome
                .installed
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Good"]
        );
        assert_eq!(outcome.removed, vec!["Stale".to_string()]);
        let failed: Vec<(&str, &InstallError)> = outcome
            .failed
            .iter()
            .map(|f| (f.name.as_str(), &f.error))
            .collect();
        assert_eq!(failed.len(), 3, "{failed:?}");
        assert!(matches!(
            failed[0],
            (
                "Unsigned",
                InstallError::NotVerified {
                    reason: Reason::BadSignature
                }
            )
        ));
        assert!(matches!(failed[1], ("Wrong", InstallError::Checksum)));
        assert!(matches!(failed[2], ("Missing", InstallError::Io { .. })));
        assert!(outcome.changed());
        assert!(dir.path().join("Good/Plugin.php").is_file());
        assert!(!dir.path().join("Unsigned").exists());

        // Running again changes nothing.
        let again = Reconciler::new(Installer::new(dir.path(), &trust))
            .run(&source)
            .unwrap();
        assert!(again.installed.is_empty() && again.updated.is_empty() && again.removed.is_empty());
    }

    #[test]
    fn a_killed_version_is_removed_and_not_put_back() {
        let signer = signer();
        let trust = trust_with_fresh_list(&signer);
        let dir = tempfile::tempdir().unwrap();
        let bytes = plugin_zip("Bad", "1.0.0");
        let sha = sha256_hex(&bytes);
        let attestation = attest(&signer, "Bad", "1.0.0", &sha);
        Installer::new(dir.path(), &trust)
            .install("Bad", "1.0.0", &bytes, &sha, &attestation)
            .unwrap();

        let kill = sign(
            REVOCATIONS,
            json!({ "sequence": 2, "issued_at": NOW, "expires_at": NOW + 86_400, "revoked": [{ "plugin": "Bad", "version": "1.0.0", "action": "kill", "reason": "security" }] })
                .as_object()
                .unwrap(),
            &signer,
        );
        trust.apply_revocation_list(&kill).unwrap();
        let source = StaticSource {
            desired: vec![DesiredPlugin {
                name: "Bad".into(),
                version: "1.0.0".into(),
                sha256: sha,
                attestation: Some(attestation),
            }],
            packages: vec![(("Bad".into(), "1.0.0".into()), bytes)],
        };
        let outcome = Reconciler::new(Installer::new(dir.path(), &trust))
            .run(&source)
            .unwrap();
        assert_eq!(outcome.removed, vec!["Bad".to_string()]);
        assert!(outcome.installed.is_empty() && outcome.failed.is_empty());
        assert!(!dir.path().join("Bad").exists());

        let _ = Trust::new(TrustedKeys::pinned());
    }
}
