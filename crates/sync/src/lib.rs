//! Plugin data in two places, kept the same (ADR 0001, decision 4).
//!
//! A plugin that runs on the device keeps its rows in the PHP host's own
//! SQLite; the same plugin on the workspace keeps them in core's database.
//! Both serve them through the SDK's `SyncController`, so both speak one wire
//! contract: a changes feed with a cursor (`GET ?updatedSince=`), idempotent
//! create on `clientUuid`, update and delete with `If-Match: <version>` that
//! answer `409 {serverItem}` when the base moved, and tombstones. This crate
//! relays between the two ends through that contract and nothing else: it
//! never opens either database, and it never invents a field. What it keeps
//! for itself is in [`store`]: per row and resource, the id and version each
//! end last had; per resource and end, the feed cursor; and the rows both
//! ends changed, parked for a person to resolve field by field.
//!
//! Which resources: those the loaded plugins declare through SDK 1.173's
//! `Contribute\PluginSyncResourcesInterface`, read from the PHP host at
//! `/__whity/sync-resources` ([`discover`]). The same plugin registers the
//! same `/api/...` path on both ends; core serves it under `/api/v1`, the PHP
//! host as is, so a [`Resource`] carries the path for each end.
//!
//! Last-write-wins is not here on purpose: business records need a person to
//! decide. Disjoint changes never collide, because a relay only ever writes a
//! row the other end has not changed since; the moment both ends changed it,
//! the write fails with `409` and the two versions are parked.

pub mod discover;
pub mod relay;
pub mod store;
pub mod wire;

pub use discover::discover;
pub use relay::{relay_cycle, Resource, Summary};
pub use store::{Conflict, End, Store};
pub use wire::{Leg, Row};

/// Where the SDK's router puts the version: after the first segment, so the
/// `/api/demo-catalog/items` a plugin registers is served at
/// `/api/v1/demo-catalog/items` on both ends.
pub fn versioned_path(path: &str) -> String {
    match path[1..].find('/') {
        Some(pos) => format!("{}/v1{}", &path[..pos + 1], &path[pos + 1..]),
        None => format!("{path}/v1"),
    }
}

#[cfg(test)]
mod tests {
    use super::versioned_path;

    #[test]
    fn the_version_goes_after_the_first_segment() {
        assert_eq!(
            versioned_path("/api/demo-catalog/items"),
            "/api/v1/demo-catalog/items"
        );
        assert_eq!(versioned_path("/api"), "/api/v1");
    }
}
