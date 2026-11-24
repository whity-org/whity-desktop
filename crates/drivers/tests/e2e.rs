//! Boots the real FrankenPHP host with the BridgeProbe fixture plugin and
//! drives a call from PHP, over curl, through the bridge, into the printer
//! driver. Needs `npm run php:setup` and `npm run sdk:copy` first, then:
//! `WHITY_PHP_HOST_SMOKE=1 cargo test -p whity-drivers --test e2e -- --ignored`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use whity_drivers::{Bridge, DefaultPolicy};
use whity_php_host::{proxy, Layout, Sidecar};

#[test]
#[ignore]
fn a_plugin_reaches_the_printer_driver_only_for_what_it_declared() {
    if std::env::var_os("WHITY_PHP_HOST_SMOKE").is_none() {
        return;
    }
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = crate_dir.join("../..");
    let data = std::env::temp_dir().join(format!("whity-drivers-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();

    let bridge = Bridge::spawn(
        Arc::new(whity_drivers::default_registry()),
        Arc::new(DefaultPolicy),
    )
    .unwrap();

    // The fixture plugin is the host's "downloaded" root for this run.
    let plugins_root = data.join("plugins-downloaded");
    std::fs::create_dir_all(&plugins_root).unwrap();
    copy_dir(
        &crate_dir.join("tests/fixtures/plugins/BridgeProbe"),
        &plugins_root.join("BridgeProbe"),
    );

    let layout = Layout {
        frankenphp_dir: repo.join("src-tauri/resources/frankenphp"),
        php_host_dir: repo.join("php-host"),
        data_dir: data.clone(),
    };
    let sidecar =
        Sidecar::spawn(layout, bridge.env(), |e| eprintln!("event: {e:?}")).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(40);
    while !sidecar.is_ready() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(sidecar.is_ready());
    let port = sidecar.port();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let get = |path: &str| {
        runtime
            .block_on(proxy::forward(port, "GET", path, None))
            .unwrap()
    };

    let report = get("/__whity/native-bridge");
    assert_eq!(report.status, 200, "{report:?}");
    assert_eq!(report.body["available"], true, "{report:?}");
    assert_eq!(
        report.body["plugins"]["BridgeProbe"],
        serde_json::json!(["printer.list"]),
        "{report:?}"
    );
    assert_eq!(
        bridge.grants(),
        vec![("BridgeProbe".to_string(), vec!["printer.list".to_string()])]
    );

    let printers = get("/api/bridge-probe/printers");
    assert_eq!(printers.status, 200, "{printers:?}");
    assert_eq!(printers.body["ok"], true, "{printers:?}");
    assert!(
        printers.body["result"]["printers"].is_array(),
        "{printers:?}"
    );

    let print = runtime
        .block_on(proxy::forward(
            port,
            "POST",
            "/api/bridge-probe/print",
            None,
        ))
        .unwrap();
    assert_eq!(print.status, 403, "{print:?}");
    assert_eq!(print.body["code"], "not_declared", "{print:?}");

    sidecar.shutdown();
    let _ = std::fs::remove_dir_all(&data);
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
