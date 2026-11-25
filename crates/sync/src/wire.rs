//! The sync wire contract, as `Whity\Sdk\Sync\SyncController` serves it:
//! `GET {path}?updatedSince=&includeDeleted=1&limit=` answers
//! `{data: [row], cursor, hasMore}`; `POST {path}` with `clientUuid` answers
//! the row (201 made, 200 already there); `PATCH` and `DELETE {path}/{id}`
//! with `If-Match: <version>` answer the row, or `409 {serverItem}` when the
//! version moved, or `404` when the row is gone. A row is its sync fields and,
//! flattened beside them, the resource's own fields, which this crate carries
//! as they are.

use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

/// One end of a relay: the PHP host on loopback (no token) or the workspace
/// (the device token). `base` is an origin without a trailing slash.
#[derive(Debug, Clone)]
pub struct Leg {
    pub base: String,
    pub token: Option<String>,
}

impl Leg {
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        let mut base: String = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self { base, token }
    }
}

/// A row as the wire carries it. The sync fields are typed so none of them
/// can fall into `domain` and go back out in a write body; `tenantId` in
/// particular is read-only, each end derives the tenant from the caller.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: i64,
    #[serde(default)]
    pub tenant_id: Option<i64>,
    #[serde(default)]
    pub client_uuid: Option<String>,
    pub version: i64,
    #[serde(default)]
    pub deleted_at: Option<String>,
    #[serde(default)]
    pub updated_by: Option<i64>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(flatten)]
    pub domain: serde_json::Map<String, serde_json::Value>,
}

impl Row {
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}

/// Why a call failed, so the relay can react: `Unauthorized` ends the cycle
/// (the token is dead), `Retryable` is the network or a 5xx, `Permanent` is
/// any other refusal, which a retry will not change.
#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    Unauthorized,
    Retryable(String),
    Permanent(String),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Unauthorized => write!(f, "unauthorized"),
            WireError::Retryable(m) | WireError::Permanent(m) => write!(f, "{m}"),
        }
    }
}

/// What a conditional write did.
#[derive(Debug)]
pub enum Write {
    Applied(Row),
    /// The version moved on that end; carries its current row.
    Conflict(Row),
    /// The row is gone on that end.
    Gone,
}

/// One page of a changes feed.
#[derive(Debug)]
pub struct Page {
    pub rows: Vec<Row>,
    pub cursor: String,
    pub has_more: bool,
}

#[derive(Deserialize)]
struct DataEnvelope {
    data: Row,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConflictEnvelope {
    server_item: Row,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangesEnvelope {
    data: Vec<Row>,
    cursor: String,
    has_more: bool,
}

fn request(req: RequestBuilder, leg: &Leg) -> RequestBuilder {
    let req = req.header("X-Requested-With", "XMLHttpRequest");
    match &leg.token {
        Some(token) => req.bearer_auth(token),
        None => req,
    }
}

/// One page of the changes feed after `cursor`, tombstones included.
pub fn changes(
    client: &Client,
    leg: &Leg,
    path: &str,
    cursor: &str,
    limit: u32,
) -> Result<Page, WireError> {
    let url = format!(
        "{}{path}?updatedSince={cursor}&includeDeleted=1&limit={limit}",
        leg.base
    );
    let response = request(client.get(url), leg).send().map_err(network)?;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(WireError::Unauthorized);
    }
    let text = body(response)?;
    if !status.is_success() {
        return Err(classify("changes feed", status, &text));
    }
    let envelope: ChangesEnvelope = serde_json::from_str(&text)
        .map_err(|e| WireError::Permanent(format!("the changes feed isn't one: {e}")))?;
    Ok(Page {
        rows: envelope.data,
        cursor: envelope.cursor,
        has_more: envelope.has_more,
    })
}

/// Create, idempotent on `client_uuid`; answers the row either way.
pub fn create(
    client: &Client,
    leg: &Leg,
    path: &str,
    client_uuid: &str,
    domain: &serde_json::Map<String, serde_json::Value>,
) -> Result<Row, WireError> {
    let mut payload = domain.clone();
    payload.insert(
        "clientUuid".to_string(),
        serde_json::Value::String(client_uuid.to_string()),
    );
    let response = request(client.post(format!("{}{path}", leg.base)), leg)
        .json(&payload)
        .send()
        .map_err(network)?;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(WireError::Unauthorized);
    }
    let text = body(response)?;
    if !status.is_success() {
        return Err(classify("create", status, &text));
    }
    let envelope: DataEnvelope = serde_json::from_str(&text)
        .map_err(|e| WireError::Permanent(format!("the create answer isn't a row: {e}")))?;
    Ok(envelope.data)
}

/// Update `id` if it is still at `base_version`.
pub fn update(
    client: &Client,
    leg: &Leg,
    path: &str,
    id: i64,
    base_version: i64,
    domain: &serde_json::Map<String, serde_json::Value>,
) -> Result<Write, WireError> {
    let mut payload = domain.clone();
    payload.insert(
        "baseVersion".to_string(),
        serde_json::Value::from(base_version),
    );
    let response = request(client.patch(format!("{}{path}/{id}", leg.base)), leg)
        .header("If-Match", base_version.to_string())
        .json(&payload)
        .send()
        .map_err(network)?;
    outcome(response, "update")
}

/// Soft-delete `id` if it is still at `base_version`.
pub fn delete(
    client: &Client,
    leg: &Leg,
    path: &str,
    id: i64,
    base_version: i64,
) -> Result<Write, WireError> {
    let response = request(client.delete(format!("{}{path}/{id}", leg.base)), leg)
        .header("If-Match", base_version.to_string())
        .send()
        .map_err(network)?;
    outcome(response, "delete")
}

fn outcome(response: Response, what: &str) -> Result<Write, WireError> {
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(WireError::Unauthorized);
    }
    if status == StatusCode::NOT_FOUND {
        return Ok(Write::Gone);
    }
    let text = body(response)?;
    if status == StatusCode::CONFLICT {
        let envelope: ConflictEnvelope = serde_json::from_str(&text).map_err(|e| {
            WireError::Permanent(format!("the {what} conflict answer has no serverItem: {e}"))
        })?;
        return Ok(Write::Conflict(envelope.server_item));
    }
    if !status.is_success() {
        return Err(classify(what, status, &text));
    }
    let envelope: DataEnvelope = serde_json::from_str(&text)
        .map_err(|e| WireError::Permanent(format!("the {what} answer isn't a row: {e}")))?;
    Ok(Write::Applied(envelope.data))
}

fn classify(what: &str, status: StatusCode, text: &str) -> WireError {
    let message = format!("{what} answered {}: {}", status.as_u16(), shorten(text));
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        WireError::Retryable(message)
    } else {
        WireError::Permanent(message)
    }
}

fn network(e: reqwest::Error) -> WireError {
    WireError::Retryable(format!("couldn't reach that end: {e}"))
}

fn body(response: Response) -> Result<String, WireError> {
    response
        .text()
        .map_err(|e| WireError::Retryable(format!("couldn't read the answer: {e}")))
}

fn shorten(text: &str) -> String {
    let mut out: String = text.chars().take(200).collect();
    if out.len() < text.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_keeps_its_own_fields_apart_from_the_sync_ones() {
        let row: Row = serde_json::from_str(
            r#"{"id":4,"tenantId":1,"clientUuid":"u1","version":2,"deletedAt":null,
                "updatedBy":null,"createdAt":"2026-11-25","updatedAt":"2026-11-25",
                "name":"Bread","status":"active"}"#,
        )
        .unwrap();
        assert_eq!(row.id, 4);
        assert_eq!(row.client_uuid.as_deref(), Some("u1"));
        assert!(!row.is_deleted());
        assert_eq!(
            row.domain.len(),
            2,
            "only name and status are the resource's own"
        );
        assert_eq!(row.domain["name"], "Bread");
    }

    #[test]
    fn a_leg_drops_the_trailing_slash() {
        assert_eq!(
            Leg::new("https://app.example.test/", None).base,
            "https://app.example.test"
        );
    }
}
