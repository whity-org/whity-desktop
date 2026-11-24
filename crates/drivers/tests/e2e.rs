//! Boots the real FrankenPHP host with the BridgeProbe fixture plugin and
//! drives calls from PHP, over curl, through the bridge, into the printer
//! driver: an Allowed call, an undeclared one, and one the device policy has
//! on Ask first, which becomes a proposal the test approves.
//! Needs `npm run php:setup` and `npm run sdk:copy` first, then:
//! `WHITY_PHP_HOST_SMOKE=1 cargo test -p whity-drivers --test e2e -- --ignored`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use whity_drivers::{Bridge, Decision, ProposalEvent, StoredPolicy};
use whity_php_host::{proxy, Layout, Sidecar};

#[test]
#[ignore]
fn a_plugin_reaches_the_printer_driver_only_as_the_device_allows() {
    if std::env::var_os("WHITY_PHP_HOST_SMOKE").is_none() {
        return;
    }
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = crate_dir.join("../..");
    let data = std::env::temp_dir().join(format!("whity-drivers-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();

    let policy = Arc::new(StoredPolicy::load(&data.join("hardware-policy.json")));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = events.clone();
    let bridge = Bridge::spawn(
        Arc::new(whity_drivers::default_registry()),
        policy.clone(),
        Arc::new(move |event: ProposalEvent| {
            sink.lock().unwrap().push(match event {
                ProposalEvent::Opened { .. } => "opened".into(),
                ProposalEvent::Answered { .. } => "answered".into(),
            })
        }),
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
    let call = |method: &str, path: &str| {
        runtime
            .block_on(proxy::forward(port, method, path, None))
            .unwrap()
    };

    let report = call("GET", "/__whity/native-bridge");
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

    // Allowed by the floor.
    let printers = call("GET", "/api/bridge-probe/printers");
    assert_eq!(printers.status, 200, "{printers:?}");
    assert_eq!(printers.body["ok"], true, "{printers:?}");
    assert!(
        printers.body["result"]["printers"].is_array(),
        "{printers:?}"
    );

    // Never declared.
    let print = call("POST", "/api/bridge-probe/print");
    assert_eq!(print.status, 403, "{print:?}");
    assert_eq!(print.body["code"], "not_declared", "{print:?}");

    // Ask first: the call becomes a proposal, the person approves, it runs.
    policy
        .set("BridgeProbe", "printer.list", Some(Decision::AskFirst))
        .unwrap();
    let asked = call("GET", "/api/bridge-probe/printers");
    assert_eq!(asked.status, 202, "{asked:?}");
    assert_eq!(asked.body["code"], "ask_first", "{asked:?}");
    let id = asked.body["proposalId"]
        .as_str()
        .expect("a proposal id")
        .to_string();

    let waiting = call("GET", &format!("/api/bridge-probe/proposals/{id}"));
    assert_eq!(waiting.body["outcome"]["state"], "waiting", "{waiting:?}");
    assert_eq!(bridge.proposals().list().len(), 1);

    let answered = bridge.proposals().answer(&id, true).unwrap();
    assert!(
        matches!(answered.outcome, whity_drivers::Outcome::Done { .. }),
        "{answered:?}"
    );
    let done = call("GET", &format!("/api/bridge-probe/proposals/{id}"));
    assert_eq!(done.body["outcome"]["state"], "done", "{done:?}");
    assert!(
        done.body["outcome"]["result"]["printers"].is_array(),
        "{done:?}"
    );
    assert_eq!(*events.lock().unwrap(), vec!["opened", "answered"]);

    // Off: refused outright, no proposal.
    policy
        .set("BridgeProbe", "printer.list", Some(Decision::Off))
        .unwrap();
    let off = call("GET", "/api/bridge-probe/printers");
    assert_eq!(off.status, 403, "{off:?}");
    assert_eq!(off.body["code"], "off", "{off:?}");
    assert_eq!(bridge.proposals().list().len(), 1);

    // A made-up proposal id is not this plugin's.
    let unknown = call(
        "GET",
        &format!("/api/bridge-probe/proposals/{}", "0".repeat(64)),
    );
    assert_eq!(unknown.status, 404, "{unknown:?}");

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
