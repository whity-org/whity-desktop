//! A person settles a parked row. Both ends live: they pick each differing
//! field's final value and the merged row is written to both ends, each with
//! the version that was parked as its base, so a change made meanwhile on
//! either end is refused and the row is parked again with the fresh versions
//! instead of being overwritten. One end deleted the row while the other
//! changed it: the person deletes it on both, or keeps the changed row as a
//! new record and lets the deletion stand. Nothing here decides for them.

use reqwest::blocking::Client;
use serde_json::{Map, Value};

use crate::relay::Resource;
use crate::store::{Conflict, End, Store};
use crate::wire::{self, Leg, Row, Write};

/// What the person decided for a parked row.
#[derive(Debug, Clone, PartialEq)]
pub enum Choice {
    /// Both ends live: the final value of each field the person settled.
    /// A field not named keeps the workspace's value.
    Fields(Map<String, Value>),
    /// One end deleted the row and the other changed it: delete on both.
    DeleteBoth,
    /// One end deleted the row and the other changed it: keep the changed
    /// row as a new record (a new `clientUuid`); the deletion stands.
    KeepAsNew,
}

/// How a resolution ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Both ends now carry the settled row; nothing is parked for it.
    Done,
    /// An end changed the row again meanwhile. Nothing was overwritten; the
    /// row is parked again with the fresh versions for the person to look at.
    ChangedAgain,
}

pub fn resolve(
    store: &Store,
    client: &Client,
    local: &Leg,
    remote: &Leg,
    resource: &Resource,
    client_uuid: &str,
    choice: Choice,
) -> Result<Resolved, String> {
    let key = resource.path.as_str();
    let parked = store
        .conflict(key, client_uuid)?
        .ok_or_else(|| "this row isn't waiting for anyone".to_string())?;
    let leg = |end: End| match end {
        End::Local => local,
        End::Remote => remote,
    };
    let row_at = |end: End| -> &Row {
        match end {
            End::Local => &parked.local,
            End::Remote => &parked.remote,
        }
    };

    match (
        parked.local.is_deleted(),
        parked.remote.is_deleted(),
        choice,
    ) {
        (true, true, _) => {
            store.unpark(key, client_uuid)?;
            Ok(Resolved::Done)
        }
        (false, false, Choice::Fields(chosen)) => {
            let mut merged = parked.remote.domain.clone();
            for (field, value) in chosen {
                if parked.local.domain.contains_key(&field)
                    || parked.remote.domain.contains_key(&field)
                {
                    merged.insert(field, value);
                }
            }
            let mut fresh = parked.clone();
            for end in [End::Local, End::Remote] {
                let was = row_at(end);
                let written = wire::update(
                    client,
                    leg(end),
                    resource.path_at(end),
                    was.id,
                    was.version,
                    &merged,
                )
                .map_err(|e| format!("{}: {e}", end_words(end)))?;
                match written {
                    Write::Applied(row) => {
                        store.remember(key, client_uuid, end, (row.id, row.version))?;
                        set_row(&mut fresh, end, row);
                    }
                    Write::Conflict(row) => {
                        set_row(&mut fresh, end, row);
                        store.park(key, client_uuid, &fresh.local, &fresh.remote)?;
                        return Ok(Resolved::ChangedAgain);
                    }
                    Write::Gone => store.forget(key, client_uuid, end)?,
                }
            }
            store.unpark(key, client_uuid)?;
            Ok(Resolved::Done)
        }
        (false, false, _) => Err("both ends still have this row: settle its fields".to_string()),
        (local_deleted, _, choice) => {
            let dead = if local_deleted {
                End::Local
            } else {
                End::Remote
            };
            let live = dead.other();
            let live_row = row_at(live);
            match choice {
                Choice::Fields(_) => Err(format!(
                    "the row was deleted {}: delete it on both or keep it as new",
                    end_words(dead)
                )),
                Choice::DeleteBoth => {
                    let written = wire::delete(
                        client,
                        leg(live),
                        resource.path_at(live),
                        live_row.id,
                        live_row.version,
                    )
                    .map_err(|e| format!("{}: {e}", end_words(live)))?;
                    match written {
                        Write::Applied(row) => {
                            store.remember(key, client_uuid, live, (row.id, row.version))?
                        }
                        Write::Conflict(row) => {
                            let mut fresh = parked.clone();
                            set_row(&mut fresh, live, row);
                            store.park(key, client_uuid, &fresh.local, &fresh.remote)?;
                            return Ok(Resolved::ChangedAgain);
                        }
                        Write::Gone => store.forget(key, client_uuid, live)?,
                    }
                    store.unpark(key, client_uuid)?;
                    Ok(Resolved::Done)
                }
                Choice::KeepAsNew => {
                    let copy_uuid = new_uuid();
                    // The copy is not recorded as seen: the next cycle reads it from
                    // this end's feed and carries it over like any new row.
                    wire::create(
                        client,
                        leg(live),
                        resource.path_at(live),
                        &copy_uuid,
                        &live_row.domain,
                    )
                    .map_err(|e| format!("{}: {e}", end_words(live)))?;
                    let written = wire::delete(
                        client,
                        leg(live),
                        resource.path_at(live),
                        live_row.id,
                        live_row.version,
                    )
                    .map_err(|e| format!("{}: {e}", end_words(live)))?;
                    match written {
                        Write::Applied(row) => {
                            store.remember(key, client_uuid, live, (row.id, row.version))?
                        }
                        Write::Conflict(row) => {
                            let mut fresh = parked.clone();
                            set_row(&mut fresh, live, row);
                            store.park(key, client_uuid, &fresh.local, &fresh.remote)?;
                            return Ok(Resolved::ChangedAgain);
                        }
                        Write::Gone => store.forget(key, client_uuid, live)?,
                    }
                    store.unpark(key, client_uuid)?;
                    Ok(Resolved::Done)
                }
            }
        }
    }
}

fn set_row(conflict: &mut Conflict, end: End, row: Row) {
    match end {
        End::Local => conflict.local = row,
        End::Remote => conflict.remote = row,
    }
}

fn end_words(end: End) -> &'static str {
    match end {
        End::Local => "on this computer",
        End::Remote => "in the workspace",
    }
}

/// A fresh version-4 UUID for a kept copy.
fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::Rng::fill(&mut rand::thread_rng(), &mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::new_uuid;

    #[test]
    fn a_copy_gets_a_version_4_uuid() {
        let uuid = new_uuid();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4");
        assert_ne!(uuid, new_uuid());
    }
}
