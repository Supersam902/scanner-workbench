use obd_dashboard::adapter::{DiagnosticAdapter, DiagnosticResponse};
use obd_dashboard::app::ObdEvent;
use obd_dashboard::diagnostics::*;
use obd_dashboard::elm327::{ConnectionInfo, Elm327Error, block_on};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, atomic::AtomicBool, mpsc};

fn query(bytes: &[u8]) -> Query {
    Query {
        bytes: bytes.into(),
        target: None,
        name: "test".into(),
        signals: Vec::new(),
        verified: false,
    }
}

#[test]
fn legacy_oxygen_tests_decode_values_and_preserve_unknown_or_malformed_results() {
    let decode = |id, sensor, payload: &[u8]| {
        obd_dashboard::diagnostics::decode(
            &query(&[5, id, sensor]),
            &DiagnosticResponse::new(Some(0x10), payload.to_vec()),
            false,
        )
    };
    let threshold = decode(1, 1, &[0x45, 1, 1, 0x5a]).unwrap();
    assert_eq!(
        (threshold[0].numeric, threshold[0].unit.as_str()),
        (Some(450.0), "mV")
    );
    let timing = decode(9, 1, &[0x45, 9, 1, 2, 0, 4]).unwrap();
    assert_eq!(timing[0].value, "80 (min 0, max 160)");
    assert_eq!(timing[0].status, "Monitor completion unverified");
    let one_limit = decode(9, 1, &[0x45, 9, 1, 2, 4]).unwrap();
    assert_eq!(one_limit[0].status, "Limits unavailable");
    let reversed = decode(5, 1, &[0x45, 5, 1, 8, 9, 5]).unwrap();
    assert!(reversed[0].status.starts_with("Invalid limits"));
    let unknown = decode(0x81, 1, &[0x45, 0x81, 1, 1, 2, 3]).unwrap();
    assert_eq!(
        (unknown[0].numeric, unknown[0].value.as_str()),
        (None, "010203")
    );
    assert!(decode(5, 1, &[0x45, 5, 2, 0, 0, 1]).is_err());
    assert!(decode(5, 1, &[0x45, 5, 1]).is_err());
    assert!(decode(5, 3, &[0x45, 5, 3, 1]).is_err());
}
fn response(payload: &[u8]) -> DiagnosticResponse {
    DiagnosticResponse::new(Some(0x7e8), payload.into())
}

#[test]
fn manual_can_mode01_group_accepts_reordered_raw_data() {
    let q = query(&[1, 5, 12]);
    let reply = response(&[0x41, 12, 0x0c, 0x80, 5, 128]);
    let rows = decode(&q, &reply, true).unwrap();
    assert_eq!(rows[0].request, "01050C");
    assert_eq!(rows[0].raw, "410C0C800580");
    assert_eq!(rows[0].unit, "raw");
    assert!(decode(&q, &response(&[0x41, 13, 0]), true).is_err());
}

#[test]
fn bundled_catalog_loads_only_generic_engine_data_requests() {
    let generic = catalog(include_str!("../data/obdb/saej1979.json"), None).unwrap();
    assert_eq!(generic.len(), 205);
    assert!(
        generic
            .iter()
            .all(|q| q.bytes.first() == Some(&1) && q.target.is_none())
    );
    assert!(catalog(r#"{"commands":[{"cmd":{"04":""}}]}"#, None).is_err());
    assert!(catalog(r#"{"commands":[{"cmd":{"21":"82"}}]}"#, None).is_err());
}

#[test]
fn bit_decoding_handles_signed_fields_nulls_maps_and_truncation() {
    let signed = json!({"fmt":{"bix":4,"len":12,"sign":true,"div":4}});
    assert_eq!(signal_value(&signed, &[0x0f, 0xfc]).unwrap().1, Some(-1.0));
    let mapped = json!({"fmt":{"len":8,"map":{"1":{"description":"Closed loop"}}}});
    assert_eq!(signal_value(&mapped, &[1]).unwrap().0, "Closed loop");
    let null = json!({"fmt":{"len":8,"nullmin":255,"nullmax":255}});
    assert!(signal_value(&null, &[255]).is_err());
    assert!(signal_value(&signed, &[0]).is_err());
    assert!(signal_value(&json!({"fmt":{"len":4294967304u64}}), &[0]).is_err());
}

#[test]
fn null_limits_are_scaled_inclusive_bounds_and_normal_speed_and_fuel_remain_readable() {
    let generic = catalog(include_str!("../data/obdb/saej1979.json"), None).unwrap();
    let speed = generic.iter().find(|q| q.bytes == [1, 0x0d]).unwrap();
    for byte in [0, 50, 254] {
        assert_eq!(
            decode(speed, &response(&[0x41, 0x0d, byte]), true).unwrap()[0].numeric,
            Some(f64::from(byte))
        );
    }
    assert_eq!(
        decode(speed, &response(&[0x41, 0x0d, 255]), true).unwrap()[0].value,
        "Unavailable"
    );
    let fuel = generic.iter().find(|q| q.bytes == [1, 0x2f]).unwrap();
    assert!(
        decode(fuel, &response(&[0x41, 0x2f, 128]), true).unwrap()[0]
            .numeric
            .unwrap()
            > 50.0
    );
    let rpm = serde_json::json!({"fmt":{"len":16,"div":4,"nullmax":16383.5}});
    assert!(signal_value(&rpm, &[0xff, 0xfe]).is_err());
    assert_eq!(signal_value(&rpm, &[0xff, 0xfc]).unwrap().1, Some(16383.0));
    let temperature = serde_json::json!({"fmt":{"len":8,"add":-40,"nullmin":-40}});
    assert!(signal_value(&temperature, &[0]).is_err());
    assert_eq!(signal_value(&temperature, &[1]).unwrap().1, Some(-39.0));
    let range = serde_json::json!({"fmt":{"len":8,"nullmin":0,"nullmax":255}});
    assert!(signal_value(&range, &[0]).is_err());
    assert!(signal_value(&range, &[255]).is_err());
    assert_eq!(signal_value(&range, &[100]).unwrap().1, Some(100.0));
}

#[test]
fn byte_aligned_little_endian_signals_decode_and_ambiguous_bit_layouts_fail_explicitly() {
    let fmt = serde_json::json!({"fmt":{"len":16,"blsb":true}});
    assert_eq!(signal_value(&fmt, &[0x34, 0x12]).unwrap().1, Some(4660.0));
    let signed = serde_json::json!({"fmt":{"len":16,"blsb":true,"sign":true,"div":4}});
    assert_eq!(signal_value(&signed, &[0xfc, 0xff]).unwrap().1, Some(-1.0));
    assert!(
        signal_value(
            &serde_json::json!({"fmt":{"bix":1,"len":8,"blsb":true}}),
            &[0, 0]
        )
        .is_err()
    );
}

#[test]
fn generic_extended_pid_and_readiness_decode_from_the_bundled_definitions() {
    let generic = catalog(include_str!("../data/obdb/saej1979.json"), None).unwrap();
    let rpm = generic.iter().find(|q| q.bytes == [1, 0x0c]).unwrap();
    assert_eq!(
        decode(rpm, &response(&[0x41, 0x0c, 0x1a, 0xf8]), true).unwrap()[0].numeric,
        Some(1726.0)
    );
    let status = generic.iter().find(|q| q.bytes == [1, 1]).unwrap();
    let values = decode(status, &response(&[0x41, 1, 0x81, 0x07, 0, 0]), true).unwrap();
    assert!(
        values
            .iter()
            .any(|r| r.name == "Malfunction indicator lamp" && r.numeric == Some(1.0))
    );
    assert!(
        values
            .iter()
            .any(|r| r.name == "Number of DTCs stored in this ECU" && r.numeric == Some(1.0))
    );
    let mut freeze = rpm.clone();
    freeze.bytes = vec![2, 0x0c, 0];
    assert_eq!(
        decode(&freeze, &response(&[0x42, 0x0c, 0, 0x1a, 0xf8]), true).unwrap()[0].numeric,
        Some(1726.0)
    );
    let values = decode(status, &response(&[0x41, 1, 0, 0x08, 0x42, 0x02]), true).unwrap();
    assert!(
        values
            .iter()
            .any(|r| r.name == "NOx/SCR monitoring" && r.value == "Not ready")
    );
    assert!(
        values
            .iter()
            .any(|r| r.name == "Particulate filter monitoring" && r.value == "Ready")
    );
    assert!(
        values
            .iter()
            .any(|r| r.name == "Misfire monitoring" && r.value == "Not supported")
    );
    assert!(
        !values
            .iter()
            .any(|r| r.name == "Oxygen sensor heater monitoring")
    );
    assert_eq!(
        decode(&query(&[2, 2, 0]), &response(&[0x42, 2, 0, 1, 0x33]), false).unwrap()[0].value,
        "P0133"
    );
}

#[test]
fn drive_cycle_readiness_decodes_enable_and_completion_without_claiming_support() {
    let cycle = query(&[1, 0x41]);
    let spark = decode(&cycle, &response(&[0x41, 0x41, 0, 0x25, 0x31, 0x01]), true).unwrap();
    for (name, value) in [
        ("Ignition monitor type", "Spark"),
        ("Misfire monitoring", "Ready"),
        ("Fuel system monitoring", "Disabled or unsupported"),
        ("Catalyst monitoring", "Not ready"),
        ("Evaporative system monitoring", "Disabled or unsupported"),
        ("A/C refrigerant monitoring", "Ready"),
        ("Oxygen sensor monitoring", "Ready"),
    ] {
        assert!(spark.iter().any(|r| r.name == name && r.value == value));
    }
    assert!(spark.iter().all(|r| r.raw == "414100253101"));
    assert!(!spark.iter().any(|r| matches!(
        r.name.as_str(),
        "Malfunction indicator lamp" | "Number of DTCs stored in this ECU"
    )));

    let compression = decode(&cycle, &response(&[0x41, 0x41, 0, 0x0c, 0x42, 0x02]), true).unwrap();
    for (name, value) in [
        ("Ignition monitor type", "Compression"),
        ("NOx/SCR monitoring", "Not ready"),
        ("Particulate filter monitoring", "Ready"),
    ] {
        assert!(
            compression
                .iter()
                .any(|r| r.name == name && r.value == value)
        );
    }
    assert!(
        !compression
            .iter()
            .any(|r| r.name == "Oxygen sensor monitoring")
    );
    assert!(decode(&cycle, &response(&[0x41, 0x41, 0, 0x07, 0]), true).is_err());
}

#[test]
fn monitor_limits_use_known_scaling_and_never_guess_unknown_ids() {
    let q = query(&[6, 0xa1]);
    let rows = decode(
        &q,
        &response(&[0x46, 0xa1, 1, 0x81, 0xff, 0xfe, 0xff, 0xfc, 0, 2]),
        true,
    )
    .unwrap();
    assert_eq!(rows[0].numeric, Some(-2.0));
    assert_eq!(rows[0].status, "PASS");
    let rows = decode(
        &q,
        &response(&[0x46, 0xa1, 1, 0xf0, 0, 1, 0, 0, 0, 2]),
        true,
    )
    .unwrap();
    assert!(rows[0].numeric.is_none());
    assert!(rows[0].status.contains("unverified"));
    for scale in [1, 0x16, 0xf0] {
        let rows = decode(
            &q,
            &response(&[0x46, 0xa1, 1, scale, 0, 0, 0, 0, 0, 0]),
            true,
        )
        .unwrap();
        assert_eq!(rows[0].status, "Not completed");
        assert!(rows[0].numeric.is_none());
    }
    let zero_pass = decode(&q, &response(&[0x46, 0xa1, 1, 1, 0, 0, 0, 0, 0, 2]), true).unwrap();
    assert_eq!(zero_pass[0].numeric, Some(0.0));
    assert_eq!(zero_pass[0].status, "PASS");
    let invalid = decode(&q, &response(&[0x46, 0xa1, 1, 1, 0, 0, 0, 2, 0, 1]), true).unwrap();
    assert!(invalid[0].status.starts_with("Invalid limits"));
    let o2_voltage = decode(
        &q,
        &response(&[0x46, 0xa1, 1, 0x0a, 0x0b, 0xb0, 0x0b, 0xb0, 0x0b, 0xb0]),
        true,
    )
    .unwrap();
    assert_eq!(o2_voltage[0].unit, "V");
    assert!((o2_voltage[0].numeric.unwrap() - 0.365024).abs() < 1e-9);
    assert_eq!(o2_voltage[0].status, "PASS");
    assert!(decode(&q, &response(&[0x46, 0xa1, 1, 1, 0]), true).is_err());
    assert!(decode(&q, &response(&[0x7f, 6, 0x11]), true).is_err());
    let first = decode(&q, &response(&[0x46, 0xa1, 1, 0, 2, 0, 0, 0, 4]), false).unwrap();
    let second = decode(&q, &response(&[0x46, 0xa1, 2, 0, 3, 0, 0, 0, 4]), false).unwrap();
    assert_ne!(first[0].name, second[0].name);
    assert_eq!(first[0].unit, "raw");
    assert!(first[0].numeric.is_none());
    let legacy = decode(&q, &response(&[0x46, 0xa1, 0x81, 0, 2, 0, 1]), false).unwrap();
    assert_eq!(legacy[0].name, "TID A1 / CID 01 (min)");
    assert_eq!(legacy[0].value, "0x0002 (min 0x0001)");
    assert!(decode(&query(&[8, 1]), &response(&[0x48, 2]), true).is_err());
}

#[test]
fn code_lists_cannot_report_empty_from_an_inconsistent_count() {
    for mode in [3, 7, 10] {
        let q = query(&[mode]);
        let service = mode + 0x40;
        for bad in [
            [service, 0, 1, 0x33],
            [service, 1, 0, 0],
            [service, 2, 1, 0x33],
        ] {
            assert!(decode(&q, &response(&bad), true).is_err());
        }
        let empty = decode(&q, &response(&[service, 0, 0, 0]), true).unwrap();
        assert_eq!(empty[0].value, "No codes reported by this ECU");
        let code = decode(&q, &response(&[service, 1, 1, 0x33, 0, 0]), true).unwrap();
        assert_eq!(code[0].value, "P0133");
        assert!(decode(&q, &response(&[service]), false).is_err());
        assert_eq!(
            decode(&q, &response(&[0x7f, mode, 0x11]), true).unwrap_err(),
            "ECU reports this code service is unsupported"
        );
    }
}

#[test]
fn permanent_codes_and_vehicle_information_remain_per_ecu() {
    assert!(
        decode(&query(&[4]), &response(&[0x44]), true).unwrap()[0]
            .value
            .contains("acknowledged")
    );
    assert!(decode(&query(&[4]), &response(&[0x7f, 4, 0x22]), true).is_err());
    let rows = decode(&query(&[10]), &response(&[0x4a, 1, 1, 0x33]), true).unwrap();
    assert_eq!(rows[0].value, "P0133");
    let padded = decode(&query(&[3]), &response(&[0x43, 1, 1, 0x33, 0]), true).unwrap();
    assert_eq!(padded[0].value, "P0133");
    assert!(decode(&query(&[3]), &response(&[0x43, 1, 0x33, 0]), false).is_err());
    assert!(decode(&query(&[10]), &response(&[0x4a, 2, 1]), true).is_err());
    let rows = decode(
        &query(&[9, 6]),
        &response(&[0x49, 6, 1, 0x12, 0x34, 0x56, 0x78]),
        true,
    )
    .unwrap();
    assert_eq!(rows[0].value, "12345678");
    let rows = decode(
        &query(&[9, 4]),
        &response(b"\x49\x04\x01CALIBRATION\0\0\0\0\0"),
        true,
    )
    .unwrap();
    assert_eq!(rows[0].value, "CALIBRATION");
}

#[test]
fn vehicle_information_validates_item_counts_and_labels_monitor_counters() {
    let cal = query(&[9, 4]);
    let calibration = b"\x49\x04\x02ABCDEFGHIJKLMNO1ABCDEFGHIJKLMNO2";
    assert_eq!(
        decode(&cal, &response(calibration), true).unwrap()[0].value,
        "ABCDEFGHIJKLMNO1 | ABCDEFGHIJKLMNO2"
    );
    assert!(decode(&cal, &response(&calibration[..19]), true).is_err());
    assert!(
        decode(
            &cal,
            &response(b"\x49\x04\x01CAL\0OTHER\0\0\0\0\0\0\0"),
            true
        )
        .is_err()
    );
    let cvn = query(&[9, 6]);
    assert!(decode(&cvn, &response(&[0x49, 6, 1, 0x12, 0x34, 0x56]), true).is_err());

    let mut performance = vec![0x49, 0x0b, 18];
    for value in 1..=18u16 {
        performance.extend(value.to_be_bytes());
    }
    let q = query(&[9, 0x0b]);
    let rows = decode(&q, &response(&performance), true).unwrap();
    assert_eq!(
        (rows[1].name.as_str(), rows[1].numeric),
        ("Ignition cycles", Some(2.0))
    );
    assert_eq!(
        (rows[2].name.as_str(), rows[2].value.as_str()),
        ("NMHC catalyst", "3 / 4")
    );
    assert_eq!(
        (rows[9].name.as_str(), rows[9].value.as_str()),
        ("Fuel system", "17 / 18")
    );
    performance.pop();
    assert!(decode(&q, &response(&performance), true).is_err());
}

#[test]
fn legacy_multiple_packets_do_not_lose_faults_or_vin_fragments() {
    let replies = normalize_legacy(
        &query(&[3]),
        vec![
            response(&[0x43, 1, 0x33, 0, 0]),
            response(&[0x43, 0xc1, 0, 0, 0]),
        ],
        false,
    )
    .unwrap();
    assert_eq!(replies.len(), 1);
    let codes = decode(&query(&[3]), &replies[0], false).unwrap();
    assert!(codes[0].value.contains("P0133") && codes[0].value.contains("U0100"));
    let q = query(&[9, 2]);
    let replies = normalize_legacy(
        &q,
        vec![
            response(b"\x49\x02\x02ABCDEFG"),
            response(b"\x49\x02\x011234567890"),
            response(b"\x49\x02\x02ABCDEFG"),
        ],
        false,
    )
    .unwrap();
    assert_eq!(
        decode(&q, &replies[0], false).unwrap()[0].value,
        "1234567890ABCDEFG"
    );
}

#[test]
fn can_vin_requires_one_complete_identifier() {
    let q = query(&[9, 2]);
    let vin = decode(&q, &response(b"\x49\x02\x01jtdbr32e720123456"), true).unwrap();
    assert_eq!(vin[0].value, "JTDBR32E720123456");
    for (payload, error) in [
        (&b"\x49\x02"[..], "count or sequence"),
        (&b"\x49\x02\x02JTDBR32E720123456"[..], "count or sequence"),
        (&b"\x49\x02\x01JTDB"[..], "Expected 17"),
        (&b"\x49\x02\x01JTDBR32E7201234567"[..], "Expected 17"),
        (&b"\x49\x02\x01JTDBR32E72012O456"[..], "invalid characters"),
        (
            &b"\x49\x02\x01JTDBR32E72012\x00456"[..],
            "invalid characters",
        ),
    ] {
        assert!(
            decode(&q, &response(payload), true)
                .unwrap_err()
                .contains(error)
        );
    }
}

struct Fake {
    expected: VecDeque<(Vec<u8>, Vec<DiagnosticResponse>)>,
    info: ConnectionInfo,
}
impl Fake {
    fn new(expected: Vec<(Vec<u8>, Vec<DiagnosticResponse>)>) -> Self {
        Self {
            expected: expected.into(),
            info: ConnectionInfo {
                port: "fixture".into(),
                baud: 0,
                protocol: "CAN".into(),
                elm_version: "fixture".into(),
                voltage: None,
            },
        }
    }
}
impl DiagnosticAdapter for Fake {
    async fn request(
        &mut self,
        payload: &[u8],
        _: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        let (expected, replies) = self
            .expected
            .pop_front()
            .expect("Unexpected vehicle operation");
        assert_eq!(payload, expected);
        Ok(replies)
    }
    fn connection_info(&self) -> &ConnectionInfo {
        &self.info
    }
    async fn voltage(&mut self) -> Result<String, Elm327Error> {
        Ok("12V".into())
    }
    async fn delay(&mut self, _: u64) {}
}
fn job(queries: Vec<Query>) -> Job {
    Job {
        id: 1,
        queries,
        discovery: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        allow_control: false,
    }
}

#[test]
fn legacy_vin_read_preserves_one_complete_ecu_reply_with_other_malformed_sequences() {
    let reply = |source, payload: &[u8]| DiagnosticResponse::new(Some(source), payload.to_vec());
    let mut adapter = Fake::new(vec![(
        vec![9, 2],
        vec![
            reply(0x10, b"\x49\x02\x01\0\0\0J"),
            reply(0x18, b"\x49\x02\x01\0\0\0J"),
            reply(0x10, b"\x49\x02\x02TDBR"),
            reply(0x10, b"\x49\x02\x0332E7"),
            reply(0x18, b"\x49\x02\x0332E7"),
            reply(0x20, b"\x49\x02\x01\0\0\0J"),
            reply(0x10, b"\x49\x02\x042012"),
            reply(0x10, b"\x49\x02\x053456"),
        ],
    )]);
    adapter.info.protocol = "ISO 9141-2".into();
    let (tx, rx) = mpsc::channel();
    block_on(execute(&mut adapter, job(vec![query(&[9, 2])]), &tx));
    let mut rows: Vec<_> = rx
        .try_iter()
        .filter_map(|event| match event {
            ObdEvent::Diagnostics(Update::Row(1, row)) => Some(row),
            _ => None,
        })
        .collect();
    rows.sort_by_key(|row| row.source);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].source, Some(0x10));
    assert_eq!(rows[0].value, "JTDBR32E720123456");
    assert_eq!(rows[1].source, Some(0x18));
    assert_eq!(rows[1].value, "Unavailable");
    assert!(
        rows[1]
            .status
            .contains("legacy vehicle-information sequence")
    );
    assert_eq!(rows[2].source, Some(0x20));
    assert_eq!(rows[2].value, "Unavailable");
    assert!(rows[2].status.contains("Expected 17"));
}

#[test]
fn service_discovery_unions_ecus_and_follows_continuations() {
    let mut adapter = Fake::new(vec![
        (
            vec![1, 0],
            vec![
                response(&[0x41, 0, 0x80, 0, 0, 0]),
                response(&[0x41, 0, 0x40, 0, 0, 1]),
            ],
        ),
        (vec![1, 0x20], vec![response(&[0x41, 0x20, 0x80, 0, 0, 0])]),
    ]);
    let (tx, rx) = mpsc::channel();
    let mut job = job(Vec::new());
    job.discovery = Some((1, 0));
    block_on(execute(&mut adapter, job, &tx));
    assert!(
        rx.try_iter().any(
            |e| matches!(e,ObdEvent::Diagnostics(Update::Support(1,1,pids)) if pids==[1,2,0x21])
        )
    );
    assert!(adapter.expected.is_empty());
}

#[test]
fn discovery_does_not_publish_a_partial_bitmap_when_a_known_ecu_stops_replying() {
    let first_ecu = response(&[0x41, 0, 0x80, 0, 0, 1]);
    let mut second_ecu = response(&[0x41, 0x20, 0x40, 0, 0, 0]);
    second_ecu.source = Some(0x7e9);
    let mut adapter = Fake::new(vec![
        (vec![1, 0], vec![first_ecu]),
        (vec![1, 0x20], vec![second_ecu]),
    ]);
    let (tx, rx) = mpsc::channel();
    let mut job = job(Vec::new());
    job.discovery = Some((1, 0));
    block_on(execute(&mut adapter, job, &tx));
    let updates: Vec<_> = rx.try_iter().collect();
    assert!(
        !updates
            .iter()
            .any(|e| matches!(e, ObdEvent::Diagnostics(Update::Support(..))))
    );
    assert!(updates.iter().any(|e| matches!(e, ObdEvent::Diagnostics(Update::Done(_, message)) if message.contains("Discovery incomplete"))));
}

#[test]
fn a_pending_first_bitmap_does_not_make_the_other_ecus_bitmap_complete() {
    let ready = response(&[0x41, 0, 0x80, 0, 0, 0]);
    let mut pending = response(&[0x7f, 1, 0x78]);
    pending.source = Some(0x7e9);
    let mut adapter = Fake::new(vec![(vec![1, 0], vec![ready, pending])]);
    let (tx, rx) = mpsc::channel();
    let mut job = job(Vec::new());
    job.discovery = Some((1, 0));
    block_on(execute(&mut adapter, job, &tx));
    let updates: Vec<_> = rx.try_iter().collect();
    assert!(
        !updates
            .iter()
            .any(|e| matches!(e, ObdEvent::Diagnostics(Update::Support(..))))
    );
    assert!(updates.iter().any(|e| matches!(e, ObdEvent::Diagnostics(Update::Done(_, message)) if message.contains("remained pending"))));
}

#[test]
fn controls_require_confirmation_and_cancellation_does_not_transmit() {
    let (tx, rx) = mpsc::channel();
    let mut adapter = Fake::new(Vec::new());
    block_on(execute(
        &mut adapter,
        job(vec![query(&[4]), query(&[8, 1])]),
        &tx,
    ));
    assert_eq!(rx.try_iter().filter(|e|matches!(e,ObdEvent::Diagnostics(Update::Row(_,Reading{value,..})) if value=="Blocked")).count(),2);
    let job = job(vec![query(&[1, 0x0c])]);
    job.cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    block_on(execute(&mut adapter, job, &tx));
    assert!(
        rx.try_iter()
            .any(|e| matches!(e,ObdEvent::Diagnostics(Update::Done(_,s)) if s=="Cancelled"))
    );
}

#[test]
fn active_tests_and_enhanced_requests_are_blocked_even_when_clear_is_confirmed() {
    let mut adapter = Fake::new(Vec::new());
    let (tx, rx) = mpsc::channel();
    let mut job = job(vec![
        query(&[8, 1]),
        query(&[0x19, 2, 0xff]),
        query(&[0x21, 0x82]),
        query(&[0x22, 0xf1, 0x90]),
    ]);
    job.allow_control = true;
    block_on(execute(&mut adapter, job, &tx));
    let blocked: Vec<String> = rx
        .try_iter()
        .filter_map(|event| match event {
            ObdEvent::Diagnostics(Update::Row(1, reading)) if reading.value == "Blocked" => {
                Some(reading.request)
            }
            _ => None,
        })
        .collect();
    assert_eq!(blocked, ["0801", "1902FF", "2182", "22F190"]);
}

#[test]
fn generic_diagnostics_render_without_a_vehicle() {
    let mut panel = Panel::default();
    let context = egui::Context::default();
    for can in [false, true] {
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1300.0, 900.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    assert!(panel.show(ui, false, can).is_none());
                });
            },
        );
        assert!(!output.shapes.is_empty());
    }
}
