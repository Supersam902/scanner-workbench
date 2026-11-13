#![cfg(all(target_os = "windows", target_arch = "x86"))]

use obd_dashboard::diagnostics::{PidSupport, Query, Reading, catalog};
use obd_dashboard::j2534::{CanProtocol, DriverInfo, ProbePhase};
use obd_dashboard::scanner::{
    model::{Results, Sample},
    worker::{
        AttemptOutcome, Command, CommandSender, ConnectionAttempt, Event, ReadMode,
        spawn_with_driver, spawn_with_driver_state,
    },
};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

fn fixture(mode: u8) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "scanner-driver-{}-{}-{mode}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let dll = dir.join("mongoose_fixture.dll");
    let compiler = std::env::var_os("CC_i686_pc_windows_gnu")
        .unwrap_or_else(|| "i686-w64-mingw32-gcc.exe".into());
    assert!(
        std::process::Command::new(compiler)
            .args(["-shared", "-Wall", "-Wextra", "-Werror", "-Wl,--kill-at"])
            .arg(format!("-DFIXTURE_MODE={mode}"))
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scanner_driver.c"))
            .arg("-o")
            .arg(&dll)
            .status()
            .unwrap()
            .success()
    );
    dll
}

fn marks(dll: &Path, kind: &str) -> u64 {
    std::fs::metadata(dll.with_extension(format!("dll.{kind}"))).map_or(0, |m| m.len())
}

fn wait_for_marker(dll: &Path, kind: &str, count: u64, timeout: Duration) {
    let until = Instant::now() + timeout;
    while marks(dll, kind) < count {
        assert!(
            Instant::now() < until,
            "missing driver fixture marker {kind}"
        );
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn worker(path: Arc<Mutex<PathBuf>>) -> (CommandSender, mpsc::Receiver<Event>) {
    worker_with_drivers(move || {
        let path = path.lock().unwrap();
        vec![
            driver_entry("TIS Techstream VIM", &path),
            driver_entry("Mongoose-Plus MFC3", &path),
        ]
    })
}

fn driver_entry(name: &str, path: &Path) -> DriverInfo {
    DriverInfo {
        name: name.into(),
        path: path.to_owned(),
    }
}

fn worker_with_drivers(
    drivers: impl Fn() -> Vec<DriverInfo> + Send + 'static,
) -> (CommandSender, mpsc::Receiver<Event>) {
    spawn_with_driver(
        Some(env!("CARGO_BIN_EXE_scanner-workstation").into()),
        drivers,
    )
}

fn worker_with_saved_connection(
    state: &Path,
    drivers: Vec<DriverInfo>,
) -> (CommandSender, mpsc::Receiver<Event>) {
    spawn_with_driver_state(
        Some(env!("CARGO_BIN_EXE_scanner-workstation").into()),
        Some(state.to_owned()),
        move || drivers.clone(),
    )
}

fn send(tx: &CommandSender, command: Command) {
    assert!(tx.try_send(command).is_ok());
}

fn until(rx: &mpsc::Receiver<Event>, timeout: Duration, matches: impl Fn(&Event) -> bool) -> Event {
    let deadline = Instant::now() + timeout;
    loop {
        let event = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("scanner worker did not produce the expected event");
        if matches(&event) {
            return event;
        }
    }
}

fn connect(tx: &CommandSender, id: u64) {
    send(
        tx,
        Command::Connect {
            id,
            protocol: None,
            ecu: None,
            cancel: Arc::new(AtomicBool::new(false)),
        },
    );
}

fn next_protocol_attempt(rx: &mpsc::Receiver<Event>, id: u64) -> ConnectionAttempt {
    match until(rx, Duration::from_secs(8), |event| match event {
        Event::ConnectAttempt(actual, attempt) => *actual == id && attempt.protocol.is_some(),
        Event::Failed(actual, _) => *actual == id,
        _ => false,
    }) {
        Event::ConnectAttempt(_, attempt) => attempt,
        Event::Failed(_, reason) => panic!("connection failed before a protocol result: {reason}"),
        _ => unreachable!(),
    }
}

fn connected(rx: &mpsc::Receiver<Event>, id: u64) -> PidSupport {
    connected_as(rx, id, "Mongoose-Plus MFC3", Duration::from_secs(8))
}

fn connected_as(
    rx: &mpsc::Receiver<Event>,
    id: u64,
    expected_driver: &str,
    timeout: Duration,
) -> PidSupport {
    let event = until(rx, timeout, |event| {
        matches!(
            event,
            Event::Connected { id: actual, .. } | Event::Failed(actual, _) if *actual == id
        )
    });
    match event {
        Event::Connected {
            driver,
            protocol,
            demo,
            pid_support,
            ..
        } => {
            assert!(!demo);
            assert_eq!(driver.as_deref(), Some(expected_driver));
            assert_eq!(protocol, CanProtocol::Can29Bit500K.label());
            pid_support
        }
        Event::Failed(_, reason) => panic!("connection failed: {reason}"),
        _ => unreachable!(),
    }
}

#[test]
fn connect_reuses_the_probe_and_checks_announced_live_pid_ranges() {
    let dll = fixture(6);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    let seed = connected(&rx, 1);
    assert_eq!(
        seed.reported.into_iter().collect::<Vec<_>>(),
        [1, 5, 12, 13]
    );
    assert_eq!(seed.checked_through, 0x1f);
    assert_eq!(seed.next_range, Some(0x20));

    let Event::LivePidSupport(_, extended) = until(
        &rx,
        Duration::from_secs(6),
        |event| matches!(event, Event::LivePidSupport(1, coverage) if coverage.next_range.is_none()),
    ) else {
        unreachable!()
    };
    assert_eq!(
        extended.reported.into_iter().collect::<Vec<_>>(),
        [1, 5, 12, 13, 0x21, 0x42]
    );
    assert_eq!(extended.checked_through, u8::MAX);
    assert!(extended.problem.is_none());
    assert_eq!(marks(&dll, "pidmap-00"), 1);
    assert_eq!(marks(&dll, "pidmap-20"), 1);
    assert_eq!(marks(&dll, "pidmap-40"), 1);
}

#[test]
fn reconnect_uses_the_saved_protocol_and_rechecks_it_before_falling_back() {
    let extended = fixture(0);
    let standard = fixture(21);
    let state = extended.parent().unwrap().join("state");
    let cache = state.join("last-connection.json");
    let entry = |name| vec![driver_entry(name, &extended)];
    {
        let (tx, rx) = worker_with_saved_connection(&state, entry("Mongoose-Plus MFC3"));
        connect(&tx, 1);
        let first = next_protocol_attempt(&rx, 1);
        assert_eq!(first.protocol, Some(CanProtocol::Can11Bit500K));
        assert_eq!(
            first.outcome,
            AttemptOutcome::ProbeFailed(ProbePhase::Channel)
        );
        assert!(!first.remembered_protocol);
        let second = next_protocol_attempt(&rx, 1);
        assert_eq!(second.protocol, Some(CanProtocol::Can29Bit500K));
        assert_eq!(second.outcome, AttemptOutcome::Verified);
        connected(&rx, 1);
        send(&tx, Command::Disconnect { id: 2 });
        until(&rx, Duration::from_secs(4), |event| {
            matches!(event, Event::Disconnected(2))
        });
    }
    let original = std::fs::read(&cache).unwrap();
    {
        // The registry display name can change while the installed DLL stays the same.
        let (tx, rx) = worker_with_saved_connection(&state, entry("Mongoose-Plus MFC3 alias"));
        send(
            &tx,
            Command::Connect {
                id: 1,
                protocol: Some(CanProtocol::Can11Bit500K),
                ecu: None,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let manual = next_protocol_attempt(&rx, 1);
        assert_eq!(manual.protocol, Some(CanProtocol::Can11Bit500K));
        assert!(!manual.remembered_protocol);
        until(&rx, Duration::from_secs(4), |event| {
            matches!(event, Event::Failed(1, _))
        });
        assert_eq!(std::fs::read(&cache).unwrap(), original);
        connect(&tx, 2);
        let first = next_protocol_attempt(&rx, 2);
        assert_eq!(first.protocol, Some(CanProtocol::Can29Bit500K));
        assert!(first.remembered_protocol);
        assert_eq!(first.outcome, AttemptOutcome::Verified);
        connected_as(&rx, 2, "Mongoose-Plus MFC3 alias", Duration::from_secs(4));
        send(&tx, Command::Disconnect { id: 3 });
        until(&rx, Duration::from_secs(4), |event| {
            matches!(event, Event::Disconnected(3))
        });
    }

    // Use the same registered path with a fixture that answers only standard CAN.
    std::fs::copy(&standard, &extended).unwrap();
    let (tx, rx) = worker_with_saved_connection(&state, entry("Mongoose-Plus MFC3"));
    connect(&tx, 1);
    let first = next_protocol_attempt(&rx, 1);
    assert_eq!(first.protocol, Some(CanProtocol::Can29Bit500K));
    assert!(first.remembered_protocol);
    assert_eq!(
        first.outcome,
        AttemptOutcome::ProbeFailed(ProbePhase::Channel)
    );
    let second = next_protocol_attempt(&rx, 1);
    assert_eq!(second.protocol, Some(CanProtocol::Can11Bit500K));
    assert_eq!(second.outcome, AttemptOutcome::Verified);
    assert!(!second.remembered_protocol);
    until(&rx, Duration::from_secs(4), |event| {
        matches!(event,
            Event::Connected { id: 1, protocol, .. } if protocol == CanProtocol::Can11Bit500K.label()
        )
    });
    send(&tx, Command::Disconnect { id: 2 });
    until(&rx, Duration::from_secs(4), |event| {
        matches!(event, Event::Disconnected(2))
    });
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(cache).unwrap()).unwrap();
    assert_eq!(saved["protocol"], "Can11Bit500K");
}

#[test]
fn an_ecu_missing_after_raw_discovery_keeps_pid_coverage_unchecked() {
    let dll = fixture(7);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll)));
    connect(&tx, 1);
    let support = connected(&rx, 1);
    assert_eq!(support.reported.into_iter().collect::<Vec<_>>(), [1, 12]);
    assert_eq!(support.checked_through, 0);
    assert!(support.problem.unwrap().contains("18DAF145"));
}

fn read(tx: &CommandSender, id: u64, bytes: &[u8], control: bool) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    send(
        tx,
        Command::Read {
            id,
            queries: vec![Query {
                bytes: bytes.to_vec(),
                name: "Fixture read".into(),
                target: None,
                signals: vec![],
                verified: false,
            }],
            discovery: None,
            mode: if control {
                ReadMode::Control
            } else {
                ReadMode::Diagnostic
            },
            cancel: cancel.clone(),
        },
    );
    cancel
}

fn live_results(
    tx: &CommandSender,
    rx: &mpsc::Receiver<Event>,
    id: u64,
    group: bool,
) -> (Vec<Reading>, bool) {
    let catalog = catalog(include_str!("../data/obdb/saej1979.json"), None).unwrap();
    let queries = [5, 12, 13]
        .into_iter()
        .map(|pid| {
            catalog
                .iter()
                .find(|q| q.bytes == [1, pid])
                .unwrap()
                .clone()
        })
        .collect();
    send(
        tx,
        Command::Read {
            id,
            queries,
            discovery: None,
            mode: ReadMode::Live { group },
            cancel: Arc::new(AtomicBool::new(false)),
        },
    );
    let mut rows = Vec::new();
    let mut fell_back = false;
    let mut finished_pids = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            Event::LiveRows(op, _, new) if op == id => rows.extend(new),
            Event::LiveGroupingFallback(op) if op == id => fell_back = true,
            Event::Progress(op, done, 3, _) if op == id => finished_pids = done,
            Event::Finished(op, _) if op == id => break,
            Event::LinkLost(_, reason) | Event::Failed(_, reason) => panic!("{reason}"),
            _ => {}
        }
    }
    assert_eq!(finished_pids, 3);
    (rows, fell_back)
}

#[test]
fn live_group_accepts_reordered_pids_and_different_ecu_subsets() {
    let dll = fixture(10);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    connected(&rx, 1);
    let (rows, fell_back) = live_results(&tx, &rx, 2, true);
    assert!(!fell_back);
    assert_eq!(marks(&dll, "group-live"), 1);
    assert_eq!(marks(&dll, "single-05"), 0);
    assert_eq!(marks(&dll, "single-0c"), 0);
    assert_eq!(marks(&dll, "single-0d"), 0);
    assert_eq!(
        rows.iter()
            .map(|r| (r.source, r.request.as_str(), r.numeric, r.raw.as_str()))
            .collect::<Vec<_>>(),
        [
            (Some(0x18DAF127), "0105", Some(88.0), "410580"),
            (Some(0x18DAF127), "010C", Some(800.0), "410C0C80"),
            (Some(0x18DAF145), "010C", Some(1000.0), "410C0FA0"),
            (Some(0x18DAF127), "010D", Some(43.0), "410D2B"),
        ],
    );
    let (individual, fell_back) = live_results(&tx, &rx, 3, false);
    assert!(!fell_back);
    assert_eq!(individual.len(), rows.len());
    assert_eq!(marks(&dll, "group-live"), 1);
    assert_eq!(marks(&dll, "single-05"), 1);
    assert_eq!(marks(&dll, "single-0c"), 1);
    assert_eq!(marks(&dll, "single-0d"), 1);
}

#[test]
fn malformed_live_group_retries_only_the_affected_pid_and_keeps_the_good_ecu() {
    let dll = fixture(11);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    connected(&rx, 1);
    let (rows, fell_back) = live_results(&tx, &rx, 2, true);
    assert!(fell_back);
    assert_eq!(rows.len(), 4);
    assert!(
        rows.iter()
            .any(|r| r.source == Some(0x18DAF127) && r.raw == "410C0C80")
    );
    assert!(
        rows.iter()
            .any(|r| r.source == Some(0x18DAF145) && r.raw == "410C0FA0")
    );
    assert_eq!(marks(&dll, "group-live"), 1);
    assert_eq!(marks(&dll, "single-0c"), 1);
    assert_eq!(marks(&dll, "single-05"), 0);
    assert_eq!(marks(&dll, "single-0d"), 0);
    let (next, fell_back) = live_results(&tx, &rx, 3, true);
    assert!(!fell_back);
    assert_eq!(next.len(), rows.len());
    assert_eq!(marks(&dll, "group-live"), 1);
    assert_eq!(marks(&dll, "single-0c"), 2);
    assert_eq!(marks(&dll, "single-05"), 1);
    assert_eq!(marks(&dll, "single-0d"), 1);
}

#[test]
fn live_reads_report_a_silent_expected_ecu_without_marking_the_other_ecu_failed() {
    let dll = fixture(12);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    connected(&rx, 1);
    for id in [2, 3] {
        let (rows, fell_back) = live_results(&tx, &rx, id, true);
        assert_eq!(fell_back, id == 2);
        assert_eq!(rows.len(), 4);
        let unavailable: Vec<_> = rows.iter().filter(|r| r.value == "Unavailable").collect();
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].source, Some(0x18DAF145));
        assert_eq!(unavailable[0].request, "010C");
        assert_eq!(unavailable[0].status, "ECU did not return this PID");
        assert!(
            rows.iter().any(|r| r.source == Some(0x18DAF127)
                && r.request == "010C"
                && r.numeric.is_some())
        );
    }
    assert_eq!(marks(&dll, "group-live"), 1);
    assert_eq!(marks(&dll, "single-0c"), 2);
}

#[test]
fn live_reads_remember_an_ecu_that_replied_despite_incomplete_discovery() {
    let dll = fixture(20);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll)));
    connect(&tx, 1);
    let support = connected(&rx, 1);
    assert!(support.problem.is_some());
    assert_eq!(support.checked_through, 0);
    let (first, _) = live_results(&tx, &rx, 2, false);
    assert!(
        first.iter().any(|r| r.source == Some(0x18DAF145)
            && r.request == "010C"
            && r.numeric == Some(1000.0))
    );
    let (second, _) = live_results(&tx, &rx, 3, false);
    assert!(
        second.iter().any(|r| r.source == Some(0x18DAF145)
            && r.request == "010C"
            && r.value == "Unavailable")
    );
    assert!(
        second
            .iter()
            .any(|r| r.source == Some(0x18DAF127) && r.request == "010C" && r.numeric.is_some())
    );
}

#[test]
fn vehicle_reply_state_distinguishes_silence_from_partial_or_unverified_reads() {
    let dll = fixture(15);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    let support = connected(&rx, 1);
    assert_eq!(support.reported.into_iter().collect::<Vec<_>>(), [5, 12]);

    let Event::VehicleReply(1, initial) = until(&rx, Duration::from_secs(3), |event| {
        matches!(event, Event::VehicleReply(1, _))
    }) else {
        unreachable!()
    };
    assert!(!initial.missed);
    let mut last_reply = initial.last_reply;
    for (id, pid, missed, expected_raw) in [
        (2, 0x0c, Some(false), Some("410C0C80")),
        (3, 0x0e, None, None),
        (4, 0x05, None, None),
        (5, 0x0c, Some(true), None),
        (6, 0x0c, Some(false), Some("410C0C80")),
        (7, 0x00, Some(true), None),
        (8, 0x0f, Some(false), Some("7F0112")),
        (9, 0x00, Some(false), None),
    ] {
        if pid == 0 {
            send(
                &tx,
                Command::Read {
                    id,
                    queries: vec![],
                    discovery: None,
                    mode: ReadMode::CheckVehicleReply,
                    cancel: Arc::new(AtomicBool::new(false)),
                },
            );
        } else {
            read(&tx, id, &[1, pid], false);
        }
        let deadline = Instant::now() + Duration::from_secs(7);
        let mut states = Vec::new();
        let mut rows = Vec::new();
        let completion = loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::VehicleReply(1, state) => states.push(state),
                Event::Rows(op, new) if op == id => rows.extend(new),
                Event::Finished(op, message) if op == id => break message,
                Event::LinkLost(_, reason) | Event::Failed(_, reason) => panic!("{reason}"),
                _ => {}
            }
        };
        assert_eq!(
            states.iter().map(|state| state.missed).collect::<Vec<_>>(),
            missed.into_iter().collect::<Vec<_>>(),
            "PID {pid:02X} during operation {id}"
        );
        if let Some(state) = states.first() {
            if state.missed {
                assert_eq!(last_reply, state.last_reply);
            } else {
                assert!(state.last_reply > last_reply);
                last_reply = state.last_reply;
            }
        }
        if pid == 0 {
            assert!(rows.is_empty());
            assert_eq!(
                completion,
                if missed == Some(true) {
                    "No vehicle reply received."
                } else {
                    "Vehicle replied."
                }
            );
        } else if let Some(raw) = expected_raw {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].source, Some(0x18DAF127));
            assert_eq!(rows[0].raw, raw);
        } else {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].value, "Unavailable");
            assert!(rows[0].status.contains("no vehicle response"));
        }
    }
    assert_eq!(
        marks(&dll, "pidmap-00"),
        3,
        "only connect and the two explicit checks sent PID 00"
    );
}

#[test]
fn code_scan_marks_only_the_silent_ecu_and_category() {
    let dll = fixture(16);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    let event = until(&rx, Duration::from_secs(8), |event| {
        matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
    });
    assert!(
        matches!(event, Event::Connected { protocol, .. } if protocol == CanProtocol::Can11Bit500K.label())
    );

    let first = scan_codes(&tx, &rx, 2, false);
    assert!(
        first
            .iter()
            .any(|row| row.source == Some(0x7ea) && row.request == "03" && row.value == "P0300")
    );
    let second = scan_codes(&tx, &rx, 3, false);
    assert!(second.iter().any(|row| row.source == Some(0x7e8)
        && row.request == "03"
        && row.value == "No codes reported by this ECU"));
    assert!(
        second
            .iter()
            .any(|row| row.source == Some(0x7ea) && row.request == "0A" && row.value == "P0300")
    );
    assert_eq!(
        second
            .iter()
            .filter(|r| r.value == "Unavailable")
            .map(|r| { (r.source, r.request.as_str(), r.status.as_str()) })
            .collect::<Vec<_>>(),
        [
            (Some(0x7ea), "03", "ECU did not return this code list"),
            (
                Some(0x7e8),
                "0A",
                "ECU reports this code service is unsupported"
            ),
        ]
    );
    assert_eq!(
        marks(&dll, "code-count"),
        0,
        "CAN does not require a separate count check"
    );

    send(&tx, Command::Demo { id: 4 });
    until(&rx, Duration::from_secs(8), |event| {
        matches!(
            event,
            Event::Connected {
                id: 4,
                demo: true,
                ..
            }
        )
    });
    assert!(
        scan_codes(&tx, &rx, 5, false)
            .iter()
            .all(|row| row.source == Some(0x7e8) && row.value != "Unavailable")
    );
}

#[test]
fn legacy_code_scan_uses_fresh_per_ecu_counts_to_explain_silence() {
    let dll = fixture(17);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    let event = until(&rx, Duration::from_secs(8), |event| {
        matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
    });
    match event {
        Event::Connected { protocol, .. } => assert_eq!(protocol, CanProtocol::Iso9141.label()),
        Event::Failed(_, reason) => panic!("{reason}"),
        _ => unreachable!(),
    }
    assert_eq!(marks(&dll, "fast-kwp"), 1);
    assert_eq!(marks(&dll, "slow-iso"), 1);
    assert_eq!(marks(&dll, "slow-kwp"), 0);
    assert_eq!(marks(&dll, "keepalive"), 1);
    let first = scan_codes(&tx, &rx, 2, false);
    assert!(
        first
            .iter()
            .any(|r| r.source == Some(0x18) && r.request == "03" && r.value == "P0133")
    );
    assert!(first.iter().any(|r| r.source == Some(0x10)
        && r.request == "03"
        && r.value == "No codes reported by this ECU"
        && r.raw == "410100070000"));
    // The post-clear flow already includes PID 01. Move it ahead of the stored-code
    // request and reuse it, including for the ECU that reported a code last time.
    let second = scan_codes(&tx, &rx, 3, true);
    let stored: Vec<_> = second.iter().filter(|r| r.request == "03").collect();
    assert_eq!(stored.len(), 2);
    assert!(
        stored
            .iter()
            .all(|r| r.value == "No codes reported by this ECU" && r.raw == "410100070000")
    );
    assert_eq!(marks(&dll, "code-count"), 2);
    let third = scan_codes(&tx, &rx, 4, false);
    assert!(third.iter().any(|r| r.source == Some(0x18)
        && r.request == "03"
        && r.value == "Unavailable"
        && r.status == "PID 01 count was 1. Mode 03 returned 0 codes."));
}

#[test]
fn automatic_connect_verifies_fast_and_slow_kwp_before_starting_keepalive() {
    let definitions = catalog(include_str!("../data/obdb/saej1979.json"), None).unwrap();
    let rpm = definitions.iter().find(|q| q.bytes == [1, 0x0c]).unwrap();
    for (mode, expected_protocol) in [(22, CanProtocol::KwpFast), (23, CanProtocol::KwpSlow)] {
        let dll = fixture(mode);
        let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
        connect(&tx, 1);
        let event = until(&rx, Duration::from_secs(16), |event| {
            matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
        });
        match event {
            Event::Connected {
                protocol,
                pid_support,
                ..
            } => {
                assert_eq!(protocol, expected_protocol.label());
                assert!(pid_support.reported.contains(&0x0c));
            }
            Event::Failed(_, reason) => panic!("{expected_protocol:?}: {reason}"),
            _ => unreachable!(),
        }
        let slow = u64::from(expected_protocol == CanProtocol::KwpSlow);
        assert_eq!(marks(&dll, "fast-kwp"), 1);
        assert_eq!(marks(&dll, "slow-iso"), slow);
        assert_eq!(marks(&dll, "slow-kwp"), slow);
        assert_eq!(marks(&dll, "wrong-header"), 0);
        assert_eq!(marks(&dll, "pidmap-00"), 1);
        assert_eq!(marks(&dll, "keepalive"), 1);

        send(
            &tx,
            Command::Read {
                id: 2,
                queries: vec![rpm.clone()],
                discovery: None,
                mode: ReadMode::Diagnostic,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let event = until(&rx, Duration::from_secs(6), |event| {
            matches!(event, Event::Rows(2, _) | Event::Failed(2, _))
        });
        assert!(
            matches!(event, Event::Rows(2, rows) if rows.iter().any(|row|
            row.source == Some(0x10) && row.numeric == Some(800.0)))
        );
        send(&tx, Command::Disconnect { id: 3 });
        until(&rx, Duration::from_secs(4), |event| {
            matches!(event, Event::Disconnected(3))
        });
        assert_eq!(marks(&dll, "stop-keepalive"), 1);
        assert_eq!(marks(&dll, "disconnect"), 1 + 2 * slow);
        assert_eq!(marks(&dll, "close"), 1);
    }
}

#[test]
fn waiting_between_kline_attempts_keeps_voltage_live_and_can_be_cancelled() {
    let dll = fixture(23);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    let cancel = Arc::new(AtomicBool::new(false));
    send(
        &tx,
        Command::Connect {
            id: 1,
            protocol: None,
            ecu: None,
            cancel: cancel.clone(),
        },
    );
    until(&rx, Duration::from_secs(8), |event| {
        matches!(event,
            Event::ConnectProgress(1, progress) if progress == "Waiting for K-line to settle before ISO 9141-2"
        )
    });
    let reading = until(&rx, Duration::from_secs(2), |event| {
        matches!(event, Event::Voltage(Some(_)))
    });
    assert!(
        matches!(reading, Event::Voltage(Some(sample)) if sample.driver.path == dll && sample.millivolts == 12600)
    );
    cancel.store(true, Ordering::Relaxed);
    let event = until(&rx, Duration::from_secs(3), |event| {
        matches!(event, Event::Failed(1, _) | Event::Connected { id: 1, .. })
    });
    assert!(matches!(event, Event::Failed(_, reason) if reason == "Connection cancelled."));
    assert_eq!(marks(&dll, "fast-kwp"), 1);
    assert_eq!(marks(&dll, "slow-iso"), 0);
    assert_eq!(marks(&dll, "slow-kwp"), 0);
    assert_eq!(marks(&dll, "close"), 1);
}

fn scan_codes(
    tx: &CommandSender,
    rx: &mpsc::Receiver<Event>,
    id: u64,
    readiness: bool,
) -> Vec<Reading> {
    let queries = [vec![3], vec![7], vec![10]]
        .into_iter()
        .chain(readiness.then_some(vec![1, 1]))
        .map(|bytes| Query {
            bytes,
            target: None,
            name: "Code scan".into(),
            signals: vec![],
            verified: false,
        })
        .collect();
    send(
        tx,
        Command::Read {
            id,
            queries,
            discovery: None,
            mode: ReadMode::Scan,
            cancel: Arc::new(AtomicBool::new(false)),
        },
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut rows = Vec::new();
    loop {
        match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            Event::Rows(op, batch) if op == id => rows.extend(batch),
            Event::Finished(op, _) if op == id => return rows,
            Event::LinkLost(_, reason) | Event::Failed(_, reason) => panic!("{reason}"),
            _ => {}
        }
    }
}

#[test]
fn automatic_j1850_scans_preserve_legacy_ecu_data_and_freeze_frame() {
    for (mode, protocol) in [(28, CanProtocol::J1850Vpw), (29, CanProtocol::J1850Pwm)] {
        let dll = fixture(mode);
        let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
        connect(&tx, 1);
        let event = until(&rx, Duration::from_secs(8), |event| {
            matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
        });
        match event {
            Event::Connected {
                protocol: connected,
                pid_support,
                ..
            } => {
                assert_eq!(connected, protocol.label());
                assert!(pid_support.reported.contains(&12));
            }
            Event::Failed(_, reason) => panic!("{reason}"),
            _ => unreachable!(),
        }
        let definitions = catalog(include_str!("../data/obdb/saej1979.json"), None).unwrap();
        let mut queries = vec![
            definitions
                .iter()
                .find(|q| q.bytes == [1, 12])
                .unwrap()
                .clone(),
        ];
        for bytes in [vec![1, 1], vec![1, 0x41], vec![3], vec![7]] {
            queries.push(Query {
                bytes,
                name: "Emissions scan".into(),
                target: None,
                signals: Vec::new(),
                verified: false,
            });
        }
        send(
            &tx,
            Command::Read {
                id: 2,
                queries,
                discovery: None,
                mode: ReadMode::EmissionsScan,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(9);
        let mut rows = Vec::new();
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::Rows(2, batch) => rows.extend(batch),
                Event::Finished(2, _) => break,
                Event::LinkLost(_, reason) | Event::Failed(_, reason) => panic!("{reason}"),
                _ => {}
            }
        }
        assert!(rows.iter().any(|row| row.source == Some(0x10)
            && row.request == "03"
            && row.value == "P0133, P0171"));
        assert!(rows.iter().any(|row| row.source == Some(0x18)
            && row.request == "03"
            && row.value == "No codes reported by this ECU"));
        assert!(rows.iter().any(|row| row.source == Some(0x18)
            && row.request == "010C"
            && row.numeric == Some(1632.0)));
        assert!(rows.iter().any(|row| row.source == Some(0x10)
            && row.request == "0141"
            && row.value == "Not ready"));
        assert!(rows.iter().any(|row| row.source == Some(0x10)
            && row.request == "020200"
            && row.value == "P0133"));
        assert!(rows.iter().any(|row| row.source == Some(0x10)
            && row.request == "020500"
            && row.numeric == Some(88.0)));
        send(&tx, Command::Disconnect { id: 3 });
        until(&rx, Duration::from_secs(4), |event| {
            matches!(event, Event::Disconnected(3))
        });
        assert_eq!(marks(&dll, "pwm-node"), u64::from(mode == 29));
        assert_eq!(marks(&dll, "pwm-response"), u64::from(mode == 29));
        assert_eq!(marks(&dll, "unexpected-init"), 0);
        assert_eq!(marks(&dll, "unexpected-keepalive"), 0);
        assert_eq!(marks(&dll, "clear"), 0);
        assert_eq!(marks(&dll, "disconnect"), 1);
    }
}

#[test]
fn initial_vin_uses_the_verified_session_without_finishing_a_foreground_read() {
    let dll = fixture(21);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    assert!(matches!(
        until(&rx, Duration::from_secs(8), |event| {
            matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
        }),
        Event::Connected { protocol, .. } if protocol == CanProtocol::Can11Bit500K.label()
    ));
    for id in [0, 1] {
        send(
            &tx,
            Command::Read {
                id,
                queries: vec![],
                discovery: None,
                mode: ReadMode::Identify,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
    }
    send(
        &tx,
        Command::Read {
            id: 2,
            queries: vec![Query {
                bytes: vec![1, 1],
                target: None,
                name: "Readiness since clear".into(),
                signals: vec![],
                verified: false,
            }],
            discovery: None,
            mode: ReadMode::Scan,
            cancel: Arc::new(AtomicBool::new(false)),
        },
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut identified = false;
    let mut readiness = false;
    loop {
        match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            Event::InitialVin(1, rows) => {
                assert!(rows.iter().any(|row| row.source == Some(0x7e8)
                    && row.request == "0902"
                    && row.value == "JTDBR32E720123456"));
                identified = true;
            }
            Event::InitialVin(id, _) => panic!("VIN reply from the wrong connection: {id}"),
            Event::Rows(2, rows) => {
                readiness |= rows.iter().any(|row| row.request == "0101");
            }
            Event::Finished(2, _) => break,
            Event::Finished(id, _) => panic!("VIN read emitted a foreground completion: {id}"),
            Event::LinkLost(_, reason) | Event::Failed(_, reason) => panic!("{reason}"),
            _ => {}
        }
    }
    assert!(identified && readiness);
}

#[test]
fn emissions_scan_reads_drive_cycle_and_freeze_frame_while_flagging_missing_codes() {
    let dll = fixture(21);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    let event = until(&rx, Duration::from_secs(8), |event| {
        matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
    });
    match event {
        Event::Connected { protocol, .. } => {
            assert_eq!(protocol, CanProtocol::Can11Bit500K.label())
        }
        Event::Failed(_, reason) => panic!("{reason}"),
        _ => unreachable!(),
    }
    let queries = [
        vec![1, 1],
        vec![1, 0x41],
        vec![3],
        vec![7],
        vec![10],
        vec![9, 2],
    ]
    .into_iter()
    .map(|bytes| Query {
        bytes,
        target: None,
        name: "Emissions scan".into(),
        signals: vec![],
        verified: false,
    })
    .collect();
    send(
        &tx,
        Command::Read {
            id: 2,
            queries,
            discovery: None,
            mode: ReadMode::EmissionsScan,
            cancel: Arc::new(AtomicBool::new(false)),
        },
    );
    let deadline = Instant::now() + Duration::from_secs(14);
    let mut rows = Vec::new();
    loop {
        match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            Event::Rows(2, batch) => rows.extend(batch),
            Event::Finished(2, _) => break,
            Event::LinkLost(_, reason) | Event::Failed(_, reason) => panic!("{reason}"),
            _ => {}
        }
    }
    assert_eq!(marks(&dll, "clear"), 0);
    assert!(rows.iter().any(|row| row.source == Some(0x7e8)
        && row.request == "03"
        && row.value == "No codes reported by this ECU"));
    assert!(rows.iter().any(|row| row.source == Some(0x7ea)
        && row.request == "03"
        && row.status == "ECU did not return this code list"));
    assert!(rows.iter().any(|row| row.source == Some(0x7e8)
        && row.request == "0902"
        && row.value == "JTDBR32E720123456"));
    assert!(rows.iter().any(|row| row.source == Some(0x7e8)
        && row.request == "0141"
        && row.name == "Catalyst monitoring"
        && row.value == "Not ready"));
    assert!(rows.iter().any(|row| row.source == Some(0x7ea)
        && row.request == "0141"
        && row.status == "ECU reports this request is unsupported"));
    assert!(
        rows.iter().any(|row| row.source == Some(0x7e8)
            && row.request == "020200"
            && row.value == "P0133")
    );
    assert!(rows.iter().any(|row| row.source == Some(0x7e8)
        && row.request == "020500"
        && row.numeric == Some(88.0)));
    assert!(rows.iter().any(|row| row.source == Some(0x7ea)
        && row.request == "020200"
        && row.value == "No freeze frame stored"));
    assert!(
        !rows
            .iter()
            .any(|row| row.source == Some(0x7ea) && row.request == "020500")
    );
    assert_eq!(marks(&dll, "frame-map-00"), 1);
    assert_eq!(marks(&dll, "frame-data"), 1);
}

#[test]
fn monitor_sweep_reads_advertised_ranges_and_reports_the_missing_ecu() {
    let dll = fixture(18);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    let event = until(&rx, Duration::from_secs(8), |event| {
        matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
    });
    match event {
        Event::Connected { protocol, .. } => {
            assert_eq!(protocol, CanProtocol::Can11Bit500K.label())
        }
        Event::Failed(_, reason) => panic!("{reason}"),
        _ => unreachable!(),
    }
    let scan = |id| {
        send(
            &tx,
            Command::Read {
                id,
                queries: vec![],
                discovery: None,
                mode: ReadMode::MonitorSweep,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut support = None;
        let mut rows = Vec::new();
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::MonitorSupport(op, value) if op == id => support = Some(value),
                Event::Rows(op, batch) if op == id => rows.extend(batch),
                Event::Finished(op, text) if op == id => {
                    assert!(text.contains("Discovery was incomplete"), "{text}");
                    return (support.unwrap(), rows);
                }
                Event::LinkLost(_, reason) | Event::Failed(_, reason) => panic!("{reason}"),
                _ => {}
            }
        }
    };
    let (support, rows) = scan(2);
    assert_eq!(
        support.reported.into_iter().collect::<Vec<_>>(),
        [1, 0x21, 0x41]
    );
    assert!(support.problem.unwrap().contains("ECU 7EA"));
    assert_eq!(marks(&dll, "mode-06"), 6);
    assert_eq!(rows.len(), 5);
    for (request, status) in [
        ("0601", "Not completed"),
        ("0601", "PASS"),
        ("0621", "FAIL"),
    ] {
        assert!(
            rows.iter()
                .any(|r| r.source == Some(0x7e8) && r.request == request && r.status == status)
        );
    }
    assert!(rows.iter().any(|r| r.source == Some(0x7e8)
        && r.request == "0641"
        && r.status.contains("unverified")));
    assert!(rows.iter().any(|r| r.source == Some(0x7ea)
        && r.request == "0601"
        && r.status == "ECU did not return this monitor result"));
    let (support, refreshed) = scan(3);
    assert_eq!(support.reported.into_iter().collect::<Vec<_>>(), [1, 0x21]);
    assert_eq!(marks(&dll, "mode-06"), 11);
    assert!(refreshed.iter().any(|r| r.source == Some(0x7e8)
        && r.request == "0641"
        && r.status == "Monitor discovery incomplete"));
    let mut results = Results::default();
    for (at_ms, rows) in [(1000, rows), (2000, refreshed)] {
        results.apply(&Sample {
            at_ms,
            rows,
            ..Default::default()
        });
    }
    let last_41 = results
        .rows
        .values()
        .find(|v| v.latest.reading.request == "0641")
        .unwrap();
    assert_eq!(last_41.latest.at_ms, 1000);
    assert!(results.refresh_failed(&last_41.latest));
}

#[test]
fn kline_oxygen_sweep_uses_sensor_masks_and_keeps_ecu_results_separate() {
    let dll = fixture(24);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    let connected = until(&rx, Duration::from_secs(9), |event| {
        matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
    });
    assert!(
        matches!(connected, Event::Connected { protocol, .. } if protocol == CanProtocol::Iso9141.label())
    );

    let read_tests = |id, mode| {
        send(
            &tx,
            Command::Read {
                id,
                queries: Vec::new(),
                discovery: None,
                mode,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut rows = Vec::new();
        let mut support = None;
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::Rows(op, batch) if op == id => rows.extend(batch),
                Event::MonitorSupport(op, map) if op == id => support = Some(map),
                Event::Finished(op, _) if op == id => return (rows, support),
                Event::LinkLost(_, error) | Event::Failed(_, error) => panic!("{error}"),
                _ => {}
            }
        }
    };
    let (rows, _) = read_tests(2, ReadMode::OxygenSweep);
    let result = |source, request: &str| {
        rows.iter()
            .find(|row| row.source == Some(source) && row.request == request)
            .unwrap()
    };
    assert_eq!(marks(&dll, "oxygen"), 5);
    assert_eq!(marks(&dll, "oxygen-continuation"), 1);
    assert_eq!(result(0x10, "050110").numeric, Some(450.0));
    let switch = result(0x10, "050510");
    assert!(switch.name.starts_with("Bank 2 sensor 1"));
    assert_eq!(
        (switch.status.as_str(), switch.numeric),
        ("PASS", Some(72.0))
    );
    let incomplete = result(0x18, "050510");
    assert!(incomplete.name.starts_with("Bank 3 sensor 1"));
    assert_eq!(incomplete.status, "Not completed");
    assert_eq!(result(0x10, "052110").status, "FAIL");
    assert_eq!(marks(&dll, "clear"), 0);

    let (refresh, _) = read_tests(
        3,
        ReadMode::OxygenTest {
            sensor: 0x10,
            test: 5,
        },
    );
    assert!(
        refresh
            .iter()
            .any(|row| row.source == Some(0x10) && row.request == "050510" && row.status == "PASS")
    );
    assert!(refresh.iter().any(|row| row.source == Some(0x18)
        && row.request == "050510"
        && row.status == "ECU did not return this monitor result"));

    let (wrong_sensor, _) = read_tests(
        4,
        ReadMode::OxygenTest {
            sensor: 0x10,
            test: 9,
        },
    );
    assert!(wrong_sensor.iter().any(|row| row.request == "050910"
        && row.source.is_none()
        && row.status.contains("no vehicle response")));
    let (legacy, support) = read_tests(5, ReadMode::MonitorSweep);
    let support = support.unwrap();
    assert_eq!(support.reported.into_iter().collect::<Vec<_>>(), [5]);
    assert!(support.problem.is_none());
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].value, "0x0002 (max 0x0003)");
}

#[test]
fn kline_vehicle_information_detects_incomplete_calibrations_by_ecu() {
    let dll = fixture(19);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    send(
        &tx,
        Command::Connect {
            id: 1,
            protocol: Some(CanProtocol::Iso9141),
            ecu: None,
            cancel: Arc::new(AtomicBool::new(false)),
        },
    );
    let event = until(&rx, Duration::from_secs(8), |event| {
        matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
    });
    match event {
        Event::Connected { protocol, .. } => assert_eq!(protocol, CanProtocol::Iso9141.label()),
        Event::Failed(_, reason) => panic!("{reason}"),
        _ => unreachable!(),
    }
    send(
        &tx,
        Command::Read {
            id: 2,
            queries: vec![],
            discovery: None,
            mode: ReadMode::VehicleInformation,
            cancel: Arc::new(AtomicBool::new(false)),
        },
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut support = None;
    let mut rows = Vec::new();
    loop {
        match rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            Event::VehicleInfoSupport(2, value) => support = Some(value),
            Event::Rows(2, batch) => rows.extend(batch),
            Event::Finished(2, _) => break,
            Event::Failed(_, reason) | Event::LinkLost(_, reason) => panic!("{reason}"),
            _ => {}
        }
    }
    let support = support.unwrap();
    assert_eq!(
        support.reported.into_iter().collect::<Vec<_>>(),
        [2, 3, 4, 5, 6]
    );
    assert!(support.problem.is_none());
    assert_eq!(marks(&dll, "mode-09"), 6);
    assert_eq!(rows.len(), 3);
    assert!(
        rows.iter().any(|r| r.source == Some(0x10)
            && r.request == "0902"
            && r.value == "JTDBR32E720123456")
    );
    assert!(rows.iter().any(|r| r.source == Some(0x10)
        && r.request == "0904"
        && r.value == "Unavailable"
        && r.status == "ECU announced 8 messages. Received 4."));
    assert!(
        rows.iter()
            .any(|r| r.source == Some(0x18) && r.request == "0906" && r.value == "12345678")
    );
}

#[test]
fn readiness_checks_partial_bitmap_and_previously_observed_ecus_per_pid() {
    let dll = fixture(14);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    let event = until(&rx, Duration::from_secs(8), |event| {
        matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))
    });
    let Event::Connected { pid_support, .. } = event else {
        panic!("fixture did not connect");
    };
    assert_eq!(pid_support.checked_through, 0);
    assert!(pid_support.reported.contains(&1));
    assert!(pid_support.problem.is_some());

    let read = |id| {
        send(
            &tx,
            Command::Read {
                id,
                queries: [1, 0x41]
                    .map(|pid| Query {
                        bytes: vec![1, pid],
                        name: "Readiness".into(),
                        target: None,
                        signals: vec![],
                        verified: false,
                    })
                    .into(),
                discovery: None,
                mode: ReadMode::Scan,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut rows = Vec::new();
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::Rows(op, batch) if op == id => rows.extend(batch),
                Event::Finished(op, _) if op == id => return rows,
                Event::LinkLost(_, reason) | Event::Failed(_, reason) => panic!("{reason}"),
                _ => {}
            }
        }
    };
    // The engine is advertised even in a partial bitmap. The other ECU is
    // learned from its first readiness response and can go silent later.
    for (id, responding, missing) in [(2, 0x7ea, 0x7e8), (3, 0x7e8, 0x7ea)] {
        let rows = read(id);
        let failures: Vec<_> = rows
            .iter()
            .filter(|r| r.value == "Unavailable")
            .map(|r| (r.source, r.request.as_str(), r.status.as_str()))
            .collect();
        assert_eq!(
            failures,
            [(Some(missing), "0101", "ECU did not return this PID")]
        );
        assert!(
            rows.iter()
                .any(|r| r.source == Some(responding) && r.raw == "410100072120")
        );
        assert!(
            rows.iter()
                .any(|r| r.source == Some(0x7e8) && r.raw == "414100072120")
        );
    }
    assert_eq!(marks(&dll, "group-live"), 0);

    send(&tx, Command::Demo { id: 4 });
    until(&rx, Duration::from_secs(8), |event| {
        matches!(
            event,
            Event::Connected {
                id: 4,
                demo: true,
                ..
            }
        )
    });
    let demo = read(5);
    assert!(!demo.is_empty());
    assert!(
        demo.iter()
            .all(|r| r.source == Some(0x7e8) && r.value != "Unavailable")
    );
    assert!(demo.iter().any(|r| r.request == "0141"));
}

#[test]
fn freeze_frame_scan_uses_reported_pids_and_skips_data_for_an_explicitly_empty_frame() {
    let dll = fixture(13);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    assert!(matches!(
        until(&rx, Duration::from_secs(8), |event| matches!(event, Event::Connected { id: 1, .. } | Event::Failed(1, _))),
        Event::Connected { protocol, .. } if protocol == CanProtocol::Can11Bit500K.label()
    ));

    let read = |id, frame| {
        send(
            &tx,
            Command::Read {
                id,
                queries: catalog(include_str!("../data/obdb/saej1979.json"), None).unwrap(),
                discovery: None,
                mode: ReadMode::FreezeFrame(frame),
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut rows = Vec::new();
        loop {
            match rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Event::Rows(op, new) if op == id => rows.extend(new),
                Event::Finished(op, status) if op == id => return (rows, status),
                Event::LinkLost(_, reason) | Event::Failed(_, reason) => panic!("{reason}"),
                _ => {}
            }
        }
    };

    let (empty, status) = read(2, 0);
    let trigger = empty.iter().find(|r| r.request == "020200").unwrap();
    assert_eq!(trigger.raw, "4202000000");
    assert_eq!(trigger.value, "No freeze frame stored");
    assert!(empty.iter().any(|r| r.request == "020000"
        && r.name == "Freeze-frame PID discovery"
        && r.status.contains("PID discovery incomplete")));
    assert!(status.contains("ECU reported no freeze frame"));
    assert_eq!(marks(&dll, "frame-data"), 0);

    let (rows, status) = read(3, 3);
    assert!(status.contains("Freeze frame 3 read"), "{status}");
    assert_eq!(rows.len(), 5);
    assert!(
        rows.iter()
            .any(|r| r.request == "020203" && r.source == Some(0x7e8) && r.value == "P0133")
    );
    assert!(
        rows.iter()
            .any(|r| r.request == "020503" && r.source == Some(0x7e8) && r.numeric == Some(88.0))
    );
    for request in ["024F03", "026F03"] {
        assert!(rows.iter().any(|r| r.request == request
            && r.source == Some(0x7e8)
            && r.value == "01ABCD"
            && r.unit == "raw"));
    }
    assert_eq!(rows.iter().filter(|r| r.source == Some(0x7ea)).count(), 1);
    assert_eq!(marks(&dll, "frame-trigger"), 2);
    assert_eq!(marks(&dll, "frame-map-00"), 2);
    assert_eq!(marks(&dll, "frame-map-20"), 1);
    assert_eq!(marks(&dll, "frame-map-40"), 1);
    assert_eq!(marks(&dll, "frame-map-60"), 1);
    assert_eq!(marks(&dll, "frame-data"), 3);

    let (rows, _) = read(4, 3);
    assert!(
        rows.iter()
            .any(|r| r.request == "020203" && r.source == Some(0x7e8) && r.value == "Unavailable")
    );
    assert!(rows.iter().any(|r| r.request == "020203"
        && r.source == Some(0x7ea)
        && r.value == "No freeze frame stored"));
}

#[test]
fn helper_discovers_two_29_bit_ecus_and_retains_good_reply_when_other_times_out() {
    let dll = fixture(0);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    until(
        &rx,
        Duration::from_secs(5),
        |event| matches!(event, Event::Voltage(Some(sample)) if sample.millivolts == 12600),
    );
    assert_eq!(
        marks(&dll, "channel"),
        0,
        "passive voltage must not open an ECU channel"
    );
    let opens = marks(&dll, "open");
    connect(&tx, 1);
    connected(&rx, 1);
    assert_eq!(
        marks(&dll, "open"),
        opens + 1,
        "reuse the device between protocol attempts"
    );
    assert_eq!(
        marks(&dll, "channel"),
        2,
        "raw discovery followed by ISO-TP"
    );
    until(
        &rx,
        Duration::from_secs(5),
        |event| matches!(event, Event::Voltage(Some(sample)) if sample.millivolts == 12700),
    );

    read(&tx, 2, &[1, 0x0c], false);
    let Event::Rows(_, rows) = until(&rx, Duration::from_secs(5), |e| {
        matches!(e, Event::Rows(2, _))
    }) else {
        unreachable!()
    };
    let sources: Vec<_> = rows.iter().map(|row| row.source).collect();
    assert_eq!(sources, [Some(0x18DAF127), Some(0x18DAF145)]);

    read(&tx, 3, &[3], false);
    let Event::Rows(_, rows) = until(&rx, Duration::from_secs(6), |e| {
        matches!(e, Event::Rows(3, _))
    }) else {
        unreachable!()
    };
    assert!(
        rows.iter()
            .any(|r| r.source == Some(0x18DAF127) && r.value == "P0133")
    );
    assert!(rows.iter().any(|r| {
        r.source == Some(0x18DAF145)
            && r.value == "Unavailable"
            && r.status.contains("remained pending")
            && r.raw == "7F0378"
    }));
    assert_eq!(rows.len(), 2);
}

#[test]
fn helper_preserves_good_ecu_data_alongside_bad_and_incomplete_frames() {
    let (tx, rx) = worker(Arc::new(Mutex::new(fixture(0))));
    connect(&tx, 1);
    connected(&rx, 1);

    for (id, request, values, failure) in [
        (
            2,
            &[7][..],
            ["No codes reported by this ECU", "Unavailable"],
            Some("padding error"),
        ),
        (
            3,
            &[9, 2][..],
            ["JTDBR32E720123456", "JTDBR32E720123457"],
            None,
        ),
        (
            4,
            &[9, 4][..],
            ["MFC3-FIXTURE-001", "Unavailable"],
            Some("did not finish"),
        ),
    ] {
        read(&tx, id, request, false);
        let Event::Rows(_, mut rows) = until(
            &rx,
            Duration::from_secs(6),
            |e| matches!(e, Event::Rows(actual, _) if *actual == id),
        ) else {
            unreachable!()
        };
        rows.sort_by_key(|row| row.source);
        assert_eq!(rows.len(), 2, "request {request:02X?}");
        assert_eq!(rows[0].source, Some(0x18DAF127));
        assert_eq!(rows[1].source, Some(0x18DAF145));
        assert_eq!([rows[0].value.as_str(), rows[1].value.as_str()], values);
        if let Some(failure) = failure {
            assert!(rows[1].status.contains(failure), "{:?}", rows[1]);
        }
    }
}

#[test]
fn connection_attempts_distinguish_transmit_failure_from_waiting_for_a_reply() {
    let protocol = CanProtocol::Can11Bit500K;
    for (mode, phase, detail, result) in [
        (
            25,
            ProbePhase::Transmit,
            "PassThruWriteMsgs failed (0x09): Only sent 0 of 1 messages",
            "Diagnostic request transmission failed",
        ),
        (
            26,
            ProbePhase::Reply,
            "no vehicle response",
            "No OBD-II reply was verified",
        ),
        (
            27,
            ProbePhase::Transmit,
            "PassThruWriteMsgs failed (0x09): Only sent 0 of 1 messages",
            "Diagnostic request transmission failed",
        ),
    ] {
        let dll = fixture(mode);
        let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
        send(
            &tx,
            Command::Connect {
                id: 1,
                protocol: Some(protocol),
                ecu: None,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let first = until(&rx, Duration::from_secs(8), |event| {
            matches!(event, Event::ConnectAttempt(1, _))
        });
        assert!(matches!(first, Event::ConnectAttempt(1, attempt)
            if attempt.driver == "Mongoose-Plus MFC3"
                && attempt.protocol.is_none() && attempt.outcome == AttemptOutcome::Opened));
        let probe = until(&rx, Duration::from_secs(6), |event| {
            matches!(event, Event::ConnectAttempt(1, _))
        });
        let Event::ConnectAttempt(_, probe) = probe else {
            unreachable!()
        };
        assert_eq!(probe.protocol, Some(protocol));
        assert_eq!(probe.outcome, AttemptOutcome::ProbeFailed(phase));
        let message = probe.detail.unwrap();
        assert!(message.contains(detail), "{message}");
        if mode == 27 {
            assert!(message.contains("PassThruDisconnect failed"), "{message}");
        }
        let finished = until(&rx, Duration::from_secs(4), |event| {
            matches!(event, Event::Failed(1, _) | Event::Connected { id: 1, .. })
        });
        assert!(matches!(finished, Event::Failed(_, reason) if reason.contains(result)));
        assert_eq!(marks(&dll, "pidmap-00"), 1);
        assert_eq!(marks(&dll, "wait-for-reply"), u64::from(mode == 26));
        assert_eq!(marks(&dll, "disconnect"), 1);
    }
}

#[test]
fn a_driver_that_stalls_open_times_out_and_the_next_connect_starts_fresh() {
    let stalled = fixture(1);
    let healthy = fixture(0);
    let selection = Arc::new(Mutex::new(stalled.clone()));
    let (tx, rx) = worker(selection.clone());
    connect(&tx, 1);
    // Time the stalled driver call once the Windows helper has loaded the DLL.
    wait_for_marker(&stalled, "open", 1, Duration::from_secs(25));
    let event = until(&rx, Duration::from_secs(6), |event| {
        matches!(event, Event::Failed(1, _))
    });
    assert!(
        matches!(event, Event::Failed(_, reason) if reason.contains("did not respond while opening"))
    );
    assert_eq!(marks(&stalled, "open"), 1);
    *selection.lock().unwrap() = healthy;
    connect(&tx, 2);
    connected(&rx, 2);
}

#[test]
fn connect_can_interrupt_a_stalled_passive_voltage_sample() {
    let dll = fixture(4);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    wait_for_marker(&dll, "passive", 1, Duration::from_secs(6));
    connect(&tx, 1);
    wait_for_marker(&dll, "open", 2, Duration::from_millis(1650));
    connected(&rx, 1);
    until(
        &rx,
        Duration::from_secs(5),
        |event| matches!(event, Event::Voltage(Some(sample)) if sample.millivolts == 12600),
    );
}

#[test]
fn connect_tries_another_mongoose_after_a_stall_and_a_silent_vehicle() {
    let stalled = fixture(1);
    let silent = fixture(8);
    let healthy = fixture(0);
    let entries = vec![
        driver_entry("Mongoose ISO/CAN", &healthy),
        driver_entry("Mongoose-Plus MFC3", &stalled),
        driver_entry("Mongoose-Plus MFC3 secondary", &silent),
        driver_entry("Mongoose alias", &silent),
    ];
    let (tx, rx) = worker_with_drivers(move || entries.clone());

    connect(&tx, 1);
    wait_for_marker(&stalled, "open", 1, Duration::from_secs(25));
    connected_as(&rx, 1, "Mongoose ISO/CAN", Duration::from_secs(13));
    assert_eq!(marks(&stalled, "open"), 1);
    assert_eq!(
        marks(&silent, "open"),
        1,
        "duplicate DLL aliases must share one attempt"
    );
    assert_eq!(
        marks(&silent, "channel"),
        1,
        "the earlier DLL opened its channel"
    );
    assert_eq!(marks(&silent, "disconnect"), 1);
    assert_eq!(marks(&silent, "close"), 1);
    assert_eq!(marks(&healthy, "open"), 1);
}

#[test]
fn selected_j2534_driver_supplies_voltage_and_connections() {
    let mongoose = fixture(0);
    let fixture_dll = fixture(0);
    let other = fixture_dll.with_file_name("generic_j2534.dll");
    std::fs::rename(fixture_dll, &other).unwrap();
    let entries = Arc::new(Mutex::new(vec![
        driver_entry("Mongoose-Plus MFC3", &mongoose),
        driver_entry("Other J2534 adapter", &other),
    ]));
    let available = entries.clone();
    let (tx, rx) = worker_with_drivers(move || available.lock().unwrap().clone());
    send(&tx, Command::SelectDriver(Some(other.clone())));
    until(
        &rx,
        Duration::from_secs(8),
        |event| matches!(event, Event::Voltage(Some(sample)) if sample.driver.path == other && sample.millivolts == 12600),
    );
    assert_eq!(marks(&mongoose, "open"), 0);
    assert_eq!(marks(&other, "channel"), 0);

    connect(&tx, 1);
    connected_as(&rx, 1, "Other J2534 adapter", Duration::from_secs(8));
    assert_eq!(marks(&mongoose, "open"), 0);
    assert_eq!(
        marks(&other, "channel"),
        2,
        "29-bit CAN uses a raw discovery channel followed by ISO-TP"
    );

    send(&tx, Command::Disconnect { id: 2 });
    until(&rx, Duration::from_secs(4), |event| {
        matches!(event, Event::Disconnected(2))
    });
    entries
        .lock()
        .unwrap()
        .retain(|driver| driver.path != other);
    connect(&tx, 3);
    until(
        &rx,
        Duration::from_secs(4),
        |event| matches!(event, Event::Failed(3, reason) if reason.contains("no longer listed")),
    );
    assert_eq!(marks(&mongoose, "open"), 0);

    send(&tx, Command::SelectDriver(None));
    connect(&tx, 4);
    connected(&rx, 4);
}

#[test]
fn cancelling_connect_does_not_start_the_next_driver() {
    let stalled = fixture(1);
    let healthy = fixture(0);
    let entries = vec![
        driver_entry("Mongoose-Plus MFC3", &stalled),
        driver_entry("Mongoose ISO/CAN", &healthy),
    ];
    let (tx, rx) = worker_with_drivers(move || entries.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    send(
        &tx,
        Command::Connect {
            id: 1,
            protocol: None,
            ecu: None,
            cancel: cancel.clone(),
        },
    );
    wait_for_marker(&stalled, "open", 1, Duration::from_secs(25));
    cancel.store(true, Ordering::Relaxed);
    let attempt = until(&rx, Duration::from_secs(3), |event| {
        matches!(event, Event::ConnectAttempt(1, _))
    });
    assert!(matches!(attempt, Event::ConnectAttempt(_, attempt)
        if attempt.protocol.is_none() && attempt.outcome == AttemptOutcome::Cancelled));
    let outcome = until(&rx, Duration::from_secs(3), |event| {
        matches!(event, Event::Failed(1, _) | Event::Connected { id: 1, .. })
    });
    assert!(matches!(outcome, Event::Failed(_, reason) if reason == "Connection cancelled."));
    assert_eq!(marks(&healthy, "open"), 0);
}

#[test]
fn passive_voltage_tries_other_drivers_and_keeps_sampling_during_practice() {
    let unreadable = fixture(8);
    let unpowered = fixture(9);
    let powered = fixture(0);
    let entries = Arc::new(Mutex::new(vec![
        driver_entry("Mongoose-Plus MFC3", &unreadable),
        driver_entry("Mongoose-Plus MFC3 secondary", &unpowered),
    ]));
    let selection = entries.clone();
    let (tx, rx) = worker_with_drivers(move || selection.lock().unwrap().clone());

    let zero = until(
        &rx,
        Duration::from_secs(8),
        |event| matches!(event, Event::Voltage(Some(sample)) if sample.millivolts == 0),
    );
    assert!(matches!(zero, Event::Voltage(Some(sample)) if sample.driver.path == unpowered));

    entries
        .lock()
        .unwrap()
        .push(driver_entry("Mongoose ISO/CAN", &powered));
    let positive = until(
        &rx,
        Duration::from_secs(8),
        |event| matches!(event, Event::Voltage(Some(sample)) if sample.millivolts == 12600),
    );
    assert!(matches!(positive, Event::Voltage(Some(sample)) if sample.driver.path == powered));
    let other_opens = [marks(&unreadable, "open"), marks(&unpowered, "open")];

    send(&tx, Command::Demo { id: 1 });
    until(&rx, Duration::from_secs(4), |event| {
        matches!(
            event,
            Event::Connected {
                id: 1,
                demo: true,
                ..
            }
        )
    });
    until(
        &rx,
        Duration::from_secs(6),
        |event| matches!(event, Event::Voltage(Some(sample)) if sample.driver.path == powered && sample.millivolts == 12600),
    );
    assert_eq!(
        [marks(&unreadable, "open"), marks(&unpowered, "open")],
        other_opens
    );
    for dll in [&unreadable, &unpowered, &powered] {
        assert_eq!(marks(dll, "channel"), 0);
        assert_eq!(marks(dll, "close"), marks(dll, "open"));
    }

    // Once connected, the selected diagnostic cable supplies the single readout.
    let powered_opens = marks(&powered, "open");
    connect(&tx, 2);
    connected_as(
        &rx,
        2,
        "Mongoose-Plus MFC3 secondary",
        Duration::from_secs(8),
    );
    let measurement = until(&rx, Duration::from_secs(5), |event| {
        matches!(event, Event::Voltage(Some(_)))
    });
    assert!(
        matches!(measurement, Event::Voltage(Some(sample)) if sample.driver.path == unpowered && sample.millivolts == 0)
    );
    assert_eq!(marks(&powered, "open"), powered_opens);
}

#[test]
fn stopping_a_stalled_clear_drops_the_helper_without_repeating_the_command() {
    let stalled = fixture(2);
    let healthy = fixture(0);
    let selection = Arc::new(Mutex::new(stalled.clone()));
    let (tx, rx) = worker(selection.clone());
    connect(&tx, 1);
    connected(&rx, 1);
    let cancel = read(&tx, 2, &[4], true);
    wait_for_marker(&stalled, "clear", 1, Duration::from_secs(4));
    cancel.store(true, Ordering::Relaxed);
    let event = until(&rx, Duration::from_secs(2), |e| {
        matches!(e, Event::LinkLost(1, _))
    });
    assert!(
        matches!(event, Event::LinkLost(_, reason) if reason.contains("Stopped the unresponsive driver") && reason.contains("ECU may have acted"))
    );
    *selection.lock().unwrap() = healthy.clone();
    connect(&tx, 3);
    connected(&rx, 3);
    assert_eq!(marks(&stalled, "clear"), 1);
    assert_eq!(marks(&healthy, "clear"), 0);
}

#[test]
fn a_native_driver_crash_does_not_end_the_scanner_worker() {
    let crashed = fixture(3);
    let healthy = fixture(0);
    let selection = Arc::new(Mutex::new(crashed));
    let (tx, rx) = worker(selection.clone());
    connect(&tx, 1);
    connected(&rx, 1);
    read(&tx, 2, &[1, 0x0c], false);
    let event = until(&rx, Duration::from_secs(5), |e| {
        matches!(e, Event::LinkLost(1, _))
    });
    assert!(
        matches!(event, Event::LinkLost(_, reason) if reason.contains("Driver process exited"))
    );
    *selection.lock().unwrap() = healthy;
    connect(&tx, 3);
    connected(&rx, 3);
}

#[test]
fn invalid_diagnostic_channel_keeps_completed_rows_and_allows_reconnect() {
    let dll = fixture(5);
    let (tx, rx) = worker(Arc::new(Mutex::new(dll.clone())));
    connect(&tx, 1);
    connected(&rx, 1);
    read(&tx, 2, &[1, 0x0c], false);
    let Event::Rows(_, rows) = until(&rx, Duration::from_secs(5), |event| {
        matches!(event, Event::Rows(2, _))
    }) else {
        unreachable!()
    };
    assert!(rows.iter().any(|row| row.source == Some(0x18DAF127)
        && row.raw == "410C0C80"
        && row.value != "Unavailable"));
    assert!(
        rows.iter()
            .any(|row| row.value == "Unavailable" && row.status.contains("0x02"))
    );

    let event = until(&rx, Duration::from_secs(3), |event| {
        matches!(event, Event::LinkLost(1, _) | Event::Finished(2, _))
    });
    assert!(
        matches!(event, Event::LinkLost(_, reason) if reason.contains("diagnostic channel is no longer valid"))
    );
    until(
        &rx,
        Duration::from_secs(6),
        |event| matches!(event, Event::Voltage(Some(sample)) if sample.millivolts == 12600),
    );
    assert_eq!(
        marks(&dll, "channel"),
        2,
        "voltage sampling must leave vehicle diagnostics disconnected"
    );

    connect(&tx, 3);
    connected(&rx, 3);
}
