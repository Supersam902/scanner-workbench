#![cfg(any(target_os = "linux", target_os = "windows"))]

use obd_dashboard::j2534::{CanProtocol, ProbePhase};
use obd_dashboard::scanner::model::ElmConnection;
use obd_dashboard::scanner::worker::{
    AttemptOutcome, Command, Event, ReadMode, VoltageAdapter, spawn_with_driver,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

fn wait_for(
    receiver: &mpsc::Receiver<Event>,
    timeout: Duration,
    accept: impl Fn(&Event) -> bool,
) -> Event {
    let end = Instant::now() + timeout;
    loop {
        let event = receiver
            .recv_timeout(end.saturating_duration_since(Instant::now()))
            .expect("expected scanner event");
        if accept(&event) {
            return event;
        }
        if let Event::Failed(id, reason) = event {
            panic!("Operation {id} failed: {reason}");
        }
    }
}

fn send_read(tx: &obd_dashboard::scanner::worker::CommandSender, id: u64, pid: u8) {
    tx.try_send(Command::Read {
        id,
        queries: vec![obd_dashboard::diagnostics::Query {
            bytes: vec![1, pid],
            target: None,
            name: "Live data".into(),
            signals: Vec::new(),
            verified: false,
        }],
        discovery: None,
        mode: ReadMode::Diagnostic,
        cancel: Arc::new(AtomicBool::new(false)),
    })
    .unwrap_or_else(|error| panic!("Cannot send read: {error}"));
}

fn start_tcp_adapter(listener: TcpListener) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut commands = Vec::new();
        let mut command = Vec::new();
        while reader.read_until(b'\r', &mut command).unwrap_or(0) != 0 {
            let text = String::from_utf8(command[..command.len() - 1].to_vec()).unwrap();
            command.clear();
            let reply = match text.as_str() {
                "ATZ" | "ATI" => "ELM327 v2.3",
                "ATRV" => "12.7V",
                "ATDPN" => "A6",
                "0100" => "SEARCHING...\r7E80641008010000000\r7E90641008010000000",
                "010C" => "7E804410C0BB8000000\r7E904410C0C80000000",
                "0101" => "7E80641010007650400\r7E90641018107656500",
                "0902" => "7E810144902014A5444\r7E82142523332453732\r7E82230313233343536",
                _ if text.starts_with("AT") => "OK",
                _ => "NO DATA",
            };
            commands.push(text);
            if write!(stream, "{reply}\r>").is_err() {
                break;
            }
        }
        commands
    })
}

#[test]
fn tcp_adapter_supplies_disconnected_voltage_and_separate_ecu_scan_results() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let connection = ElmConnection::Tcp(listener.local_addr().unwrap().to_string());
    let server = start_tcp_adapter(listener);
    let (tx, rx) = spawn_with_driver(
        Some(env!("CARGO_BIN_EXE_scanner-workstation").into()),
        Vec::new,
    );
    tx.try_send(Command::SelectElm(Some(connection.clone())))
        .unwrap_or_else(|error| panic!("{error}"));
    wait_for(&rx, Duration::from_secs(7), |event| {
        matches!(event, Event::Voltage(Some(sample))
            if sample.adapter == VoltageAdapter::Elm(connection.clone()) && sample.millivolts == 12700)
    });
    tx.try_send(Command::ConnectElm {
        id: 1,
        connection,
        cancel: Arc::new(AtomicBool::new(false)),
    })
    .unwrap_or_else(|error| panic!("{error}"));
    let connected = wait_for(&rx, Duration::from_secs(8), |event| {
        matches!(event, Event::Connected { id: 1, .. })
    });
    assert!(
        matches!(connected, Event::Connected { pid_support, .. } if pid_support.reported.contains(&12))
    );

    tx.try_send(Command::Read {
        id: 1,
        queries: Vec::new(),
        discovery: None,
        mode: ReadMode::Identify,
        cancel: Arc::new(AtomicBool::new(false)),
    })
    .unwrap_or_else(|error| panic!("{error}"));
    let vin = wait_for(&rx, Duration::from_secs(4), |event| {
        matches!(event, Event::InitialVin(1, _))
    });
    assert!(
        matches!(vin, Event::InitialVin(_, rows) if rows.iter().any(|row| row.value == "JTDBR32E720123456" && row.source == Some(0x7e8)))
    );

    for (id, pid) in [(2, 12), (3, 1)] {
        send_read(&tx, id, pid);
        let evidence = wait_for(
            &rx,
            Duration::from_secs(4),
            |event| matches!(event, Event::Rows(actual, _) if *actual == id),
        );
        let Event::Rows(_, rows) = evidence else {
            unreachable!()
        };
        for ecu in [0x7e8, 0x7e9] {
            assert!(
                rows.iter()
                    .any(|row| row.source == Some(ecu) && row.value != "Unavailable")
            );
        }
    }
    tx.try_send(Command::Shutdown)
        .unwrap_or_else(|error| panic!("{error}"));
    let commands = server.join().unwrap();
    assert_eq!(commands.first().map(String::as_str), Some("ATRV"));
    assert!(commands.iter().any(|command| command == "ATH1"));
    assert!(commands.iter().all(|command| command != "04"));
}

#[test]
fn elm_connection_reports_distinguish_setup_protocol_reply_and_transport_failures() {
    for (failure, response, outcome, expected_protocol, expected_detail) in [
        (
            "ATH1",
            Some("?"),
            AttemptOutcome::ElmSetupFailed,
            None,
            "ATH1 rejected",
        ),
        (
            "ATDPN",
            Some("?"),
            AttemptOutcome::ElmProtocolFailed,
            None,
            "ATDPN did not report",
        ),
        (
            "ATDPN",
            Some("A0"),
            AttemptOutcome::ProbeFailed(ProbePhase::Reply),
            None,
            "ATDPN=0",
        ),
        (
            "0100",
            Some("7E80341008000000000"),
            AttemptOutcome::ProbeFailed(ProbePhase::Reply),
            Some(CanProtocol::Can11Bit500K),
            "ECU 7E8 · Support bitmap is truncated · 410080",
        ),
        (
            "0100",
            None,
            AttemptOutcome::ElmTransportFailed,
            None,
            "TCP adapter disconnected",
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let connection = ElmConnection::Tcp(listener.local_addr().unwrap().to_string());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut command = Vec::new();
            while reader.read_until(b'\r', &mut command).unwrap_or(0) != 0 {
                let text = std::str::from_utf8(&command)
                    .unwrap()
                    .trim_end_matches('\r');
                let reply = if text == failure {
                    let Some(response) = response else {
                        break;
                    };
                    response
                } else {
                    match text {
                        "ATZ" | "ATI" => "ELM327 v2.3",
                        "0100" => "NO DATA",
                        "ATDPN" => "A6",
                        _ => "OK",
                    }
                };
                write!(stream, "{reply}\r>").unwrap();
                command.clear();
            }
        });
        let (tx, rx) = spawn_with_driver(
            Some(env!("CARGO_BIN_EXE_scanner-workstation").into()),
            Vec::new,
        );
        tx.try_send(Command::ConnectElm {
            id: 1,
            connection,
            cancel: Arc::new(AtomicBool::new(false)),
        })
        .unwrap_or_else(|error| panic!("{error}"));
        let attempt = wait_for(
            &rx,
            Duration::from_secs(8),
            |event| matches!(event, Event::ConnectAttempt(1, attempt) if attempt.outcome != AttemptOutcome::Opened),
        );
        let Event::ConnectAttempt(_, attempt) = attempt else {
            unreachable!()
        };
        assert_eq!(attempt.outcome, outcome);
        assert_eq!(attempt.protocol, expected_protocol);
        let detail = attempt.detail.unwrap();
        assert!(detail.contains(expected_detail), "{detail}");
        let end = wait_for(&rx, Duration::from_secs(3), |event| {
            matches!(event, Event::Failed(1, _) | Event::Connected { id: 1, .. })
        });
        assert!(matches!(end, Event::Failed(1, _)));
        tx.try_send(Command::Shutdown)
            .unwrap_or_else(|error| panic!("{error}"));
        server.join().unwrap();
    }
}

#[test]
fn stalled_elm_probe_can_be_cancelled_and_the_next_connection_starts_fresh() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let stalled = ElmConnection::Tcp(listener.local_addr().unwrap().to_string());
    let (opened, waiting) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        let mut command = Vec::new();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        assert!(reader.read_until(b'\r', &mut command).unwrap() > 0);
        assert_eq!(command, b"ATZ\r");
        opened.send(()).unwrap();
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            result => panic!("Cancelled worker kept the socket open: {result:?}"),
        }
    });
    let (tx, rx) = spawn_with_driver(
        Some(env!("CARGO_BIN_EXE_scanner-workstation").into()),
        Vec::new,
    );
    let cancel = Arc::new(AtomicBool::new(false));
    tx.try_send(Command::ConnectElm {
        id: 1,
        connection: stalled,
        cancel: cancel.clone(),
    })
    .unwrap_or_else(|error| panic!("{error}"));
    waiting.recv_timeout(Duration::from_secs(4)).unwrap();
    cancel.store(true, Ordering::Relaxed);
    wait_for(
        &rx,
        Duration::from_secs(3),
        |event| matches!(event, Event::Failed(1, reason) if reason == "Connection cancelled."),
    );
    server.join().unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = start_tcp_adapter(listener);
    tx.try_send(Command::ConnectElm {
        id: 2,
        connection: ElmConnection::Tcp(address.to_string()),
        cancel: Arc::new(AtomicBool::new(false)),
    })
    .unwrap_or_else(|error| panic!("{error}"));
    wait_for(&rx, Duration::from_secs(8), |event| {
        matches!(event, Event::Connected { id: 2, .. })
    });
    tx.try_send(Command::Shutdown)
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        server
            .join()
            .unwrap()
            .iter()
            .any(|command| command == "0100")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn elm_usb_serial_uses_the_workbench_connect_path_over_a_pty() {
    use std::os::fd::FromRawFd;

    let mut master = -1;
    let mut slave = -1;
    let mut name = [0i8; 256];
    // openpty returns two owned descriptors and writes the device path.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: The descriptors and terminated name were populated by openpty.
    let (mut master, slave, port) = unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
            std::ffi::CStr::from_ptr(name.as_ptr())
                .to_str()
                .unwrap()
                .to_owned(),
        )
    };
    let server = std::thread::spawn(move || {
        let mut reader = BufReader::new(master.try_clone().unwrap());
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            if reader.read_until(b'\r', &mut bytes).unwrap_or(0) == 0 {
                break;
            }
            let request = std::str::from_utf8(&bytes).unwrap().trim();
            let response = match request {
                "ATZ" | "ATI" => "ELM327 v2.3",
                "ATDPN" => "A3",
                "0100" => "486B1041008010000094",
                "010C" => "486B10410C0BB8D3",
                "ATRV" => "12.6V",
                _ => "OK",
            };
            if write!(master, "{response}\r>").is_err() {
                break;
            }
        }
    });
    let (tx, rx) = spawn_with_driver(
        Some(env!("CARGO_BIN_EXE_scanner-workstation").into()),
        Vec::new,
    );
    tx.try_send(Command::ConnectElm {
        id: 1,
        connection: ElmConnection::Serial {
            port,
            baud: Some(38400),
        },
        cancel: Arc::new(AtomicBool::new(false)),
    })
    .unwrap_or_else(|error| panic!("{error}"));
    let connected = wait_for(&rx, Duration::from_secs(8), |event| {
        matches!(event, Event::Connected { id: 1, .. })
    });
    assert!(
        matches!(connected, Event::Connected { protocol, pid_support, .. } if protocol.contains("ISO 9141-2") && pid_support.reported.contains(&12))
    );
    send_read(&tx, 2, 12);
    assert!(matches!(
        wait_for(&rx, Duration::from_secs(4), |event| matches!(event, Event::Rows(2, _))),
        Event::Rows(_, rows) if rows.iter().any(|row| row.source == Some(0x10) && row.value != "Unavailable")
    ));
    tx.try_send(Command::Shutdown)
        .unwrap_or_else(|error| panic!("{error}"));
    drop(slave);
    server.join().unwrap();
}
