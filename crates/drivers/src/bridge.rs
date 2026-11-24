//! The loopback bridge plugin PHP calls to reach hardware.
//!
//! Two kinds of caller, two secrets:
//! - The PHP host, once per boot, with the host secret Rust gave it in its
//!   environment: `POST /handles` with every loaded plugin and the
//!   capabilities it declared. The answer is one opaque handle per plugin.
//!   Minting again replaces every earlier handle: a restarted host starts
//!   clean.
//! - A plugin, through the client the host built for it, with its handle:
//!   `POST /native/<capability>` with the operation's arguments as JSON.
//!
//! A call is refused with a code the PHP side can act on: `unauthorized` (no
//! or unknown handle), `not_declared` (the plugin never asked for that
//! capability), `off` and `ask_first` (the device's policy), `unsupported`
//! (no driver in this build), `invalid_args`, `failed`.
//!
//! Loopback only, one thread, `tiny_http`: a handful of calls a minute from
//! one process on the same machine.

use crate::driver::{DriverError, Registry};
use crate::policy::{Decision, Policy};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Header, Method, Request, Response, Server};

pub const HOST_SECRET_HEADER: &str = "X-Whity-Host-Secret";
pub const HANDLE_HEADER: &str = "X-Whity-Handle";
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// What the app keeps: where the bridge listens and the secret the PHP host
/// gets in its environment (`WHITY_NATIVE_BRIDGE_URL`, `WHITY_NATIVE_BRIDGE_SECRET`).
pub struct BridgeHandle {
    pub port: u16,
    pub host_secret: String,
    grants: Arc<Mutex<HashMap<String, Grant>>>,
}

impl BridgeHandle {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The plugins that hold a handle right now, with their declared
    /// capabilities, for a settings screen. Handles themselves stay inside.
    pub fn grants(&self) -> Vec<(String, Vec<String>)> {
        let grants = self.grants.lock().unwrap();
        let mut out: Vec<(String, Vec<String>)> = grants
            .values()
            .map(|g| {
                let mut caps: Vec<String> = g.capabilities.iter().cloned().collect();
                caps.sort();
                (g.plugin.clone(), caps)
            })
            .collect();
        out.sort();
        out
    }

    /// The environment the PHP host needs to find and call the bridge.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("WHITY_NATIVE_BRIDGE_URL".into(), self.url()),
            (
                "WHITY_NATIVE_BRIDGE_SECRET".into(),
                self.host_secret.clone(),
            ),
        ]
    }
}

#[derive(Clone, Debug)]
struct Grant {
    plugin: String,
    capabilities: HashSet<String>,
}

#[derive(Deserialize)]
struct HandlesRequest {
    plugins: Vec<PluginDeclaration>,
}

#[derive(Deserialize)]
struct PluginDeclaration {
    name: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Serialize)]
struct HandlesResponse {
    handles: HashMap<String, String>,
    /// Declared capabilities no driver in this build offers; recorded all the
    /// same so the message is "unsupported", not "not declared".
    unsupported: Vec<String>,
}

pub struct Bridge;

impl Bridge {
    /// Bind a loopback port, make the host secret, and serve on a thread for
    /// the life of the process.
    pub fn spawn(registry: Arc<Registry>, policy: Arc<dyn Policy>) -> Result<BridgeHandle, String> {
        let server = Server::http("127.0.0.1:0")
            .map_err(|e| format!("failed to bind the native bridge: {e}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .ok_or("the native bridge has no IP address")?
            .port();
        let host_secret = random_token();
        let grants: Arc<Mutex<HashMap<String, Grant>>> = Arc::new(Mutex::new(HashMap::new()));
        let state = State {
            registry,
            policy,
            host_secret: host_secret.clone(),
            grants: grants.clone(),
        };
        thread::Builder::new()
            .name("whity-native-bridge".into())
            .spawn(move || {
                for request in server.incoming_requests() {
                    state.handle(request);
                }
            })
            .map_err(|e| format!("failed to start the native bridge thread: {e}"))?;
        Ok(BridgeHandle {
            port,
            host_secret,
            grants,
        })
    }
}

struct State {
    registry: Arc<Registry>,
    policy: Arc<dyn Policy>,
    host_secret: String,
    grants: Arc<Mutex<HashMap<String, Grant>>>,
}

impl State {
    fn handle(&self, mut request: Request) {
        let url = request.url().to_string();
        let method = request.method().clone();
        let body = match read_body(&mut request) {
            Ok(body) => body,
            Err(reason) => return refuse(request, 400, "invalid_body", &reason),
        };
        match (method, url.as_str()) {
            (Method::Post, "/handles") => self.mint(request, &body),
            (Method::Post, path) if path.starts_with("/native/") => {
                let capability = path["/native/".len()..].to_string();
                self.call(request, &capability, &body)
            }
            _ => refuse(request, 404, "not_found", "no such endpoint"),
        }
    }

    fn mint(&self, request: Request, body: &[u8]) {
        let provided = header(&request, HOST_SECRET_HEADER);
        if !provided
            .as_deref()
            .is_some_and(|p| constant_time_eq(p.as_bytes(), self.host_secret.as_bytes()))
        {
            return refuse(
                request,
                401,
                "unauthorized",
                "the host secret is missing or wrong",
            );
        }
        let declared: HandlesRequest = match serde_json::from_slice(body) {
            Ok(d) => d,
            Err(e) => {
                return refuse(
                    request,
                    400,
                    "invalid_body",
                    &format!("plugins list expected: {e}"),
                )
            }
        };
        let mut handles = HashMap::new();
        let mut unsupported = Vec::new();
        let mut grants = HashMap::new();
        for plugin in declared.plugins {
            if plugin.name.is_empty() || handles.contains_key(&plugin.name) {
                return refuse(
                    request,
                    400,
                    "invalid_body",
                    "plugin names must be unique and non-empty",
                );
            }
            let mut capabilities = HashSet::new();
            for capability in plugin.capabilities {
                if !self.registry.offers(&capability) {
                    unsupported.push(format!("{}: {capability}", plugin.name));
                }
                capabilities.insert(capability);
            }
            let handle = random_token();
            handles.insert(plugin.name.clone(), handle.clone());
            grants.insert(
                handle,
                Grant {
                    plugin: plugin.name,
                    capabilities,
                },
            );
        }
        unsupported.sort();
        *self.grants.lock().unwrap() = grants;
        respond(
            request,
            200,
            &HandlesResponse {
                handles,
                unsupported,
            },
        );
    }

    fn call(&self, request: Request, capability: &str, body: &[u8]) {
        let grant = header(&request, HANDLE_HEADER)
            .and_then(|h| self.grants.lock().unwrap().get(h.as_str()).cloned());
        let Some(grant) = grant else {
            return refuse(
                request,
                401,
                "unauthorized",
                "the plugin handle is missing or unknown",
            );
        };
        if !grant.capabilities.contains(capability) {
            return refuse(
                request,
                403,
                "not_declared",
                &format!("{} did not declare {capability}", grant.plugin),
            );
        }
        match self.policy.decide(&grant.plugin, capability) {
            Decision::Allowed => {}
            Decision::Off => {
                return refuse(
                    request,
                    403,
                    "off",
                    &format!("{capability} is off for {} on this device", grant.plugin),
                )
            }
            Decision::AskFirst => {
                return refuse(
                    request,
                    403,
                    "ask_first",
                    &format!(
                        "{capability} needs the person's approval for {}",
                        grant.plugin
                    ),
                )
            }
        }
        let args: Value = if body.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => {
                    return refuse(
                        request,
                        400,
                        "invalid_args",
                        &format!("arguments must be JSON: {e}"),
                    )
                }
            }
        };
        match self.registry.dispatch(capability, args) {
            Ok(result) => respond(request, 200, &json!({ "ok": true, "result": result })),
            Err(DriverError::Unsupported(m)) => refuse(request, 404, "unsupported", &m),
            Err(DriverError::InvalidArgs(m)) => refuse(request, 400, "invalid_args", &m),
            Err(DriverError::Failed(m)) => refuse(request, 500, "failed", &m),
        }
    }
}

fn read_body(request: &mut Request) -> Result<Vec<u8>, String> {
    let declared = request.body_length().unwrap_or(0);
    if declared > MAX_BODY_BYTES {
        return Err("body too large".into());
    }
    let mut body = Vec::with_capacity(declared);
    request
        .as_reader()
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    if body.len() > MAX_BODY_BYTES {
        return Err("body too large".into());
    }
    Ok(body)
}

fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn respond<T: Serialize>(request: Request, status: u16, body: &T) {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".into());
    let response = Response::from_string(text)
        .with_status_code(status)
        .with_header(
            Header::from_bytes("Content-Type", "application/json").expect("static header"),
        );
    let _ = request.respond(response);
}

fn refuse(request: Request, status: u16, code: &str, message: &str) {
    respond(
        request,
        status,
        &json!({ "ok": false, "code": code, "error": message }),
    );
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::tests::EchoDriver;
    use std::io::Write;
    use std::net::TcpStream;

    struct AllowEcho;
    impl Policy for AllowEcho {
        fn decide(&self, _plugin: &str, capability: &str) -> Decision {
            match capability {
                "echo.say" => Decision::Allowed,
                "echo.fail" => Decision::AskFirst,
                _ => Decision::Off,
            }
        }
    }

    fn bridge() -> BridgeHandle {
        let mut registry = Registry::new();
        registry.add(Box::new(EchoDriver));
        Bridge::spawn(Arc::new(registry), Arc::new(AllowEcho)).unwrap()
    }

    /// A tiny HTTP/1.1 client: the bridge's own callers are curl and tests.
    fn post(port: u16, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, Value) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut request = format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let status: u16 = raw[9..12].parse().unwrap();
        let json_body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
        (
            status,
            serde_json::from_str(json_body).unwrap_or(Value::Null),
        )
    }

    fn mint(bridge: &BridgeHandle, plugins: Value) -> Value {
        let (status, body) = post(
            bridge.port,
            "/handles",
            &[(HOST_SECRET_HEADER, &bridge.host_secret)],
            &json!({ "plugins": plugins }).to_string(),
        );
        assert_eq!(status, 200, "{body}");
        body
    }

    #[test]
    fn minting_needs_the_host_secret() {
        let bridge = bridge();
        let (status, body) = post(
            bridge.port,
            "/handles",
            &[(HOST_SECRET_HEADER, "wrong")],
            r#"{"plugins":[]}"#,
        );
        assert_eq!(status, 401);
        assert_eq!(body["code"], "unauthorized");
        let (status, _) = post(bridge.port, "/handles", &[], r#"{"plugins":[]}"#);
        assert_eq!(status, 401);
    }

    #[test]
    fn a_plugin_calls_only_what_it_declared_and_the_policy_allows() {
        let bridge = bridge();
        let minted = mint(
            &bridge,
            json!([
                { "name": "Probe", "capabilities": ["echo.say", "echo.fail", "scanner.scan"] },
                { "name": "Quiet", "capabilities": [] }
            ]),
        );
        assert_eq!(minted["unsupported"], json!(["Probe: scanner.scan"]));
        let probe = minted["handles"]["Probe"].as_str().unwrap();
        let quiet = minted["handles"]["Quiet"].as_str().unwrap();
        assert_eq!(probe.len(), 64);
        assert_eq!(
            bridge.grants(),
            vec![
                (
                    "Probe".to_string(),
                    vec!["echo.fail".into(), "echo.say".into(), "scanner.scan".into()]
                ),
                ("Quiet".to_string(), vec![])
            ]
        );

        let (status, body) = post(
            bridge.port,
            "/native/echo.say",
            &[(HANDLE_HEADER, probe)],
            r#"{"hi":1}"#,
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["result"]["said"]["hi"], 1);

        let (status, body) = post(
            bridge.port,
            "/native/echo.say",
            &[(HANDLE_HEADER, quiet)],
            "{}",
        );
        assert_eq!(status, 403);
        assert_eq!(body["code"], "not_declared");

        let (status, body) = post(
            bridge.port,
            "/native/echo.fail",
            &[(HANDLE_HEADER, probe)],
            "{}",
        );
        assert_eq!(status, 403);
        assert_eq!(body["code"], "ask_first");

        let (status, body) = post(
            bridge.port,
            "/native/scanner.scan",
            &[(HANDLE_HEADER, probe)],
            "{}",
        );
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["code"], "off");

        let (status, body) = post(
            bridge.port,
            "/native/echo.say",
            &[(HANDLE_HEADER, "nope")],
            "{}",
        );
        assert_eq!(status, 401);
        assert_eq!(body["code"], "unauthorized");

        let (status, body) = post(
            bridge.port,
            "/native/echo.say",
            &[(HANDLE_HEADER, probe)],
            "not json",
        );
        assert_eq!(status, 400);
        assert_eq!(body["code"], "invalid_args");
    }

    #[test]
    fn minting_again_replaces_every_handle() {
        let bridge = bridge();
        let first = mint(
            &bridge,
            json!([{ "name": "Probe", "capabilities": ["echo.say"] }]),
        );
        let old = first["handles"]["Probe"].as_str().unwrap().to_string();
        mint(
            &bridge,
            json!([{ "name": "Probe", "capabilities": ["echo.say"] }]),
        );
        let (status, body) = post(
            bridge.port,
            "/native/echo.say",
            &[(HANDLE_HEADER, &old)],
            "{}",
        );
        assert_eq!(status, 401, "{body}");
    }

    #[test]
    fn the_host_secret_cannot_be_used_as_a_handle() {
        let bridge = bridge();
        mint(
            &bridge,
            json!([{ "name": "Probe", "capabilities": ["echo.say"] }]),
        );
        let (status, _) = post(
            bridge.port,
            "/native/echo.say",
            &[(HANDLE_HEADER, &bridge.host_secret)],
            "{}",
        );
        assert_eq!(status, 401);
    }
}
