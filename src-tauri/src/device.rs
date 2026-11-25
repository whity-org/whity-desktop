//! This computer as a device of a workspace's core (ADR 0001, decision 5;
//! whity-core WHIT-1406).
//!
//! A Whity ID token opens the workspace's MCP endpoint and nothing else. For
//! core's REST (sync, the plugin catalogue, core-served updates) the app enrols
//! the computer as a device: it runs core's native sign-in, in which the
//! browser signs the person in through the identity provider and core sends a
//! one-time code back to a loopback listener here; the code and the PKCE
//! verifier become a token-mode session, and that session enrols the device.
//! The long-lived device credential goes to the OS keychain and nowhere else;
//! `state.json` keeps the device's id and when it last signed in online, for
//! the offline lock (`auth.desktop_login_max_hours`, echoed by core as
//! `desktop_login_max_seconds`). A short-lived access token is fetched from
//! the credential when something needs it and kept in memory only.

use crate::connections::Connections;
use crate::loopback::Loopback;
use crate::oauth::{self, Pkce};
use crate::store::{device_account, Device};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

pub const PLATFORM: &str = if cfg!(windows) {
    "windows"
} else if cfg!(target_os = "macos") {
    "macos"
} else {
    "linux"
};

/// Core's own lock when the instance sets none (72 hours).
const DEFAULT_MAX_LOGIN_SECONDS: i64 = 72 * 3600;

pub const NOT_ENROLLED: &str = "not_enrolled";
pub const DEVICE_REVOKED: &str = "device_revoked";
pub const KEYCHAIN: &str = "keychain";

/// A sign-in provider the workspace offers, for the person to pick when there
/// is more than one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provider {
    pub provider_key: String,
    pub display_name: String,
}

/// A workspace the person belongs to, when core asks which one to enrol in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    pub tenant_id: i64,
    pub name: Option<String>,
}

/// What the webview sees of this computer's enrolment. No credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub connection: String,
    pub id: i64,
    pub name: String,
    pub email: Option<String>,
    pub tenant_id: Option<i64>,
    pub last_online_auth_at: i64,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub seconds_remaining: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "outcome")]
pub enum Enrolment {
    Enrolled {
        device: DeviceView,
    },
    /// The person belongs to several workspaces on this instance: core wants
    /// to know which one. The selection token stays here, in memory.
    ChooseWorkspace {
        memberships: Vec<Membership>,
    },
}

/// A tenant choice core is waiting for, kept in memory between
/// [`Connections::enrol_device`] and [`Connections::choose_workspace`].
pub struct Pending {
    origin: Url,
    selection_token: String,
}

/// `GET /api/v1/auth/sso/<provider>/start?mode=native&port=…&code_challenge=…&state=…`.
pub fn start_url(
    origin: &Url,
    provider: &str,
    port: u16,
    pkce: &Pkce,
    state: &str,
) -> Result<Url, String> {
    if provider.is_empty()
        || !provider
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err("that isn't a sign-in provider's name".into());
    }
    let mut url = origin
        .join(&format!("/api/v1/auth/sso/{provider}/start"))
        .map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("mode", "native")
        .append_pair("port", &port.to_string())
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("state", state);
    Ok(url)
}

/// The loopback request core redirected the browser to: the one-time code,
/// after the app's own state was echoed. A failure comes back the same way,
/// as `error=<reason>`, so the app can stop waiting and say why.
pub fn parse_callback(target: &str, state: &str) -> Result<String, String> {
    let url = Url::parse(&format!("http://127.0.0.1{target}")).map_err(|e| e.to_string())?;
    let get = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if get("state").as_deref() != Some(state) {
        return Err("the sign-in that came back isn't the one this app started".into());
    }
    if let Some(error) = get("error") {
        return Err(format!(
            "sign_in_{}",
            error
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        ));
    }
    match get("code") {
        Some(code)
            if code.len() == 43
                && code
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            Ok(code)
        }
        _ => Err("the sign-in came back without a code".into()),
    }
}

/// The offline lock: how long since this computer last signed in online,
/// against the instance's limit. Locked means core must be reached again.
pub fn lock_state(device: &Device, now: i64) -> (bool, Option<&'static str>, Option<i64>) {
    let max = device
        .max_login_seconds
        .unwrap_or(DEFAULT_MAX_LOGIN_SECONDS)
        .max(1);
    let elapsed = now - device.last_online_auth_at;
    if elapsed <= max {
        (false, None, Some(max - elapsed))
    } else {
        (true, Some("ttl_expired"), None)
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// What this computer is called in the workspace's device list.
pub fn hostname() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Whity Desktop".to_string())
}

fn view(device: &Device, now: i64) -> DeviceView {
    let (locked, reason, remaining) = lock_state(device, now);
    DeviceView {
        connection: device.connection.clone(),
        id: device.id,
        name: device.name.clone(),
        email: device.email.clone(),
        tenant_id: device.tenant_id,
        last_online_auth_at: device.last_online_auth_at,
        locked,
        lock_reason: reason.map(str::to_string),
        seconds_remaining: remaining,
    }
}

fn memberships_of(value: &Value) -> Vec<Membership> {
    value
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let tenant_id = row.get("tenant_id").and_then(Value::as_i64)?;
                    let name = ["tenant_name", "name", "slug", "tenant_slug"]
                        .iter()
                        .find_map(|k| row.get(k).and_then(Value::as_str))
                        .map(str::to_string);
                    Some(Membership { tenant_id, name })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A token-mode session body: the access token and who it is.
struct Session {
    access_token: String,
    email: Option<String>,
    tenant_id: Option<i64>,
}

fn session_of(body: &Value) -> Option<Session> {
    let access_token = body.get("access_token")?.as_str()?.to_string();
    let user = body.get("user");
    Some(Session {
        access_token,
        email: user
            .and_then(|u| u.get("email"))
            .and_then(Value::as_str)
            .map(str::to_string),
        tenant_id: user
            .and_then(|u| u.get("tenant_id"))
            .and_then(Value::as_i64),
    })
}

impl Connections {
    /// The sign-in providers the workspace's core offers.
    pub async fn sign_in_providers(&self, connection: &str) -> Result<Vec<Provider>, String> {
        let origin = self.origin_of(connection)?;
        let url = origin
            .join("/api/v1/auth/sso/providers")
            .map_err(|e| e.to_string())?;
        let body: Value = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        Ok(body
            .get("data")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| serde_json::from_value(r.clone()).ok())
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Enrol this computer with the workspace's core: the native sign-in in
    /// the browser, the one-time code on loopback, the exchange, then the
    /// device. `open` shows the person the start URL; tests pass a fake browser.
    pub async fn enrol_device<F>(
        &self,
        connection: &str,
        provider: &str,
        open: F,
    ) -> Result<Enrolment, String>
    where
        F: FnOnce(&Url) -> Result<(), String>,
    {
        let origin = self.origin_of(connection)?;
        let pkce = Pkce::new();
        let state = oauth::random_token();
        let loopback = Loopback::bind().await?;
        let port: u16 = loopback
            .redirect_uri
            .rsplit(':')
            .next()
            .and_then(|rest| rest.split('/').next())
            .and_then(|p| p.parse().ok())
            .ok_or("no loopback port")?;
        open(&start_url(&origin, provider, port, &pkce, &state)?)?;

        let target = loopback.receive(self.window).await?;
        let code = parse_callback(&target, &state)?;

        let exchanged: Value = self
            .post_json(
                origin
                    .join("/api/v1/auth/sso/native/exchange")
                    .map_err(|e| e.to_string())?,
                None,
                &serde_json::json!({ "code": code, "code_verifier": pkce.verifier }),
            )
            .await?;
        if exchanged
            .get("requires_tenant_selection")
            .and_then(Value::as_bool)
            == Some(true)
        {
            let selection_token = exchanged
                .get("selection_token")
                .and_then(Value::as_str)
                .ok_or("core asked for a workspace but gave no way to answer")?
                .to_string();
            let memberships = memberships_of(exchanged.get("memberships").unwrap_or(&Value::Null));
            self.pending.lock().unwrap().insert(
                connection.to_string(),
                Pending {
                    origin,
                    selection_token,
                },
            );
            return Ok(Enrolment::ChooseWorkspace { memberships });
        }
        let session = session_of(&exchanged).ok_or("the sign-in gave no session")?;
        let device = self.finish_enrolment(connection, &origin, &session).await?;
        Ok(Enrolment::Enrolled { device })
    }

    /// The person's answer to [`Enrolment::ChooseWorkspace`].
    pub async fn choose_workspace(
        &self,
        connection: &str,
        tenant_id: i64,
    ) -> Result<DeviceView, String> {
        let pending = self
            .pending
            .lock()
            .unwrap()
            .remove(connection)
            .ok_or("nothing is waiting for a workspace choice; sign in again")?;
        let selected: Value = self
            .post_json(
                pending.origin.join("/api/v1/auth/select-tenant").map_err(|e| e.to_string())?,
                None,
                &serde_json::json!({ "selection_token": pending.selection_token, "tenant_id": tenant_id }),
            )
            .await?;
        let session = session_of(&selected).ok_or("the workspace choice gave no session")?;
        self.finish_enrolment(connection, &pending.origin, &session)
            .await
    }

    async fn finish_enrolment(
        &self,
        connection: &str,
        origin: &Url,
        session: &Session,
    ) -> Result<DeviceView, String> {
        let name = hostname();
        let enrolled: Value = self
            .post_json(
                origin.join("/api/v1/devices").map_err(|e| e.to_string())?,
                Some(&session.access_token),
                &serde_json::json!({ "name": name, "platform": PLATFORM }),
            )
            .await?;
        let id = enrolled
            .get("id")
            .and_then(Value::as_i64)
            .ok_or("the device was not enrolled")?;
        let credential = enrolled
            .get("credential")
            .and_then(Value::as_str)
            .ok_or("the device was enrolled but no credential came back")?;
        // The credential is the device: without a keychain to keep it there is
        // no enrolment, since a file would be a token on disk.
        self.secrets
            .set(&device_account(connection), credential)
            .map_err(|_| KEYCHAIN.to_string())?;

        let device = Device {
            connection: connection.to_string(),
            id,
            name,
            email: session.email.clone(),
            tenant_id: session.tenant_id,
            last_online_auth_at: now_secs(),
            max_login_seconds: None,
            credential_expires_at: enrolled
                .get("expires_at")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        self.update_state(|state| {
            state.devices.retain(|d| d.connection != connection);
            state.devices.push(device.clone());
        })?;
        Ok(view(&device, now_secs()))
    }

    /// A short-lived core access token for this computer, from the device
    /// credential in the keychain. Each success is an online sign-in for the
    /// offline lock. A refused credential (the device was revoked in the
    /// workspace) forgets the device here too.
    pub async fn device_access_token(&self, connection: &str) -> Result<String, String> {
        let device = self.device_of(connection).ok_or(NOT_ENROLLED)?;
        let credential = self
            .secrets
            .get(&device_account(connection))
            .map_err(|_| KEYCHAIN.to_string())?
            .ok_or(NOT_ENROLLED)?;
        let origin = self.origin_of(connection)?;
        let url = origin
            .join("/api/v1/devices/token")
            .map_err(|e| e.to_string())?;
        let response = self
            .http
            .post(url)
            .bearer_auth(&credential)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("X-Auth-Mode", "token")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let _ = self.secrets.delete(&device_account(connection));
            let _ = self.update_state(|state| state.devices.retain(|d| d.connection != connection));
            return Err(DEVICE_REVOKED.into());
        }
        let body: Value = response
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let token = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or("the device credential gave no session")?
            .to_string();
        let max = body
            .get("desktop_login_max_seconds")
            .and_then(Value::as_i64);
        let _ = device;
        self.update_state(|state| {
            if let Some(d) = state
                .devices
                .iter_mut()
                .find(|d| d.connection == connection)
            {
                d.last_online_auth_at = now_secs();
                if max.is_some() {
                    d.max_login_seconds = max;
                }
            }
        })?;
        Ok(token)
    }

    /// This computer's enrolment in each workspace, with the lock as of now.
    pub fn devices(&self) -> Vec<DeviceView> {
        let now = now_secs();
        self.state().devices.iter().map(|d| view(d, now)).collect()
    }

    pub fn device(&self, connection: &str) -> Option<DeviceView> {
        self.device_of(connection).map(|d| view(&d, now_secs()))
    }

    /// Take this computer out of the workspace: revoke the device there when
    /// core can be reached, and forget the credential here either way.
    pub async fn forget_device(&self, connection: &str) -> Result<(), String> {
        if let Some(device) = self.device_of(connection) {
            if let (Ok(origin), Ok(token)) = (
                self.origin_of(connection),
                self.device_access_token(connection).await,
            ) {
                if let Ok(url) = origin.join(&format!("/api/v1/devices/{}", device.id)) {
                    let _ = self
                        .http
                        .delete(url)
                        .bearer_auth(token)
                        .header("X-Requested-With", "XMLHttpRequest")
                        .send()
                        .await;
                }
            }
        }
        let _ = self.secrets.delete(&device_account(connection));
        self.pending.lock().unwrap().remove(connection);
        self.update_state(|state| state.devices.retain(|d| d.connection != connection))
    }

    fn device_of(&self, connection: &str) -> Option<Device> {
        self.state()
            .devices
            .into_iter()
            .find(|d| d.connection == connection)
    }

    /// The workspace's origin, from its MCP resource.
    fn origin_of(&self, connection: &str) -> Result<Url, String> {
        let (_, resource, _) = self
            .workspace(connection)
            .ok_or("not signed in to that workspace")?;
        Url::parse(&resource)
            .and_then(|u| u.join("/"))
            .map_err(|e| e.to_string())
    }

    /// A token-mode POST to core: JSON in, JSON out, a 4xx or 5xx as an error
    /// with the body's `error` when it has one.
    async fn post_json(
        &self,
        url: Url,
        bearer: Option<&str>,
        body: &Value,
    ) -> Result<Value, String> {
        let mut request = self
            .http
            .post(url)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("X-Auth-Mode", "token")
            .json(body);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        let text = response.text().await.map_err(|e| e.to_string())?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            let reason = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("core answered {}", status.as_u16()));
            return Err(reason);
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use crate::store::{Connection, StateFile};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn the_start_url_carries_mode_port_challenge_and_state() {
        let origin = Url::parse("https://app.whity.dev/").unwrap();
        let pkce = Pkce::new();
        let url = start_url(&origin, "whity_id", 43127, &pkce, "state-1").unwrap();
        assert_eq!(url.path(), "/api/v1/auth/sso/whity_id/start");
        let q: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(q.contains(&("mode".into(), "native".into())));
        assert!(q.contains(&("port".into(), "43127".into())));
        assert!(q.contains(&("code_challenge".into(), pkce.challenge.clone())));
        assert!(q.contains(&("state".into(), "state-1".into())));
        assert!(start_url(&origin, "Bad Provider", 1, &pkce, "s").is_err());
    }

    #[test]
    fn the_callback_must_echo_the_state_and_carry_a_code() {
        let code = oauth::random_token();
        assert_eq!(
            parse_callback(&format!("/callback?state=s-1&code={code}"), "s-1").unwrap(),
            code
        );
        assert!(parse_callback(&format!("/callback?state=other&code={code}"), "s-1").is_err());
        assert_eq!(
            parse_callback("/callback?state=s-1&error=denied", "s-1").unwrap_err(),
            "sign_in_denied"
        );
        assert!(parse_callback("/callback?state=s-1&code=short", "s-1").is_err());
    }

    #[test]
    fn the_lock_follows_the_last_online_sign_in() {
        let device = Device {
            connection: "c".into(),
            id: 1,
            name: "PC".into(),
            email: None,
            tenant_id: None,
            last_online_auth_at: 1_000,
            max_login_seconds: Some(600),
            credential_expires_at: None,
        };
        assert_eq!(lock_state(&device, 1_100), (false, None, Some(500)));
        assert_eq!(
            lock_state(&device, 1_601),
            (true, Some("ttl_expired"), None)
        );
        let default = Device {
            max_login_seconds: None,
            ..device
        };
        assert_eq!(
            lock_state(&default, 1_000 + 72 * 3600),
            (false, None, Some(0))
        );
    }

    /// A workspace's core on one local port: the provider list, the native
    /// exchange, tenant selection, device enrolment and the credential
    /// exchange, recording what it was sent.
    type Seen = Arc<Mutex<Vec<(String, String, Option<String>)>>>;

    struct Core {
        origin: String,
        challenge: Arc<Mutex<Option<String>>>,
        seen: Seen,
    }

    fn core(several_workspaces: bool) -> Core {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let origin = format!(
            "http://127.0.0.1:{}",
            server.server_addr().to_ip().unwrap().port()
        );
        let challenge: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (c, s) = (challenge.clone(), seen.clone());
        thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let url = request.url().to_string();
                let header = |name: &str| {
                    request
                        .headers()
                        .iter()
                        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
                        .map(|h| h.value.as_str().to_string())
                };
                let auth = header("Authorization");
                let token_mode = header("X-Auth-Mode").as_deref() == Some("token");
                s.lock()
                    .unwrap()
                    .push((url.clone(), body.clone(), auth.clone()));
                let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let session = |tenant: i64| {
                    serde_json::json!({ "access_token": "at-dev", "refresh_token": "rt-dev", "token_type": "Bearer", "expires_in": 900,
                        "user": { "id": 5, "email": "amro@example.test", "role": "admin", "tenant_id": tenant } })
                };
                let (status, json) = match url.as_str() {
                    "/api/v1/auth/sso/providers" => (
                        200,
                        serde_json::json!({ "data": [{ "provider_key": "whity_id", "display_name": "Whity ID" }], "password_sign_in": false }),
                    ),
                    "/api/v1/auth/sso/native/exchange" => {
                        let verifier = parsed["code_verifier"].as_str().unwrap_or("");
                        let expected = c.lock().unwrap().clone();
                        let ok = token_mode
                            && parsed["code"].as_str()
                                == Some("native-code-1234567890123456789012345678901")
                            && expected.as_deref() == Some(Pkce::challenge_of(verifier).as_str());
                        if !ok {
                            (
                                401,
                                serde_json::json!({ "error": "Invalid or expired code" }),
                            )
                        } else if several_workspaces {
                            (
                                200,
                                serde_json::json!({ "requires_tenant_selection": true, "selection_token": "sel-1",
                                "memberships": [{ "tenant_id": 3, "tenant_name": "Example Bakery" }, { "tenant_id": 4, "tenant_name": "Example Clinic" }] }),
                            )
                        } else {
                            (200, session(3))
                        }
                    }
                    "/api/v1/auth/select-tenant" => {
                        if token_mode
                            && parsed["selection_token"] == "sel-1"
                            && parsed["tenant_id"] == 4
                        {
                            (200, session(4))
                        } else {
                            (401, serde_json::json!({ "error": "bad selection" }))
                        }
                    }
                    "/api/v1/devices" => {
                        if auth.as_deref() == Some("Bearer at-dev")
                            && parsed["platform"].as_str() == Some(PLATFORM)
                        {
                            (
                                201,
                                serde_json::json!({ "id": 7, "credential": "cred-1", "name": parsed["name"], "platform": PLATFORM, "expires_at": "2027-02-23T00:00:00+00:00" }),
                            )
                        } else {
                            (401, serde_json::json!({ "error": "Unauthenticated" }))
                        }
                    }
                    "/api/v1/devices/token" => {
                        if auth.as_deref() == Some("Bearer cred-1") {
                            (
                                200,
                                serde_json::json!({ "access_token": "at-2", "token_type": "Bearer", "expires_in": 900, "desktop_login_max_seconds": 3600 }),
                            )
                        } else {
                            (
                                401,
                                serde_json::json!({ "error": "Invalid device credential" }),
                            )
                        }
                    }
                    "/api/v1/devices/7" => (200, serde_json::json!({ "revoked": true })),
                    _ => (404, serde_json::json!({ "error": "not found" })),
                };
                let content =
                    tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap();
                let _ = request.respond(
                    tiny_http::Response::from_string(json.to_string())
                        .with_status_code(status)
                        .with_header(content),
                );
            }
        });
        Core {
            origin,
            challenge,
            seen,
        }
    }

    /// A browser that signs in at once: it reads the start URL core would get,
    /// remembers the challenge for the mock core, and calls the loopback the
    /// way core's callback does.
    fn browser(challenge: Arc<Mutex<Option<String>>>) -> impl FnOnce(&Url) -> Result<(), String> {
        move |url: &Url| {
            let get = |k: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.into_owned())
                    .unwrap()
            };
            assert_eq!(get("mode"), "native");
            *challenge.lock().unwrap() = Some(get("code_challenge"));
            let callback = format!(
                "http://127.0.0.1:{}/callback?code=native-code-1234567890123456789012345678901&state={}",
                get("port"),
                get("state")
            );
            tokio::spawn(async move {
                let _ = reqwest::get(callback).await;
            });
            Ok(())
        }
    }

    fn connections(origin: &str) -> (Connections, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("whity-desktop-device-{}", oauth::random_token()));
        let connections = Connections::new(StateFile::new(&dir), Box::new(MemoryStore::default()));
        connections
            .update_state(|state| {
                state.connections.push(Connection {
                    id: "ws-1".into(),
                    resource: format!("{origin}/mcp"),
                    name: Some("Example Bakery".into()),
                    issuer: "https://id.whity.dev".into(),
                    token_endpoint: "https://id.whity.dev/oauth/token".into(),
                    client_id: "whity-desktop".into(),
                    agent: None,
                })
            })
            .unwrap();
        (connections, dir)
    }

    #[tokio::test]
    async fn enrolling_keeps_the_credential_in_the_keychain_and_the_rest_in_state() {
        let core = core(false);
        let (connections, dir) = connections(&core.origin);

        let providers = connections.sign_in_providers("ws-1").await.unwrap();
        assert_eq!(providers[0].provider_key, "whity_id");

        let outcome = connections
            .enrol_device("ws-1", "whity_id", browser(core.challenge.clone()))
            .await
            .unwrap();
        let Enrolment::Enrolled { device } = outcome else {
            panic!("expected an enrolment, got {outcome:?}");
        };
        assert_eq!(device.id, 7);
        assert_eq!(device.tenant_id, Some(3));
        assert_eq!(device.email.as_deref(), Some("amro@example.test"));
        assert!(!device.locked);

        let saved = std::fs::read_to_string(dir.join("state.json")).unwrap();
        assert!(
            !saved.contains("cred-1") && !saved.contains("at-dev"),
            "no credential or token on disk: {saved}"
        );
        assert!(saved.contains("\"id\": 7") || saved.contains("\"id\":7"));
        assert_eq!(connections.devices().len(), 1);

        // The credential becomes a session on demand, and that counts as an online sign-in.
        let before = connections.device("ws-1").unwrap().last_online_auth_at;
        let token = connections.device_access_token("ws-1").await.unwrap();
        assert_eq!(token, "at-2");
        let after = connections.device("ws-1").unwrap();
        assert!(after.last_online_auth_at >= before);
        assert_eq!(
            after.seconds_remaining.map(|s| s > 3500 && s <= 3600),
            Some(true)
        );

        // Forgetting revokes the device there and drops the credential here.
        connections.forget_device("ws-1").await.unwrap();
        assert!(connections.device("ws-1").is_none());
        assert_eq!(
            connections.device_access_token("ws-1").await.unwrap_err(),
            NOT_ENROLLED
        );
        let seen = core.seen.lock().unwrap();
        assert!(seen
            .iter()
            .any(|(url, _, auth)| url == "/api/v1/devices/7"
                && auth.as_deref() == Some("Bearer at-2")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn several_workspaces_mean_a_choice_before_the_device_is_enrolled() {
        let core = core(true);
        let (connections, dir) = connections(&core.origin);

        let outcome = connections
            .enrol_device("ws-1", "whity_id", browser(core.challenge.clone()))
            .await
            .unwrap();
        let Enrolment::ChooseWorkspace { memberships } = outcome else {
            panic!("expected a choice, got {outcome:?}");
        };
        assert_eq!(memberships.len(), 2);
        assert_eq!(memberships[1].name.as_deref(), Some("Example Clinic"));
        assert!(
            connections.device("ws-1").is_none(),
            "nothing is enrolled yet"
        );

        let device = connections.choose_workspace("ws-1", 4).await.unwrap();
        assert_eq!(device.tenant_id, Some(4));
        assert!(
            connections.choose_workspace("ws-1", 4).await.is_err(),
            "the selection token is used once"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_revoked_credential_forgets_the_device() {
        let core = core(false);
        let (connections, dir) = connections(&core.origin);
        connections
            .enrol_device("ws-1", "whity_id", browser(core.challenge.clone()))
            .await
            .unwrap();
        // Core no longer knows the credential: a 401 on the exchange.
        connections.secrets_set_for_test(&device_account("ws-1"), "cred-gone");
        assert_eq!(
            connections.device_access_token("ws-1").await.unwrap_err(),
            DEVICE_REVOKED
        );
        assert!(connections.device("ws-1").is_none());
        let _ = std::fs::remove_dir_all(dir);
        let _ = Duration::from_secs(0);
    }
}
