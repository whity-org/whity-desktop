//! Which resources to relay: the PHP host lists at `/__whity/sync-resources`
//! what its loaded plugins declare through SDK 1.173's
//! `Contribute\PluginSyncResourcesInterface`, already checked against the plugin's
//! own routes and already carrying the served path.

use reqwest::blocking::Client;
use serde::Deserialize;

use crate::relay::Resource;
use crate::versioned_path;
use crate::wire::Leg;

#[derive(Deserialize)]
struct Listing {
    data: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    plugin: String,
    /// As the plugin registered it, `/api/...`.
    path: String,
    /// Where the host serves it.
    route_path: String,
}

/// The resources the PHP host on `local` serves for syncing.
pub fn discover(client: &Client, local: &Leg) -> Result<Vec<Resource>, String> {
    let response = client
        .get(format!("{}/__whity/sync-resources", local.base))
        .send()
        .map_err(|e| format!("couldn't reach the PHP host: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "the PHP host answered {} for its sync resources",
            response.status().as_u16()
        ));
    }
    let listing: Listing = response
        .json()
        .map_err(|e| format!("the PHP host's sync resources aren't a list: {e}"))?;
    Ok(listing
        .data
        .into_iter()
        .map(|e| Resource {
            plugin: e.plugin,
            remote_path: versioned_path(&e.path),
            path: e.path,
            local_path: e.route_path,
        })
        .collect())
}
