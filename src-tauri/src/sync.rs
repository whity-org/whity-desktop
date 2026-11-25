//! Plugin data kept in two places (ADR 0001, decision 4): for each enrolled
//! workspace, `crates/sync` relays the resources this computer's plugins
//! declare between the PHP host on loopback and the workspace, with the
//! device token. Rows both ends changed are parked in the relay's store for a
//! person; nothing is overwritten. Runs after the plugin reconcile, on a
//! timer and on Sync now, always on a worker thread.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use url::Url;
use whity_sync::{Choice, Leg, Resolved, Store};

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

/// A parked row as the device row shows it: which fields differ and how each
/// end has them. The sync fields stay out; a person settles content.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictView {
    /// The resource path as the plugin registered it.
    pub resource: String,
    pub client_uuid: String,
    /// Seconds since the Unix epoch.
    pub detected_at: i64,
    /// A name or title the row carries, when it has one.
    pub title: Option<String>,
    pub local_deleted: bool,
    pub remote_deleted: bool,
    pub fields: Vec<FieldView>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldView {
    pub name: String,
    pub local: Value,
    pub remote: Value,
}

pub fn view_of(conflict: &whity_sync::Conflict) -> ConflictView {
    let mut names: Vec<&String> = conflict.local.domain.keys().collect();
    for name in conflict.remote.domain.keys() {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let fields = names
        .into_iter()
        .filter_map(|name| {
            let local = conflict
                .local
                .domain
                .get(name)
                .cloned()
                .unwrap_or(Value::Null);
            let remote = conflict
                .remote
                .domain
                .get(name)
                .cloned()
                .unwrap_or(Value::Null);
            (local != remote).then(|| FieldView {
                name: name.clone(),
                local,
                remote,
            })
        })
        .collect();
    let title = ["name", "title", "label"].iter().find_map(|key| {
        conflict
            .remote
            .domain
            .get(*key)
            .or_else(|| conflict.local.domain.get(*key))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    ConflictView {
        resource: conflict.resource.clone(),
        client_uuid: conflict.client_uuid.clone(),
        detected_at: conflict.detected_at,
        title,
        local_deleted: conflict.local.is_deleted(),
        remote_deleted: conflict.remote.is_deleted(),
        fields,
    }
}

/// The rows waiting for a person, for one workspace.
pub fn list_conflicts(data_dir: &Path, connection: &str) -> Result<Vec<ConflictView>, String> {
    let path = store_path(data_dir, connection);
    if !path.exists() {
        return Ok(vec![]);
    }
    Ok(Store::open(&path)?
        .conflicts()?
        .iter()
        .map(view_of)
        .collect())
}

/// What the person decided, as the webview sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChoiceInput {
    Fields {
        fields: serde_json::Map<String, Value>,
    },
    DeleteBoth,
    KeepAsNew,
}

impl From<ChoiceInput> for Choice {
    fn from(input: ChoiceInput) -> Self {
        match input {
            ChoiceInput::Fields { fields } => Choice::Fields(fields),
            ChoiceInput::DeleteBoth => Choice::DeleteBoth,
            ChoiceInput::KeepAsNew => Choice::KeepAsNew,
        }
    }
}

/// Settle one parked row on both ends. Blocking. Answers `done`, or
/// `changed_again` when an end changed the row meanwhile and it was parked
/// again with the fresh versions.
#[allow(clippy::too_many_arguments)]
pub fn resolve(
    connection: &str,
    origin: Url,
    token: String,
    port: u16,
    data_dir: &Path,
    resource: &str,
    client_uuid: &str,
    choice: Choice,
) -> Result<&'static str, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("whity-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let local = Leg::new(format!("http://127.0.0.1:{port}"), None);
    let remote = Leg::new(origin.as_str(), Some(token));
    let found = whity_sync::discover(&client, &local)?
        .into_iter()
        .find(|r| r.path == resource)
        .ok_or("no plugin on this computer serves that data any more")?;
    let store = Store::open(&store_path(data_dir, connection))?;
    Ok(
        match whity_sync::resolve(
            &store,
            &client,
            &local,
            &remote,
            &found,
            client_uuid,
            choice,
        )? {
            Resolved::Done => "done",
            Resolved::ChangedAgain => "changed_again",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(version: i64, deleted: bool, fields: &[(&str, Value)]) -> whity_sync::Row {
        let mut domain = serde_json::Map::new();
        for (k, v) in fields {
            domain.insert((*k).to_string(), v.clone());
        }
        whity_sync::Row {
            id: 1,
            tenant_id: Some(1),
            client_uuid: Some("u1".into()),
            version,
            deleted_at: deleted.then(|| "2026-11-25".to_string()),
            updated_by: None,
            created_at: None,
            updated_at: None,
            domain,
        }
    }

    #[test]
    fn a_view_lists_only_the_fields_that_differ_and_names_the_row() {
        let conflict = whity_sync::Conflict {
            resource: "/api/demo-catalog/items".into(),
            client_uuid: "u1".into(),
            local: row(
                2,
                false,
                &[("name", "Mine".into()), ("status", "active".into())],
            ),
            remote: row(
                3,
                true,
                &[("name", "Theirs".into()), ("status", "active".into())],
            ),
            detected_at: 7,
        };
        let view = view_of(&conflict);
        assert_eq!(view.title.as_deref(), Some("Theirs"));
        assert!(view.remote_deleted && !view.local_deleted);
        assert_eq!(view.fields.len(), 1);
        assert_eq!(view.fields[0].name, "name");
        assert_eq!(view.fields[0].local, "Mine");
    }

    #[test]
    fn a_choice_arrives_tagged_by_kind() {
        let fields: ChoiceInput =
            serde_json::from_str(r#"{"kind":"fields","fields":{"name":"Agreed"}}"#).unwrap();
        assert!(matches!(Choice::from(fields), Choice::Fields(f) if f["name"] == "Agreed"));
        let keep: ChoiceInput = serde_json::from_str(r#"{"kind":"keep_as_new"}"#).unwrap();
        assert_eq!(Choice::from(keep), Choice::KeepAsNew);
    }

    #[test]
    fn the_store_file_is_named_after_the_workspace_safely() {
        let path = store_path(Path::new("/data"), "https://app.whity.dev/mcp");
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "sync-https___app_whity_dev_mcp.sqlite"
        );
    }
}
