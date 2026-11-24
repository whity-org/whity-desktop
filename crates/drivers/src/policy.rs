//! What the person at this device allows each plugin to do with its
//! hardware, in Whity's three words: Off, Ask first, Allowed.
//!
//! The bridge asks a [`Policy`] before every call. [`StoredPolicy`] is the
//! real one: one setting per plugin and capability, kept in a JSON file under
//! the app's data directory and changed from the settings screen. Anything
//! not set falls back to the floor in the ADR: printing is Allowed, everything
//! else is Off. `Ask first` turns the call into a proposal (see
//! [`crate::proposals`]) that runs only when the person approves it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Off,
    AskFirst,
    Allowed,
}

pub trait Policy: Send + Sync {
    fn decide(&self, plugin: &str, capability: &str) -> Decision;
}

/// The floor: printing is Allowed for every plugin that declares it,
/// everything else is Off until the person turns it on.
#[derive(Default)]
pub struct DefaultPolicy;

impl DefaultPolicy {
    pub fn floor(capability: &str) -> Decision {
        if capability.starts_with("printer.") {
            Decision::Allowed
        } else {
            Decision::Off
        }
    }
}

impl Policy for DefaultPolicy {
    fn decide(&self, _plugin: &str, capability: &str) -> Decision {
        Self::floor(capability)
    }
}

/// One row of the settings screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Setting {
    pub plugin: String,
    pub capability: String,
    pub decision: Decision,
    /// True when nothing was set and the decision is the floor.
    pub is_default: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Settings {
    /// plugin -> capability -> decision
    #[serde(default)]
    plugins: BTreeMap<String, BTreeMap<String, Decision>>,
}

/// The device's settings, persisted. Readers never block on the file: it is
/// read once at start and written on every change.
pub struct StoredPolicy {
    path: PathBuf,
    settings: Mutex<Settings>,
}

impl StoredPolicy {
    /// Load from `path`; a missing or unreadable file means "nothing set",
    /// and an unreadable one is logged, not trusted.
    pub fn load(path: &Path) -> Self {
        let settings = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                eprintln!("[drivers] hardware policy at {} is not readable ({e}); starting from the floor", path.display());
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        Self {
            path: path.to_path_buf(),
            settings: Mutex::new(settings),
        }
    }

    /// Set one decision and write the file. `None` clears the setting back to
    /// the floor.
    pub fn set(
        &self,
        plugin: &str,
        capability: &str,
        decision: Option<Decision>,
    ) -> Result<(), String> {
        if plugin.is_empty() || !is_capability(capability) {
            return Err(format!(
                "not a plugin and capability: {plugin} / {capability}"
            ));
        }
        let mut settings = self.settings.lock().unwrap();
        match decision {
            Some(d) => {
                settings
                    .plugins
                    .entry(plugin.to_string())
                    .or_default()
                    .insert(capability.to_string(), d);
            }
            None => {
                if let Some(caps) = settings.plugins.get_mut(plugin) {
                    caps.remove(capability);
                    if caps.is_empty() {
                        settings.plugins.remove(plugin);
                    }
                }
            }
        }
        let json = serde_json::to_vec_pretty(&*settings).map_err(|e| e.to_string())?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .map_err(|e| format!("failed to write the hardware policy: {e}"))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("failed to replace the hardware policy: {e}"))?;
        Ok(())
    }

    /// The rows for the plugins and capabilities the bridge currently knows
    /// (`grants`), each with its decision and whether it is the floor.
    pub fn settings_for(&self, grants: &[(String, Vec<String>)]) -> Vec<Setting> {
        let settings = self.settings.lock().unwrap();
        let mut rows = Vec::new();
        for (plugin, capabilities) in grants {
            for capability in capabilities {
                let set = settings
                    .plugins
                    .get(plugin)
                    .and_then(|caps| caps.get(capability))
                    .copied();
                rows.push(Setting {
                    plugin: plugin.clone(),
                    capability: capability.clone(),
                    decision: set.unwrap_or_else(|| DefaultPolicy::floor(capability)),
                    is_default: set.is_none(),
                });
            }
        }
        rows
    }
}

impl Policy for StoredPolicy {
    fn decide(&self, plugin: &str, capability: &str) -> Decision {
        self.settings
            .lock()
            .unwrap()
            .plugins
            .get(plugin)
            .and_then(|caps| caps.get(capability))
            .copied()
            .unwrap_or_else(|| DefaultPolicy::floor(capability))
    }
}

/// `<driver>.<operation>`, lower case.
pub fn is_capability(capability: &str) -> bool {
    let Some((driver, op)) = capability.split_once('.') else {
        return false;
    };
    let ok = |s: &str, extra: char| {
        !s.is_empty()
            && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == extra)
    };
    ok(driver, '-') && ok(op, '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("whity-policy-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("hardware-policy.json")
    }

    #[test]
    fn the_floor_allows_printing_and_nothing_else() {
        let policy = DefaultPolicy;
        assert_eq!(policy.decide("Any", "printer.print"), Decision::Allowed);
        assert_eq!(policy.decide("Any", "printer.list"), Decision::Allowed);
        assert_eq!(policy.decide("Any", "serial.open"), Decision::Off);
    }

    #[test]
    fn a_setting_overrides_the_floor_and_survives_a_reload() {
        let path = temp_file("reload");
        let policy = StoredPolicy::load(&path);
        assert_eq!(policy.decide("Probe", "printer.print"), Decision::Allowed);
        policy
            .set("Probe", "printer.print", Some(Decision::AskFirst))
            .unwrap();
        policy
            .set("Probe", "serial.open", Some(Decision::Allowed))
            .unwrap();
        assert_eq!(policy.decide("Probe", "printer.print"), Decision::AskFirst);
        assert_eq!(policy.decide("Probe", "serial.open"), Decision::Allowed);
        assert_eq!(policy.decide("Other", "serial.open"), Decision::Off);

        let again = StoredPolicy::load(&path);
        assert_eq!(again.decide("Probe", "printer.print"), Decision::AskFirst);
        again.set("Probe", "printer.print", None).unwrap();
        assert_eq!(again.decide("Probe", "printer.print"), Decision::Allowed);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rows_follow_the_grants_and_say_what_is_default() {
        let path = temp_file("rows");
        let policy = StoredPolicy::load(&path);
        policy
            .set("Probe", "serial.open", Some(Decision::AskFirst))
            .unwrap();
        let rows = policy.settings_for(&[(
            "Probe".into(),
            vec!["printer.list".into(), "serial.open".into()],
        )]);
        assert_eq!(
            rows,
            vec![
                Setting {
                    plugin: "Probe".into(),
                    capability: "printer.list".into(),
                    decision: Decision::Allowed,
                    is_default: true
                },
                Setting {
                    plugin: "Probe".into(),
                    capability: "serial.open".into(),
                    decision: Decision::AskFirst,
                    is_default: false
                },
            ]
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_bad_name_is_refused_and_a_bad_file_is_not_trusted() {
        let path = temp_file("bad");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        let policy = StoredPolicy::load(&path);
        assert_eq!(policy.decide("Probe", "serial.open"), Decision::Off);
        assert!(policy
            .set("", "printer.print", Some(Decision::Off))
            .is_err());
        assert!(policy
            .set("Probe", "Printer.Print", Some(Decision::Off))
            .is_err());
        assert!(policy.set("Probe", "printer", Some(Decision::Off)).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn capability_names() {
        assert!(is_capability("printer.print"));
        assert!(is_capability("card-reader.read_track2"));
        assert!(!is_capability("printer"));
        assert!(!is_capability(".print"));
        assert!(!is_capability("printer.Print"));
        assert!(!is_capability("printer.print.now"));
    }
}
