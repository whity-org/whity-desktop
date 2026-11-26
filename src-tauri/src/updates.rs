//! Signed updates, served by the workspace with Whity's feed as the fallback
//! (ADR 0001, decision 2b; whity-core WC-app-self-update). Every update is
//! minisign-verified against the one Whity release key built into the app,
//! whatever the delivery path: an instance can decide which version its
//! devices run and when, but it cannot ship its own binary.
//!
//! Delivery order: for each enrolled, unlocked workspace, core's
//! `GET /api/v1/desktop-app-updates/latest` with the device token, asked for
//! this device's channel; then, when no workspace offers anything, Whity's
//! own `latest.json` on GitHub Releases for that channel. The device token
//! goes only to the workspace that issued it, so the two legs are two
//! separate checks, never one endpoint list.
//!
//! Channels (decision 6): `stable` or `beta`, chosen per device and kept in a
//! small file beside the app's state; a beta device also takes stable
//! releases when they are newer.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::Manager;
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::connections::Connections;

/// Empty until the release key exists; the updater is off while it is.
const PUBLIC_KEY: &str = include_str!("../updater.pub");

/// Whity's own feed. tauri-action publishes `latest.json` with every release;
/// `/releases/latest/` is the newest release that is not marked pre-release,
/// so it is the stable channel on its own.
const STABLE_FEED: &str =
    "https://github.com/whity-org/whity-desktop/releases/latest/download/latest.json";
/// Where the beta channel's feed is found: the newest release of any kind.
const RELEASES_API: &str =
    "https://api.github.com/repos/whity-org/whity-desktop/releases?per_page=10";

/// The release channel a device follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Beta,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }

    pub fn parse(text: &str) -> Option<Channel> {
        match text.trim() {
            "stable" => Some(Channel::Stable),
            "beta" => Some(Channel::Beta),
            _ => None,
        }
    }
}

/// The channel file beside the app's state; absent means stable.
pub fn channel_path(config_dir: &Path) -> PathBuf {
    config_dir.join("update-channel")
}

pub fn read_channel(config_dir: &Path) -> Channel {
    std::fs::read_to_string(channel_path(config_dir))
        .ok()
        .and_then(|text| Channel::parse(&text))
        .unwrap_or(Channel::Stable)
}

pub fn write_channel(config_dir: &Path, channel: Channel) -> Result<(), String> {
    std::fs::create_dir_all(config_dir).map_err(|e| e.to_string())?;
    std::fs::write(channel_path(config_dir), channel.as_str()).map_err(|e| e.to_string())
}

/// An update already downloaded, waiting for the person to restart.
#[derive(Default)]
pub struct PendingUpdate(pub Mutex<Option<(Update, Vec<u8>)>>);

/// What a check found, as the window shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub version: String,
    /// `workspace` when a workspace's core offered it, `whity` for Whity's feed.
    pub source: &'static str,
}

fn public_key() -> Option<&'static str> {
    Some(PUBLIC_KEY.trim()).filter(|key| !key.is_empty())
}

/// The workspace's update endpoint for this device: core answers 204 when the
/// device is current, or the manifest the updater expects. The updater fills
/// `{{target}}` and `{{current_version}}` itself.
pub fn workspace_endpoint(origin: &url::Url, channel: Channel) -> Result<url::Url, String> {
    let mut endpoint = origin
        .join("/api/v1/desktop-app-updates/latest")
        .map_err(|e| e.to_string())?;
    endpoint.set_query(Some(&format!(
        "target={{{{target}}}}&current_version={{{{current_version}}}}&channel={}",
        channel.as_str()
    )));
    Ok(endpoint)
}

/// Whity's feed for a channel. Stable is a fixed address; beta is the newest
/// release's `latest.json` whatever its marking, found through the releases
/// list, and stable's when there is none.
async fn whity_feed(client: &reqwest::Client, channel: Channel) -> Result<url::Url, String> {
    let stable = url::Url::parse(STABLE_FEED).map_err(|e| e.to_string())?;
    if channel == Channel::Stable {
        return Ok(stable);
    }
    let releases: Vec<serde_json::Value> = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    Ok(beta_feed_in(&releases).unwrap_or(stable))
}

/// The newest release with a `latest.json`, drafts left out. A runtime mirror
/// or any other release without one is skipped.
pub fn beta_feed_in(releases: &[serde_json::Value]) -> Option<url::Url> {
    releases
        .iter()
        .filter(|r| r["draft"] != true)
        .find_map(|r| {
            r["assets"].as_array()?.iter().find_map(|asset| {
                (asset["name"] == "latest.json")
                    .then(|| asset["browser_download_url"].as_str())
                    .flatten()
                    .and_then(|u| url::Url::parse(u).ok())
            })
        })
}

/// Look for a newer version and download it. Returns what is ready when one is.
pub async fn check(
    app: &tauri::AppHandle,
    connections: &Connections,
    pending: &PendingUpdate,
) -> Result<Option<Found>, String> {
    let Some(key) = public_key().filter(|_| !cfg!(debug_assertions)) else {
        return Ok(None);
    };
    let channel = read_channel(&app.path().app_config_dir().map_err(|e| e.to_string())?);

    // The workspaces first: each with its own token, to its own origin only.
    for device in connections.devices() {
        if device.locked {
            continue;
        }
        let Ok(token) = connections.device_access_token(&device.connection).await else {
            continue;
        };
        let Ok(origin) = connections.origin_of(&device.connection) else {
            continue;
        };
        let endpoint = workspace_endpoint(&origin, channel)?;
        let updater = app
            .updater_builder()
            .pubkey(key)
            .endpoints(vec![endpoint])
            .map_err(|e| e.to_string())?
            .header("Authorization", format!("Bearer {token}"))
            .map_err(|e| e.to_string())?
            .header("X-Requested-With", "XMLHttpRequest")
            .map_err(|e| e.to_string())?
            .build()
            .map_err(|e| e.to_string())?;
        // A workspace that answers nothing or badly is not the last word: the
        // next one, then Whity's feed, still get asked.
        if let Ok(Some(update)) = updater.check().await {
            return stage(update, pending, "workspace").await.map(Some);
        }
    }

    let client = reqwest::Client::builder()
        .user_agent(concat!("whity-desktop/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let feed = whity_feed(&client, channel).await?;
    let update = app
        .updater_builder()
        .pubkey(key)
        .endpoints(vec![feed])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    match update {
        Some(update) => stage(update, pending, "whity").await.map(Some),
        None => Ok(None),
    }
}

/// Download the update once; a check that finds the same version again
/// answers from what is already waiting.
async fn stage(
    update: Update,
    pending: &PendingUpdate,
    source: &'static str,
) -> Result<Found, String> {
    let version = update.version.clone();
    if let Some((ready, _)) = pending.0.lock().unwrap().as_ref() {
        if ready.version == version {
            return Ok(Found { version, source });
        }
    }
    let bytes = update
        .download(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    *pending.0.lock().unwrap() = Some((update, bytes));
    Ok(Found { version, source })
}

/// Whether an update is downloaded and waiting for a restart.
pub fn is_pending(pending: &PendingUpdate) -> bool {
    pending.0.lock().unwrap().is_some()
}

/// Install what was downloaded and restart. On Windows the installer ends this
/// process itself.
pub fn install(app: &tauri::AppHandle, pending: &PendingUpdate) -> Result<(), String> {
    let (update, bytes) = pending
        .0
        .lock()
        .unwrap()
        .take()
        .ok_or("no update is waiting")?;
    update.install(bytes).map_err(|e| e.to_string())?;
    app.restart()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_workspace_endpoint_keeps_the_updater_variables_and_names_the_channel() {
        let origin = url::Url::parse("https://app.whity.dev/").unwrap();
        let endpoint = workspace_endpoint(&origin, Channel::Beta).unwrap();
        assert_eq!(
            endpoint.as_str(),
            "https://app.whity.dev/api/v1/desktop-app-updates/latest?target={{target}}&current_version={{current_version}}&channel=beta"
        );
    }

    #[test]
    fn the_beta_feed_is_the_newest_release_that_has_a_manifest() {
        let releases = vec![
            json!({"draft": true, "assets": [{"name": "latest.json", "browser_download_url": "https://example.test/draft/latest.json"}]}),
            json!({"draft": false, "assets": [{"name": "frankenphp-linux-x86_64", "browser_download_url": "https://example.test/mirror/frankenphp"}]}),
            json!({"draft": false, "prerelease": true, "assets": [{"name": "latest.json", "browser_download_url": "https://example.test/v0.3.0-beta.1/latest.json"}]}),
            json!({"draft": false, "assets": [{"name": "latest.json", "browser_download_url": "https://example.test/v0.2.0/latest.json"}]}),
        ];
        assert_eq!(
            beta_feed_in(&releases).unwrap().as_str(),
            "https://example.test/v0.3.0-beta.1/latest.json"
        );
        assert!(beta_feed_in(&[]).is_none());
    }

    #[test]
    fn the_channel_is_a_file_and_defaults_to_stable() {
        let dir = std::env::temp_dir().join(format!("whity-channel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read_channel(&dir), Channel::Stable);
        write_channel(&dir, Channel::Beta).unwrap();
        assert_eq!(read_channel(&dir), Channel::Beta);
        std::fs::write(channel_path(&dir), "nightly").unwrap();
        assert_eq!(
            read_channel(&dir),
            Channel::Stable,
            "an unknown word falls back to stable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
