//! The relay against two ends that speak the wire contract the SDK's
//! `SyncController` serves: rows made on one end appear on the other, a
//! change travels, a deletion travels, nothing echoes back, and a row both
//! ends changed is parked with both versions while each end keeps its own.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Map, Value};
use whity_sync::{relay_cycle, End, Leg, Resource, Store};

/// As the plugin registers it, and where the device's PHP host serves it.
const PATH: &str = "/api/demo-catalog/items";
/// Where the workspace serves it.
const REMOTE_PATH: &str = "/api/v1/demo-catalog/items";

#[derive(Clone)]
struct FakeRow {
    id: i64,
    client_uuid: String,
    version: i64,
    deleted: bool,
    change_seq: i64,
    domain: Map<String, Value>,
}

impl FakeRow {
    fn to_json(&self) -> Value {
        let mut out = json!({
            "id": self.id,
            "tenantId": 1,
            "clientUuid": self.client_uuid,
            "version": self.version,
            "deletedAt": if self.deleted { Value::String("2026-11-25 10:00:00".into()) } else { Value::Null },
            "updatedBy": Value::Null,
            "createdAt": "2026-11-25 09:00:00",
            "updatedAt": "2026-11-25 10:00:00",
        });
        for (k, v) in &self.domain {
            out[k] = v.clone();
        }
        out
    }
}

#[derive(Default)]
struct Table {
    rows: BTreeMap<i64, FakeRow>,
    next_id: i64,
    seq: i64,
    /// Requests that carried no bearer token, counted so a test can assert
    /// the workspace end was only ever called with one.
    requests_without_token: usize,
}

/// One end: a SyncController in miniature, serving one resource.
struct FakeEnd {
    base: String,
    table: Arc<Mutex<Table>>,
    needs_token: Option<String>,
}

impl FakeEnd {
    fn start(served: &'static str, needs_token: Option<&str>) -> FakeEnd {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let table = Arc::new(Mutex::new(Table {
            next_id: 1,
            ..Table::default()
        }));
        let shared = table.clone();
        let token = needs_token.map(str::to_string);
        let expected_token = token.clone();
        thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let (status, payload) = {
                    let mut table = shared.lock().unwrap();
                    let bearer = request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("Authorization"))
                        .map(|h| h.value.as_str().to_string());
                    if bearer.is_none() {
                        table.requests_without_token += 1;
                    }
                    match &expected_token {
                        Some(expected)
                            if bearer.as_deref() != Some(&format!("Bearer {expected}")) =>
                        {
                            (401, json!({"error": "Unauthorized"}))
                        }
                        _ => {
                            let if_match = request
                                .headers()
                                .iter()
                                .find(|h| h.field.equiv("If-Match"))
                                .and_then(|h| h.value.as_str().parse::<i64>().ok());
                            handle(
                                &mut table,
                                served,
                                request.method().as_str(),
                                request.url(),
                                &body,
                                if_match,
                            )
                        }
                    }
                };
                let text = payload.to_string();
                let response = tiny_http::Response::from_string(text)
                    .with_status_code(status)
                    .with_header(
                        "Content-Type: application/json"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    );
                let _ = request.respond(response);
            }
        });
        FakeEnd {
            base,
            table,
            needs_token: token,
        }
    }

    fn leg(&self) -> Leg {
        Leg::new(&self.base, self.needs_token.clone())
    }

    /// A row made on this end directly, as a person would through the plugin.
    fn insert(&self, uuid: &str, name: &str) -> i64 {
        let mut table = self.table.lock().unwrap();
        let mut domain = Map::new();
        domain.insert("name".into(), Value::String(name.into()));
        domain.insert("status".into(), Value::String("active".into()));
        let id = table.next_id;
        table.next_id += 1;
        table.seq += 1;
        let seq = table.seq;
        table.rows.insert(
            id,
            FakeRow {
                id,
                client_uuid: uuid.into(),
                version: 1,
                deleted: false,
                change_seq: seq,
                domain,
            },
        );
        id
    }

    fn rename(&self, uuid: &str, name: &str) {
        let mut table = self.table.lock().unwrap();
        table.seq += 1;
        let seq = table.seq;
        let row = table
            .rows
            .values_mut()
            .find(|r| r.client_uuid == uuid)
            .unwrap();
        row.domain.insert("name".into(), Value::String(name.into()));
        row.version += 1;
        row.change_seq = seq;
    }

    fn delete(&self, uuid: &str) {
        let mut table = self.table.lock().unwrap();
        table.seq += 1;
        let seq = table.seq;
        let row = table
            .rows
            .values_mut()
            .find(|r| r.client_uuid == uuid)
            .unwrap();
        row.deleted = true;
        row.version += 1;
        row.change_seq = seq;
    }

    fn row(&self, uuid: &str) -> Option<FakeRow> {
        self.table
            .lock()
            .unwrap()
            .rows
            .values()
            .find(|r| r.client_uuid == uuid)
            .cloned()
    }

    fn count(&self) -> usize {
        self.table.lock().unwrap().rows.len()
    }
}

fn handle(
    table: &mut Table,
    served: &str,
    method: &str,
    url: &str,
    body: &str,
    if_match: Option<i64>,
) -> (u16, Value) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let query: BTreeMap<&str, &str> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .collect();
    if path == served && method == "GET" {
        let since: i64 = query
            .get("updatedSince")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let limit: usize = query
            .get("limit")
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);
        let mut rows: Vec<&FakeRow> = table
            .rows
            .values()
            .filter(|r| r.change_seq > since)
            .collect();
        rows.sort_by_key(|r| r.change_seq);
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let cursor = rows.last().map(|r| r.change_seq).unwrap_or(since);
        return (
            200,
            json!({
                "data": rows.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
                "cursor": cursor.to_string(),
                "hasMore": has_more,
            }),
        );
    }
    if path == served && method == "POST" {
        let payload: Map<String, Value> = serde_json::from_str(body).unwrap_or_default();
        let uuid = payload
            .get("clientUuid")
            .and_then(Value::as_str)
            .unwrap_or("made-here")
            .to_string();
        if let Some(existing) = table.rows.values().find(|r| r.client_uuid == uuid) {
            return (200, json!({"data": existing.to_json()}));
        }
        let mut domain = payload;
        domain.remove("clientUuid");
        let id = table.next_id;
        table.next_id += 1;
        table.seq += 1;
        let row = FakeRow {
            id,
            client_uuid: uuid,
            version: 1,
            deleted: false,
            change_seq: table.seq,
            domain,
        };
        table.rows.insert(id, row.clone());
        return (201, json!({"data": row.to_json()}));
    }
    if let Some(id) = path
        .strip_prefix(&format!("{served}/"))
        .and_then(|s| s.parse::<i64>().ok())
    {
        let Some(current) = table.rows.get(&id).cloned() else {
            return (404, json!({"error": "Item not found"}));
        };
        if method == "DELETE" && current.deleted {
            return (200, json!({"data": current.to_json()}));
        }
        if if_match.is_some_and(|base| base != current.version)
            || (method == "PATCH" && current.deleted)
        {
            return (
                409,
                json!({"error": "Version conflict", "serverItem": current.to_json()}),
            );
        }
        table.seq += 1;
        let seq = table.seq;
        let row = table.rows.get_mut(&id).unwrap();
        match method {
            "PATCH" => {
                let mut payload: Map<String, Value> =
                    serde_json::from_str(body).unwrap_or_default();
                payload.remove("baseVersion");
                for (k, v) in payload {
                    row.domain.insert(k, v);
                }
            }
            "DELETE" => row.deleted = true,
            _ => return (405, json!({"error": "Method not allowed"})),
        }
        row.version += 1;
        row.change_seq = seq;
        return (200, json!({"data": row.to_json()}));
    }
    (404, json!({"error": "Not found"}))
}

struct World {
    device: FakeEnd,
    workspace: FakeEnd,
    store: Store,
    client: reqwest::blocking::Client,
    resources: Vec<Resource>,
}

impl World {
    fn new() -> World {
        World {
            device: FakeEnd::start(PATH, None),
            workspace: FakeEnd::start(REMOTE_PATH, Some("device-token")),
            store: Store::in_memory().unwrap(),
            client: reqwest::blocking::Client::new(),
            resources: vec![Resource {
                plugin: "DemoCatalog".into(),
                path: PATH.into(),
                local_path: PATH.into(),
                remote_path: REMOTE_PATH.into(),
            }],
        }
    }

    fn cycle(&self) -> whity_sync::Summary {
        relay_cycle(
            &self.store,
            &self.client,
            &self.device.leg(),
            &self.workspace.leg(),
            &self.resources,
        )
    }
}

#[test]
fn a_row_made_on_the_device_reaches_the_workspace_and_never_echoes_back() {
    let w = World::new();
    w.device.insert("u1", "Bread");

    let first = w.cycle();
    assert_eq!(first.relayed, 1, "{first:?}");
    assert!(first.failures.is_empty(), "{first:?}");
    let there = w
        .workspace
        .row("u1")
        .expect("the row reached the workspace");
    assert_eq!(there.domain["name"], "Bread");

    let second = w.cycle();
    assert_eq!(
        second.relayed, 0,
        "the workspace's copy must not come back as a change: {second:?}"
    );
    assert_eq!(w.device.count(), 1);
    assert_eq!(w.workspace.count(), 1);
    assert_eq!(
        w.workspace.table.lock().unwrap().requests_without_token,
        0,
        "the workspace end always gets the token"
    );
}

#[test]
fn a_change_and_a_deletion_in_the_workspace_reach_the_device() {
    let w = World::new();
    w.device.insert("u1", "Bread");
    w.cycle();

    w.workspace.rename("u1", "Sourdough");
    let changed = w.cycle();
    assert_eq!(changed.relayed, 1, "{changed:?}");
    let here = w.device.row("u1").unwrap();
    assert_eq!(here.domain["name"], "Sourdough");
    assert_eq!(here.version, 2);

    w.workspace.delete("u1");
    let deleted = w.cycle();
    assert_eq!(deleted.relayed, 1, "{deleted:?}");
    assert!(
        w.device.row("u1").unwrap().deleted,
        "the tombstone travelled"
    );
    assert_eq!(w.cycle().relayed, 0);
}

#[test]
fn a_row_both_ends_changed_is_parked_and_each_end_keeps_its_own() {
    let w = World::new();
    w.device.insert("u1", "Bread");
    w.cycle();

    w.device.rename("u1", "Mine");
    w.workspace.rename("u1", "Theirs");
    let summary = w.cycle();
    assert_eq!(summary.relayed, 0, "{summary:?}");
    assert!(summary.conflicts >= 1, "{summary:?}");

    let parked = w.store.conflicts().unwrap();
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].resource, PATH);
    assert_eq!(parked[0].local.domain["name"], "Mine");
    assert_eq!(parked[0].remote.domain["name"], "Theirs");
    assert_eq!(
        w.device.row("u1").unwrap().domain["name"],
        "Mine",
        "nobody's change was overwritten"
    );
    assert_eq!(w.workspace.row("u1").unwrap().domain["name"], "Theirs");

    // A later change on one end only is relayed against the versions parked.
    w.workspace.rename("u1", "Agreed");
    let later = w.cycle();
    assert_eq!(later.relayed, 1, "{later:?}");
    assert_eq!(w.device.row("u1").unwrap().domain["name"], "Agreed");
    assert_eq!(
        w.store.conflict_count().unwrap(),
        0,
        "a relay that went through unparks the row"
    );
}

#[test]
fn a_dead_credential_ends_the_cycle_and_leaves_the_cursor() {
    let w = World::new();
    w.device.insert("u1", "Bread");
    let bad = Leg::new(&w.workspace.base, Some("wrong".into()));
    let summary = relay_cycle(&w.store, &w.client, &w.device.leg(), &bad, &w.resources);
    assert_eq!(summary.relayed, 0);
    assert_eq!(summary.failures.len(), 1, "{summary:?}");
    assert_eq!(
        w.store.cursor(PATH, End::Local).unwrap(),
        "0",
        "nothing was consumed"
    );
    assert_eq!(w.workspace.count(), 0);
}

#[test]
fn a_resource_the_workspace_does_not_serve_fails_alone() {
    let w = World::new();
    w.device.insert("u1", "Bread");
    let mut resources = w.resources.clone();
    resources.push(Resource {
        plugin: "Glossary".into(),
        path: "/api/glossary/terms".into(),
        local_path: "/api/glossary/terms".into(),
        remote_path: "/api/v1/glossary/terms".into(),
    });
    let summary = relay_cycle(
        &w.store,
        &w.client,
        &w.device.leg(),
        &w.workspace.leg(),
        &resources,
    );
    assert_eq!(summary.relayed, 1, "{summary:?}");
    assert_eq!(
        summary.failures.len(),
        2,
        "both legs of the missing resource: {summary:?}"
    );
    assert!(summary.failures[0].contains("Glossary"));
}
