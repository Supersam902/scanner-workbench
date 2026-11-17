use super::*;
use crate::adapter::DiagnosticAdapter;
use crate::elm327::block_on;

fn reading(value: &str, numeric: Option<f64>) -> Reading {
    Reading {
        source: Some(0x7e8),
        request: "010C".into(),
        name: "Engine RPM".into(),
        value: value.into(),
        unit: "rpm".into(),
        raw: "410C0C80".into(),
        numeric,
        status: "ECU responded".into(),
        signal_failure: false,
    }
}
fn vehicle() -> Vehicle {
    Vehicle {
        make: "Toyota".into(),
        model: "Camry".into(),
        year: "2015".into(),
        registration: "TEST-123".into(),
        ..Default::default()
    }
}
fn sample(at: u64, value: &str, numeric: Option<f64>) -> Sample {
    Sample {
        at_ms: at,
        rows: vec![reading(value, numeric)],
        ..Default::default()
    }
}
fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "scanner-test-{}-{}-{}",
        std::process::id(),
        name,
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ))
}
fn pump(app: &mut Workbench) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while app.busy && Instant::now() < deadline {
        app.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!app.busy, "worker did not finish: {}", app.status);
}

fn render_context() -> egui::Context {
    let ctx = egui::Context::default();
    appearance::configure(&ctx);
    ctx
}

#[test]
fn connection_page_keeps_the_latest_mongoose_result_after_starting_practice() {
    use crate::j2534::ProbePhase;
    use worker::{AttemptOutcome, ConnectionAttempt};

    let mut app = Workbench::new(temp("connection-results"));
    let (events, rx) = mpsc::channel();
    app.rx = rx;
    app.protocol_override = Some(CanProtocol::Can11Bit500K);
    app.voltage = Some(VoltageSample {
        adapter: VoltageAdapter::J2534(DriverInfo {
            name: "Mongoose-Plus MFC3".into(),
            path: PathBuf::from("sensitive-driver-directory/adapter.dll"),
        }),
        millivolts: 12700,
        sampled_at: Instant::now(),
    });
    app.vehicle.vin = "JTDBR32E720123456".into();
    app.logs.push_back("Private log line".into());
    app.connect();
    let id = app.epoch;
    let attempt = || ConnectionAttempt {
        driver: "Mongoose-Plus MFC3".into(),
        protocol: Some(CanProtocol::Can11Bit500K),
        remembered_protocol: true,
        outcome: AttemptOutcome::ProbeFailed(ProbePhase::Transmit),
        detail: Some("PassThruWriteMsgs failed (0x09): Only sent 0 of 1 messages".into()),
    };
    events
        .send(Event::ConnectAttempt(id - 1, attempt()))
        .unwrap();
    events.send(Event::ConnectAttempt(id, attempt())).unwrap();
    let result = "The adapter opened. Diagnostic request transmission failed.";
    events.send(Event::Failed(id, result.into())).unwrap();
    events.send(Event::ConnectAttempt(id, attempt())).unwrap();
    app.tick();
    assert_eq!(app.connection_report.as_ref().unwrap().attempts.len(), 1);
    assert_eq!(app.connection_error.as_deref(), Some(result));
    events.send(Event::Voltage(None)).unwrap();

    app.start_demo();
    events
        .send(Event::Connected {
            id: app.epoch,
            driver: None,
            protocol: "Practice".into(),
            demo: true,
            pid_support: PidSupport::default(),
        })
        .unwrap();
    app.tick();
    assert!(app.connected && app.demo);
    app.protocol_override = Some(CanProtocol::KwpFast);
    app.page = Page::Connection;
    let ctx = render_context();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 560.0));
    let draw = |app: &mut Workbench, events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let output = draw(&mut app, vec![]);
    let labels: BTreeSet<_> = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.job.text.clone()),
            _ => None,
        })
        .collect();
    for label in [
        "Connection attempts",
        "Copy connection report",
        "CAN 11-bit / 500 kbit/s",
        "Last verified protocol",
        "Request transmission failed",
        "PassThruWriteMsgs failed (0x09): Only sent 0 of 1 messages",
    ] {
        assert!(labels.contains(label), "Missing {label}");
    }
    let pos = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.job.text == "Copy connection report" => {
                let rect = text.visual_bounding_rect();
                shape
                    .clip_rect
                    .intersect(screen)
                    .contains_rect(rect)
                    .then_some(rect.center())
            }
            _ => None,
        })
        .expect("copy report is visible in the compact window");
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    draw(&mut app, vec![egui::Event::PointerMoved(pos), button(true)]);
    let copied = draw(&mut app, vec![button(false)]);
    let report = copied
        .platform_output
        .commands
        .iter()
        .find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text),
            _ => None,
        })
        .expect("connection report was copied");
    assert!(report.contains(&format!("Scanner Workbench {VERSION}")));
    assert!(report.contains("Protocol selection: CAN 11-bit / 500 kbit/s"));
    assert!(report.contains("Last DLC voltage at Connect: 12.7 V · Mongoose-Plus MFC3"));
    assert!(report.contains("Request transmission failed"));
    assert!(report.contains("Next check: Check the DLC connection and ignition"));
    for private in [
        "sensitive-driver-directory",
        "Private log line",
        "JTDBR32E720123456",
        "KWP2000",
    ] {
        assert!(!report.contains(private), "Copied unrelated data {private}");
    }
    assert!(draw(&mut app, vec![]).shapes.iter().any(|shape| {
        matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == "Copied")
    }));
    app.connect();
    assert!(
        app.connection_error.is_none()
            && app.connection_report.as_ref().unwrap().attempts.is_empty()
    );
    let id = app.epoch;
    events.send(Event::Failed(id, result.into())).unwrap();
    app.tick();
    assert_eq!(app.connection_error.as_deref(), Some(result));
}

#[test]
fn connection_report_preserves_connect_evidence_and_excludes_later_voltage() {
    use worker::{ReplyCheckOutcome, ReplyEvidence};

    let connection = ElmConnection::Serial {
        port: "COM7".into(),
        baud: None,
    };
    let mut report = connection_report::Report::new(Some(&connection), None, None, None);
    let wrong_adapter = VoltageSample {
        adapter: VoltageAdapter::Elm(ElmConnection::Tcp("192.168.0.10:35000".into())),
        millivolts: 10000,
        sampled_at: Instant::now(),
    };
    report.observe_voltage(&wrong_adapter);
    assert!(report.voltage_label().is_none());
    report.observe_voltage(&VoltageSample {
        adapter: VoltageAdapter::Elm(connection.clone()),
        millivolts: 12100,
        sampled_at: Instant::now(),
    });
    report.finish("Vehicle connected · CAN 11-bit / 500 kbit/s");
    report.record_link_loss("TCP adapter disconnected".into());
    report.observe_voltage(&VoltageSample {
        adapter: VoltageAdapter::Elm(connection),
        millivolts: 13100,
        sampled_at: Instant::now(),
    });
    let check = ReplyCheck {
        protocol: CanProtocol::Can11Bit500K,
        outcome: ReplyCheckOutcome::Partial,
        responses: vec![ReplyEvidence {
            source: Some(0x7e8),
            description: "Support bitmap is truncated".into(),
            raw: "410080".into(),
        }],
    };
    let text = report.text(Some(&check));
    assert!(text.contains("Adapter: ELM-compatible Serial · COM7\nBaud rate: Automatic"));
    assert!(text.contains("Result: Vehicle connected"));
    assert!(text.contains("Link lost: TCP adapter disconnected"));
    assert!(text.contains("Last DLC voltage at Connect: 12.1 V"));
    assert!(text.contains("Request 0100\nVehicle replied. Some ECU results are unverified."));
    assert!(text.contains("ECU 7E8 · Support bitmap is truncated · 410080"));
    assert!(!text.contains("13.1 V"));
}

#[test]
fn the_driver_picker_restores_the_preference_for_connection_and_voltage() {
    let root = temp("driver-picker");
    let driver = DriverInfo {
        name: "Other J2534 adapter".into(),
        path: "other-fixture.dll".into(),
    };
    {
        let mut app = Workbench::new(root.clone());
        app.select_driver(Some(driver.clone()));
        assert_eq!(app.driver_override.as_ref(), Some(&driver));
        assert_eq!(app.status, "Driver preference saved.");
    }
    let mut app = Workbench::with_worker(root.clone(), |_, settings| {
        assert_eq!(settings.preferred_driver.as_ref(), Some(&driver));
        worker::spawn_with_driver(None, Vec::new)
    });
    let (events, rx) = mpsc::channel();
    app.rx = rx;
    app.page = Page::Connection;
    assert_eq!(app.driver_override.as_ref(), Some(&driver));
    app.installed_drivers = Some(vec![]);

    let ctx = render_context();
    let output = ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 640.0),
            )),
            ..Default::default()
        },
        |ctx| app.draw(ctx),
    );
    assert!(output.shapes.iter().any(|shape| {
        matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == driver.name)
    }));
    assert!(output.shapes.iter().any(|shape| {
        matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text.contains("Selected driver is missing."))
    }));

    for candidate in [
        driver.clone(),
        DriverInfo {
            name: "Mongoose".into(),
            path: "mongoose-fixture.dll".into(),
        },
    ] {
        events
            .send(Event::Voltage(Some(VoltageSample {
                adapter: VoltageAdapter::J2534(candidate),
                millivolts: 12600,
                sampled_at: Instant::now(),
            })))
            .unwrap();
    }
    app.tick();
    assert_eq!(
        app.voltage.as_ref().unwrap().adapter,
        VoltageAdapter::J2534(driver.clone())
    );
    app.connect();
    assert!(app.status.contains(&driver.name));
    app.select_driver(None);
    assert_eq!(app.driver_override.as_ref(), Some(&driver));
    app.cancel_operation();
    app.select_driver(None);
    assert!(app.driver_override.is_none());
    drop(app);
    let app = Workbench::with_worker(root, |_, settings| {
        assert!(settings.preferred_driver.is_none());
        worker::spawn_with_driver(None, Vec::new)
    });
    assert!(app.driver_override.is_none());
}

#[test]
fn saving_a_driver_preference_can_retry_after_a_write_failure() {
    let root = temp("retry-driver-preference");
    std::fs::write(&root, b"blocked settings directory").unwrap();
    let mut app = Workbench::new(root.clone());
    let driver = DriverInfo {
        name: "Other J2534 adapter".into(),
        path: "other-fixture.dll".into(),
    };
    app.select_driver(Some(driver.clone()));
    assert_eq!(app.driver_override.as_ref(), Some(&driver));
    assert!(app.settings.preferred_driver.is_none());
    assert!(app.driver_save_error.is_some());
    std::fs::remove_file(&root).unwrap();
    app.save_driver_preference();
    assert!(app.driver_save_error.is_none());
    assert_eq!(
        Settings::load(&root).unwrap().preferred_driver,
        Some(driver)
    );
}

#[test]
fn elm_adapter_setup_validates_saves_and_routes_header_connect() {
    let root = temp("elm-adapter-setup");
    let connection = ElmConnection::Serial {
        port: "COM7".into(),
        baud: None,
    };
    {
        let mut app = Workbench::new(root.clone());
        app.adapter_kind = AdapterKind::Serial;
        app.connect();
        assert!(!app.busy && app.page == Page::Connection);
        assert!(
            app.adapter_problem
                .as_deref()
                .unwrap()
                .contains("serial port")
        );
        app.serial_port = "COM7".into();
        app.serial_baud = "not a baud rate".into();
        assert!(!app.apply_adapter_choice());
        app.serial_baud.clear();
        assert!(app.apply_adapter_choice());
        assert_eq!(app.elm_connection.as_ref(), Some(&connection));
    }
    let mut app = Workbench::with_worker(root.clone(), |_, settings| {
        assert_eq!(settings.elm_connection.as_ref(), Some(&connection));
        worker::spawn_with_driver(None, Vec::new)
    });
    assert_eq!(app.adapter_kind, AdapterKind::Serial);
    assert_eq!(app.serial_port, "COM7");
    app.serial_ports = Some(vec!["COM7".into()]);
    app.page = Page::Connection;
    let output = render_context().run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 640.0),
            )),
            ..Default::default()
        },
        |ctx| app.draw(ctx),
    );
    assert!(output.shapes.iter().any(|shape| {
        matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == "COM / serial port")
    }));
    app.connect();
    assert!(app.busy && app.status.contains("COM7"));
    app.cancel_operation();
    app.adapter_kind = AdapterKind::Tcp;
    app.tcp_address = "tcp://adapter".into();
    assert!(!app.apply_adapter_choice());
    app.tcp_address = "[::1]:35000".into();
    assert!(app.apply_adapter_choice());
    assert_eq!(
        Settings::load(&root).unwrap().elm_connection,
        Some(ElmConnection::Tcp("[::1]:35000".into()))
    );
    app.adapter_kind = AdapterKind::J2534;
    assert!(app.apply_adapter_choice());
    assert!(Settings::load(&root).unwrap().elm_connection.is_none());
}

#[test]
fn background_poll_keeps_toolbar_steady_and_yields_to_a_foreground_read() {
    let mut app = Workbench::new(temp("background-poll"));
    app.start_demo();
    pump(&mut app);
    app.page = Page::Live;
    let (events, rx) = mpsc::channel();
    app.rx = rx;
    app.start_live();
    app.next_live = Instant::now() + Duration::from_secs(10);
    let ctx = render_context();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960.0, 640.0));
    let draw = |app: &mut Workbench| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let toolbar = |output: egui::FullOutput| {
        output
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text)
                    if matches!(
                        text.galley.job.text.as_str(),
                        "Stop live"
                            | "Read once"
                            | "Presets"
                            | "Record"
                            | "Graphs"
                            | "Polling live data."
                    ) =>
                {
                    Some((
                        text.galley.job.text.clone(),
                        text.visual_bounding_rect(),
                        text.fallback_color,
                    ))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let idle = toolbar(draw(&mut app));
    assert_eq!(idle.len(), 6);
    app.next_live = Instant::now();
    app.tick();
    let poll = app.epoch;
    assert!(app.poll_in_flight && !app.busy);
    events
        .send(Event::Progress(poll, 2, 4, "RPM".into()))
        .unwrap();
    events
        .send(Event::LiveRows(
            poll,
            Instant::now(),
            vec![reading("800", Some(800.0))],
        ))
        .unwrap();
    app.tick();
    assert_eq!(idle, toolbar(draw(&mut app)));
    assert!(
        app.results
            .rows
            .values()
            .any(|row| row.latest.reading.value == "800")
    );
    events
        .send(Event::Finished(poll, "Operation finished.".into()))
        .unwrap();
    app.tick();
    assert!(!app.poll_in_flight && !app.busy && app.live);
    assert_eq!(app.status, "Polling live data.");

    app.next_live = Instant::now();
    app.tick();
    let old_poll = app.epoch;
    let old_cancel = app.cancel.clone();
    app.read_codes();
    assert!(app.busy && !app.live && !app.poll_in_flight);
    assert!(old_cancel.load(Ordering::Relaxed));
    events
        .send(Event::LiveRows(
            old_poll,
            Instant::now(),
            vec![reading("900", Some(900.0))],
        ))
        .unwrap();
    events
        .send(Event::Finished(old_poll, "Operation finished.".into()))
        .unwrap();
    app.tick();
    assert!(app.busy);
    assert!(
        !app.results
            .rows
            .values()
            .any(|row| row.latest.reading.value == "900")
    );
}

#[test]
fn recording_preview_and_full_csv_leave_the_vehicle_session_available() {
    let root = temp("recording-csv");
    let mut app = Workbench::new(root.clone());
    app.start_demo();
    pump(&mut app);
    let mut capture = Capture::new(vehicle(), true, 1000);
    let mut first = reading("-1.25", Some(-1.25));
    first.name = "Fuel, \"bank 1\"".into();
    first.unit = "°C".into();
    first.raw = "410C0C80".into();
    let mut failure = reading("Unavailable", None);
    failure.source = Some(0x7ea);
    failure.status = "  =1+1".into();
    capture.samples = vec![
        Sample {
            at_ms: 1000,
            rows: vec![first],
            ..Default::default()
        },
        Sample {
            at_ms: 1500,
            clear_requested: true,
            ..Default::default()
        },
        Sample {
            at_ms: 2000,
            graph_break: true,
            ..Default::default()
        },
        Sample {
            at_ms: 2500,
            rows: vec![failure],
            ..Default::default()
        },
    ];
    let path = root.join("shared").join("Road test.JSON");
    save_json(&path, &capture).unwrap();
    app.preview_recording(path.clone());
    let preview = app.recording_preview.as_ref().unwrap();
    assert_eq!((preview.readings, preview.issues), (2, 1));
    assert!(app.connected && app.playback.is_none());
    app.export_capture_csv(preview.capture.clone());
    assert!(app.connected && app.playback.is_none());
    let csv = std::fs::read_to_string(app.last_export.as_ref().unwrap()).unwrap();
    assert!(csv.starts_with("\u{feff}elapsed_ms,timestamp_utc,session,event,"));
    assert!(csv.contains("\"Fuel, \"\"bank 1\"\"\",\"'-1.25\",-1.25,\"°C\""));
    assert!(csv.contains("500,\"1970-01-01T00:00:01.500Z\",\"Practice\",\"Clear requested\""));
    assert!(csv.contains("1000,\"1970-01-01T00:00:02.000Z\",\"Practice\",\"Graph break\""));
    assert!(csv.contains("1500,\"1970-01-01T00:00:02.500Z\",\"Practice\",\"Read issue\",\"7EA\""));
    assert!(csv.contains("\"'  =1+1\""));
    assert!(csv.contains("\"410C0C80\"\r\n"));
    assert_eq!(csv.lines().count(), 5);

    app.start_recording();
    app.start_recording();
    app.finish_recording();
    assert!(app.saved_recordings.is_empty());
    app.open_recording(path);
    assert!(Arc::ptr_eq(
        &app.playback.as_ref().unwrap().capture(),
        &app.recording_preview.as_ref().unwrap().capture,
    ));
    assert!(!app.connected);
    let end = app.playback.as_ref().unwrap().end_ms();
    app.seek_replay(end);
    assert_eq!(app.results.failures.len(), 1);
}

#[test]
fn recorded_event_markers_survive_saving_and_jump_to_their_exact_readings() {
    let root = temp("recording-markers");
    let mut app = Workbench::new(root.clone());
    app.start_demo();
    pump(&mut app);
    app.start_recording();
    let start = app.recording.as_ref().unwrap().started_ms;
    app.apply_sample(sample(start + 100, "800", Some(800.0)));
    app.marker_note = "  =check, \"intermittent\"  ".into();
    app.mark_recording();
    assert!(app.marker_note.is_empty());
    let first = app.recording.as_ref().unwrap().markers[0].at_ms;
    app.apply_sample(sample(first + 100, "900", Some(900.0)));
    app.mark_recording();
    assert!(app.connected && !app.busy);
    app.finish_recording();

    let path = app.saved_recordings[0].path.clone();
    let capture = recordings::Preview::open(path.clone()).unwrap().capture;
    assert_eq!(capture.markers.len(), 2);
    assert_eq!(capture.markers[0].note, "=check, \"intermittent\"");
    assert!(capture.markers[1].note.is_empty());
    let last = capture.markers[1].at_ms;
    assert_eq!(capture.end_ms(), last);
    let csv = root.join("markers.csv");
    recordings::save_csv(&csv, &capture).unwrap();
    let csv = std::fs::read_to_string(csv).unwrap();
    let rows: Vec<_> = csv.lines().skip(1).collect();
    assert_eq!(rows.len(), 4);
    assert!(rows[0].contains("\"Reading\""));
    assert!(
        rows[1].contains("\"Marker\",\"\",\"\",\"Event marker\",\"'=check, \"\"intermittent\"\"\"")
    );
    assert!(rows[2].contains("\"Reading\""));
    assert!(rows[3].starts_with(&format!("{},", last - start)));
    assert!(rows[3].contains("\"Marker\""));

    app.page = Page::Recordings;
    let ctx = render_context();
    let draw = |app: &mut Workbench, events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 640.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let click_at = |app: &mut Workbench, pos| {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        draw(app, vec![egui::Event::PointerMoved(pos), button(true)]);
        draw(app, vec![button(false)]);
    };
    let click = |app: &mut Workbench, label| {
        let output = draw(app, vec![]);
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.job.text == label => {
                    let rect = text.visual_bounding_rect();
                    shape
                        .clip_rect
                        .expand(1.0)
                        .contains_rect(rect)
                        .then_some(rect.center())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing replay control {label}"));
        click_at(app, pos);
    };
    let engine_rpm = |app: &Workbench| {
        app.results
            .rows
            .values()
            .next()
            .unwrap()
            .latest
            .reading
            .numeric
    };
    assert_eq!(app.recording_preview.as_ref().unwrap().path, path);
    click(&mut app, "Review graphs");
    assert_eq!(app.page, Page::Live);
    assert!(app.graphs && app.playback.is_some() && !app.connected);
    pump(&mut app);
    assert!(app.results.rows.is_empty());
    let find_amber_marker = |output: &egui::FullOutput| {
        output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::LineSegment { points, stroke }
                    if stroke.color == appearance::AMBER
                        && points[0].x == points[1].x
                        && (points[1].y - points[0].y - 100.0).abs() < 0.1
                        && shape.clip_rect.contains(points[0].lerp(points[1], 0.5)) =>
                {
                    Some(points[0].lerp(points[1], 0.5))
                }
                _ => None,
            })
            .expect("chart shows a recording marker")
    };
    let marker = find_amber_marker(&draw(&mut app, vec![]));
    app.replay_playing = true;
    click_at(&mut app, marker);
    assert!(!app.replay_playing);
    assert_eq!(app.replay_at, first);
    assert_eq!(engine_rpm(&app), Some(800.0));

    app.seek_replay(start);
    app.replay_playing = true;
    click(&mut app, "Next marker");
    assert!(!app.replay_playing);
    assert_eq!(app.replay_at, first);
    assert_eq!(engine_rpm(&app), Some(800.0));
    click(&mut app, "Next marker");
    assert_eq!(app.replay_at, last);
    assert_eq!(engine_rpm(&app), Some(900.0));
    click(&mut app, "Previous marker");
    assert_eq!(app.replay_at, first);
    assert_eq!(app.replay_index, 1);
    assert_eq!(engine_rpm(&app), Some(800.0));

    app.start_demo();
    pump(&mut app);
    app.start_recording();
    app.mark_recording();
    app.finish_recording();
    assert_eq!(app.saved_recordings.len(), 2);
    let mark_only = &app.recording_preview.as_ref().unwrap().capture;
    assert_eq!(mark_only.row_count(), 0);
    assert_eq!(mark_only.markers.len(), 1);
    mark_only.validate().unwrap();
}

#[test]
fn failed_exports_preserve_the_existing_file() {
    let root = temp("atomic-write");
    let path = root.join("capture.csv");
    use std::io::Write;
    write_file(&path, |file| file.write_all(b"previous")).unwrap();
    let failure = write_file(&path, |file| {
        file.write_all(b"partial")?;
        Err(std::io::Error::other("write interrupted"))
    });
    assert!(failure.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"previous");
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
}

#[test]
fn failed_live_reads_preserve_values_age_and_statistics() {
    let mut results = Results::default();
    results.apply(&sample(1000, "800", Some(800.0)));
    results.apply(&sample(2000, "900", Some(900.0)));
    results.apply(&sample(3000, "Unavailable", None));
    let state = results.rows.values().next().unwrap();
    assert_eq!(state.latest.at_ms, 2000);
    assert_eq!(state.latest.reading.value, "900");
    assert_eq!((state.minimum, state.maximum), (Some(800.0), Some(900.0)));
    assert_eq!(results.failures.len(), 1);
    results.apply(&sample(4000, "850", Some(850.0)));
    assert!(results.failures.is_empty());
    let trace = graphs::Trace::new(&results.rows.values().next().unwrap().history, 0, 4000);
    assert_eq!(trace.runs.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);
    assert_eq!(trace.point_near(3000, 500), None);
    assert_eq!(trace.point_near(4000, 0).unwrap().value, Some(850.0));
}

#[test]
fn recording_graph_gaps_follow_ecu_failures_and_polling_pauses() {
    let rpm = |source, n| Reading {
        source: Some(source),
        ..reading("800", Some(n))
    };
    let first = rpm(0x7e8, 800.0);
    let second = rpm(0x7ea, 900.0);
    let mut capture = Capture::new(vehicle(), false, 1000);
    capture.samples = vec![
        Sample {
            at_ms: 1000,
            rows: vec![first.clone(), second.clone()],
            ..Default::default()
        },
        Sample {
            at_ms: 2000,
            rows: vec![
                Reading {
                    value: "Unavailable".into(),
                    numeric: None,
                    ..first.clone()
                },
                second.clone(),
            ],
            ..Default::default()
        },
        Sample {
            at_ms: 3000,
            rows: vec![first.clone(), second.clone()],
            ..Default::default()
        },
        Sample {
            at_ms: 4000,
            graph_break: true,
            ..Default::default()
        },
        Sample {
            at_ms: 5000,
            rows: vec![first.clone(), second.clone()],
            ..Default::default()
        },
    ];
    let replay: Capture = serde_json::from_slice(&serde_json::to_vec(&capture).unwrap()).unwrap();
    replay.validate().unwrap();
    let results = replay.replay(5000);
    let a = graphs::Trace::new(&results.rows[&row_key(&first)].history, 0, 5000);
    let b = graphs::Trace::new(&results.rows[&row_key(&second)].history, 0, 5000);
    assert_eq!(a.runs.iter().map(Vec::len).collect::<Vec<_>>(), [1, 1, 1]);
    assert_eq!(b.runs.iter().map(Vec::len).collect::<Vec<_>>(), [3, 1]);
    assert_eq!(a.point_near(4000, 500), None);
    assert_eq!(b.point_near(4000, 500), None);
}

#[test]
fn replay_charts_keep_both_sides_of_the_cursor_and_full_recorded_history() {
    let start = 1_000_000;
    let mut capture = Capture::new(vehicle(), false, start);
    capture.samples = (0..1400)
        .map(|offset| {
            let mut sample = sample(start + offset * 100, "800", Some(800.0));
            if offset == 754 {
                sample.rows[0].value = "Unavailable".into();
                sample.rows[0].numeric = None;
            } else if offset >= 1100 {
                sample.rows[0].unit = "Hz".into();
            }
            sample
        })
        .collect();
    capture.samples[758].graph_break = true;
    let cursor = start + 75_000;
    capture.markers.push(RecordingMarker {
        at_ms: cursor,
        note: "Intermittent idle".into(),
    });
    capture.validate().unwrap();
    let playback = replay_graphs::ReplayCapture::new(Arc::new(capture));
    let graphs = playback.graphs();
    let signal = graphs.signals.values().next().unwrap();
    let beginning = playback.axis(start, super::graphs::Window::TenSeconds);
    assert_eq!(
        signal.series_at(start).points_in(beginning.axis)[0].at_ms,
        start
    );
    let window = playback.axis(cursor, super::graphs::Window::TenSeconds);
    assert_eq!(window.axis.start_ms, cursor - 5000);
    assert_eq!(window.axis.time_at(0.5), cursor);
    assert_eq!(window.axis.end_ms, cursor + 5000);
    assert_eq!(
        window.marker_near(cursor + 1, 5).unwrap().note,
        "Intermittent idle"
    );
    assert!(window.marker_near(cursor + 6, 5).is_none());
    let series = signal.series_at(cursor);
    let points = series.points_in(window.axis);
    assert!(
        points
            .iter()
            .any(|point| point.at_ms < cursor && point.value.is_some())
    );
    assert!(
        points
            .iter()
            .any(|point| point.at_ms > cursor && point.value.is_some())
    );
    let trace = super::graphs::Trace::new(points, window.axis.start_ms, window.axis.end_ms);
    assert_eq!(trace.runs.len(), 3);
    assert_eq!(trace.point_near(start + 75_400, 0), None);
    let at_cursor = playback.replay(cursor);
    assert_eq!(at_cursor.rows.values().next().unwrap().latest.at_ms, cursor);
    let ending = playback.axis(playback.end_ms(), super::graphs::Window::TenSeconds);
    assert_eq!(ending.axis.end_ms, playback.end_ms());
    assert_eq!(signal.series_at(ending.cursor_ms).unit, "Hz");
    assert_eq!(series.unit, "RPM");
}
#[test]
fn recording_roundtrip_replays_values_failures_and_bounds() {
    let mut capture = Capture::new(vehicle(), false, 1000);
    capture.samples = vec![
        sample(1000, "800", Some(800.0)),
        sample(2000, "900", Some(900.0)),
        sample(3000, "Unavailable", None),
    ];
    capture.validate().unwrap();
    let root = temp("capture");
    let path = root.join("sample.json");
    save_json(&path, &capture).unwrap();
    save_json(&path, &capture).unwrap();
    let loaded: Capture = load_json(&path).unwrap();
    loaded.validate().unwrap();
    assert_eq!(
        loaded
            .replay(1500)
            .rows
            .values()
            .next()
            .unwrap()
            .latest
            .reading
            .value,
        "800"
    );
    assert_eq!(loaded.replay(3000).failures.len(), 1);
    capture.samples[1].at_ms = 999;
    assert!(capture.validate().is_err());
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(root);
}
#[test]
fn replay_atomic_uds_replacement_cannot_erase_another_ecu() {
    let mut a = reading("123456", None);
    a.request = "1902FF".into();
    a.name = "DTC 123456".into();
    let mut b = a.clone();
    b.source = Some(0x7e9);
    let mut capture = Capture::new(vehicle(), false, 1000);
    capture.samples.push(Sample {
        at_ms: 1000,
        rows: vec![a.clone(), b],
        ..Default::default()
    });
    a.name = "ECU DTC summary".into();
    a.value = "0 matching records".into();
    capture.samples.push(Sample {
        at_ms: 2000,
        rows: vec![a],
        replace: Some((Some(0x7e8), "1902FF".into())),
        ..Default::default()
    });
    capture.validate().unwrap();
    let result = capture.replay(2000);
    assert_eq!(result.rows.len(), 2);
    assert!(
        result
            .rows
            .values()
            .any(|v| v.latest.reading.source == Some(0x7e9) && v.latest.reading.value == "123456")
    );
    capture.samples[1].replace = Some((Some(0x7e9), "1902FF".into()));
    assert!(capture.validate().is_err());
}
#[test]
fn comparisons_require_same_vehicle_and_fresh_reread() {
    let mut r = Results::default();
    r.apply(&sample(1000, "800", Some(800.0)));
    let before = Snapshot::from_results(&vehicle(), false, 1500, &r);
    let after = Snapshot::from_results(&vehicle(), false, 2000, &r);
    assert_eq!(comparison(&before, &after).unwrap()[0].3, "Not retested");
    r.apply(&sample(2200, "900", Some(900.0)));
    let after = Snapshot::from_results(&vehicle(), false, 2400, &r);
    assert!(
        comparison(&before, &after).unwrap()[0]
            .3
            .contains("+100.000")
    );
    let mut wrong = after.clone();
    wrong.vehicle.year = "2016".into();
    assert!(comparison(&before, &wrong).is_err());
    wrong = after;
    wrong.demo = true;
    assert!(comparison(&before, &wrong).is_err());
    r.apply(&sample(2500, "Unavailable", None));
    let failed = Snapshot::from_results(&vehicle(), false, 2600, &r);
    assert_eq!(
        comparison(&before, &failed).unwrap()[0].3,
        "Read failed / not verified"
    );
}

#[test]
fn replay_keeps_clear_outcomes_and_marks_pre_clear_evidence_by_ecu() {
    let result = |ecu, request: &str, value: &str| Reading {
        source: Some(ecu),
        request: request.into(),
        name: request.into(),
        ..reading(value, None)
    };
    let mut capture = Capture::new(vehicle(), false, 1000);
    capture.samples = vec![
        Sample {
            at_ms: 1000,
            rows: vec![
                result(0x7e8, "03", "P0133"),
                result(0x7e9, "03", "P0300"),
                result(0x7e8, "0A", "P0133"),
                result(0x7e8, "0101", "Ready"),
                result(0x7e8, "020C00", "1200"),
            ],
            ..Default::default()
        },
        Sample {
            at_ms: 2000,
            clear_requested: true,
            ..Default::default()
        },
        Sample {
            at_ms: 2100,
            rows: vec![
                result(0x7e8, "04", "Clear acknowledged by this ECU"),
                result(0x7e9, "04", "Unavailable"),
            ],
            ..Default::default()
        },
        Sample {
            at_ms: 2500,
            rows: vec![result(0x7e8, "03", "No codes reported by this ECU")],
            ..Default::default()
        },
        Sample {
            at_ms: 3000,
            clear_requested: true,
            ..Default::default()
        },
    ];
    let capture: Capture = serde_json::from_slice(&serde_json::to_vec(&capture).unwrap()).unwrap();
    capture.validate().unwrap();
    let before = Snapshot::from_results(&vehicle(), false, 1000, &capture.replay(1000));
    let submitted = capture.replay(2000);
    assert_eq!(submitted.clear_attempt_ms(), Some(2000));
    assert_eq!(
        submitted
            .rows
            .values()
            .filter(|v| submitted.predates_clear(&v.latest))
            .count(),
        4
    );
    let mut results = capture.replay(2500);
    let row = |ecu, request| row_key(&result(ecu, request, ""));
    assert!(!results.predates_clear(&results.rows[&row(0x7e8, "03")].latest));
    assert!(results.predates_clear(&results.rows[&row(0x7e9, "03")].latest));
    assert!(!results.predates_clear(&results.rows[&row(0x7e8, "0A")].latest));
    assert_eq!(results.clear_attempt_ms(), Some(2000));
    assert!(results.failures.contains_key(&scope(Some(0x7e9), "04")));
    let after = Snapshot::from_results(&vehicle(), false, 2500, &results);
    let html = report(&after, Some(&before));
    assert!(html.contains("1 ECU acknowledgment"));
    assert!(html.contains("Before clear request · reread needed"));
    assert!(
        comparison(&before, &after)
            .unwrap()
            .iter()
            .any(|(name, _, _, state)| {
                name == "03 / ECU 7E9" && state == "Reread needed after clear request"
            })
    );

    results.apply(&capture.samples[4]);
    assert_eq!(results.clear_attempt_ms(), Some(3000));
    assert!(!results.rows.contains_key(&row(0x7e8, "04")));
    assert!(!results.failures.contains_key(&scope(Some(0x7e9), "04")));
    let mut legacy = capture.clone();
    legacy.samples.remove(1);
    assert_eq!(legacy.replay(2500).clear_attempt_ms(), Some(2100));
}

#[test]
fn conflicting_vins_stay_visible_until_the_ecu_replies_agree() {
    let mut app = Workbench::new(temp("vin-conflict"));
    let vin = |source, value: &str| Reading {
        source: Some(source),
        request: "0902".into(),
        name: "VIN".into(),
        value: value.into(),
        ..reading(value, None)
    };
    let first = vin(0x7e8, "JTDBR32E720123456");
    let second = vin(0x7ea, "JTDBR32E720123457");
    app.apply_sample(Sample {
        at_ms: 1000,
        rows: vec![first.clone()],
        ..Default::default()
    });
    assert_eq!(app.vehicle.vin, first.value);
    app.apply_sample(Sample {
        at_ms: 2000,
        rows: vec![
            Reading {
                name: "Manual read".into(),
                ..first.clone()
            },
            second.clone(),
        ],
        ..Default::default()
    });
    assert!(app.vehicle.vin.is_empty() && app.observed_vin.is_none());
    assert_eq!(app.results.rows.len(), 2);
    assert!(report(&app.snapshot(), None).contains("ECUs reported different VINs."));
    app.apply_sample(Sample {
        at_ms: 3000,
        rows: vec![first.clone(), vin(0x7ea, "Unavailable")],
        ..Default::default()
    });
    assert!(app.vin_conflict);
    assert!(
        app.results
            .refresh_failed(&app.results.rows[&row_key(&second)].latest)
    );
    app.apply_sample(Sample {
        at_ms: 4000,
        rows: vec![vin(0x7ea, &first.value)],
        ..Default::default()
    });
    assert!(!app.vin_conflict);
    assert_eq!(app.vehicle.vin, first.value);
}

#[test]
fn monitor_replay_corrects_old_results_and_replaces_only_the_refreshed_ecu() {
    let old_zero = Reading {
        source: Some(0x7e8),
        request: "0621".into(),
        name: "MID 21 / TID 01".into(),
        value: "0.0000 (min 0.0000, max 0.0000)".into(),
        raw: "46210101000000000000".into(),
        unit: "count".into(),
        numeric: Some(0.0),
        status: "PASS".into(),
        signal_failure: false,
    };
    let other = Reading {
        source: Some(0x7ea),
        name: "MID 21 / TID 02".into(),
        raw: "46210201000400000006".into(),
        value: "4.0000 (min 0.0000, max 6.0000)".into(),
        numeric: Some(4.0),
        ..old_zero.clone()
    };
    let refreshed = Reading {
        source: Some(0x7e8),
        ..other.clone()
    };
    let timeout = Reading {
        value: "Unavailable".into(),
        name: "Monitor 21".into(),
        unit: String::new(),
        raw: String::new(),
        numeric: None,
        status: "ECU did not return this monitor result".into(),
        ..other.clone()
    };
    let mut capture = Capture::new(vehicle(), false, 1000);
    capture.samples = vec![
        Sample {
            at_ms: 1000,
            rows: vec![old_zero.clone(), other.clone()],
            ..Default::default()
        },
        Sample {
            at_ms: 2000,
            rows: vec![refreshed.clone(), timeout],
            ..Default::default()
        },
    ];
    capture.validate().unwrap();
    let before = capture.replay(1000);
    let corrected = &before.rows[&row_key(&old_zero)].latest;
    assert_eq!(corrected.reading.status, "Not completed");
    assert_eq!(corrected.reading.numeric, None);

    let after = capture.replay(2000);
    assert!(!after.rows.contains_key(&row_key(&old_zero)));
    assert!(!after.refresh_failed(&after.rows[&row_key(&refreshed)].latest));
    assert!(after.refresh_failed(&after.rows[&row_key(&other)].latest));
    let snapshot = Snapshot::from_results(&vehicle(), false, 2000, &after);
    assert!(report(&snapshot, None).contains("Refresh failed · result unverified"));
}

#[test]
fn report_escapes_notes_and_labels_simulation_and_failed_reads() {
    let mut v = vehicle();
    v.notes = "<script>alert('x')</script>".into();
    let mut r = Results::default();
    r.apply(&sample(1000, "800", Some(800.0)));
    r.apply(&sample(2000, "Unavailable", None));
    let html = report(&Snapshot::from_results(&v, true, 3000, &r), None);
    assert!(html.contains("DEMONSTRATION DATA"));
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("Unavailable"));
    assert!(html.contains("Refresh failed · result unverified"));
    assert!(!html.contains("Age at capture"));
    assert!(!html.contains("2.0s"));
}

#[test]
fn a_legacy_monitor_result_renders_and_survives_recording_replay() {
    use crate::adapter::DiagnosticResponse;

    let query = Query {
        bytes: vec![5, 5, 0x10],
        target: None,
        name: "Oxygen test".into(),
        signals: vec![],
        verified: false,
    };
    let mut rows = diagnostics::decode(
        &query,
        &DiagnosticResponse::new(Some(0x10), vec![0x45, 5, 0x10, 18, 0, 25]),
        false,
    )
    .unwrap();
    rows[0].name = format!("Bank 2 sensor 1 / {}", rows[0].name);
    rows[0].status = "PASS".into();
    let mut capture = Capture::new(vehicle(), false, 1000);
    capture.samples.push(Sample {
        at_ms: 1100,
        rows,
        ..Default::default()
    });
    capture.validate().unwrap();
    let mut app = Workbench::new(temp("legacy-monitor-page"));
    app.connected = true;
    app.protocol = CanProtocol::Iso9141.label().into();
    app.page = Page::MonitorTests;
    app.results = capture.replay(1100);
    let ctx = render_context();
    let output = ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 640.0),
            )),
            ..Default::default()
        },
        |ctx| app.draw(ctx),
    );
    let labels: BTreeSet<_> = output
        .shapes
        .into_iter()
        .filter_map(|shape| {
            if let egui::Shape::Text(text) = shape.shape {
                Some(text.galley.job.text.clone())
            } else {
                None
            }
        })
        .collect();
    for label in [
        "Read oxygen tests",
        "Read test",
        "Pass",
        "Rich-to-lean switch time",
        "72 ms",
    ] {
        assert!(labels.contains(label), "Missing {label}");
    }
    let html = report(
        &Snapshot::from_results(&vehicle(), false, 1200, &app.results),
        None,
    );
    assert!(html.contains("On-board monitor test results"));
    assert!(html.contains("Bank 2 sensor 1 / Rich-to-lean switch time"));
    assert!(html.contains("72 (min 0, max 100) ms"));
}

#[test]
fn printed_report_links_freeze_frame_by_ecu_and_code_and_preserves_evidence_state() {
    let mut app = Workbench::new(temp("print-scan"));
    app.start_demo();
    pump(&mut app);
    app.read_emissions_scan();
    pump(&mut app);
    let mut other_ecu = reading("P0133", None);
    other_ecu.source = Some(0x7ea);
    other_ecu.request = "03".into();
    other_ecu.name = "Stored emissions codes".into();
    let mut different_code = other_ecu.clone();
    different_code.source = Some(0x7e8);
    different_code.value = "P0133, P0300".into();
    app.apply_sample(Sample {
        at_ms: app.now(),
        rows: vec![other_ecu, different_code],
        ..Default::default()
    });
    let html = report(&app.snapshot(), None);
    assert!(html.contains("<td>7E8</td><td>Stored</td><th scope=\"row\">P0133</th>"));
    assert!(html.contains("<td>7EA</td><td>Stored</td><th scope=\"row\">P0133</th>"));
    // Stored and permanent from the frame's ECU share one anchor. The matching
    // code from the other ECU must not be associated with that frame.
    assert_eq!(html.matches("href=\"#frame-7E8-0\"").count(), 2);
    assert!(html.contains("<td>No ECU result</td>"));
    assert!(html.contains("<td>No matching frame</td>"));
    assert!(!html.contains("frame-7EA-0"));
    assert!(html.contains("Trigger code P0133"));
    assert!(html.contains("2080"));
    assert!(!html.contains("s before capture"));
    assert!(!html.contains("<th>Read status</th>"));
    assert!(html.contains("MIL On"));
    assert!(html.contains("<th>Since clear</th><th>This drive cycle</th>"));
    assert!(html.contains("Oxygen sensor monitoring</th><td>Complete</td><td>Incomplete"));
    assert!(
        html.contains(
            "Evaporative system monitoring</th><td>Incomplete</td><td>Disabled this cycle"
        )
    );

    let mut repeated_trigger = reading("P0133", None);
    repeated_trigger.request = "020200".into();
    repeated_trigger.name = "DTC that caused freeze-frame storage".into();
    repeated_trigger.raw = "4202000133".into();
    app.apply_sample(Sample {
        at_ms: app.now(),
        rows: vec![repeated_trigger],
        ..Default::default()
    });
    let html = report(&app.snapshot(), None);
    assert!(html.contains("Frame 0</a><br>Trigger only"));
    assert!(html.contains("Before latest trigger read"));

    let mut failed = reading("Unavailable", None);
    failed.request = "020200".into();
    failed.name = "DTC that caused freeze-frame storage".into();
    failed.status = "No reply".into();
    app.apply_sample(Sample {
        at_ms: app.now(),
        rows: vec![failed],
        ..Default::default()
    });
    let html = report(&app.snapshot(), None);
    assert!(!html.contains("href=\"#frame-7E8-0\""));
    assert!(html.contains("<td>Unverified</td>"));
    assert!(html.contains("Trigger code P0133</strong>"));
    assert!(
        html.contains("<small class=\"result-note\">Refresh failed · result unverified</small>")
    );
    assert!(html.contains("No reply"));
}

#[test]
fn freeze_frame_link_opens_the_matching_ecu_and_updates_on_replay() {
    let mut app = Workbench::new(temp("freeze-frame-link"));
    let mut code = reading("P0133", None);
    code.request = "03".into();
    code.name = "Stored emissions codes".into();
    let mut trigger = reading("P0133", None);
    trigger.request = "020203".into();
    trigger.name = "DTC that caused freeze-frame storage".into();
    trigger.raw = "4202030133".into();
    let mut rpm = reading("2080", Some(2080.0));
    rpm.request = "020C03".into();
    let mut other_ecu = rpm.clone();
    other_ecu.source = Some(0x7ea);
    other_ecu.value = "3175".into();
    other_ecu.numeric = Some(3175.0);
    let mut missing_trigger = trigger.clone();
    missing_trigger.value = "Unavailable".into();
    missing_trigger.status = "ECU did not return this PID".into();
    let mut capture = Capture::new(vehicle(), false, 1000);
    capture.samples = vec![
        Sample {
            at_ms: 1000,
            rows: vec![code, trigger.clone(), rpm, other_ecu],
            ..Default::default()
        },
        Sample {
            at_ms: 2000,
            rows: vec![trigger],
            ..Default::default()
        },
        Sample {
            at_ms: 3000,
            rows: vec![missing_trigger],
            ..Default::default()
        },
    ];
    capture.validate().unwrap();
    app.playback = Some(replay_graphs::ReplayCapture::new(Arc::new(capture)));
    app.seek_replay(1000);
    app.page = Page::Codes;
    let ctx = render_context();
    let draw = |app: &mut Workbench, events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 640.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let text_rect = |output: &egui::FullOutput, label| {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.job.text == label => {
                Some(text.visual_bounding_rect())
            }
            _ => None,
        })
    };
    let codes = draw(&mut app, vec![]);
    let link = text_rect(&codes, "Frame 3").expect("code table has a frame link");
    let pointer = link.center();
    let button = |pressed| egui::Event::PointerButton {
        pos: pointer,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    draw(
        &mut app,
        vec![egui::Event::PointerMoved(pointer), button(true)],
    );
    draw(&mut app, vec![button(false)]);
    assert_eq!(app.page, Page::FreezeFrame);
    assert_eq!((app.frame, app.frame_ecu), (3, Some(0x7e8)));
    let frame = draw(&mut app, vec![]);
    for label in ["Frame 3 · ECU 7E8", "Trigger code P0133", "2080.00"] {
        assert!(text_rect(&frame, label).is_some(), "Missing {label}");
    }
    assert!(text_rect(&frame, "3175.00").is_none());
    assert!(!app.busy && !app.connected);

    app.seek_replay(2000);
    let frame = draw(&mut app, vec![]);
    assert!(text_rect(&frame, "Before latest trigger").is_some());
    assert!(text_rect(&frame, "No verified values from this trigger read.").is_some());
    app.seek_replay(3000);
    let frame = draw(&mut app, vec![]);
    assert!(text_rect(&frame, "Trigger refresh failed · unverified").is_some());
    app.page = Page::Codes;
    let codes = draw(&mut app, vec![]);
    assert!(text_rect(&codes, "Unverified").is_some());
    assert!(text_rect(&codes, "Frame 3").is_none());
}

#[test]
fn printing_a_replayed_scan_uses_the_data_at_its_end() {
    let root = temp("print-replay-scan");
    let mut app = Workbench::new(root.clone());
    app.start_demo();
    pump(&mut app);
    app.start_recording();
    app.read_emissions_scan();
    pump(&mut app);
    let before = app
        .results
        .emissions_scan
        .as_ref()
        .unwrap()
        .ended_ms
        .unwrap();
    app.dispatch(vec![app.base_query(4, None)], None, ReadMode::Control);
    pump(&mut app);
    app.recheck_after_clear();
    pump(&mut app);
    app.read_freeze_frame();
    pump(&mut app);
    app.finish_recording();
    app.open_recording(app.saved_recordings[0].path.clone());
    pump(&mut app);
    assert!(app.latest_scan.is_none());
    let end = app.playback.as_ref().unwrap().end_ms();
    assert!(end > before);
    app.seek_replay(end);
    assert!(app.results.rows.values().any(|row| {
        row.latest.reading.request == "03"
            && row.latest.reading.value == "No codes reported by this ECU"
    }));

    let scan = app.results.emissions_scan.as_ref().expect("replayed scan");
    assert_eq!(scan.ended_ms, Some(before));
    assert_eq!(scan.state, emissions_scan::State::Finished);
    let path = app
        .export_latest_scan()
        .unwrap_or_else(|| panic!("{}", app.status));
    let printed = std::fs::read_to_string(path).unwrap();
    assert!(printed.contains("<td>Stored</td><th scope=\"row\">P0133</th>"));
    assert!(printed.contains("href=\"#frame-7E8-0\""));
    assert!(printed.contains("MIL On"));
    assert!(!printed.contains("Latest emissions clear request"));
    let session = std::fs::read_to_string(app.export_report().unwrap()).unwrap();
    assert!(session.contains("Latest emissions clear request"));
    assert!(session.contains("No codes reported by this ECU"));
    assert!(session.contains("ECU reported no stored freeze frame."));
    assert!(session.contains("<td>Frame 0 empty</td>"));
}

#[test]
fn interrupted_emissions_scan_preserves_fresh_results_and_marks_unread_checks() {
    let root = temp("interrupted-emissions");
    let mut app = Workbench::new(root.clone());
    app.start_demo();
    pump(&mut app);
    app.read_emissions_scan();
    pump(&mut app);
    let summary = app.results.emissions_scan.as_ref().unwrap();
    assert_eq!(summary.state, emissions_scan::State::Finished);
    assert!(summary.checks().all(|(_, result)| result.is_some()));

    app.start_recording();
    let (tx, rx) = mpsc::channel();
    app.rx = rx;
    app.read_emissions_scan();
    let id = app.epoch;
    let mut stored = reading("No codes reported by this ECU", None);
    stored.request = "03".into();
    stored.name = "Stored emissions codes".into();
    tx.send(Event::Rows(id, vec![stored.clone()])).unwrap();
    app.tick();
    app.stop_operation();
    tx.send(Event::Finished(id, "Operation stopped.".into()))
        .unwrap();
    app.tick();
    app.finish_recording();

    let path = &app.saved_recordings[0].path;
    let capture: Capture = load_json(path).unwrap();
    capture.validate().unwrap();
    let replay = capture.replay(capture.end_ms());
    let scan = replay.emissions_scan.as_ref().unwrap();
    assert_eq!(scan.state, emissions_scan::State::Interrupted);
    let checks: Vec<_> = scan
        .checks()
        .map(|(check, result)| (check, scan.check_label(result)))
        .collect();
    assert!(
        checks
            .iter()
            .any(|(check, text)| *check == emissions_scan::Check::Stored
                && text == "0 codes · 1 ECU reply")
    );
    assert!(
        checks
            .iter()
            .any(|(check, text)| *check == emissions_scan::Check::Vin
                && text == "No result from this scan")
    );
    let html = report(
        &Snapshot::from_results(&app.vehicle, true, capture.end_ms(), &replay),
        None,
    );
    assert!(html.contains("Emissions scan interrupted"));
    assert!(html.contains("<th scope=\"row\">VIN</th><td>No result from this scan</td>"));

    let directory = root.join("scans");
    let mut history = scan_history::History::default();
    history.refresh(&directory);
    let entries = history.entries.as_ref().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].state, emissions_scan::State::Interrupted);
    assert_eq!(entries[1].state, emissions_scan::State::Finished);
    history.select(entries[0].path.clone()).unwrap();
    let saved = &history.selected.as_ref().unwrap().1;
    assert!(saved.rows.iter().all(|row| row.reading.request == "03"));
    assert!(!report(saved, None).contains("P0133"));
}

#[test]
fn scan_history_keeps_original_report_after_clear_and_reopens_after_restart() {
    let root = temp("scan-history");
    {
        let mut app = Workbench::new(root.clone());
        app.start_demo();
        pump(&mut app);
        app.vehicle.registration = "SHOP-123".into();
        app.vehicle.notes = "Before repair".into();
        app.read_emissions_scan();
        pump(&mut app);
        assert!(app.latest_scan.is_some());
        assert!(app.saved_recordings.is_empty());

        app.vehicle.notes = "After repair".into();
        app.dispatch(vec![app.base_query(4, None)], None, ReadMode::Control);
        pump(&mut app);
        app.recheck_after_clear();
        pump(&mut app);
        assert!(app.results.rows.values().any(|row| {
            row.latest.reading.request == "03"
                && row.latest.reading.value == "No codes reported by this ECU"
        }));
        let path = app.export_latest_scan().unwrap();
        let original = std::fs::read_to_string(path).unwrap();
        assert!(original.contains("Trigger code P0133"));
        assert!(original.contains("Before repair"));
        assert!(!original.contains("After repair"));
        assert!(!original.contains("Latest emissions clear request"));
    }
    let mut app = Workbench::new(root.clone());
    app.scan_history.refresh(&root.join("scans"));
    let entries = app.scan_history.entries.as_ref().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].matches("shop-123"));
    assert!(entries[0].matches("wdd00000000000000"));
    assert!(!entries[0].matches("another-vehicle"));
    assert_eq!(entries[0].state, emissions_scan::State::Finished);
    app.scan_history.select(entries[0].path.clone()).unwrap();
    let printed = std::fs::read_to_string(app.export_saved_scan().unwrap()).unwrap();
    assert!(printed.contains("Before repair"));
    assert!(printed.contains("Trigger code P0133"));
    assert!(printed.contains("DEMONSTRATION DATA"));
    assert!(!app.connected && app.results.rows.is_empty());

    app.page = Page::ScanHistory;
    let ctx = render_context();
    let output = ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 640.0),
            )),
            ..Default::default()
        },
        |ctx| app.draw(ctx),
    );
    let labels: BTreeSet<_> = output
        .shapes
        .iter()
        .filter_map(|shape| {
            if let egui::Shape::Text(text) = &shape.shape {
                Some(text.galley.job.text.as_str())
            } else {
                None
            }
        })
        .collect();
    for label in [
        "Scan history",
        "Import scan…",
        "Practice scan",
        "Review scan",
        "Print saved scan",
        "Export scan…",
        "Readiness since clear",
    ] {
        assert!(labels.contains(label), "Missing {label}");
    }
}

#[test]
fn reviewing_a_saved_scan_keeps_its_evidence_and_the_current_session_separate() {
    let root = temp("review-saved-scan");
    let mut app = Workbench::new(root.clone());
    app.start_demo();
    pump(&mut app);
    app.read_emissions_scan();
    pump(&mut app);
    app.dispatch(vec![app.base_query(4, None)], None, ReadMode::Control);
    pump(&mut app);
    app.recheck_after_clear();
    pump(&mut app);
    app.read_freeze_frame();
    pump(&mut app);
    assert!(app.results.rows.values().any(|row| {
        row.latest.reading.request == "03"
            && row.latest.reading.value == "No codes reported by this ECU"
    }));

    let active_epoch = app.epoch;
    let clear_at = app.results.clear_attempt_ms().unwrap();
    app.scan_history.refresh(&root.join("scans"));
    let path = app.scan_history.entries.as_ref().unwrap()[0].path.clone();
    app.scan_history.select(path.clone()).unwrap();
    app.page = Page::ScanHistory;
    let ctx = render_context();
    let draw = |app: &mut Workbench, events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 640.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let text_rect = |output: &egui::FullOutput, label: &str| {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.job.text == label => {
                let rect = text.visual_bounding_rect();
                shape
                    .clip_rect
                    .expand(1.0)
                    .contains_rect(rect)
                    .then_some(rect)
            }
            _ => None,
        })
    };
    let click = |app: &mut Workbench, label: &str| {
        let output = draw(app, vec![]);
        let pointer = text_rect(&output, label)
            .unwrap_or_else(|| panic!("Missing {label}"))
            .center();
        let button = |pressed| egui::Event::PointerButton {
            pos: pointer,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        draw(app, vec![egui::Event::PointerMoved(pointer), button(true)]);
        draw(app, vec![button(false)]);
    };

    click(&mut app, "Review scan");
    assert_eq!(app.page, Page::ScanReview);
    assert!(text_rect(&draw(&mut app, vec![]), "P0133").is_some());
    click(&mut app, "Frame 0");
    let frame = draw(&mut app, vec![]);
    for label in ["Frame 0 · ECU 7E8", "Trigger code P0133", "88 °C"] {
        assert!(text_rect(&frame, label).is_some(), "Missing {label}");
    }
    click(&mut app, "Readiness");
    let readiness = draw(&mut app, vec![]);
    for label in ["MIL On", "Since clear", "This drive cycle", "Incomplete"] {
        assert!(text_rect(&readiness, label).is_some(), "Missing {label}");
    }
    click(&mut app, "Back to scan history");
    assert_eq!(app.page, Page::ScanHistory);
    assert_eq!(app.scan_history.selected.as_ref().unwrap().0, path);
    assert_eq!(app.epoch, active_epoch);
    assert_eq!(app.results.clear_attempt_ms(), Some(clear_at));
    assert!(app.connected && app.demo && !app.busy);
}

#[test]
fn saved_scan_notes_print_from_overview_and_history_without_rewriting_the_capture() {
    let root = temp("scan-notes");
    let mut app = Workbench::new(root.clone());
    app.start_demo();
    pump(&mut app);
    app.vehicle.notes = "Customer reported rough idle".into();
    app.read_emissions_scan();
    pump(&mut app);

    let (scan_path, captured) = app.latest_scan.as_ref().unwrap().clone();
    let evidence = std::fs::read(&scan_path).unwrap();
    assert_eq!(
        scan_notes::load(&scan_path).unwrap(),
        scan_notes::Notes::default()
    );
    app.scan_history.refresh(&root.join("scans"));
    app.open_scan_review(captured, Some(scan_path.clone()), Page::Home);

    let ctx = render_context();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960.0, 640.0));
    let draw = |app: &mut Workbench, events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let click = |app: &mut Workbench, label: &str| {
        let output = draw(app, vec![]);
        let pointer = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.job.text == label => {
                    let rect = text.visual_bounding_rect();
                    shape
                        .clip_rect
                        .intersect(screen)
                        .expand(1.0)
                        .contains_rect(rect)
                        .then_some(rect.center())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing {label}"));
        let button = |pressed| egui::Event::PointerButton {
            pos: pointer,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        draw(app, vec![egui::Event::PointerMoved(pointer), button(true)]);
        draw(app, vec![button(false)]);
    };

    click(&mut app, "Notes");
    click(&mut app, "Optional reference");
    draw(&mut app, vec![egui::Event::Text("WO-557 & A".into())]);
    click(&mut app, "Findings, work performed, or follow-up");
    draw(
        &mut app,
        vec![egui::Event::Paste(
            "Checked & documented\n<Follow up>".into(),
        )],
    );
    assert!(app.scan_review.as_ref().unwrap().has_unsaved_notes());
    app.page = Page::Home;
    assert!(app.export_latest_scan().is_none());
    assert_eq!(app.status, scan_notes::UNSAVED_MESSAGE);
    click(&mut app, "Unsaved scan notes");
    assert_eq!(app.page, Page::ScanReview);
    let close = render_context().run(egui::RawInput::default(), |ctx| app.prepare_to_close(ctx));
    assert!(
        close.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::CancelClose)
    );
    assert!(app.scan_review.as_ref().unwrap().has_unsaved_notes());
    click(&mut app, "Save scan notes");
    assert!(!app.scan_review.as_ref().unwrap().has_unsaved_notes());
    assert!(app.connected && app.demo);
    assert_eq!(app.vehicle.notes, "Customer reported rough idle");
    assert_eq!(std::fs::read(&scan_path).unwrap(), evidence);
    assert!(app.scan_history.entries.as_ref().unwrap()[0].matches("wo-557"));

    let printed = std::fs::read_to_string(app.export_latest_scan().unwrap()).unwrap();
    for item in [
        "WO-557 &amp; A",
        "Checked &amp; documented\n&lt;Follow up&gt;",
        "Customer reported rough idle",
        "P0133",
    ] {
        assert!(printed.contains(item), "Missing {item}");
    }
    drop(app);
    let mut reopened = Workbench::new(root.clone());
    reopened.scan_history.refresh(&root.join("scans"));
    let entries = reopened.scan_history.entries.as_ref().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].matches("wo-557"));
    reopened.scan_history.select(scan_path).unwrap();
    let printed_again = std::fs::read_to_string(reopened.export_saved_scan().unwrap()).unwrap();
    assert_eq!(printed, printed_again);
}

#[test]
fn saved_scan_transfer_preserves_evidence_and_saved_notes() {
    let folder = tempfile::tempdir().unwrap();
    let mut first = Workbench::new(folder.path().join("first"));
    first.start_demo();
    pump(&mut first);
    first.read_emissions_scan();
    pump(&mut first);
    let (original, _) = first.latest_scan.as_ref().unwrap();
    let original = original.clone();
    let evidence = std::fs::read(&original).unwrap();
    let notes = scan_notes::Notes {
        work_order: "WO-TRANSFER & 14".into(),
        technician: "Retest <after repair>".into(),
    };
    scan_notes::save(&original, &scan_notes::Notes::default(), &notes).unwrap();
    first.scan_history.select(original.clone()).unwrap();

    let without_extension = folder.path().join("emissions");
    let exported = without_extension.with_extension("obdscan");
    first.export_scan_file(without_extension.clone(), true);
    assert_eq!(first.last_export.as_ref(), Some(&exported));
    let original_export = std::fs::read(&exported).unwrap();
    assert_eq!(std::fs::read(&original).unwrap(), evidence);
    first.export_scan_file(without_extension, true);
    assert!(first.status.contains("A file already exists"));
    assert_eq!(std::fs::read(&exported).unwrap(), original_export);

    let other_root = folder.path().join("other");
    let directory = other_root.join("scans");
    let mut other = Workbench::new(other_root.clone());
    other.start_demo();
    pump(&mut other);
    let active_epoch = other.epoch;
    let live_rows = other.results.rows.len();
    other.scan_history.search = "No match".into();
    other.import_scan_file(&exported);
    assert_eq!(other.status, "Scan imported into Scan history.");
    assert!(other.scan_history.search.is_empty());
    assert!(other.connected && other.demo && !other.busy);
    assert_eq!(other.epoch, active_epoch);
    assert_eq!(other.results.rows.len(), live_rows);
    let (local, snapshot) = other.scan_history.selected.as_ref().unwrap();
    assert_eq!(local.parent(), Some(directory.as_path()));
    assert_eq!(snapshot, &first.scan_history.selected.as_ref().unwrap().1);
    let local = local.clone();

    other.import_scan_file(&original);
    assert!(other.status.contains("already in Scan history"));
    assert_eq!(other.scan_history.selected.as_ref().unwrap().0, local);
    assert_eq!(other.scan_history.entries.as_ref().unwrap().len(), 1);
    other.export_scan_file(local.clone(), true);
    assert!(other.status.contains("A file already exists"));

    let updated = scan_notes::Notes {
        work_order: "WO-UPDATED".into(),
        ..notes.clone()
    };
    scan_notes::save(&local, &notes, &updated).unwrap();
    assert_eq!(scan_notes::load(&local).unwrap(), updated);
    assert_eq!(std::fs::read(&exported).unwrap(), original_export);
    assert_eq!(std::fs::read(&original).unwrap(), evidence);
    drop(other);

    let mut reopened = Workbench::new(other_root);
    reopened.scan_history.refresh(&directory);
    let entries = reopened.scan_history.entries.as_ref().unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].matches("wo-updated"));
    reopened.scan_history.select(local).unwrap();
    let report = std::fs::read_to_string(reopened.export_saved_scan().unwrap()).unwrap();
    for value in [
        "WO-UPDATED",
        "Retest &lt;after repair&gt;",
        "Trigger code P0133",
    ] {
        assert!(report.contains(value), "Missing {value}");
    }

    reopened.import_scan_file(&exported);
    let entries = reopened.scan_history.entries.as_ref().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|entry| entry.matches("wo-updated")));
    assert!(entries.iter().any(|entry| entry.matches("wo-transfer")));
}

#[test]
fn invalid_saved_scan_import_leaves_history_untouched() {
    let folder = tempfile::tempdir().unwrap();
    let mut app = Workbench::new(folder.path().join("workbench"));
    app.start_demo();
    pump(&mut app);
    app.read_emissions_scan();
    pump(&mut app);
    let original = app.latest_scan.as_ref().unwrap().0.clone();
    app.scan_history.refresh(&app.root.join("scans"));
    app.scan_history.select(original.clone()).unwrap();
    let path = folder.path().join("invalid.OBDSCAN");
    let snapshot = app.scan_history.selected.as_ref().unwrap().1.clone();
    let mut file = serde_json::json!({ "version": 2, "scan": snapshot.as_ref(), "notes": {} });
    save_json(&path, &file).unwrap();
    app.import_scan_file(&path);
    assert!(app.status.contains("Unsupported scan file version 2"));
    assert_eq!(app.scan_history.selected.as_ref().unwrap().0, original);

    file["version"] = 1.into();
    file["scan"]["rows"][0]["at_ms"] = (snapshot.at_ms + 1).into();
    save_json(&path, &file).unwrap();
    app.import_scan_file(&path);
    assert!(app.status.contains("outside the saved scan timeline"));
    assert_eq!(app.scan_history.selected.as_ref().unwrap().0, original);
    assert_eq!(app.scan_history.entries.as_ref().unwrap().len(), 1);
}

#[test]
fn a_failed_scan_history_save_can_retry_after_more_than_one_scan() {
    let root = temp("scan-history-retry");
    std::fs::create_dir_all(&root).unwrap();
    let directory = root.join("scans");
    std::fs::write(&directory, b"blocking file").unwrap();
    let mut app = Workbench::new(root);
    app.start_demo();
    pump(&mut app);
    for _ in 0..2 {
        app.read_emissions_scan();
        pump(&mut app);
    }
    assert_eq!(app.scan_history.pending_count(), 2);
    assert!(app.scan_history.save_error.is_some());
    std::fs::remove_file(&directory).unwrap();
    app.scan_history.refresh(&directory);
    app.scan_history.retry_saves();
    assert_eq!(app.scan_history.pending_count(), 0);
    assert!(app.scan_history.save_error.is_none());
    let entries = app.scan_history.entries.as_ref().unwrap();
    assert_eq!(entries.len(), 2);
    assert_ne!(entries[0].path, entries[1].path);
    app.scan_history.retry_saves();
    assert_eq!(app.scan_history.entries.as_ref().unwrap().len(), 2);
}

#[test]
fn closing_keeps_unsaved_scans_available_for_recovery() {
    let root = temp("close-with-unsaved-scan");
    std::fs::create_dir_all(&root).unwrap();
    let blocked = root.join("scans");
    std::fs::write(&blocked, b"blocking file").unwrap();
    let mut app = Workbench::new(root.clone());
    app.start_demo();
    pump(&mut app);
    app.read_emissions_scan();
    pump(&mut app);
    assert_eq!(app.scan_history.pending_count(), 1);

    let ctx = render_context();
    let check_close = |app: &mut Workbench| {
        ctx.run(egui::RawInput::default(), |ctx| app.prepare_to_close(ctx))
            .viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::CancelClose)
    };
    assert!(check_close(&mut app));
    assert_eq!(app.page, Page::ScanHistory);
    assert_eq!(app.scan_history.pending_count(), 1);
    assert_eq!(app.scan_history.save_pending_as(&blocked), 0);
    assert_eq!(app.scan_history.pending_count(), 1);

    let recovery = root.join("recovered");
    assert_eq!(app.scan_history.save_pending_as(&recovery), 1);
    assert_eq!(app.scan_history.pending_count(), 0);
    assert!(!check_close(&mut app));
    let path = std::fs::read_dir(recovery)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    app.scan_history.select(path).unwrap();
    let snapshot = &app.scan_history.selected.as_ref().unwrap().1;
    assert!(snapshot.demo);
    assert!(snapshot.rows.iter().any(|row| row.reading.value == "P0133"));
}

#[test]
fn saved_scan_comparison_prints_changes_after_clear_and_restart() {
    use scan_comparison::Assessment;
    let root = temp("compare-saved-scans");
    {
        let mut app = Workbench::new(root.clone());
        app.start_demo();
        pump(&mut app);
        app.page = Page::Codes;
        app.start_recording();
        app.capture_follow_up();
        assert!(!app.busy);
        app.capture_emissions_scan(Some(ClearScanStage::Before));
        pump(&mut app);
        let (before_path, before) = app.before_clear_scan.clone().unwrap();
        assert!(before_path.is_file());
        assert!(
            before
                .rows
                .iter()
                .any(|row| row.reading.request == "03" && row.reading.value == "P0133")
        );
        assert!(
            before
                .rows
                .iter()
                .any(|row| row.reading.request.starts_with("02"))
        );
        assert!(report(&before, None).contains("Before clear"));
        assert!(app.results.clear_attempt_ms().is_none());
        app.confirm_emissions_clear();
        assert!(app.results.clear_attempt_ms().is_none());
        app.confirm_clear = true;
        app.confirm_emissions_clear();
        assert!(app.results.clear_attempt_ms().is_none());
        app.clear_ready = true;
        app.confirm_emissions_clear();
        assert!(app.clearing);
        pump(&mut app);
        assert_eq!(app.clear_baseline.as_ref(), Some(&before_path));
        assert!(
            app.results
                .rows
                .values()
                .any(|row| row.latest.reading.request == "04")
        );
        app.capture_follow_up();
        pump(&mut app);
        assert_eq!(app.page, Page::ScanHistory);
        assert_eq!(app.scan_history.comparison.as_ref().unwrap().0, before_path);
        let after = &app.scan_history.selected.as_ref().unwrap().1;
        assert!(report(after, None).contains("After clear request"));

        app.finish_recording();
        let capture: Capture = load_json(&app.saved_recordings[0].path).unwrap();
        capture.validate().unwrap();
        let replay = capture.replay(capture.end_ms());
        assert_eq!(
            replay.emissions_scan.unwrap().clear_stage,
            Some(ClearScanStage::After)
        );
        let csv = root.join("before-after.csv");
        recordings::save_csv(&csv, &capture).unwrap();
        let csv = std::fs::read_to_string(csv).unwrap();
        assert!(csv.contains("Before clear"));
        assert!(csv.contains("After clear request"));
    }

    let mut app = Workbench::new(root.clone());
    app.scan_history.refresh(&root.join("scans"));
    let entries = app.scan_history.entries.as_ref().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].clear_stage, Some(ClearScanStage::After));
    assert_eq!(entries[1].clear_stage, Some(ClearScanStage::Before));
    assert!(entries[1].matches("before clear"));
    let later = entries[0].path.clone();
    let earlier = entries[1].path.clone();
    app.scan_history.select(later.clone()).unwrap();
    let selected = &app.scan_history.selected.as_ref().unwrap().1;
    let entries = app.scan_history.entries.as_ref().unwrap();
    assert!(entries[1].precedes(selected));
    assert!(!entries[0].precedes(selected));
    app.scan_history.compare_with(earlier.clone()).unwrap();
    let comparison = &app.scan_history.comparison.as_ref().unwrap().1;
    let [codes, readiness, cycle] = &comparison.sections;
    assert!(codes.changes.iter().any(|row| row.source == Some(0x7e8)
        && row.item == "Stored P0133"
        && row.assessment == Assessment::NoLongerReported));
    assert!(codes.changes.iter().any(|row| row.source == Some(0x7e8)
        && row.item == "Permanent P0133"
        && row.assessment == Assessment::StillReported));
    assert!(
        readiness
            .changes
            .iter()
            .any(|row| row.item == "Check-engine light"
                && row.before == "On"
                && row.after == "Off")
    );
    assert!(
        cycle
            .changes
            .iter()
            .any(|row| row.item == "Catalyst monitoring"
                && row.before == "Complete"
                && row.after == "Incomplete")
    );
    let printable = std::fs::read_to_string(app.export_scan_comparison().unwrap()).unwrap();
    for label in [
        "Emissions scan comparison",
        "DEMONSTRATION DATA",
        "Print / Save as PDF",
        "Readiness this drive cycle",
        "Still reported",
        "No longer reported",
    ] {
        assert!(printable.contains(label), "Missing {label}");
    }
    assert!(!app.connected && app.results.rows.is_empty());
    app.page = Page::ScanHistory;
    let ctx = render_context();
    let output = ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 640.0),
            )),
            ..Default::default()
        },
        |ctx| app.draw(ctx),
    );
    let labels: BTreeSet<_> = output
        .shapes
        .iter()
        .filter_map(|shape| {
            if let egui::Shape::Text(text) = &shape.shape {
                Some(text.galley.job.text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(labels.contains("Print comparison"));
    assert!(labels.contains("Before / after"));

    app.scan_history.select(earlier).unwrap();
    assert!(app.scan_history.comparison.is_none());
    assert!(app.scan_history.compare_with(later).is_err());
    assert!(app.export_scan_comparison().is_none());
}

#[test]
fn saved_scan_comparison_preserves_missing_replies_and_requires_matching_identity() {
    use scan_comparison::{Assessment, Comparison};
    let code = |source, request: &str, value: &str, status: &str| Reading {
        source: Some(source),
        request: request.into(),
        name: "Code list".into(),
        status: status.into(),
        ..reading(value, None)
    };
    let archived = |start, rows| {
        let mut results = Results::default();
        results.apply(&Sample {
            at_ms: start,
            emissions_scan: Some(ScanEvent::Started),
            ..Default::default()
        });
        results.apply(&Sample {
            at_ms: start + 1,
            rows,
            ..Default::default()
        });
        results.apply(&Sample {
            at_ms: start + 2,
            emissions_scan: Some(ScanEvent::Interrupted),
            ..Default::default()
        });
        Snapshot::from_emissions_scan(&vehicle(), false, &results).unwrap()
    };
    let empty = "No codes reported by this ECU";
    let mut before = archived(
        100,
        vec![
            code(0x7e8, "03", "P0133", "ECU responded"),
            code(0x7ea, "03", "P0133", "ECU responded"),
            code(0x7e8, "07", empty, "ECU responded"),
        ],
    );
    let mut after = archived(
        200,
        vec![
            code(0x7e8, "03", empty, "ECU responded"),
            code(
                0x7ea,
                "03",
                "Unavailable",
                "ECU did not return this code list",
            ),
            code(0x7e8, "07", "P0133", "ECU responded"),
            code(
                0x7e8,
                "0A",
                "Unavailable",
                "ECU reports this code service is unsupported",
            ),
        ],
    );
    let comparison = Comparison::new(&before, &after).unwrap();
    let changes = &comparison.sections[0].changes;
    assert!(changes.iter().any(|row| row.source == Some(0x7ea)
        && row.item == "Stored P0133"
        && row.after == "No reply"
        && row.assessment == Assessment::Unverified));
    assert!(changes.iter().any(|row| row.source == Some(0x7e8)
        && row.item == "Stored P0133"
        && row.assessment == Assessment::NoLongerReported));
    assert!(
        changes
            .iter()
            .any(|row| row.item == "Pending P0133" && row.assessment == Assessment::NewCode)
    );
    assert!(changes.iter().any(|row| row.item == "Permanent codes"
        && row.before == "Not read"
        && row.after == "Unsupported"
        && row.assessment == Assessment::Unverified));

    after.vehicle.registration.clear();
    assert!(Comparison::new(&before, &after).is_err());
    before.vehicle.vin = "JTDBR32E720123456".into();
    after.vehicle.vin = "jtdbr32e720123456".into();
    assert!(Comparison::new(&before, &after).is_ok());
    after.demo = true;
    assert!(Comparison::new(&before, &after).is_err());
    after.demo = false;
    after.vehicle.vin = "JTDBR32E720123457".into();
    after
        .vehicle
        .registration
        .clone_from(&before.vehicle.registration);
    assert!(Comparison::new(&before, &after).is_err());
    after.vehicle.vin.clone_from(&before.vehicle.vin);
    after.rows.push(Stamped {
        reading: code(0x7e8, "0902", "JTDBR32E720123457", "ECU responded"),
        at_ms: 201,
    });
    assert!(
        Comparison::new(&before, &after)
            .err()
            .unwrap()
            .contains("conflicting VIN")
    );
}
#[test]
fn garage_validates_vin_and_supported_years_without_slicing_unicode() {
    assert!(!valid_vin("🔥🔥🔥🔥🔥"));
    assert!(!valid_vin("WDD000000I0000000"));
    assert!(valid_vin("WDD00000000000000"));
    let mut v = vehicle();
    v.validate().unwrap();
    v.year = "1995".into();
    assert!(v.validate().is_err());
    v.year = "1996".into();
    v.validate().unwrap();
}
#[test]
fn simulator_clears_only_emissions_and_keeps_permanent_codes() {
    let mut adapter = worker::DemoAdapter::new();
    assert_eq!(
        block_on(adapter.request(&[3], 100)).unwrap()[0].payload,
        [0x43, 1, 1, 0x33]
    );
    assert_eq!(
        block_on(adapter.request(&[4], 100)).unwrap()[0].payload,
        [0x44]
    );
    assert_eq!(
        block_on(adapter.request(&[3], 100)).unwrap()[0].payload,
        [0x43, 0]
    );
    assert_eq!(
        block_on(adapter.request(&[10], 100)).unwrap()[0].payload,
        [0x4a, 1, 1, 0x33]
    );
    assert_eq!(
        block_on(adapter.request(&[0x19, 2, 0xff], 100)).unwrap()[0].payload,
        [0x7f, 0x19, 0x11]
    );
}
#[test]
fn full_practice_workflow_scans_reads_records_replays_and_exports() {
    let root = temp("workflow");
    let mut app = Workbench::new(root.clone());
    app.start_demo();
    pump(&mut app);
    assert!(app.connected && app.demo);
    app.refresh_pid_support();
    pump(&mut app);
    assert!(app.pid_support.reported.contains(&12));
    app.read_readiness();
    pump(&mut app);
    assert!(
        app.results
            .rows
            .values()
            .any(|v| v.latest.reading.request == "0101")
    );
    app.start_recording();
    app.read_vehicle_information();
    pump(&mut app);
    assert!(app.vehicle_info_support.as_ref().unwrap().problem.is_none());
    assert!(app.results.rows.values().any(|row| {
        row.latest.reading.request == "0908"
            && row.latest.reading.name == "Catalyst bank 1"
            && row.latest.reading.value == "42 / 48"
    }));
    app.read_supported_monitors();
    pump(&mut app);
    assert_eq!(
        app.monitor_support.as_ref().unwrap().reported,
        BTreeSet::from([1, 2, 0x21])
    );
    for (request, state) in [
        ("0601", "PASS"),
        ("0602", "Not completed"),
        ("0621", "FAIL"),
    ] {
        assert!(
            app.results.rows.values().any(|v| {
                v.latest.reading.request == request && v.latest.reading.status == state
            })
        );
    }
    app.read_emissions_scan();
    pump(&mut app);
    assert!(
        app.results
            .rows
            .values()
            .any(|v| v.latest.reading.request == "03")
    );
    assert_eq!(
        app.results.emissions_scan.as_ref().unwrap().state,
        emissions_scan::State::Finished,
    );
    app.save_baseline();
    assert!(app.baseline.is_some());
    app.dispatch(vec![app.base_query(4, None)], None, ReadMode::Control);
    assert!(app.clearing);
    assert!(app.results.clear_attempt_ms().is_some());
    pump(&mut app);
    assert!(!app.clearing);
    assert!(
        app.results
            .rows
            .values()
            .any(|v| v.latest.reading.request == "04")
    );
    app.recheck_after_clear();
    pump(&mut app);
    assert!(app.results.rows.values().any(|v| {
        v.latest.reading.request == "03"
            && v.latest.reading.value == "No codes reported by this ECU"
            && !app.results.predates_clear(&v.latest)
    }));
    app.once(app.selected_queries());
    pump(&mut app);
    assert!(app.recording_rows > 0);
    app.finish_recording();
    assert!(app.recording.is_none() && app.unsaved.is_none());
    app.export_report();
    assert!(app.last_export.as_ref().unwrap().exists());
    let html = std::fs::read_to_string(app.last_export.as_ref().unwrap()).unwrap();
    assert!(html.contains("<summary><strong>Emissions scan</strong>"));
    for heading in ["Trouble codes", "Emissions readiness", "Freeze frame"] {
        assert!(html.contains(&format!("<h2>{heading}</h2>")));
    }
    assert_eq!(app.saved_recordings.len(), 1);
    let path = app.saved_recordings[0].path.clone();
    app.open_recording(path);
    assert!(!app.connected);
    assert!(app.playback.is_some());
    let cap = app.playback.as_ref().unwrap();
    cap.validate().unwrap();
    app.replay_at = cap.end_ms();
    app.results = cap.replay(app.replay_at);
    assert!(!app.results.rows.is_empty());
    assert_eq!(
        app.results.emissions_scan.as_ref().unwrap().state,
        emissions_scan::State::Finished,
    );
    assert!(app.results.rows.values().any(|v| {
        v.latest.reading.request == "0602" && v.latest.reading.status == "Not completed"
    }));
    // Files are retained in the test's uniquely named temp folder for inspection.
}
#[test]
fn stop_rejects_queued_old_events_and_cancels_clear_confirmation() {
    let mut app = Workbench::new(temp("epoch"));
    let id = app.epoch;
    app.confirm_clear = true;
    app.cancel_operation();
    assert_ne!(app.epoch, id);
    assert!(!app.confirm_clear);
    assert!(!app.live);
    let (events, rx) = mpsc::channel();
    app.rx = rx;
    events
        .send(Event::Update(Update::Row(id, reading("800", Some(800.0)))))
        .unwrap();
    events.send(Event::Finished(id, "stale".into())).unwrap();
    app.tick();
    assert!(app.results.rows.is_empty());
    assert_ne!(app.status, "stale");
    app.manual = "2701".into();
    assert!(app.manual_query().is_err());
    app.manual = "222017".into();
    assert!(app.manual_query().is_err());
    app.manual = "0801".into();
    assert!(app.manual_query().is_err());
    app.manual = "0902".into();
    assert!(app.manual_query().is_ok());
    app.busy = true;
    app.stop_operation();
    assert!(app.busy && app.cancel.load(Ordering::Relaxed));
}

#[test]
fn favorites_preset_does_not_add_unpinned_engine_items() {
    let mut app = Workbench::new(temp("favorites"));
    app.settings.favorites.insert("OBD:010C".into());
    app.preset("Favorites");
    assert_eq!(app.selected, BTreeSet::from(["OBD:010C".into()]));
}

#[test]
fn saved_presets_keep_their_pids_across_restarts_and_vehicle_support_changes() {
    let root = temp("live-presets");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("settings.json"), r#"{"favorites":["OBD:010C"]}"#).unwrap();
    let rpm = "OBD:010C".to_owned();
    let speed = "OBD:010D".to_owned();
    let selection = BTreeSet::from([rpm.clone(), speed]);
    let mut app = Workbench::new(root.clone());
    let engine = app.selected.clone();
    assert!(app.settings.live_presets.is_empty());
    app.selected = selection.clone();
    app.save_live_preset("  Road test  ").unwrap();
    app.selected = BTreeSet::from(["OBD:0105".into()]);
    app.save_live_preset("Cold start").unwrap();
    drop(app);

    let mut app = Workbench::new(root.clone());
    assert_eq!(app.selected, engine);
    assert_eq!(app.settings.favorites, BTreeSet::from([rpm.clone()]));
    assert_eq!(app.settings.live_presets.len(), 2);
    app.load_live_preset("Road test").unwrap();
    app.start_demo();
    pump(&mut app);
    app.pid_support.checked_through = 31;
    app.pid_support.reported = BTreeSet::from([0x0c]);
    assert_eq!(app.selected, selection);
    assert_eq!(
        app.selected_live_queries()
            .map(query_key)
            .collect::<Vec<_>>(),
        vec![rpm]
    );
    assert_eq!(app.settings.live_presets["Road test"], selection);
    assert!(!app.live && !app.poll_in_flight);
    app.reset_results();
    assert_eq!(app.selected_live_queries().count(), 2);
    assert_eq!(app.selected, selection);

    let mut other = Workbench::new(root.clone());
    other.selected = BTreeSet::from(["OBD:0105".into()]);
    assert!(other.save_live_preset("road TEST").is_err());
    assert_eq!(
        Settings::load(&root).unwrap().live_presets["Road test"],
        selection
    );
    other.delete_live_preset("Road test", &selection).unwrap();
    other.save_live_preset("Road test").unwrap();
    assert!(app.delete_live_preset("Road test", &selection).is_err());
    assert_eq!(
        Settings::load(&root).unwrap().live_presets["Road test"],
        other.selected
    );
}

#[test]
fn live_preset_menu_saves_loads_and_deletes_in_a_compact_window() {
    let mut app = Workbench::new(temp("live-preset-menu"));
    app.page = Page::Live;
    app.selected = BTreeSet::from(["OBD:010C".into()]);
    let ctx = render_context();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 560.0));
    let draw = |app: &mut Workbench, events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let click = |app: &mut Workbench, label: &str| {
        let output = draw(app, vec![]);
        let pointer = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.job.text == label => {
                    let rect = text.visual_bounding_rect();
                    shape
                        .clip_rect
                        .intersect(screen)
                        .expand(1.0)
                        .contains_rect(rect)
                        .then_some(rect.center())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("Missing {label}"));
        let button = |pressed| egui::Event::PointerButton {
            pos: pointer,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        draw(app, vec![egui::Event::PointerMoved(pointer), button(true)]);
        draw(app, vec![button(false)]);
    };

    click(&mut app, "Presets");
    click(&mut app, "Save current selection…");
    draw(&mut app, vec![]);
    draw(&mut app, vec![egui::Event::Text("Idle test".into())]);
    click(&mut app, "Save preset");
    assert_eq!(app.status, "Saved preset “Idle test”.");
    assert_eq!(app.settings.live_presets["Idle test"], app.selected);
    app.preset("Engine");
    click(&mut app, "Presets");
    click(&mut app, "Idle test");
    assert_eq!(app.selected, BTreeSet::from(["OBD:010C".into()]));
    assert!(!app.busy && !app.live);

    click(&mut app, "Presets");
    click(&mut app, "Delete…");
    click(&mut app, "Delete preset");
    assert!(Settings::load(&app.root).unwrap().live_presets.is_empty());
    assert_eq!(app.selected, BTreeSet::from(["OBD:010C".into()]));
}

#[test]
fn separate_windows_keep_each_others_garage_and_favorite_edits() {
    let root = temp("shared-settings");
    let mut older = Workbench::new(root.clone());
    let mut other = Workbench::new(root.clone());
    other
        .edit_settings(|settings| {
            settings.favorites.insert("OBD:010C".into());
        })
        .unwrap();

    older.draft = vehicle();
    older.save_garage();
    assert_eq!(older.status, "Vehicle saved to your garage.");
    assert_eq!(
        older.settings.favorites,
        BTreeSet::from(["OBD:010C".into()])
    );
    other
        .edit_settings(|settings| {
            settings.favorites.insert("OBD:010D".into());
        })
        .unwrap();
    drop(older);

    let saved = Settings::load(&root).unwrap();
    assert_eq!(saved.garage, vec![vehicle()]);
    assert_eq!(
        saved.favorites,
        BTreeSet::from(["OBD:010C".into(), "OBD:010D".into()])
    );
}

#[test]
fn a_competing_settings_write_can_retry_after_the_first_edit_finishes() {
    let root = temp("settings-lock");
    let writer_root = root.clone();
    let (locked, entered) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        Settings::edit(&writer_root, |settings| {
            locked.send(()).unwrap();
            resumed.recv_timeout(Duration::from_secs(4)).unwrap();
            settings.garage.push(vehicle());
        })
    });
    entered.recv_timeout(Duration::from_secs(4)).unwrap();
    let competing = Settings::edit(&root, |settings| {
        settings.favorites.insert("OBD:010C".into());
    });
    resume.send(()).unwrap();
    writer.join().unwrap().unwrap();
    assert!(competing.unwrap_err().contains("Another scanner"));

    let saved = Settings::edit(&root, |settings| {
        settings.favorites.insert("OBD:010C".into());
    })
    .unwrap();
    assert_eq!(saved.garage, vec![vehicle()]);
    assert_eq!(saved.favorites, BTreeSet::from(["OBD:010C".into()]));
}

#[test]
fn unavailable_signals_keep_their_own_state_in_live_data_and_old_recordings() {
    use crate::adapter::DiagnosticResponse;
    use serde_json::json;

    let query = Query {
        bytes: vec![1, 0x66],
        target: None,
        name: "Three test signals".into(),
        signals: ["A", "B", "C"].into_iter().enumerate().map(|(index, name)| {
            json!({"name": name, "fmt": {"bix": index * 8, "len": 8, "nullmax": 255}})
        }).collect(),
        verified: false,
    };
    let decode = |data: [u8; 3]| {
        let mut bytes = vec![0x41, 0x66];
        bytes.extend(data);
        diagnostics::decode(&query, &DiagnosticResponse::new(Some(0x7e8), bytes), true).unwrap()
    };
    let mut capture = Capture::new(vehicle(), false, 1000);
    capture.samples = vec![
        Sample {
            at_ms: 1000,
            rows: decode([20, 30, 40]),
            ..Default::default()
        },
        Sample {
            at_ms: 2000,
            rows: decode([21, 255, 255]),
            ..Default::default()
        },
    ];
    assert!(!capture.samples[1].rows[0].signal_failure);
    assert!(capture.samples[1].rows[1].signal_failure && capture.samples[1].rows[2].signal_failure);
    let mut legacy = serde_json::to_value(&capture).unwrap();
    for index in [1, 2] {
        legacy["samples"][1]["rows"][index]
            .as_object_mut()
            .unwrap()
            .remove("signal_failure");
    }
    let legacy: Capture = serde_json::from_value(legacy).unwrap();
    legacy.validate().unwrap();

    for capture in [capture, legacy] {
        let before = Snapshot::from_results(&vehicle(), false, 1000, &capture.replay(1000));
        let mut results = capture.replay(2000);
        let current = |name| {
            results
                .rows
                .values()
                .find(|v| v.latest.reading.name == name)
                .unwrap()
        };
        assert_eq!(results.failures.len(), 2);
        assert_eq!(current("A").latest.reading.numeric, Some(21.0));
        assert!(!results.refresh_failed(&current("A").latest));
        assert!(results.refresh_failed(&current("B").latest));
        assert!(results.refresh_failed(&current("C").latest));
        let after = Snapshot::from_results(&vehicle(), false, 2000, &results);
        let compared = comparison(&before, &after).unwrap();
        assert!(compared[0].3.starts_with("Change +1.000"));
        assert_eq!(compared[1].3, "Read failed / not verified");
        assert_eq!(compared[2].3, "Read failed / not verified");
        results.apply(&Sample {
            at_ms: 3000,
            rows: decode([22, 32, 255]),
            ..Default::default()
        });
        assert_eq!(results.failures.len(), 1);
        assert_eq!(results.failures.values().next().unwrap().reading.name, "C");
    }
}

#[test]
fn an_unscoped_request_failure_keeps_last_values_until_the_next_ecu_reply() {
    let mut r = Results::default();
    let good = reading("800", Some(800.0));
    r.apply(&sample(2000, "800", Some(800.0)));
    let mut failed = reading("Unavailable", None);
    failed.source = None;
    r.apply(&Sample {
        at_ms: 3000,
        rows: vec![failed.clone()],
        ..Default::default()
    });
    assert!(r.refresh_failed(&r.rows.values().next().unwrap().latest));
    r.apply(&Sample {
        at_ms: 4000,
        rows: vec![failed, good],
        ..Default::default()
    });
    assert_eq!(r.failures.len(), 1);
    assert!(!r.refresh_failed(&r.rows.values().next().unwrap().latest));
    r.apply(&sample(5000, "900", Some(900.0)));
    assert!(r.failures.is_empty());
}

#[test]
fn a_freeze_frame_scan_follows_valid_ranges_when_another_bitmap_is_truncated() {
    use crate::adapter::DiagnosticResponse;
    let mut scan = diagnostics::SupportScan::new(2, 3);
    scan.accept(Ok(vec![
        DiagnosticResponse::new(Some(0x7e8), vec![0x42, 0, 3, 8, 0, 0, 1]),
        DiagnosticResponse::new(Some(0x7ea), vec![0x42, 0, 3, 0]),
    ]));
    assert_eq!(scan.support.next_range, Some(0x20));
    assert!(scan.support.problem.as_deref().unwrap().contains("7EA"));
    scan.accept(Ok(vec![DiagnosticResponse::new(
        Some(0x7e8),
        vec![0x42, 0x20, 3, 0x80, 0, 0, 0],
    )]));
    assert_eq!(scan.support.reported, BTreeSet::from([5, 0x21]));
    assert_eq!(scan.pid_sources(0x21), Some(&BTreeSet::from([Some(0x7e8)])));
    assert_eq!(scan.support.checked_through, 0);
    assert!(scan.support.problem.is_some());
}

#[test]
fn practice_freeze_frame_reads_raw_data_and_replaces_an_explicitly_erased_frame() {
    let mut app = Workbench::new(temp("freeze-frame"));
    app.start_demo();
    pump(&mut app);
    app.read_freeze_frame();
    pump(&mut app);
    let has = |app: &Workbench, request, value| {
        app.results
            .rows
            .values()
            .any(|v| v.latest.reading.request == request && v.latest.reading.value == value)
    };
    assert!(has(&app, "020200", "P0133"));
    assert!(has(&app, "026F00", "018032"));

    app.dispatch(vec![app.base_query(4, None)], None, ReadMode::Control);
    pump(&mut app);
    app.read_freeze_frame();
    pump(&mut app);
    assert!(has(&app, "020200", "No freeze frame stored"));
    assert!(!has(&app, "026F00", "018032"));
    assert!(
        app.results
            .failures
            .values()
            .all(|v| v.reading.freeze_frame_number() != Some(0))
    );
}

#[test]
fn playback_seek_resets_incremental_cursor_and_failed_save_retains_capture() {
    let root = temp("seek");
    let mut app = Workbench::new(root.clone());
    let mut cap = Capture::new(vehicle(), true, 1000);
    cap.samples = vec![
        sample(1000, "800", Some(800.0)),
        sample(2000, "900", Some(900.0)),
    ];
    app.playback = Some(replay_graphs::ReplayCapture::new(Arc::new(cap.clone())));
    app.seek_replay(2000);
    assert_eq!(app.replay_index, 2);
    app.seek_replay(1000);
    assert_eq!(app.replay_index, 1);
    assert_eq!(
        app.results
            .rows
            .values()
            .next()
            .unwrap()
            .latest
            .reading
            .value,
        "800"
    );
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("recordings"), b"folder blocked for test").unwrap();
    app.unsaved = Some(cap);
    app.save_recording();
    assert!(app.unsaved.is_some());
    assert!(app.status.contains("save failed"));
}

#[test]
fn connect_opens_trouble_codes_and_queues_one_readiness_and_code_sweep() {
    let folder = tempfile::tempdir().unwrap();
    let (tx, commands) = worker::CommandSender::test_channel();
    let (events, rx) = mpsc::channel();
    let mut app = Workbench::with_worker(folder.path().to_owned(), |_, _| (tx, rx));
    app.page = Page::Connection;
    app.code_category = Some(codes::Category::Pending);
    app.readiness_period = ReadinessPeriod::DriveCycle;

    app.connect();
    let failed = app.epoch;
    assert!(matches!(commands.try_recv(), Ok(Command::Connect { id, .. }) if id == failed));
    events
        .send(Event::Failed(failed, "No vehicle reply".into()))
        .unwrap();
    app.tick();
    assert_eq!(app.page, Page::Connection);
    assert!(!app.connected && commands.try_recv().is_err());

    app.connect();
    let connection = app.epoch;
    assert!(matches!(commands.try_recv(), Ok(Command::Connect { id, .. }) if id == connection));
    assert_eq!(app.page, Page::Connection);
    assert!(commands.try_recv().is_err());
    let connected = || Event::Connected {
        id: connection,
        driver: Some("J2534 adapter".into()),
        protocol: CanProtocol::Can11Bit500K.label().into(),
        demo: false,
        pid_support: PidSupport::default(),
    };
    events.send(connected()).unwrap();
    events.send(connected()).unwrap();
    app.tick();
    assert_eq!(app.page, Page::Codes);
    assert_eq!(app.code_category, None);
    assert_eq!(app.readiness_period, ReadinessPeriod::SinceClear);
    assert!(app.connected && app.busy && !app.connecting);
    assert_eq!(app.status, "Reading trouble codes and readiness…");
    assert!(matches!(
        commands.try_recv(),
        Ok(Command::Read { id, mode: ReadMode::Identify, .. }) if id == connection
    ));
    let Ok(Command::Read {
        id: sweep,
        queries,
        discovery: None,
        mode: ReadMode::Scan,
        ..
    }) = commands.try_recv()
    else {
        panic!("expected one combined diagnostic operation");
    };
    let requests: Vec<_> = queries.iter().map(|query| query.bytes.as_slice()).collect();
    assert_eq!(requests, [&[1, 1][..], &[3], &[7], &[10], &[1, 0x41]]);
    assert!(queries.iter().all(|query| query.target.is_none()));
    assert_eq!(app.connection_id, Some(connection));
    assert_eq!(app.epoch, sweep);
    assert!(commands.try_recv().is_err());

    events
        .send(Event::InitialVin(
            connection,
            vec![Reading {
                request: "0902".into(),
                name: "VIN".into(),
                ..reading("JTDBR32E720123456", None)
            }],
        ))
        .unwrap();
    events
        .send(Event::Rows(
            sweep,
            vec![Reading {
                request: "03".into(),
                name: "Stored emissions codes".into(),
                ..reading("P0133", None)
            }],
        ))
        .unwrap();
    app.tick();
    assert_eq!(app.observed_vin.as_deref(), Some("JTDBR32E720123456"));
    assert!(app.busy);
    let render = render_context().run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(960.0, 640.0),
            )),
            ..Default::default()
        },
        |ctx| app.draw(ctx),
    );
    for label in ["Trouble codes", "P0133", "View readiness"] {
        assert!(
            render.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == label)
            }),
            "Missing {label}"
        );
    }
    events
        .send(Event::Finished(sweep, "Operation finished.".into()))
        .unwrap();
    app.tick();
    assert!(app.connected && !app.busy);
    assert_eq!(app.page, Page::Codes);
    assert!(commands.try_recv().is_err());
}

#[test]
fn a_new_vehicle_connection_does_not_reuse_previous_vehicle_identity() {
    let mut app = Workbench::new(temp("new-vehicle"));
    app.vehicle = vehicle();
    app.vehicle.vin = "WDD00000000000000".into();
    app.observed_vin = Some(app.vehicle.vin.clone());
    let (events, rx) = mpsc::channel();
    app.rx = rx;
    events
        .send(Event::Connected {
            id: app.epoch,
            driver: Some("Mongoose-Plus MFC3".into()),
            protocol: CanProtocol::Can29Bit500K.label().into(),
            demo: false,
            pid_support: PidSupport::default(),
        })
        .unwrap();
    app.tick();
    assert!(app.connected && app.vehicle.vin.is_empty() && app.observed_vin.is_none());
}

#[test]
fn header_vin_highlights_on_hover_copies_and_resets_between_connections() {
    let mut app = Workbench::new(temp("header-vin"));
    let (events, rx) = mpsc::channel();
    app.rx = rx;
    let id = app.epoch;
    let mut vin = reading("JTDBR32E720123456", None);
    vin.request = "0902".into();
    vin.name = "VIN".into();
    app.page = Page::Live;
    events
        .send(Event::Connected {
            id,
            driver: Some("Mongoose-Plus MFC3".into()),
            protocol: CanProtocol::Can11Bit500K.label().into(),
            demo: false,
            pid_support: PidSupport::default(),
        })
        .unwrap();
    events
        .send(Event::InitialVin(id, vec![vin.clone()]))
        .unwrap();
    app.tick();
    assert!(app.connected && app.vin_request.is_none());
    app.voltage = Some(VoltageSample {
        adapter: VoltageAdapter::J2534(DriverInfo {
            name: "Mongoose-Plus MFC3".into(),
            path: PathBuf::new(),
        }),
        millivolts: 12600,
        sampled_at: Instant::now(),
    });

    let mut time = 0.0;
    let ctx = render_context();
    let mut draw = |app: &mut Workbench, events, elapsed| {
        time += elapsed;
        ctx.run(
            egui::RawInput {
                time: Some(time),
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 640.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let text = |output: &egui::FullOutput, label: &str| {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.job.text == label => {
                Some(text.visual_bounding_rect())
            }
            _ => None,
        })
    };
    let initial = draw(&mut app, vec![], 0.01);
    let number = text(&initial, &vin.value).unwrap();
    assert!(number.right() < text(&initial, "12.6 V").unwrap().left());
    assert!(
        text(&initial, "12.6 V").unwrap().right() < text(&initial, "Disconnect").unwrap().left()
    );
    let pointer = number.center();
    draw(&mut app, vec![egui::Event::PointerMoved(pointer)], 0.01);
    let hovered = draw(&mut app, vec![], 0.75);
    assert_eq!(
        hovered.platform_output.cursor_icon,
        egui::CursorIcon::PointingHand
    );
    assert!(hovered.shapes.iter().any(|shape| matches!(&shape.shape,
        egui::Shape::Rect(rect) if rect.fill == appearance::SELECTED && rect.rect.contains_rect(number)
    )));
    assert!(text(&hovered, "Click to copy VIN").is_some());
    let button = |pressed| egui::Event::PointerButton {
        pos: pointer,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    draw(&mut app, vec![button(true)], 0.01);
    let clicked = draw(&mut app, vec![button(false)], 0.01);
    assert!(clicked.platform_output.commands.iter().any(|command| {
        matches!(command, egui::OutputCommand::CopyText(text) if text == &vin.value)
    }));
    assert!(text(&clicked, "Copied").is_some());
    app.vin_copied_until = Some(Instant::now());
    assert!(text(&draw(&mut app, vec![], 0.02), "Copied").is_none());

    app.disconnect();
    events
        .send(Event::InitialVin(id, vec![vin.clone()]))
        .unwrap();
    app.tick();
    assert!(text(&draw(&mut app, vec![], 0.02), &vin.value).is_none());
}

#[test]
fn vehicle_reply_status_uses_the_connection_id_and_preserves_scan_results() {
    use super::worker::{ReplyCheck, ReplyCheckOutcome, ReplyEvidence, VehicleReply};

    let mut app = Workbench::new(temp("vehicle-reply"));
    let (events, rx) = mpsc::channel();
    app.rx = rx;
    let id = app.epoch;
    events
        .send(Event::Connected {
            id,
            driver: Some("Mongoose-Plus MFC3".into()),
            protocol: CanProtocol::Can11Bit500K.label().into(),
            demo: false,
            pid_support: PidSupport::default(),
        })
        .unwrap();
    app.tick();
    assert!(!app.vehicle_reply.unwrap().missed);
    app.results.apply(&sample(1000, "800", Some(800.0)));
    app.epoch += 1;

    let last_reply = Instant::now();
    let missed = VehicleReply {
        last_reply,
        missed: true,
    };
    events.send(Event::VehicleReply(id, missed)).unwrap();
    app.tick();
    assert!(app.connected && app.vehicle_reply.unwrap().missed);
    assert_eq!(app.results.rows.len(), 1);

    let check = ReplyCheck {
        outcome: ReplyCheckOutcome::Incomplete,
        protocol: CanProtocol::Can11Bit500K,
        responses: vec![ReplyEvidence {
            source: Some(0x7e8),
            description: "Support bitmap is truncated".into(),
            raw: "410080".into(),
        }],
    };
    let publish_check = |connection, operation| {
        events
            .send(Event::VehicleReplyChecked {
                connection,
                operation,
                check: check.clone(),
            })
            .unwrap();
    };
    publish_check(id, app.epoch - 1);
    publish_check(id - 1, app.epoch);
    app.tick();
    assert!(app.vehicle_reply_check.is_none());
    publish_check(id, app.epoch);
    app.tick();
    assert_eq!(
        app.vehicle_reply_check.as_ref().map(|(_, result)| result),
        Some(&check)
    );

    app.page = Page::Connection;
    let ctx = render_context();
    let draw = |app: &mut Workbench, events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 640.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let output = draw(&mut app, vec![]);
    let pos = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.job.text == "Copy reply check" => {
                Some(text.visual_bounding_rect().center())
            }
            _ => None,
        })
        .expect("reply check has a copy button");
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    draw(&mut app, vec![egui::Event::PointerMoved(pos), button(true)]);
    let output = draw(&mut app, vec![button(false)]);
    assert!(output.platform_output.commands.iter().any(|command| {
        matches!(command, egui::OutputCommand::CopyText(text)
            if text.contains("CAN 11-bit / 500 kbit/s\nRequest 0100")
                && text.contains("ECU 7E8 · Support bitmap is truncated · 410080"))
    }));

    let log_count = app.logs.len();
    events.send(Event::VehicleReply(id, missed)).unwrap();
    let replied = VehicleReply {
        last_reply,
        missed: false,
    };
    events.send(Event::VehicleReply(id - 1, replied)).unwrap();
    app.tick();
    assert_eq!(app.logs.len(), log_count);
    assert!(app.vehicle_reply.unwrap().missed);

    events.send(Event::VehicleReply(id, replied)).unwrap();
    app.tick();
    assert!(!app.vehicle_reply.unwrap().missed);
    assert_eq!(app.logs.len(), log_count + 1);

    events
        .send(Event::LinkLost(id, "Cable disconnected".into()))
        .unwrap();
    events.send(Event::VehicleReply(id, missed)).unwrap();
    publish_check(id, app.epoch);
    app.tick();
    assert!(!app.connected && app.vehicle_reply.is_none());
    assert_eq!(
        app.vehicle_reply_check.as_ref().map(|(_, result)| result),
        Some(&check)
    );
    assert!(
        app.connection_error
            .as_deref()
            .is_some_and(|error| error.contains("Cable disconnected"))
    );
}

#[test]
fn live_reads_use_checked_ranges_and_keep_unchecked_selections_available() {
    use crate::adapter::DiagnosticResponse;
    use crate::diagnostics::SupportScan;

    let mut app = Workbench::new(temp("live-pids"));
    app.selected = ["OBD:0105", "OBD:010C", "OBD:0121", "OBD:0142"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let (events, rx) = mpsc::channel();
    app.rx = rx;
    let id = app.epoch;
    let mut scan = SupportScan::new(1, 0);
    scan.accept(Ok(vec![DiagnosticResponse::new(
        Some(0x7e8),
        vec![0x41, 0, 0, 0x10, 0, 1],
    )]));
    events
        .send(Event::Connected {
            id,
            driver: Some("Mongoose-Plus MFC3".into()),
            protocol: CanProtocol::Can11Bit500K.label().into(),
            demo: false,
            pid_support: scan.support.clone(),
        })
        .unwrap();
    app.tick();
    let selected = |app: &Workbench| {
        app.selected_queries()
            .iter()
            .map(|q| q.bytes[1])
            .collect::<Vec<_>>()
    };
    assert_eq!(selected(&app), [0x0c, 0x21, 0x42]);

    // A timed out continuation must not make higher PIDs appear unsupported.
    scan.accept(Ok(Vec::new()));
    app.epoch += 1; // User may have read codes while the support scan was in progress.
    events
        .send(Event::LivePidSupport(id, scan.support))
        .unwrap();
    app.tick();
    assert!(app.pid_support.problem.is_some());
    assert_eq!(selected(&app), [0x0c, 0x21, 0x42]);
    app.skip_unreported = false;
    let requested = selected(&app);
    assert_eq!(requested.first(), Some(&0x0c));
    assert!(requested.contains(&0x05));
}

#[test]
fn code_refresh_replaces_only_its_ecu_and_category_in_replay_and_reports() {
    let code = |source, request: &str, name: &str, value: &str| Reading {
        source: Some(source),
        request: request.into(),
        name: name.into(),
        ..reading(value, None)
    };
    let mut capture = Capture::new(vehicle(), false, 1000);
    capture.samples = vec![
        Sample {
            at_ms: 1000,
            rows: vec![
                code(0x7e8, "03", "Stored emissions codes", "P0133"),
                code(0x7ea, "03", "Stored emissions codes", "P0300"),
                code(0x7ea, "0A", "Permanent emissions codes", "P0300"),
            ],
            ..Default::default()
        },
        Sample {
            at_ms: 2000,
            rows: vec![
                code(
                    0x7e8,
                    "03",
                    "Manual diagnostic request",
                    "No codes reported by this ECU",
                ),
                code(0x7ea, "03", "Stored emissions codes", "Unavailable"),
            ],
            ..Default::default()
        },
    ];
    capture.validate().unwrap();
    let mut before = Snapshot::from_results(&vehicle(), false, 1000, &capture.replay(1000));
    // Older baselines could retain both a named scan result and a manual result.
    before.rows.push(Stamped {
        reading: code(0x7e8, "03", "Manual diagnostic request", "P0420"),
        at_ms: 900,
    });
    let results = capture.replay(2000);
    assert_eq!(results.rows.len(), 3);
    assert_eq!(
        codes::summary(&results, codes::Category::Stored),
        "1 ECU result · 0 codes · 1 unverified · 1 read issue"
    );
    assert_eq!(
        codes::summary(&results, codes::Category::Pending),
        "Not read"
    );
    assert_eq!(
        codes::summary(&results, codes::Category::Permanent),
        "1 ECU result · 1 code"
    );
    let after = Snapshot::from_results(&vehicle(), false, 2000, &results);
    let comparisons = comparison(&before, &after).unwrap();
    assert_eq!(comparisons.len(), 3);
    assert!(
        comparisons
            .iter()
            .any(|(label, old, new, state)| label.ends_with("7E8")
                && old == "P0133"
                && new == "No codes reported by this ECU"
                && state == "Changed after reread")
    );
    assert!(
        comparisons
            .iter()
            .any(|(label, old, new, state)| label.starts_with("Stored")
                && label.ends_with("7EA")
                && old == "P0300"
                && new == "P0300"
                && state == "Read failed / not verified")
    );
    assert_eq!(
        report(&after, None)
            .matches("Refresh failed · result unverified")
            .count(),
        1
    );
}

#[test]
fn startup_splash_loads_the_workbench_photo_and_expires() {
    let mut app = Workbench::new(temp("splash"));
    app.show_startup_splash();
    let start = Instant::now();
    assert!(app.splash.visible(start));

    let ctx = render_context();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960.0, 640.0));
    for pass in 0..2 {
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            appearance::splash,
        );
        if pass == 0 {
            assert!(
                output.textures_delta.set.iter().any(|(_, change)| {
                    change.pos.is_none() && change.image.size() == [1280, 640]
                })
            );
        }
        assert!(output.shapes.iter().any(|shape| {
            matches!(&shape.shape, egui::Shape::Text(text)
                if text.galley.job.text == "Scanner Workbench"
                    && screen.contains_rect(text.visual_bounding_rect()))
        }));
    }
    assert!(app.splash.visible(start + Duration::from_millis(500)));
    assert!(!app.splash.visible(start + Duration::from_secs(1)));
}

#[cfg(windows)]
#[test]
fn startup_update_can_be_deferred_and_restarts_after_five_seconds() {
    let folder = tempfile::tempdir().unwrap();
    let (tx, commands) = worker::CommandSender::test_channel();
    let (_, rx) = mpsc::channel();
    let mut app = Workbench::with_worker(folder.path().join("data"), |_, _| (tx, rx));
    app.updates = Some(updates::Updates::test_ready(&folder.path().join("install")));
    let ctx = render_context();
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960.0, 640.0));
    let draw = |app: &mut Workbench, now, events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            },
            |ctx| {
                if !app.show_startup_update(ctx, now) {
                    app.draw(ctx);
                }
            },
        )
    };
    let text = |output: &egui::FullOutput, label: &str| {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.job.text == label => {
                Some(text.visual_bounding_rect())
            }
            _ => None,
        })
    };
    let start = Instant::now();
    draw(&mut app, start, vec![]);
    let output = draw(&mut app, start, vec![]);
    for label in [
        "Update ready",
        "Restarting in 5 seconds…",
        "Update now",
        "Later",
    ] {
        assert!(text(&output, label).is_some(), "Missing {label}");
    }
    assert!(app.restarting_for_update.is_none());
    let pointer = text(&output, "Later").unwrap().center();
    let click = |pressed| egui::Event::PointerButton {
        pos: pointer,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    draw(
        &mut app,
        start,
        vec![egui::Event::PointerMoved(pointer), click(true)],
    );
    draw(&mut app, start, vec![click(false)]);
    let deferred = draw(&mut app, start + Duration::from_secs(8), vec![]);
    assert!(text(&deferred, "Restarting…").is_none());
    assert!(app.update_ready() && app.restarting_for_update.is_none());
    let badge = text(&deferred, "Update ready").unwrap();
    assert!(deferred.shapes.iter().any(|shape| {
        matches!(&shape.shape, egui::Shape::Rect(rect)
            if rect.fill == appearance::ACCENT && rect.rect.contains_rect(badge))
    }));
    assert!(commands.try_recv().is_err());

    app.startup_update = updates::StartupUpdate::default();
    app.busy = true;
    assert!(!app.show_startup_update(&ctx, start + Duration::from_secs(9)));
    app.busy = false;
    assert!(!app.show_startup_update(&ctx, start + Duration::from_secs(10)));
    app.startup_update = updates::StartupUpdate::default();
    let next = start + Duration::from_secs(10);
    draw(&mut app, next, vec![]);
    draw(&mut app, next + Duration::from_millis(4999), vec![]);
    assert!(app.restarting_for_update.is_none());
    draw(&mut app, next + Duration::from_secs(5), vec![]);
    assert!(app.restarting_for_update.is_some());
    assert!(matches!(
        commands.try_recv(),
        Ok(Command::Disconnect { .. })
    ));
    assert!(matches!(commands.try_recv(), Ok(Command::Shutdown)));
}

#[cfg(windows)]
#[test]
fn update_action_stops_reads_and_saves_the_partial_scan_and_recording() {
    let folder = tempfile::tempdir().unwrap();
    let (tx, commands) = worker::CommandSender::test_channel();
    let (_, rx) = mpsc::channel();
    let mut app = Workbench::with_worker(folder.path().join("data"), |_, _| (tx, rx));
    app.updates = Some(updates::Updates::test_ready(&folder.path().join("install")));
    app.page = Page::Updates;
    app.connected = true;
    app.scanning_emissions = true;
    let start = app.now();
    app.recording = Some(Capture::new(vehicle(), false, start));
    app.apply_sample(Sample {
        at_ms: start,
        emissions_scan: Some(ScanEvent::Started),
        ..Default::default()
    });
    app.apply_sample(Sample {
        at_ms: start + 1,
        rows: vec![reading("800", Some(800.0))],
        ..Default::default()
    });
    app.busy = true;
    let active_read = app.cancel.clone();
    let ctx = render_context();
    let draw = |app: &mut Workbench, events| {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(960.0, 640.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        )
    };
    let output = draw(&mut app, vec![]);
    let pointer = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text)
                if text.galley.job.text == "Stop diagnostics and restart to update" =>
            {
                Some(text.visual_bounding_rect().center())
            }
            _ => None,
        })
        .expect("active diagnostics must have a restart action");
    let click = |pressed| egui::Event::PointerButton {
        pos: pointer,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    draw(
        &mut app,
        vec![egui::Event::PointerMoved(pointer), click(true)],
    );
    draw(&mut app, vec![click(false)]);
    assert!(active_read.load(Ordering::Relaxed));
    assert!(!app.connected && app.recording.is_none() && app.unsaved.is_none());
    assert!(app.restarting_for_update.is_some());
    let (path, snapshot) = app.latest_scan.as_ref().unwrap();
    assert!(path.is_file());
    assert_eq!(
        snapshot.emissions_scan.as_ref().unwrap().state,
        emissions_scan::State::Interrupted
    );
    assert_eq!(app.scan_history.pending_count(), 0);
    assert_eq!(app.saved_recordings.len(), 1);
    let recording: Capture = load_json(&app.saved_recordings[0].path).unwrap();
    recording.validate().unwrap();
    assert!(matches!(
        recording.samples.last().unwrap().emissions_scan,
        Some(ScanEvent::Interrupted)
    ));
    assert!(matches!(
        commands.try_recv(),
        Ok(Command::Disconnect { .. })
    ));
    assert!(matches!(commands.try_recv(), Ok(Command::Shutdown)));
}

#[cfg(windows)]
#[test]
fn failed_update_apply_reopens_the_scanner_with_an_idle_worker() {
    let folder = tempfile::tempdir().unwrap();
    let (tx, _) = worker::CommandSender::test_channel();
    let (_, rx) = mpsc::channel();
    let mut app = Workbench::with_worker(folder.path().join("data"), |_, _| (tx, rx));
    app.updates = Some(updates::Updates::test_ready(&folder.path().join("install")));
    // Keep the replacement worker limited to an unregistered fixture driver.
    app.driver_override = Some(DriverInfo {
        name: "Missing test driver".into(),
        path: folder.path().join("unregistered-test-driver.dll"),
    });
    app.restarting_for_update = app.updates.as_mut().and_then(updates::Updates::take_ready);
    app.busy = true;
    let cancelled = app.cancel.clone();
    app.apply_update_after_shutdown(&render_context());
    assert!(cancelled.load(Ordering::Relaxed));
    assert!(!app.busy && !app.connecting && !app.cancel.load(Ordering::Relaxed));
    assert!(app.restarting_for_update.is_none());
    assert_eq!(app.page, Page::Updates);
    assert!(matches!(
        app.updates.as_ref().unwrap().state(),
        updates::State::Failed(_)
    ));
}

#[cfg(windows)]
#[test]
fn update_save_failure_preserves_the_recording_for_recovery_and_retry() {
    let folder = tempfile::tempdir().unwrap();
    let (tx, commands) = worker::CommandSender::test_channel();
    let (_, rx) = mpsc::channel();
    let mut app = Workbench::with_worker(folder.path().join("data"), |_, _| (tx, rx));
    app.updates = Some(updates::Updates::test_ready(&folder.path().join("install")));
    std::fs::create_dir_all(&app.root).unwrap();
    std::fs::write(app.root.join("recordings"), b"blocked destination").unwrap();
    let mut recording = Capture::new(vehicle(), false, 1000);
    recording.samples.push(sample(1000, "800", Some(800.0)));
    app.unsaved = Some(recording);
    let ctx = render_context();
    app.restart_for_update(&ctx);
    assert_eq!(app.page, Page::Recordings);
    assert!(app.status.starts_with("Update waiting."));
    assert!(app.unsaved.is_some() && app.update_ready() && app.restarting_for_update.is_none());
    assert!(matches!(
        commands.try_recv(),
        Ok(Command::Disconnect { .. })
    ));
    assert!(commands.try_recv().is_err());
    app.save_recording_to(folder.path().join("recovered.json"));
    assert!(app.unsaved.is_none());
    app.restart_for_update(&ctx);
    assert!(app.restarting_for_update.is_some());
    assert!(matches!(
        commands.try_recv(),
        Ok(Command::Disconnect { .. })
    ));
    assert!(matches!(commands.try_recv(), Ok(Command::Shutdown)));
}

#[test]
fn every_new_page_renders_in_compact_and_large_windows() {
    fn visible(shapes: &[egui::epaint::ClippedShape], screen: egui::Rect, label: &str) -> bool {
        fn contains(shape: &egui::Shape, clip: egui::Rect, label: &str) -> bool {
            match shape {
                egui::Shape::Text(text) => {
                    text.galley.job.text == label
                        && clip.expand(1.0).contains_rect(text.visual_bounding_rect())
                }
                egui::Shape::Vec(shapes) => shapes.iter().any(|shape| contains(shape, clip, label)),
                _ => false,
            }
        }
        shapes
            .iter()
            .any(|clipped| contains(&clipped.shape, clipped.clip_rect.intersect(screen), label))
    }

    let mut app = Workbench::new(temp("render"));
    app.start_demo();
    pump(&mut app);
    app.read_emissions_scan();
    pump(&mut app);
    app.read_vehicle_information();
    pump(&mut app);
    app.once(app.selected_queries());
    pump(&mut app);
    app.read_readiness_period(ReadinessPeriod::DriveCycle);
    pump(&mut app);
    app.read_supported_monitors();
    pump(&mut app);
    app.dispatch(vec![app.base_query(4, None)], None, ReadMode::Control);
    pump(&mut app);
    app.voltage = Some(VoltageSample {
        adapter: VoltageAdapter::J2534(DriverInfo {
            name: "Mongoose-Plus MFC3".into(),
            path: PathBuf::new(),
        }),
        millivolts: 12600,
        sampled_at: Instant::now(),
    });
    for size in [egui::vec2(960.0, 640.0), egui::vec2(1280.0, 820.0)] {
        let ctx = render_context();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        for page in Page::ALL {
            app.page = page;
            app.voltage.as_mut().unwrap().sampled_at = Instant::now();
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            );
            assert!(!output.shapes.is_empty());
            for label in ["Scanner Workbench", "Connect", "12.6 V"] {
                assert!(
                    visible(&output.shapes, screen, label),
                    "{page:?} at {size:?} clipped {label}"
                );
            }
            for label in page.tabs().iter().map(|(_, label)| *label) {
                assert!(
                    visible(&output.shapes, screen, label),
                    "{page:?} at {size:?} clipped {label}"
                );
            }
            let important: &[&str] = match page {
                Page::Home => &[
                    "Run emissions scan",
                    "Readiness since clear",
                    "Stored codes",
                    "Pending codes",
                    "Permanent codes",
                    "VIN",
                    "Print scan report",
                ],
                Page::Codes => &[
                    "Read codes",
                    "P0133",
                    "Before clear",
                    "Save follow-up scan",
                    "Recheck codes and readiness",
                    "All codes",
                    "Stored · No current results · 1 unverified",
                ],
                Page::Live => &["Start live", "Read once", "Presets", "Graphs", "Min / max"],
                Page::Emissions => &[
                    "Since clear",
                    "This drive cycle",
                    "Read readiness",
                    "ECU 7E8",
                    "MIL On",
                    "Confirmed DTCs 1",
                    "Before clear",
                ],
                Page::VehicleInfo => &[
                    "Read vehicle information",
                    "Read VIN",
                    "WDD00000000000000",
                    "DEMO-CALIBRATION",
                    "12345678",
                ],
                Page::FreezeFrame => &["Read freeze frame"],
                Page::MonitorTests => &[
                    "Read supported monitors",
                    "Read monitor",
                    "Reported IDs",
                    "Value and limits",
                    "Before clear",
                ],
                Page::Updates => &["Software updates"],
                _ => &[],
            };
            for label in important {
                assert!(
                    visible(&output.shapes, screen, label),
                    "{page:?} at {size:?} clipped {label}"
                );
            }
        }
        app.page = Page::Codes;
        app.code_category = Some(codes::Category::Pending);
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );
        assert!(visible(
            &output.shapes,
            screen,
            "Pending · No current results · 1 unverified"
        ));
        assert!(!visible(&output.shapes, screen, "P0133"));
        app.code_category = None;
        app.page = Page::Emissions;
        app.readiness_period = ReadinessPeriod::DriveCycle;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );
        for label in ["This drive cycle", "ECU 7E8", "Before clear", "Not ready"] {
            assert!(
                visible(&output.shapes, screen, label),
                "drive-cycle view at {size:?} clipped {label}"
            );
        }
        app.readiness_period = ReadinessPeriod::SinceClear;

        app.page = Page::Connection;
        app.demo = false;
        app.vehicle_reply = Some(super::worker::VehicleReply {
            last_reply: Instant::now(),
            missed: true,
        });
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );
        for label in [
            "No vehicle reply",
            "Check vehicle reply",
            "Disconnect",
            "12.6 V",
        ] {
            assert!(
                visible(&output.shapes, screen, label),
                "connection view at {size:?} clipped {label}"
            );
        }
        app.demo = true;
        app.vehicle_reply = None;

        let previous = std::mem::take(&mut app.results);
        let mut capture = Capture::new(vehicle(), false, 1000);
        capture.samples = vec![
            sample(1000, "800", Some(800.0)),
            sample(2000, "Unavailable", None),
            sample(3000, "900", Some(900.0)),
        ];
        app.results = capture.replay(capture.end_ms());
        app.replay_at = capture.end_ms();
        app.playback = Some(replay_graphs::ReplayCapture::new(Arc::new(capture)));
        app.page = Page::Live;
        app.graphs = true;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );
        for label in [
            "Replay controls",
            "Play",
            "Rewind",
            "Signals · 1/6",
            "60 s",
            "Engine RPM · ECU 7E8",
        ] {
            assert!(
                visible(&output.shapes, screen, label),
                "graph playback at {size:?} clipped {label}"
            );
        }
        app.playback = None;
        app.graphs = false;
        app.results = previous;

        let mut capture = Capture::new(vehicle(), false, 1000);
        capture.samples.push(sample(1200, "800", Some(800.0)));
        let path = app.root.join("recordings").join("vehicle-1000.json");
        app.recording_preview = Some(recordings::Preview::new(path, capture));
        app.page = Page::Recordings;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| app.draw(ctx),
        );
        for label in [
            "Saved recordings",
            "2015 Toyota Camry",
            "Replay recording",
            "Export full CSV",
        ] {
            assert!(
                visible(&output.shapes, screen, label),
                "recording preview at {size:?} clipped {label}"
            );
        }
        #[cfg(windows)]
        assert!(visible(&output.shapes, screen, "Open recording file…"));
        app.recording_preview = None;
    }
}
