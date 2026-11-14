use super::VERSION;
use super::elm::ElmConnection;
use super::worker::{AttemptOutcome, ConnectionAttempt, ReplyCheck, VoltageAdapter, VoltageSample};
use crate::j2534::{CanProtocol, DriverInfo, ProbePhase, format_voltage, same_driver_path};
use chrono::{DateTime, Local};
use std::fmt::Write;
use std::time::Duration;

pub(super) const VOLTAGE_FRESHNESS: Duration = Duration::from_secs(6);
pub(super) const CANCELLED: &str = "Connection cancelled.";

enum Adapter {
    J2534 {
        driver: Option<DriverInfo>,
        protocol: Option<CanProtocol>,
        source: Option<u8>,
    },
    Elm(ElmConnection),
}

pub(super) struct Report {
    adapter: Adapter,
    started: DateTime<Local>,
    result: Option<String>,
    link_loss: Option<String>,
    pub(super) attempts: Vec<ConnectionAttempt>,
    voltage: Option<VoltageSample>,
}

impl Report {
    pub(super) fn new(
        elm: Option<&ElmConnection>,
        driver: Option<&DriverInfo>,
        protocol: Option<CanProtocol>,
        source: Option<u8>,
    ) -> Self {
        Self {
            adapter: elm.map_or_else(
                || Adapter::J2534 {
                    driver: driver.cloned(),
                    protocol,
                    source,
                },
                |connection| Adapter::Elm(connection.clone()),
            ),
            started: Local::now(),
            result: None,
            link_loss: None,
            attempts: Vec::new(),
            voltage: None,
        }
    }

    pub(super) fn observe_voltage(&mut self, sample: &VoltageSample) {
        if self.is_finished() {
            return;
        }
        let matches = match (&self.adapter, &sample.adapter) {
            (Adapter::J2534 { driver, .. }, VoltageAdapter::J2534(actual)) => driver
                .as_ref()
                .is_none_or(|driver| same_driver_path(&driver.path, &actual.path)),
            (Adapter::Elm(selected), VoltageAdapter::Elm(actual)) => selected == actual,
            _ => false,
        };
        if matches
            && self
                .voltage
                .as_ref()
                .is_none_or(|last| last.sampled_at <= sample.sampled_at)
        {
            self.voltage = Some(sample.clone());
        }
    }

    pub(super) fn finish(&mut self, outcome: impl Into<String>) {
        self.result = Some(outcome.into());
    }

    pub(super) fn is_finished(&self) -> bool {
        self.result.is_some()
    }

    pub(super) fn result(&self) -> Option<&str> {
        self.link_loss.as_deref().or(self.result.as_deref())
    }

    pub(super) fn record_link_loss(&mut self, reason: String) {
        self.link_loss = Some(reason);
    }

    pub(super) fn voltage_label(&self) -> Option<String> {
        let voltage = self.voltage.as_ref()?;
        Some(format!(
            "{} · {}",
            format_voltage(voltage.millivolts),
            voltage.adapter.label()
        ))
    }

    pub(super) fn next_step(&self) -> Option<&'static str> {
        let (_, _, outcome) = self
            .attempts
            .iter()
            .map(|attempt| {
                let (rank, phase) = match attempt.outcome {
                    AttemptOutcome::Opened => (0, None),
                    AttemptOutcome::OpenFailed => (1, None),
                    AttemptOutcome::DriverFailed => (2, None),
                    AttemptOutcome::ElmTransportFailed => (3, None),
                    AttemptOutcome::ElmSetupFailed | AttemptOutcome::ElmProtocolFailed => (4, None),
                    AttemptOutcome::ProbeFailed(phase) => (5, Some(phase)),
                    AttemptOutcome::Verified | AttemptOutcome::Cancelled => (6, None),
                };
                (rank, phase, attempt.outcome)
            })
            .max_by_key(|&(rank, phase, _)| (rank, phase))?;
        if self.result.as_deref() == Some(CANCELLED) {
            return None;
        }
        match outcome {
            AttemptOutcome::Opened | AttemptOutcome::Cancelled | AttemptOutcome::Verified => None,
            AttemptOutcome::OpenFailed | AttemptOutcome::ElmTransportFailed => {
                Some(match &self.adapter {
                    Adapter::J2534 { .. } => {
                        "Check USB and DLC connections. Close other apps using the adapter, then reconnect."
                    }
                    Adapter::Elm(ElmConnection::Serial { .. }) => {
                        "Check the COM port and close any program using it. Verify pairing for Bluetooth serial."
                    }
                    Adapter::Elm(ElmConnection::Tcp(_)) => {
                        "Check the adapter network, host and TCP port. Close other apps using the adapter."
                    }
                })
            }
            AttemptOutcome::ElmSetupFailed | AttemptOutcome::ElmProtocolFailed => {
                Some(match &self.adapter {
                    Adapter::Elm(ElmConnection::Serial { .. }) => {
                        "Check the adapter's baud rate and ELM327 command support. Leave baud rate blank to try automatic detection."
                    }
                    _ => {
                        "Check that this endpoint accepts ELM327 commands with response headers and automatic protocol detection."
                    }
                })
            }
            AttemptOutcome::DriverFailed => Some(
                "Reconnect the adapter. If the adapter process fails again, include this report when asking for help.",
            ),
            AttemptOutcome::ProbeFailed(ProbePhase::Reply | ProbePhase::Initialization) => Some(
                "Verify the ignition is on and the adapter supports the vehicle's OBD-II protocol.",
            ),
            AttemptOutcome::ProbeFailed(ProbePhase::Transmit) => Some(
                "Check the DLC connection and ignition before retrying. Confirm that the adapter supports the selected protocol.",
            ),
            AttemptOutcome::ProbeFailed(_) => Some(
                "Check the adapter driver's protocol support. Use Automatic if you set a protocol override for a different vehicle.",
            ),
        }
    }

    pub(super) fn text(&self, reply_check: Option<&ReplyCheck>) -> String {
        let mut text = format!(
            "Scanner Workbench {VERSION} | {} {}-bit\nConnection report · {}\n",
            if cfg!(windows) {
                "Windows"
            } else {
                std::env::consts::OS
            },
            usize::BITS,
            self.started.format("%Y-%m-%d %H:%M:%S %:z"),
        );
        match &self.adapter {
            Adapter::J2534 {
                driver,
                protocol,
                source,
            } => {
                let driver = driver
                    .as_ref()
                    .map_or("Automatic", |driver| driver.name.as_str());
                let protocol = protocol.map_or("Automatic", CanProtocol::label);
                let _ = writeln!(
                    text,
                    "Adapter: J2534 04.04\nDriver: {driver}\nProtocol selection: {protocol}"
                );
                if let Some(source) = source {
                    let _ = writeln!(text, "29-bit ECU source: {source:02X}");
                }
            }
            Adapter::Elm(connection) => {
                let _ = writeln!(text, "Adapter: ELM-compatible {}", connection.label());
                if let ElmConnection::Serial { baud, .. } = connection {
                    let baud = baud.map_or_else(|| "Automatic".into(), |baud| baud.to_string());
                    let _ = writeln!(text, "Baud rate: {baud}");
                }
            }
        }
        if let Some(result) = &self.result {
            let _ = writeln!(text, "Result: {result}");
        } else {
            text.push_str("Result: In progress\n");
        }
        if let Some(reason) = &self.link_loss {
            let _ = writeln!(text, "Link lost: {reason}");
        }
        if let Some(voltage) = self.voltage_label() {
            let _ = writeln!(text, "Last DLC voltage at Connect: {voltage}");
        } else {
            text.push_str("DLC voltage: No sample available for this attempt\n");
        }
        if self.is_finished()
            && let Some(step) = self.next_step()
        {
            let _ = writeln!(text, "Next check: {step}");
        }
        if !self.attempts.is_empty() {
            text.push_str("\nAdapter and protocol attempts\n");
        }
        for attempt in &self.attempts {
            let _ = write!(text, "{} · ", attempt.driver);
            if let Some(protocol) = attempt.protocol {
                let remembered = if attempt.remembered_protocol {
                    " (last verified)"
                } else {
                    ""
                };
                let _ = write!(text, "{}{remembered} · ", protocol.label());
            }
            let _ = writeln!(text, "{}", attempt.outcome.label());
            if let Some(detail) = &attempt.detail {
                let _ = writeln!(text, "{detail}");
            }
        }
        if let Some(check) = reply_check {
            let _ = writeln!(
                text,
                "\nVehicle reply check · {} · Request 0100",
                check.protocol.label()
            );
            text.push_str(&check.summary());
            text.push('\n');
        }
        text
    }
}
