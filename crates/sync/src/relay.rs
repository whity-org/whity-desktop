//! The relay: for each resource, read the device's changes feed and write the
//! workspace, then read the workspace's feed and write the device. A row is
//! written to the other end with that end's last-known id and version as the
//! base, so a row the other end changed since is refused with `409` and
//! parked with both versions. A row we ourselves wrote comes back on that
//! end's feed with the id and version we recorded, and is skipped.

use reqwest::blocking::Client;

use crate::store::{End, Store};
use crate::wire::{self, Leg, Row, WireError, Write};

const PAGE: u32 = 200;

/// One syncable resource, as the PHP host lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// The plugin that declared it, for reports.
    pub plugin: String,
    /// The path as the plugin registered it, `/api/...`; the store's key.
    pub path: String,
    /// Where the PHP host serves it (its router adds no version).
    pub local_path: String,
    /// Where the workspace serves it (`/api/v1/...`).
    pub remote_path: String,
}

impl Resource {
    pub fn path_at(&self, end: End) -> &str {
        match end {
            End::Local => &self.local_path,
            End::Remote => &self.remote_path,
        }
    }
}

/// What one cycle did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub resources: usize,
    /// Rows written to the other end.
    pub relayed: usize,
    /// Rows parked this cycle because both ends changed them.
    pub conflicts: usize,
    /// Plain-text reasons for the legs and rows that failed; a failed leg is
    /// retried next cycle from the same cursor.
    pub failures: Vec<String>,
}

/// Relay every resource both ways. A leg that fails leaves its cursor where
/// it was and is reported; nothing stops the other resources. A dead token on
/// the workspace end ends the cycle at once: every leg would fail the same way.
pub fn relay_cycle(
    store: &Store,
    client: &Client,
    local: &Leg,
    remote: &Leg,
    resources: &[Resource],
) -> Summary {
    let mut summary = Summary {
        resources: resources.len(),
        ..Summary::default()
    };
    for resource in resources {
        for (from, to, from_end) in [(local, remote, End::Local), (remote, local, End::Remote)] {
            match relay_direction(store, client, from, to, from_end, resource) {
                Ok(outcome) => {
                    summary.relayed += outcome.relayed;
                    summary.conflicts += outcome.conflicts;
                    summary.failures.extend(outcome.failures);
                }
                Err(LegError::Unauthorized) => {
                    summary.failures.push(
                        "the workspace no longer accepts this computer's credential".to_string(),
                    );
                    return summary;
                }
                Err(LegError::Failed(reason)) => summary.failures.push(format!(
                    "{} {} {}: {reason}",
                    resource.plugin,
                    resource.path,
                    direction_words(from_end)
                )),
            }
        }
    }
    summary
}

fn direction_words(from: End) -> &'static str {
    match from {
        End::Local => "from this computer",
        End::Remote => "from the workspace",
    }
}

enum LegError {
    Unauthorized,
    Failed(String),
}

impl From<String> for LegError {
    fn from(reason: String) -> Self {
        LegError::Failed(reason)
    }
}

#[derive(Default)]
struct DirectionOutcome {
    relayed: usize,
    conflicts: usize,
    failures: Vec<String>,
}

fn relay_direction(
    store: &Store,
    client: &Client,
    from: &Leg,
    to: &Leg,
    from_end: End,
    resource: &Resource,
) -> Result<DirectionOutcome, LegError> {
    let key = resource.path.as_str();
    let to_end = from_end.other();
    let mut outcome = DirectionOutcome::default();
    let mut cursor = store.cursor(key, from_end)?;
    loop {
        let page = match wire::changes(client, from, resource.path_at(from_end), &cursor, PAGE) {
            Ok(page) => page,
            Err(WireError::Unauthorized) => return Err(LegError::Unauthorized),
            Err(e) => return Err(LegError::Failed(e.to_string())),
        };
        for row in &page.rows {
            let Some(uuid) = row.client_uuid.as_deref() else {
                continue;
            };
            let known = store.known(key, uuid)?;
            if known.at(from_end) == Some((row.id, row.version)) {
                continue; // what we wrote there ourselves, coming back
            }
            match write_other_end(client, to, resource.path_at(to_end), row, known.at(to_end)) {
                Ok(Written::Applied(other)) => {
                    store.remember(key, uuid, from_end, (row.id, row.version))?;
                    store.remember(key, uuid, to_end, (other.id, other.version))?;
                    store.unpark(key, uuid)?;
                    outcome.relayed += 1;
                }
                Ok(Written::Nothing) => {
                    store.remember(key, uuid, from_end, (row.id, row.version))?;
                }
                Ok(Written::Conflict(other)) => {
                    let (local, remote) = match from_end {
                        End::Local => (row, &other),
                        End::Remote => (&other, row),
                    };
                    store.park(key, uuid, local, remote)?;
                    // Both ends' current versions are known now; the next change
                    // on either end is relayed against them.
                    store.remember(key, uuid, from_end, (row.id, row.version))?;
                    store.remember(key, uuid, to_end, (other.id, other.version))?;
                    outcome.conflicts += 1;
                }
                Err(WireError::Unauthorized) => return Err(LegError::Unauthorized),
                Err(e) => outcome.failures.push(format!(
                    "{} {} row {uuid} {}: {e}",
                    resource.plugin,
                    resource.path,
                    direction_words(from_end)
                )),
            }
        }
        cursor = page.cursor;
        store.set_cursor(key, from_end, &cursor)?;
        if !page.has_more {
            return Ok(outcome);
        }
    }
}

enum Written {
    Applied(Row),
    /// A tombstone the other end never had: nothing to make there.
    Nothing,
    Conflict(Row),
}

/// Create the row on the other end if it has never been there, else update or
/// delete it against the version that end last gave us. A row that vanished
/// there is made again from this one, unless this one is a tombstone too.
fn write_other_end(
    client: &Client,
    to: &Leg,
    path: &str,
    row: &Row,
    other: Option<(i64, i64)>,
) -> Result<Written, WireError> {
    let uuid = row.client_uuid.as_deref().unwrap_or_default();
    let Some((id, version)) = other else {
        if row.is_deleted() {
            return Ok(Written::Nothing);
        }
        return wire::create(client, to, path, uuid, &row.domain).map(Written::Applied);
    };
    let written = if row.is_deleted() {
        wire::delete(client, to, path, id, version)?
    } else {
        wire::update(client, to, path, id, version, &row.domain)?
    };
    Ok(match written {
        Write::Applied(other) => Written::Applied(other),
        Write::Conflict(other) => Written::Conflict(other),
        Write::Gone if row.is_deleted() => Written::Nothing,
        Write::Gone => Written::Applied(wire::create(client, to, path, uuid, &row.domain)?),
    })
}
