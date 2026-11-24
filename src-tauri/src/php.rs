//! The PHP runtime inside the app (ADR 0001, 2a). The FrankenPHP binary and
//! `php-host/` ship as bundle resources; the data directory is the app's own.
//! Status changes reach the window as `php:status` events, and the webview
//! talks to the host only through `php_request`, which stays on loopback.

use std::sync::Mutex;
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, Manager};
use whity_drivers::{Bridge, BridgeHandle, DefaultPolicy};
use whity_php_host::sidecar::simplify_path;
use whity_php_host::{proxy, Layout, PhpStatusEvent, Sidecar};

/// Managed state: the sidecar when it could be started, and the last status
/// either way, so a window opened later can ask instead of waiting for an event.
pub struct PhpHost {
    sidecar: Option<Sidecar>,
    last: Mutex<PhpStatusEvent>,
    /// The hardware bridge plugins call; `None` only when it failed to bind.
    bridge: Option<BridgeHandle>,
}

impl PhpHost {
    pub fn shutdown(&self) {
        if let Some(sidecar) = &self.sidecar {
            sidecar.shutdown();
        }
    }
}

/// Where the runtime is for this build: `frankenphp/` and `php-host/` next to
/// the app's resources, data under the app's data directory.
pub fn layout(app: &AppHandle) -> Result<Layout, String> {
    let resource = |name: &str| {
        app.path()
            .resolve(name, BaseDirectory::Resource)
            .map(|p| simplify_path(&p))
            .map_err(|e| format!("failed to resolve the {name} resource: {e}"))
    };
    Ok(Layout {
        frankenphp_dir: resource("frankenphp")?,
        php_host_dir: resource("php-host")?,
        data_dir: simplify_path(
            &app.path()
                .app_data_dir()
                .map_err(|e| e.to_string())?
                .join("php"),
        ),
    })
}

/// Start the host and keep its status. A missing runtime (a development build
/// before `npm run php:setup`) is reported as `notInstalled`, not an error:
/// everything else in the app works without PHP.
pub fn start(app: &AppHandle) -> PhpHost {
    let last = std::sync::Arc::new(Mutex::new(PhpStatusEvent::Starting));
    let listener = {
        let app = app.clone();
        let last = last.clone();
        move |event: PhpStatusEvent| {
            *last.lock().unwrap() = event.clone();
            let _ = app.emit("php:status", event);
        }
    };
    // The bridge first: the host gets its address and secret in its environment.
    let bridge = match Bridge::spawn(
        std::sync::Arc::new(whity_drivers::default_registry()),
        std::sync::Arc::new(DefaultPolicy),
    ) {
        Ok(bridge) => Some(bridge),
        Err(reason) => {
            eprintln!("[php] native bridge not started: {reason}");
            None
        }
    };
    let env = bridge.as_ref().map(BridgeHandle::env).unwrap_or_default();
    let sidecar = match layout(app).and_then(|layout| Sidecar::spawn(layout, env, listener)) {
        Ok(sidecar) => Some(sidecar),
        Err(reason) => {
            eprintln!("[php] not started: {reason}");
            let mut guard = last.lock().unwrap();
            if !matches!(*guard, PhpStatusEvent::NotInstalled) {
                *guard = PhpStatusEvent::Failed { message: reason };
            }
            None
        }
    };
    let snapshot = last.lock().unwrap().clone();
    PhpHost {
        sidecar,
        last: Mutex::new(snapshot),
        bridge,
    }
}

/// The host's current state, as the last `php:status` event.
#[tauri::command]
pub fn php_host_status(php: tauri::State<'_, PhpHost>) -> PhpStatusEvent {
    match &php.sidecar {
        Some(sidecar) if sidecar.is_ready() => PhpStatusEvent::Ready {
            port: sidecar.port(),
        },
        _ => php.last.lock().unwrap().clone(),
    }
}

/// One request to the host's plugin routes. `path` is a host path such as
/// `/api/demo-catalog`; the method and body are what the plugin route takes.
#[tauri::command]
pub async fn php_request(
    php: tauri::State<'_, PhpHost>,
    method: String,
    path: String,
    body: Option<serde_json::Value>,
) -> Result<proxy::PhpResponse, String> {
    let port = match &php.sidecar {
        Some(sidecar) if sidecar.is_ready() => sidecar.port(),
        // The exact string the webview matches to show "starting" instead of an error.
        _ => return Err("php-host-not-ready".into()),
    };
    proxy::forward(port, &method, &path, body).await
}

/// The plugins holding a hardware handle and the capabilities each declared,
/// for the settings screen to come.
#[tauri::command]
pub fn native_bridge_grants(php: tauri::State<'_, PhpHost>) -> Vec<(String, Vec<String>)> {
    php.bridge
        .as_ref()
        .map(BridgeHandle::grants)
        .unwrap_or_default()
}
