//! Spawns and supervises the FrankenPHP child that serves `php-host/`.
//!
//! The binary is started by absolute path from its own directory: the Windows
//! release is `frankenphp.exe` plus the DLLs beside it, which Windows resolves
//! from the executable's directory first, so the directory ships whole. The
//! static Linux and macOS binaries are one file in the same place.
//!
//! One worker (`--worker <index.php>,1`): every worker runs the plugin
//! migrations at boot, and two of them racing on one SQLite file was a real
//! crash in the template this came from. A single person at one computer
//! needs one.
//!
//! Crash recovery restarts the child with a 1 s, 3 s, 8 s backoff and then
//! gives up. A deliberate [`Sidecar::restart`] (a plugin was installed; the
//! worker discovers plugins once, at boot) respawns at once, without the
//! backoff and without a `Crashed` event. Every spawn picks a fresh loopback
//! port and rewrites the sandbox ini, so a plugin that edited it gets the
//! locked version back before PHP reads it.

use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::sandbox;

/// Where the pieces are on this device.
#[derive(Clone, Debug)]
pub struct Layout {
    /// The directory holding `frankenphp` (or `frankenphp.exe`) and, on
    /// Windows, its DLLs, `ext/` and `php.ini`.
    pub frankenphp_dir: PathBuf,
    /// The `php-host/` tree: `public/index.php`, `src/`, `sdk/`, `vendor/`.
    pub php_host_dir: PathBuf,
    /// The app's writable data directory. The host's SQLite file, downloaded
    /// plugins, the sandbox ini, PHP's temp and its log all live under it.
    pub data_dir: PathBuf,
}

impl Layout {
    pub fn frankenphp_exe(&self) -> PathBuf {
        let name = if cfg!(windows) {
            "frankenphp.exe"
        } else {
            "frankenphp"
        };
        self.frankenphp_dir.join(name)
    }

    /// True when a runtime is present, so the app can say "not installed"
    /// instead of failing to spawn. A development build without
    /// `npm run php:setup` has none.
    pub fn is_installed(&self) -> bool {
        self.frankenphp_exe().is_file() && self.php_host_dir.join("public/index.php").is_file()
    }

    pub fn sqlite_path(&self) -> PathBuf {
        self.data_dir.join("whity-offline.sqlite")
    }

    /// Where plugins fetched from the store are installed: the second plugin
    /// root the host scans after its bundled `php-host/plugins/`.
    pub fn downloaded_plugins_root(&self) -> PathBuf {
        self.data_dir.join("plugins-downloaded")
    }
}

/// What the host is doing, for the app to show. Serialised as
/// `{"state": "ready", "port": 1234}` and so on.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum PhpStatusEvent {
    /// No FrankenPHP binary or no host code at the expected place.
    NotInstalled,
    Starting,
    Ready {
        port: u16,
    },
    Crashed {
        message: String,
    },
    Restarting {
        attempt: u32,
    },
    /// A deliberate reload, not a failure.
    Reloading,
    Failed {
        message: String,
    },
}

const BACKOFF_SECONDS: [u64; 3] = [1, 3, 8];
const READY_TIMEOUT: Duration = Duration::from_secs(30);

type Listener = Arc<dyn Fn(PhpStatusEvent) + Send + Sync>;

enum Signal {
    Terminated(String),
}

#[derive(Clone)]
struct SpawnContext {
    layout: Layout,
    extra_env: Vec<(String, String)>,
    listener: Listener,
}

/// A running (or recovering) FrankenPHP host. Dropping it does not stop the
/// child: call [`Sidecar::shutdown`] when the app exits.
pub struct Sidecar {
    child: Arc<Mutex<Option<Child>>>,
    ready: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    restarting: Arc<AtomicBool>,
    port: Arc<AtomicU16>,
    ctx: SpawnContext,
    signal_tx: mpsc::Sender<Signal>,
}

impl Sidecar {
    /// Start the host. `extra_env` is passed to PHP on top of the paths this
    /// crate sets (`WHITY_SQLITE_PATH`, `WHITY_DOWNLOADED_PLUGINS_ROOT`,
    /// `PHP_INI_SCAN_DIR`); the native bridge's address and secret arrive
    /// this way. `listener` hears every status change, on whichever thread
    /// observed it.
    pub fn spawn(
        layout: Layout,
        extra_env: Vec<(String, String)>,
        listener: impl Fn(PhpStatusEvent) + Send + Sync + 'static,
    ) -> Result<Sidecar, String> {
        let listener: Listener = Arc::new(listener);
        if !layout.is_installed() {
            listener(PhpStatusEvent::NotInstalled);
            return Err(format!(
                "no PHP runtime at {} or no host at {}",
                layout.frankenphp_exe().display(),
                layout.php_host_dir.display()
            ));
        }
        std::fs::create_dir_all(&layout.data_dir)
            .map_err(|e| format!("failed to create the data dir: {e}"))?;
        std::fs::create_dir_all(layout.downloaded_plugins_root())
            .map_err(|e| format!("failed to create the downloaded-plugins dir: {e}"))?;

        let ctx = SpawnContext {
            layout,
            extra_env,
            listener,
        };
        let ready = Arc::new(AtomicBool::new(false));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let restarting = Arc::new(AtomicBool::new(false));
        let port = Arc::new(AtomicU16::new(0));
        let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let (signal_tx, signal_rx) = mpsc::channel::<Signal>();

        (ctx.listener)(PhpStatusEvent::Starting);
        do_spawn(&ctx, &child, &ready, &port, signal_tx.clone())?;

        {
            let ctx = ctx.clone();
            let child = child.clone();
            let ready = ready.clone();
            let shutting_down = shutting_down.clone();
            let restarting = restarting.clone();
            let port = port.clone();
            let respawn_tx = signal_tx.clone();
            thread::Builder::new()
                .name("whity-php-supervisor".into())
                .spawn(move || {
                    let mut attempt = 0usize;
                    while let Ok(Signal::Terminated(message)) = signal_rx.recv() {
                        ready.store(false, Ordering::SeqCst);
                        if shutting_down.load(Ordering::SeqCst) {
                            break;
                        }
                        if restarting.swap(false, Ordering::SeqCst) {
                            attempt = 0;
                            if let Err(e) =
                                do_spawn(&ctx, &child, &ready, &port, respawn_tx.clone())
                            {
                                (ctx.listener)(PhpStatusEvent::Failed { message: e });
                                break;
                            }
                            continue;
                        }
                        (ctx.listener)(PhpStatusEvent::Crashed {
                            message: message.clone(),
                        });
                        if attempt >= BACKOFF_SECONDS.len() {
                            (ctx.listener)(PhpStatusEvent::Failed {
                                message: format!(
                                    "gave up after {} restart attempts: {message}",
                                    BACKOFF_SECONDS.len()
                                ),
                            });
                            break;
                        }
                        (ctx.listener)(PhpStatusEvent::Restarting {
                            attempt: attempt as u32 + 1,
                        });
                        thread::sleep(Duration::from_secs(BACKOFF_SECONDS[attempt]));
                        attempt += 1;
                        if shutting_down.load(Ordering::SeqCst) {
                            break;
                        }
                        if let Err(e) = do_spawn(&ctx, &child, &ready, &port, respawn_tx.clone()) {
                            (ctx.listener)(PhpStatusEvent::Failed { message: e });
                            break;
                        }
                    }
                })
                .map_err(|e| format!("failed to start the php supervisor thread: {e}"))?;
        }

        Ok(Sidecar {
            child,
            ready,
            shutting_down,
            restarting,
            port,
            ctx,
            signal_tx,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// The loopback port of the current child, 0 before the first spawn.
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    pub fn layout(&self) -> &Layout {
        &self.ctx.layout
    }

    /// Kill the child and stop the supervisor. Safe to call twice.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Reload the host so it discovers plugins installed since it started.
    pub fn restart(&self) {
        self.restarting.store(true, Ordering::SeqCst);
        self.ready.store(false, Ordering::SeqCst);
        (self.ctx.listener)(PhpStatusEvent::Reloading);
        match self.child.lock().unwrap().take() {
            Some(mut child) => {
                let _ = child.kill();
            }
            None => {
                self.restarting.store(false, Ordering::SeqCst);
                let _ = do_spawn(
                    &self.ctx,
                    &self.child,
                    &self.ready,
                    &self.port,
                    self.signal_tx.clone(),
                );
            }
        }
    }
}

fn do_spawn(
    ctx: &SpawnContext,
    child: &Arc<Mutex<Option<Child>>>,
    ready: &Arc<AtomicBool>,
    port: &Arc<AtomicU16>,
    signal_tx: mpsc::Sender<Signal>,
) -> Result<(), String> {
    let new_port = pick_free_port()?;
    port.store(new_port, Ordering::SeqCst);
    let new_child = spawn_attempt(ctx, new_port)?;
    let pid = new_child.id();
    *child.lock().unwrap() = Some(new_child);
    spawn_exit_watcher(child.clone(), pid, signal_tx);
    let ready = ready.clone();
    let listener = ctx.listener.clone();
    thread::spawn(move || {
        if poll_readiness(new_port, READY_TIMEOUT) {
            ready.store(true, Ordering::SeqCst);
            listener(PhpStatusEvent::Ready { port: new_port });
        } else {
            listener(PhpStatusEvent::Failed {
                message: format!(
                    "php-host did not answer on port {new_port} within {}s",
                    READY_TIMEOUT.as_secs()
                ),
            });
        }
    });
    Ok(())
}

fn spawn_attempt(ctx: &SpawnContext, port: u16) -> Result<Child, String> {
    let layout = &ctx.layout;
    let ini_dir = sandbox::install(&layout.php_host_dir, &layout.data_dir)?;
    let public_dir = simplify_path(&layout.php_host_dir.join("public"));
    let index_php = public_dir.join("index.php");

    let mut command = Command::new(simplify_path(&layout.frankenphp_exe()));
    command
        .current_dir(simplify_path(&layout.frankenphp_dir))
        .args([
            "php-server",
            "--root",
            &public_dir.to_string_lossy(),
            "--listen",
            &format!("127.0.0.1:{port}"),
            "--worker",
            &format!("{},1", index_php.to_string_lossy()),
        ])
        .env("PHP_INI_SCAN_DIR", simplify_path(&ini_dir))
        .env("WHITY_SQLITE_PATH", simplify_path(&layout.sqlite_path()))
        .env("WHITY_OFFLINE_TENANT_ID", "1")
        .env(
            "WHITY_DOWNLOADED_PLUGINS_ROOT",
            simplify_path(&layout.downloaded_plugins_root()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &ctx.extra_env {
        command.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn frankenphp: {e}"))?;

    #[cfg(target_os = "windows")]
    crate::job_object::assign(child.id());

    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                eprintln!("[frankenphp] {line}");
            }
        });
    }

    Ok(child)
}

/// Watches the child in `slot` and signals the supervisor when it is gone, or
/// when `shutdown()`/`restart()` took it out of the slot. `try_wait` under the
/// lock is non-blocking, so `kill()` never waits on this thread. The pid check
/// keeps a watcher from reporting on a successor it did not start with.
fn spawn_exit_watcher(slot: Arc<Mutex<Option<Child>>>, pid: u32, signal_tx: mpsc::Sender<Signal>) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_millis(250));
        let gone = {
            let mut guard = slot.lock().unwrap();
            match guard.as_mut() {
                None => Some(format!("frankenphp (pid {pid}) was stopped")),
                Some(child) if child.id() != pid => return,
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => Some(format!("frankenphp (pid {pid}) exited: {status}")),
                    Ok(None) => None,
                    Err(e) => Some(format!(
                        "frankenphp (pid {pid}) could not be waited on: {e}"
                    )),
                },
            }
        };
        if let Some(message) = gone {
            let _ = signal_tx.send(Signal::Terminated(message));
            return;
        }
    });
}

/// GET `/__whity/health` on loopback until it answers 200 or the deadline
/// passes. Plain sockets: no HTTP client on the readiness path.
pub fn poll_readiness(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if health_ok(port) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(300));
    }
}

fn health_ok(port: u16) -> bool {
    use std::io::{Read, Write};
    let Ok(mut stream) =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if stream
        .write_all(b"GET /__whity/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut head = [0u8; 64];
    let Ok(n) = stream.read(&mut head) else {
        return false;
    };
    std::str::from_utf8(&head[..n])
        .map(|s| s.starts_with("HTTP/1.1 200") || s.starts_with("HTTP/1.0 200"))
        .unwrap_or(false)
}

/// Bind an OS-assigned loopback port and hand its number to FrankenPHP. The
/// window between dropping the listener and the child binding is accepted:
/// the supervisor picks a new port on every respawn.
fn pick_free_port() -> Result<u16, String> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|e| format!("failed to pick a free port: {e}"))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("failed to read the bound port: {e}"))
}

/// Strip Windows' `\\?\` prefix: PHP cannot open a verbatim path even though
/// the OS can. No-op elsewhere.
pub fn simplify_path(path: &Path) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(stripped) => PathBuf::from(stripped),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(
            simplify_path(Path::new(r"\\?\C:\x\y")),
            PathBuf::from(r"C:\x\y")
        );
        assert_eq!(simplify_path(Path::new("/usr/x")), PathBuf::from("/usr/x"));
    }

    #[test]
    fn a_missing_runtime_is_not_installed_and_does_not_spawn() {
        let dir =
            std::env::temp_dir().join(format!("whity-php-host-missing-{}", std::process::id()));
        let layout = Layout {
            frankenphp_dir: dir.join("frankenphp"),
            php_host_dir: dir.join("php-host"),
            data_dir: dir.join("data"),
        };
        assert!(!layout.is_installed());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let result = Sidecar::spawn(layout, vec![], move |e| sink.lock().unwrap().push(e));
        assert!(result.is_err());
        assert_eq!(*seen.lock().unwrap(), vec![PhpStatusEvent::NotInstalled]);
    }

    #[test]
    fn readiness_waits_for_a_200_on_the_health_path() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let status = if request.url() == "/__whity/health" {
                    200
                } else {
                    404
                };
                let _ = request.respond(
                    tiny_http::Response::from_string("{\"ok\":true}").with_status_code(status),
                );
            }
        });
        assert!(poll_readiness(port, Duration::from_secs(5)));
    }

    #[test]
    fn readiness_gives_up_when_nothing_listens() {
        let port = pick_free_port().unwrap();
        let started = Instant::now();
        assert!(!poll_readiness(port, Duration::from_millis(700)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn status_events_serialise_with_a_state_tag() {
        let json = serde_json::to_string(&PhpStatusEvent::Ready { port: 4321 }).unwrap();
        assert_eq!(json, r#"{"state":"ready","port":4321}"#);
        assert_eq!(
            serde_json::to_string(&PhpStatusEvent::NotInstalled).unwrap(),
            r#"{"state":"notInstalled"}"#
        );
    }
}

/// Boots the real runtime. Needs `npm run php:setup` and `npm run sdk:copy`
/// first, then: `WHITY_PHP_HOST_SMOKE=1 cargo test -p whity-php-host -- --ignored`.
#[cfg(test)]
mod smoke {
    use super::*;

    #[test]
    #[ignore]
    fn the_real_runtime_answers_health_and_plugins() {
        if std::env::var_os("WHITY_PHP_HOST_SMOKE").is_none() {
            return;
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let data =
            std::env::temp_dir().join(format!("whity-php-host-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let layout = Layout {
            frankenphp_dir: repo.join("src-tauri/resources/frankenphp"),
            php_host_dir: repo.join("php-host"),
            data_dir: data.clone(),
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let sidecar = Sidecar::spawn(layout, vec![], move |e| {
            eprintln!("event: {e:?}");
            sink.lock().unwrap().push(e);
        })
        .expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(40);
        while !sidecar.is_ready() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(200));
        }
        assert!(sidecar.is_ready(), "events: {:?}", events.lock().unwrap());
        let port = sidecar.port();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let plugins = runtime
            .block_on(crate::proxy::forward(port, "GET", "/__whity/plugins", None))
            .unwrap();
        assert_eq!(plugins.status, 200, "{plugins:?}");
        assert!(plugins.body["loaded"].is_array());
        let missing = runtime
            .block_on(crate::proxy::forward(
                port,
                "GET",
                "/api/nothing-here",
                None,
            ))
            .unwrap();
        assert_eq!(missing.status, 404);
        assert!(data.join("php-ini/whity-sandbox.ini").is_file());
        let sandbox = runtime
            .block_on(crate::proxy::forward(port, "GET", "/__whity/sandbox", None))
            .unwrap();
        assert_eq!(sandbox.status, 200, "{sandbox:?}");
        assert_eq!(sandbox.body["allowUrlFopen"], false, "{sandbox:?}");
        assert!(
            sandbox.body["openBasedir"]
                .as_str()
                .unwrap_or("")
                .contains("php-host"),
            "{sandbox:?}"
        );
        assert!(
            sandbox.body["disabledFunctions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "exec"),
            "{sandbox:?}"
        );
        sidecar.shutdown();
        thread::sleep(Duration::from_millis(500));
        assert!(!health_ok(port), "frankenphp should be gone after shutdown");
        let _ = std::fs::remove_dir_all(&data);
    }
}
