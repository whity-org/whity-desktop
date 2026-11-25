//! The plugins this computer runs, kept the same as the workspace's (ADR 0001,
//! decisions 2b, 3a and 4; whity-core WHIT-1420).
//!
//! `CoreSource` reads the workspace's core with the device's access token:
//! which plugins the workspace has turned on, which of them have a packaged
//! desktop release, each release's hash and Whity's attestation, the signed
//! revocation list the instance accepted, and the package bytes. The
//! `whity-plugins` crate does the rest: the attestation verifies against the
//! keys pinned in the app or nothing installs, packages are extracted under
//! core's guards, and the installed set converges on the desired one. When
//! anything changed, the PHP host reloads so it sees the new plugins.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;
use whity_plugins::{DesiredPlugin, PluginSource};

/// The workspace's core as a plugin source. Blocking: the reconcile runs on a
/// worker thread, never on the UI's.
pub struct CoreSource {
    client: reqwest::blocking::Client,
    origin: Url,
    token: String,
}

impl CoreSource {
    pub fn new(origin: Url, token: String) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("whity-desktop/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            origin,
            token,
        })
    }

    fn get(&self, path: &str) -> Result<reqwest::blocking::Response, String> {
        let url = self.origin.join(path).map_err(|e| e.to_string())?;
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header("X-Requested-With", "XMLHttpRequest")
            .send()
            .map_err(|e| e.to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("core answered {} for {path}", status.as_u16()));
        }
        Ok(response)
    }

    fn get_json(&self, path: &str) -> Result<Value, String> {
        self.get(path)?
            .json()
            .map_err(|e| format!("core gave no JSON for {path}: {e}"))
    }

    /// The signed revocation list the instance accepted, as its envelope.
    pub fn revocations(&self) -> Result<Option<String>, String> {
        let body = self.get_json("/api/v1/desktop-plugins/revocations")?;
        Ok(body
            .get("data")
            .and_then(|d| d.get("envelope"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }
}

/// The plugins the workspace has on, by name, from `GET /api/v1/plugins`.
fn enabled_names(list: &Value) -> Vec<String> {
    list.get("data")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|r| r.get("enabled").and_then(Value::as_bool) == Some(true))
                .filter_map(|r| r.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The latest packaged release of each plugin, from `GET /api/v1/desktop-plugins`.
fn latest_releases(catalogue: &Value) -> Vec<DesiredPlugin> {
    catalogue
        .get("data")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name")?.as_str()?;
                    let latest = entry.get("latestVersion")?.as_str()?;
                    let version = entry
                        .get("versions")?
                        .as_array()?
                        .iter()
                        .find(|v| v.get("version").and_then(Value::as_str) == Some(latest))?;
                    Some(DesiredPlugin {
                        name: name.to_string(),
                        version: latest.to_string(),
                        sha256: version.get("sha256")?.as_str()?.to_lowercase(),
                        attestation: version
                            .get("attestation")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What this computer should run: the releases of the plugins the workspace
/// has on. A plugin that is on but has no desktop release is left out, as is
/// a release for a plugin the workspace has off.
pub fn desired_from(plugins: &Value, catalogue: &Value) -> Vec<DesiredPlugin> {
    let on = enabled_names(plugins);
    latest_releases(catalogue)
        .into_iter()
        .filter(|release| on.iter().any(|name| name == &release.name))
        .collect()
}

impl PluginSource for CoreSource {
    fn desired(&self) -> Result<Vec<DesiredPlugin>, String> {
        let plugins = self.get_json("/api/v1/plugins")?;
        let catalogue = self.get_json("/api/v1/desktop-plugins")?;
        Ok(desired_from(&plugins, &catalogue))
    }

    fn download(&self, name: &str, version: &str, max_bytes: u64) -> Result<Vec<u8>, String> {
        let safe = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        };
        if !safe(name) || !safe(version) {
            return Err("not a plugin name and version".into());
        }
        let response = self.get(&format!(
            "/api/v1/desktop-plugins/{name}/versions/{version}/download"
        ))?;
        if response.content_length().is_some_and(|len| len > max_bytes) {
            return Err(format!(
                "the plugin package exceeds the maximum allowed size ({max_bytes} bytes)"
            ));
        }
        let bytes = response.bytes().map_err(|e| e.to_string())?;
        if bytes.len() as u64 > max_bytes {
            return Err(format!(
                "the plugin package exceeds the maximum allowed size ({max_bytes} bytes)"
            ));
        }
        Ok(bytes.to_vec())
    }
}

/// What the last reconcile did for one workspace, for the row in the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub connection: String,
    /// Seconds since the Unix epoch.
    pub at: i64,
    pub installed: usize,
    pub changed: Vec<String>,
    pub failed: Vec<SyncFailure>,
    /// Set when the reconcile itself could not run (no token, core unreachable).
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncFailure {
    pub name: String,
    pub version: String,
    /// `checksum`, `not_verified:<reason>`, `package` or `io`.
    pub reason: String,
}

/// The last report per connection, in memory.
#[derive(Default)]
pub struct SyncReports(pub Mutex<HashMap<String, SyncReport>>);

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn report_of(
    connection: &str,
    outcome: &whity_plugins::Outcome,
    installed: usize,
) -> SyncReport {
    let mut changed: Vec<String> = outcome
        .installed
        .iter()
        .chain(outcome.updated.iter())
        .map(|p| format!("{} {}", p.name, p.version))
        .collect();
    changed.extend(outcome.removed.iter().map(|name| format!("{name} removed")));
    SyncReport {
        connection: connection.to_string(),
        at: now_secs(),
        installed,
        changed,
        failed: outcome
            .failed
            .iter()
            .map(|f| SyncFailure {
                name: f.name.clone(),
                version: f.version.clone(),
                reason: match &f.error {
                    whity_plugins::installer::InstallError::Checksum => "checksum".into(),
                    whity_plugins::installer::InstallError::NotVerified { reason } => {
                        format!(
                            "not_verified:{}",
                            serde_json::to_value(reason)
                                .ok()
                                .and_then(|v| v.as_str().map(str::to_string))
                                .unwrap_or_default()
                        )
                    }
                    whity_plugins::installer::InstallError::Package { .. } => "package".into(),
                    whity_plugins::installer::InstallError::Io { .. } => "io".into(),
                },
            })
            .collect(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Read as _;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn desired_is_the_latest_release_of_each_plugin_the_workspace_has_on() {
        let plugins = json!({ "data": [
            { "name": "Bakery", "enabled": true },
            { "name": "Clinic", "enabled": false },
            { "name": "NoRelease", "enabled": true },
        ]});
        let catalogue = json!({ "data": [
            { "name": "Bakery", "latestVersion": "1.2.0", "versions": [
                { "version": "1.2.0", "sha256": "ABCD", "attestation": "env-1.2" },
                { "version": "1.1.0", "sha256": "0000", "attestation": null },
            ]},
            { "name": "Clinic", "latestVersion": "3.0.0", "versions": [{ "version": "3.0.0", "sha256": "ff", "attestation": null }] },
            { "name": "OffCatalogue", "latestVersion": "1.0.0", "versions": [{ "version": "1.0.0", "sha256": "ee", "attestation": "x" }] },
        ]});
        assert_eq!(
            desired_from(&plugins, &catalogue),
            vec![DesiredPlugin {
                name: "Bakery".into(),
                version: "1.2.0".into(),
                sha256: "abcd".into(),
                attestation: Some("env-1.2".into())
            }]
        );
    }

    type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

    fn core() -> (String, Seen) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let origin = format!(
            "http://127.0.0.1:{}",
            server.server_addr().to_ip().unwrap().port()
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let auth = request
                    .headers()
                    .iter()
                    .find(|h| {
                        h.field
                            .as_str()
                            .as_str()
                            .eq_ignore_ascii_case("Authorization")
                    })
                    .map(|h| h.value.as_str().to_string());
                let url = request.url().to_string();
                s.lock().unwrap().push((url.clone(), auth.clone()));
                if auth.as_deref() != Some("Bearer at-dev") {
                    let _ = request.respond(
                        tiny_http::Response::from_string("{\"error\":\"Unauthenticated\"}")
                            .with_status_code(401),
                    );
                    continue;
                }
                let (status, body, zip) = match url.as_str() {
                    "/api/v1/plugins" => (200, json!({ "data": [{ "name": "Bakery", "enabled": true }] }).to_string(), false),
                    "/api/v1/desktop-plugins" => (200, json!({ "data": [{ "name": "Bakery", "latestVersion": "1.0.0", "versions": [{ "version": "1.0.0", "sha256": "aa", "attestation": "env" }] }] }).to_string(), false),
                    "/api/v1/desktop-plugins/revocations" => (200, json!({ "data": { "sequence": 1, "issuedAt": 1, "expiresAt": 2, "envelope": "{\"key\":\"k\"}" } }).to_string(), false),
                    "/api/v1/desktop-plugins/Bakery/versions/1.0.0/download" => (200, "PK\u{3}\u{4}fake".to_string(), true),
                    _ => (404, "{}".to_string(), false),
                };
                let content = tiny_http::Header::from_bytes(
                    "Content-Type",
                    if zip {
                        "application/zip"
                    } else {
                        "application/json"
                    },
                )
                .unwrap();
                let _ = request.respond(
                    tiny_http::Response::from_string(body)
                        .with_status_code(status)
                        .with_header(content),
                );
            }
        });
        (origin, seen)
    }

    #[test]
    fn the_core_source_reads_with_the_device_token_and_caps_downloads() {
        let (origin, seen) = core();
        let source = CoreSource::new(Url::parse(&origin).unwrap(), "at-dev".into()).unwrap();
        let desired = source.desired().unwrap();
        assert_eq!(desired.len(), 1);
        assert_eq!(desired[0].attestation.as_deref(), Some("env"));
        assert_eq!(
            source.revocations().unwrap().as_deref(),
            Some("{\"key\":\"k\"}")
        );
        let bytes = source.download("Bakery", "1.0.0", 1024).unwrap();
        assert!(bytes.starts_with(b"PK"));
        assert!(
            source.download("Bakery", "1.0.0", 3).is_err(),
            "over the cap"
        );
        assert!(source.download("../x", "1.0.0", 1024).is_err());
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .all(|(_, auth)| auth.as_deref() == Some("Bearer at-dev")));

        let wrong = CoreSource::new(Url::parse(&origin).unwrap(), "nope".into()).unwrap();
        assert!(wrong.desired().unwrap_err().contains("401"));
        let _ = std::io::empty().read(&mut []);
    }

    #[test]
    fn a_report_names_what_changed_and_why_the_rest_failed() {
        use whity_plugins::installer::{InstallError, InstalledPlugin};
        let outcome = whity_plugins::Outcome {
            installed: vec![InstalledPlugin {
                name: "A".into(),
                version: "1".into(),
                sha256: "x".into(),
                attestation: "e".into(),
            }],
            updated: vec![],
            removed: vec!["Old".into()],
            failed: vec![whity_plugins::reconcile::Failure {
                name: "B".into(),
                version: "2".into(),
                error: InstallError::NotVerified {
                    reason: whity_plugins::Reason::NoKeys,
                },
            }],
        };
        let report = report_of("ws-1", &outcome, 1);
        assert_eq!(
            report.changed,
            vec!["A 1".to_string(), "Old removed".to_string()]
        );
        assert_eq!(report.failed[0].reason, "not_verified:no_keys");
        assert_eq!(report.installed, 1);
    }
}
