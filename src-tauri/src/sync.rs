//! Plugin data kept in two places (ADR 0001, decision 4): for each enrolled
//! workspace, `crates/sync` relays the resources this computer's plugins
//! declare between the PHP host on loopback and the workspace, with the
//! device token. Rows both ends changed are parked in the relay's store for a
//! person; nothing is overwritten. Runs after the plugin reconcile, on a
//! timer and on Sync now, always on a worker thread.

use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use url::Url;
use whity_sync::{Leg, Store};

/// What the last relay for a workspace did, as the device row shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSyncReport {
    pub connection: String,
    /// Seconds since the Unix epoch.
    pub at: i64,
    /// Resources the loaded plugins declare for syncing.
    pub resources: usize,
    /// Rows carried to the other end this time.
    pub relayed: usize,
    /// Rows waiting for a person because both ends changed them.
    pub parked: usize,
    /// Plain-text reasons for what did not go through this time.
    pub failures: Vec<String>,
    /// Set when the relay itself could not run (no token, no PHP runtime).
    pub error: Option<String>,
}

impl DataSyncReport {
    pub fn failed(connection: &str, error: String) -> Self {
        Self {
            connection: connection.to_string(),
            at: crate::plugins::now_secs(),
            resources: 0,
            relayed: 0,
            parked: 0,
            failures: vec![],
            error: Some(error),
        }
    }
}

/// The last report per connection, in memory.
#[derive(Default)]
pub struct DataSyncReports(pub Mutex<HashMap<String, DataSyncReport>>);

/// The relay's store for one workspace, beside the PHP data. One file per
/// workspace: what each end has for a row is a fact about that pair.
pub fn store_path(data_dir: &Path, connection: &str) -> PathBuf {
    let safe: String = connection
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir.join(format!("sync-{safe}.sqlite"))
}

/// One relay cycle for a workspace. Blocking.
pub fn run(
    connection: &str,
    origin: Url,
    token: String,
    port: u16,
    data_dir: &Path,
) -> Result<DataSyncReport, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("whity-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let local = Leg::new(format!("http://127.0.0.1:{port}"), None);
    let remote = Leg::new(origin.as_str(), Some(token));
    let resources = whity_sync::discover(&client, &local)?;
    let store = Store::open(&store_path(data_dir, connection))?;
    let summary = whity_sync::relay_cycle(&store, &client, &local, &remote, &resources);
    Ok(DataSyncReport {
        connection: connection.to_string(),
        at: crate::plugins::now_secs(),
        resources: summary.resources,
        relayed: summary.relayed,
        parked: store.conflict_count()?,
        failures: summary.failures,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_file_is_named_after_the_workspace_safely() {
        let path = store_path(Path::new("/data"), "https://app.whity.dev/mcp");
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "sync-https___app_whity_dev_mcp.sqlite"
        );
    }
}
