#![cfg(not(target_arch = "wasm32"))]

use obd_dashboard::adapter::{
    DiagnosticAdapter, DiagnosticResponse, request_hex, request_hex_report,
};
use obd_dashboard::app::{ObdEvent, PollConfig, PollMode};
use obd_dashboard::elm327::{ConnectionInfo, Elm327Error, block_on};
use obd_dashboard::{obd, obd_ops};
use std::collections::VecDeque;
use std::sync::mpsc;

type Exchange = (Vec<u8>, Result<Vec<DiagnosticResponse>, Elm327Error>);
struct ScriptedAdapter {
    exchanges: VecDeque<Exchange>,
    info: ConnectionInfo,
}
impl ScriptedAdapter {
    fn new(exchanges: Vec<Exchange>) -> Self {
        Self {
            exchanges: exchanges.into(),
            info: ConnectionInfo {
                port: "test-only".into(),
                baud: 0,
                protocol: "CAN".into(),
                elm_version: "fixture".into(),
                voltage: None,
            },
        }
    }
}
impl DiagnosticAdapter for ScriptedAdapter {
    async fn request(
        &mut self,
        payload: &[u8],
        _: u64,
    ) -> Result<Vec<DiagnosticResponse>, Elm327Error> {
        let (expected, response) = self
            .exchanges
            .pop_front()
            .expect("unexpected vehicle request");
        assert_eq!(payload, expected);
        response
    }
    fn connection_info(&self) -> &ConnectionInfo {
        &self.info
    }
    async fn voltage(&mut self) -> Result<String, Elm327Error> {
        Ok("12.6V".into())
    }
    async fn delay(&mut self, _: u64) {}
}
fn response(source: u32, payload: &[u8]) -> DiagnosticResponse {
    DiagnosticResponse::new(Some(source), payload.into())
}
fn timeout() -> Result<Vec<DiagnosticResponse>, Elm327Error> {
    Err(Elm327Error::Timeout("simulated vehicle silence".into()))
}

#[test]
fn mixed_read_replies_keep_positive_data_in_either_order_and_report_rejection() {
    for reverse in [false, true] {
        let mut replies = vec![
            response(0x7e8, &[0x43, 1, 1, 0x33]),
            response(0x7e9, &[0x7f, 3, 0x11]),
        ];
        if reverse {
            replies.reverse();
        }
        let mut adapter = ScriptedAdapter::new(vec![(vec![3], Ok(replies))]);
        let report = block_on(request_hex_report(&mut adapter, "03", 100)).unwrap();
        assert_eq!(report.lines, ["43010133"]);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("7F0311"));
    }
}

#[test]
fn negative_only_read_and_mixed_clear_both_remain_errors() {
    let mut adapter = ScriptedAdapter::new(vec![
        (vec![3], Ok(vec![response(0x7e9, &[0x7f, 3, 0x11])])),
        (
            vec![4],
            Ok(vec![
                response(0x7e8, &[0x44]),
                response(0x7e9, &[0x7f, 4, 0x22]),
            ]),
        ),
    ]);
    assert!(block_on(request_hex(&mut adapter, "03", 100)).is_err());
    assert!(block_on(request_hex(&mut adapter, "04", 100)).is_err());
}

#[test]
fn pending_timeout_keeps_the_successful_stored_scan() {
    let mut adapter = ScriptedAdapter::new(vec![
        (vec![3], Ok(vec![response(0x7e8, &[0x43, 1, 1, 0x33])])),
        (vec![7], timeout()),
    ]);
    let (tx, rx) = mpsc::channel();
    let scan = block_on(obd_ops::read_dtcs(&mut adapter, &tx));
    assert_eq!(scan.stored.as_ref().unwrap()[0].code, "P0133");
    assert!(scan.pending.is_none());
    assert_eq!(scan.warnings.len(), 1);
    assert!(matches!(rx.recv().unwrap(), ObdEvent::DtcResult(result) if result.pending.is_none()));
}

#[test]
fn failed_stored_read_still_attempts_pending_codes() {
    let mut adapter = ScriptedAdapter::new(vec![
        (vec![3], timeout()),
        (vec![7], Ok(vec![response(0x7e8, &[0x47, 1, 1, 0x33])])),
    ]);
    let (tx, _) = mpsc::channel();
    let scan = block_on(obd_ops::read_dtcs(&mut adapter, &tx));
    assert!(scan.stored.is_none());
    assert_eq!(scan.pending.unwrap()[0].code, "P0133");
    assert!(!scan.warnings.is_empty());
}

#[test]
fn rejected_clear_never_publishes_empty_success_or_starts_a_reread() {
    let mut adapter =
        ScriptedAdapter::new(vec![(vec![4], Ok(vec![response(0x7e8, &[0x7f, 4, 0x22])]))]);
    let (tx, rx) = mpsc::channel();
    let scan = block_on(obd_ops::clear_dtcs(&mut adapter, &tx));
    assert!(scan.stored.is_none() && scan.pending.is_none());
    assert!(scan.warnings[0].contains("Clear DTCs failed"));
    assert!(matches!(rx.recv().unwrap(), ObdEvent::DtcResult(_)));
    assert!(rx.try_recv().is_err());
    assert!(adapter.exchanges.is_empty());
}

#[test]
fn successful_empty_scan_is_distinct_from_a_failed_scan() {
    let mut adapter = ScriptedAdapter::new(vec![
        (vec![3], Ok(vec![response(0x7e8, &[0x43, 0])])),
        (vec![7], Ok(vec![response(0x7e8, &[0x47, 0])])),
    ]);
    let (tx, _) = mpsc::channel();
    let scan = block_on(obd_ops::read_dtcs(&mut adapter, &tx));
    assert!(scan.stored.unwrap().is_empty() && scan.pending.unwrap().is_empty());
    assert!(scan.warnings.is_empty());
}

#[test]
fn dtc_partial_ecu_failures_are_visible_alongside_valid_codes() {
    let mut adapter = ScriptedAdapter::new(vec![
        (
            vec![3],
            Ok(vec![
                response(0x7e8, &[0x43, 1, 1, 0x33]),
                response(0x7e9, &[0x7f, 3, 0x11]),
            ]),
        ),
        (vec![7], Ok(vec![response(0x7e8, &[0x47, 0])])),
    ]);
    let (tx, _) = mpsc::channel();
    let scan = block_on(obd_ops::read_dtcs(&mut adapter, &tx));
    assert_eq!(scan.stored.unwrap()[0].code, "P0133");
    assert_eq!(scan.warnings.len(), 1);
}

#[test]
fn support_maps_union_all_ecus_and_follow_any_continuation_bit() {
    for reverse in [false, true] {
        let mut first_page = vec![
            response(0x7e8, &[0x41, 0, 0x80, 0, 0, 0]),
            response(0x7e9, &[0x41, 0, 0x40, 0, 0, 1]),
        ];
        if reverse {
            first_page.reverse();
        }
        let mut adapter = ScriptedAdapter::new(vec![
            (vec![1, 0], Ok(first_page)),
            (
                vec![1, 0x20],
                Ok(vec![
                    response(0x7e9, &[0x41, 0x20, 0xc0, 0, 0, 0]),
                    response(0x7e8, &[0x41, 0x20, 0x80, 0, 0, 0]),
                ]),
            ),
        ]);
        let (tx, rx) = mpsc::channel();
        block_on(obd_ops::query_supported_pids(&mut adapter, &tx));
        assert!(
            matches!(rx.recv().unwrap(), ObdEvent::SupportedPids(pids) if pids == [1, 2, 0x21, 0x22])
        );
        assert!(adapter.exchanges.is_empty());
    }
}

#[test]
fn live_health_distinguishes_total_silence_from_an_unsupported_pid() {
    for all_silent in [false, true] {
        let config = PollConfig {
            mode: PollMode::Minimal,
            ..Default::default()
        };
        let mut adapter = ScriptedAdapter::new(vec![
            (
                vec![1, 0x0c],
                if all_silent {
                    timeout()
                } else {
                    Ok(vec![response(0x7e8, &[0x41, 0x0c, 0x1a, 0xf8])])
                },
            ),
            (vec![1, 0x0d], timeout()),
            (vec![1, 0x11], timeout()),
            (vec![1, 4], timeout()),
        ]);
        let (tx, rx) = mpsc::channel();
        block_on(obd_ops::poll_live_data(
            &mut adapter,
            &tx,
            &obd::mode01_pids(),
            &config,
        ));
        assert!(rx.try_iter().any(|event| matches!(event, ObdEvent::LiveDataHealth { responding } if responding != all_silent)));
    }
}
