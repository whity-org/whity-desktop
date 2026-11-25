//! The relay's own memory, in one SQLite file beside the PHP data. Three
//! tables: `rows` (per resource and `client_uuid`, the id and version each
//! end last had), `cursors` (per resource, where each end's feed was read up
//! to) and `conflicts` (the rows both ends changed, with both versions, for a
//! person to resolve). Nothing of the plugin's data lives here beyond those
//! two snapshots.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

use crate::wire::Row;

/// Which end of the relay a value belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The PHP host on this computer.
    Local,
    /// The workspace.
    Remote,
}

impl End {
    pub fn other(self) -> End {
        match self {
            End::Local => End::Remote,
            End::Remote => End::Local,
        }
    }

    fn column(self) -> &'static str {
        match self {
            End::Local => "local",
            End::Remote => "remote",
        }
    }
}

/// What each end last had for one row: `(id, version)` there, or `None` when
/// that end has never been written for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Known {
    pub local: Option<(i64, i64)>,
    pub remote: Option<(i64, i64)>,
}

impl Known {
    pub fn at(&self, end: End) -> Option<(i64, i64)> {
        match end {
            End::Local => self.local,
            End::Remote => self.remote,
        }
    }
}

/// A row both ends changed, as each end has it now.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub resource: String,
    pub client_uuid: String,
    pub local: Row,
    pub remote: Row,
    /// Seconds since the Unix epoch.
    pub detected_at: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn =
            Connection::open(path).map_err(|e| format!("couldn't open {}: {e}", path.display()))?;
        Self::from(conn)
    }

    pub fn in_memory() -> Result<Self, String> {
        Self::from(Connection::open_in_memory().map_err(|e| e.to_string())?)
    }

    fn from(conn: Connection) -> Result<Self, String> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS rows (
                 resource       TEXT NOT NULL,
                 client_uuid    TEXT NOT NULL,
                 local_id       INTEGER,
                 local_version  INTEGER,
                 remote_id      INTEGER,
                 remote_version INTEGER,
                 PRIMARY KEY (resource, client_uuid)
             );
             CREATE TABLE IF NOT EXISTS cursors (
                 resource      TEXT PRIMARY KEY,
                 local_cursor  TEXT NOT NULL DEFAULT '0',
                 remote_cursor TEXT NOT NULL DEFAULT '0',
                 last_relay_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS conflicts (
                 resource    TEXT NOT NULL,
                 client_uuid TEXT NOT NULL,
                 local_json  TEXT NOT NULL,
                 remote_json TEXT NOT NULL,
                 detected_at INTEGER NOT NULL,
                 PRIMARY KEY (resource, client_uuid)
             );",
        )
        .map_err(|e| format!("couldn't prepare the sync store: {e}"))?;
        Ok(Self { conn })
    }

    pub fn known(&self, resource: &str, client_uuid: &str) -> Result<Known, String> {
        self.conn
            .query_row(
                "SELECT local_id, local_version, remote_id, remote_version FROM rows
                 WHERE resource = ?1 AND client_uuid = ?2",
                params![resource, client_uuid],
                |r| {
                    let local = match (r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?) {
                        (Some(id), Some(version)) => Some((id, version)),
                        _ => None,
                    };
                    let remote = match (r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?) {
                        (Some(id), Some(version)) => Some((id, version)),
                        _ => None,
                    };
                    Ok(Known { local, remote })
                },
            )
            .optional()
            .map(|k| k.unwrap_or_default())
            .map_err(db)
    }

    /// Record what `end` now has for the row; the other end's record stays.
    pub fn remember(
        &self,
        resource: &str,
        client_uuid: &str,
        end: End,
        id_version: (i64, i64),
    ) -> Result<(), String> {
        let column = end.column();
        self.conn
            .execute(
                &format!(
                    "INSERT INTO rows (resource, client_uuid, {column}_id, {column}_version)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (resource, client_uuid) DO UPDATE SET
                         {column}_id = excluded.{column}_id,
                         {column}_version = excluded.{column}_version"
                ),
                params![resource, client_uuid, id_version.0, id_version.1],
            )
            .map(|_| ())
            .map_err(db)
    }

    /// Forget what `end` had for the row (it is gone there).
    pub fn forget(&self, resource: &str, client_uuid: &str, end: End) -> Result<(), String> {
        let column = end.column();
        self.conn
            .execute(
                &format!(
                    "UPDATE rows SET {column}_id = NULL, {column}_version = NULL
                     WHERE resource = ?1 AND client_uuid = ?2"
                ),
                params![resource, client_uuid],
            )
            .map(|_| ())
            .map_err(db)
    }

    pub fn cursor(&self, resource: &str, end: End) -> Result<String, String> {
        let column = end.column();
        self.conn
            .query_row(
                &format!("SELECT {column}_cursor FROM cursors WHERE resource = ?1"),
                params![resource],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map(|c| c.unwrap_or_else(|| "0".to_string()))
            .map_err(db)
    }

    pub fn set_cursor(&self, resource: &str, end: End, cursor: &str) -> Result<(), String> {
        let column = end.column();
        self.conn
            .execute(
                &format!(
                    "INSERT INTO cursors (resource, {column}_cursor, last_relay_at)
                     VALUES (?1, ?2, unixepoch())
                     ON CONFLICT (resource) DO UPDATE SET
                         {column}_cursor = excluded.{column}_cursor,
                         last_relay_at = excluded.last_relay_at"
                ),
                params![resource, cursor],
            )
            .map(|_| ())
            .map_err(db)
    }

    /// Park a row both ends changed, replacing an earlier parking of the same row.
    pub fn park(
        &self,
        resource: &str,
        client_uuid: &str,
        local: &Row,
        remote: &Row,
    ) -> Result<(), String> {
        let local_json = serde_json::to_string(local).map_err(|e| e.to_string())?;
        let remote_json = serde_json::to_string(remote).map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT INTO conflicts (resource, client_uuid, local_json, remote_json, detected_at)
                 VALUES (?1, ?2, ?3, ?4, unixepoch())
                 ON CONFLICT (resource, client_uuid) DO UPDATE SET
                     local_json = excluded.local_json,
                     remote_json = excluded.remote_json,
                     detected_at = excluded.detected_at",
                params![resource, client_uuid, local_json, remote_json],
            )
            .map(|_| ())
            .map_err(db)
    }

    /// The row is no longer in conflict (a relay of it went through).
    pub fn unpark(&self, resource: &str, client_uuid: &str) -> Result<(), String> {
        self.conn
            .execute(
                "DELETE FROM conflicts WHERE resource = ?1 AND client_uuid = ?2",
                params![resource, client_uuid],
            )
            .map(|_| ())
            .map_err(db)
    }

    pub fn conflict(&self, resource: &str, client_uuid: &str) -> Result<Option<Conflict>, String> {
        Ok(self
            .conflicts()?
            .into_iter()
            .find(|c| c.resource == resource && c.client_uuid == client_uuid))
    }

    pub fn conflicts(&self) -> Result<Vec<Conflict>, String> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT resource, client_uuid, local_json, remote_json, detected_at
                 FROM conflicts ORDER BY detected_at, resource, client_uuid",
            )
            .map_err(db)?;
        let rows = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .map_err(db)?;
        let mut out = Vec::new();
        for row in rows {
            let (resource, client_uuid, local_json, remote_json, detected_at) = row.map_err(db)?;
            out.push(Conflict {
                resource,
                client_uuid,
                local: serde_json::from_str(&local_json).map_err(|e| e.to_string())?,
                remote: serde_json::from_str(&remote_json).map_err(|e| e.to_string())?,
                detected_at,
            });
        }
        Ok(out)
    }

    pub fn conflict_count(&self) -> Result<usize, String> {
        self.conn
            .query_row("SELECT COUNT(*) FROM conflicts", [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .map_err(db)
    }

    /// When any resource was last relayed, seconds since the Unix epoch.
    pub fn last_relay_at(&self) -> Result<Option<i64>, String> {
        self.conn
            .query_row("SELECT MAX(last_relay_at) FROM cursors", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .map_err(db)
    }
}

fn db(e: rusqlite::Error) -> String {
    format!("sync store: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, version: i64, name: &str) -> Row {
        let mut domain = serde_json::Map::new();
        domain.insert("name".into(), serde_json::Value::String(name.into()));
        Row {
            id,
            tenant_id: Some(1),
            client_uuid: Some("u1".into()),
            version,
            deleted_at: None,
            updated_by: None,
            created_at: None,
            updated_at: None,
            domain,
        }
    }

    #[test]
    fn remembering_one_end_leaves_the_other_alone() {
        let store = Store::in_memory().unwrap();
        store.remember("/r", "u1", End::Local, (1, 1)).unwrap();
        store.remember("/r", "u1", End::Remote, (9, 1)).unwrap();
        store.remember("/r", "u1", End::Local, (1, 2)).unwrap();
        let known = store.known("/r", "u1").unwrap();
        assert_eq!(known.local, Some((1, 2)));
        assert_eq!(known.remote, Some((9, 1)));
        store.forget("/r", "u1", End::Remote).unwrap();
        assert_eq!(store.known("/r", "u1").unwrap().remote, None);
        assert_eq!(store.known("/r", "nobody").unwrap(), Known::default());
    }

    #[test]
    fn each_end_has_its_own_cursor_per_resource() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.cursor("/r", End::Local).unwrap(), "0");
        store.set_cursor("/r", End::Local, "42").unwrap();
        assert_eq!(store.cursor("/r", End::Local).unwrap(), "42");
        assert_eq!(store.cursor("/r", End::Remote).unwrap(), "0");
        assert_eq!(store.cursor("/other", End::Local).unwrap(), "0");
        assert!(store.last_relay_at().unwrap().is_some());
    }

    #[test]
    fn a_parked_row_keeps_both_versions_until_it_is_unparked() {
        let store = Store::in_memory().unwrap();
        store
            .park("/r", "u1", &row(1, 2, "Mine"), &row(9, 3, "Theirs"))
            .unwrap();
        store
            .park("/r", "u1", &row(1, 2, "Mine again"), &row(9, 3, "Theirs"))
            .unwrap();
        let parked = store.conflicts().unwrap();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].local.domain["name"], "Mine again");
        assert_eq!(parked[0].remote.version, 3);
        store.unpark("/r", "u1").unwrap();
        assert_eq!(store.conflict_count().unwrap(), 0);
    }
}
