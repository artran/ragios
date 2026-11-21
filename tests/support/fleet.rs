use super::*;
use std::{collections::BTreeSet, fmt::Write as _};

const HOSTS: usize = 134;
const SERVICES: usize = 537;
const SLOTS: usize = 12;
const OUTPUT_BYTES: usize = 96;

#[test]
fn shadow_fleet_catches_up_without_overlap_or_backlog_and_serves_freshness() {
    let dir = TempDir::new().unwrap();
    let path = prepare_fleet(&dir);
    let log = dir.path().join("starts.log");
    let mut app = Application::start(&path);
    wait_for_file(&log);
    let deadline = Instant::now() + Duration::from_secs(5);
    while runs(&log) < SLOTS {
        assert!(
            Instant::now() < deadline,
            "did not fill the execution slots"
        );
        thread::sleep(Duration::from_millis(20));
    }
    // More checks become due while the slots are full. The fixture will
    // observe any start beyond the limit while the gate is closed.
    thread::sleep(Duration::from_millis(300));

    // The first executions are held at the fixture gate. The request timeout
    // on Application's HTTP client bounds successful requests to one second.
    let overview = app.page("/", "537 checks are overdue");
    assert_eq!(overview.matches("OK (stale)").count(), SERVICES);
    let detail = app.page("/hosts/node-000/services/probe-0", "latest result is stale");
    assert!(detail.contains("saved OK"));
    fs::write(dir.path().join("gate"), "").unwrap();

    let marker = "[output truncated at 96 bytes]";
    let page = app.page_where("/status", "537 completed fixtures", |page| {
        page.matches(marker).count() == SERVICES
    });
    let captured = format!(
        "<pre>FLEET:{}\n{marker}</pre>",
        "x".repeat(OUTPUT_BYTES - 6)
    );
    assert_eq!(page.matches(&captured).count(), SERVICES);
    assert_eq!(page.matches("<td class=\"OK\">OK</td>").count(), SERVICES);
    assert!(!page.contains("checks are overdue"));
    drop(app);

    let events = fs::read_to_string(&log).unwrap();
    let mut started = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut times = Vec::new();
    let mut peak = 0;
    for line in events.lines() {
        let parts: Vec<_> = line.split_ascii_whitespace().collect();
        let key = (parts[1], parts[2]);
        match parts[0] {
            "start" => {
                assert!(active.insert(key), "check overlapped itself: {key:?}");
                assert!(started.insert(key), "missed intervals replayed: {key:?}");
                peak = peak.max(active.len());
                times.push(parts[3].parse::<f64>().unwrap());
            }
            "finish" => assert!(active.remove(&key), "finish without start: {key:?}"),
            event => panic!("unknown fixture event: {event}"),
        }
    }
    assert!(active.is_empty());
    assert_eq!(started.len(), SERVICES);
    assert_eq!(
        started
            .iter()
            .map(|(host, _)| host)
            .collect::<BTreeSet<_>>()
            .len(),
        HOSTS
    );
    assert_eq!(peak, SLOTS, "peak active checks under saturation");
    assert!(
        times[SERVICES * 3 / 4] - times[SERVICES / 4] > 1.5,
        "middle half of startup catch-up launched in a burst"
    );

    let mut restarted = Application::start(&path);
    let page = restarted.page("/status", marker);
    assert_eq!(page.matches(marker).count(), SERVICES);
    assert!(!page.contains("OK (stale)"));
    drop(restarted);
    assert_eq!(fs::read_to_string(log).unwrap(), events);
}

fn prepare_fleet(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("ragios.yaml");
    let executable = dir.path().join("check_fleet");
    fs::write(&executable, include_str!("../fixtures/check_fleet.py")).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let mut yaml = format!(
        "bind: 127.0.0.1:0\nplugin_dir: .\ndatabase: state.sqlite3\nemail: {{mode: disabled}}\n\
         interval_seconds: 300\ntimeout_seconds: 10\nstartup_spread_seconds: 5\n\
         max_concurrent_checks: {SLOTS}\nmax_output_bytes: {OUTPUT_BYTES}\nhosts:\n"
    );
    // Fixture setup only: place all prior results hours in the past so there
    // are dozens of missed intervals without making the test wait for them.
    let store = ragios::store::Store::open(&dir.path().join("state.sqlite3")).unwrap();
    let now = chrono::Utc::now();
    let old = (now - chrono::TimeDelta::hours(3)).to_rfc3339();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        for host in 0..HOSTS {
            let host = format!("node-{host:03}");
            writeln!(yaml, "  - id: {host}\n    name: {host}\n    services:").unwrap();
            for service in 0..(if host == "node-000" { 5 } else { 4 }) {
                let service = format!("probe-{service}");
                writeln!(
                    yaml,
                    "      - id: {service}\n        name: {service}\n        executable: check_fleet\n        args: [{:?}, {:?}, {host}, {service}]",
                    dir.path().join("starts.log").to_str().unwrap(),
                    dir.path().join("gate").to_str().unwrap()
                )
                .unwrap();
                store.save(
                    host.clone(),
                    service,
                    ragios::runner::CheckResult {
                        status: ragios::runner::Status::Ok,
                        output: "saved OK".into(),
                        started_at: old.clone(),
                        finished_at: old.clone(),
                        duration_ms: 5,
                    },
                    3,
                ).await.unwrap();
            }
        }
    });
    fs::write(&path, yaml).unwrap();
    path
}
