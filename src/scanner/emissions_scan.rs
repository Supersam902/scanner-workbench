use crate::diagnostics::Reading;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use super::codes;
use super::model::{ClearScanStage, Sample, ScanEvent, unavailable, valid_vin};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum State {
    Running,
    Finished,
    Interrupted,
}

impl State {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Running => "In progress",
            Self::Finished => "Finished",
            Self::Interrupted => "Interrupted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Check {
    Readiness,
    DriveCycle,
    Stored,
    Pending,
    Permanent,
    Vin,
    FreezeFrame,
}

impl Check {
    const ALL: [Self; 7] = [
        Self::Readiness,
        Self::DriveCycle,
        Self::Stored,
        Self::Pending,
        Self::Permanent,
        Self::Vin,
        Self::FreezeFrame,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Readiness => "Readiness since clear",
            Self::DriveCycle => "Readiness this drive cycle",
            Self::Stored => "Stored codes",
            Self::Pending => "Pending codes",
            Self::Permanent => "Permanent codes",
            Self::Vin => "VIN",
            Self::FreezeFrame => "Freeze frame 0",
        }
    }

    fn request(self) -> &'static str {
        match self {
            Self::Readiness => "0101",
            Self::DriveCycle => "0141",
            Self::Stored => "03",
            Self::Pending => "07",
            Self::Permanent => "0A",
            Self::Vin => "0902",
            Self::FreezeFrame => "020200",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct CheckResult {
    replies: usize,
    issues: usize,
    unsupported: usize,
    finding: String,
    attention: bool,
}

impl CheckResult {
    fn from_rows(check: Check, rows: &[&Reading]) -> Self {
        let blocked: BTreeSet<_> = rows
            .iter()
            .filter(|row| unavailable(row) && !row.is_signal_failure())
            .map(|row| row.source)
            .collect();
        let verified: Vec<_> = rows
            .iter()
            .copied()
            .filter(|row| !unavailable(row) && !blocked.contains(&row.source))
            .collect();
        let replies = verified
            .iter()
            .map(|row| row.source)
            .collect::<BTreeSet<_>>()
            .len();
        let unsupported = rows
            .iter()
            .filter(|row| {
                unavailable(row)
                    && matches!(
                        row.status.as_str(),
                        "ECU reports this code service is unsupported"
                            | crate::diagnostics::UNSUPPORTED_REQUEST
                    )
            })
            .count();
        let issues = rows.iter().filter(|row| unavailable(row)).count() - unsupported;
        let mut attention = issues > 0;
        let finding = match check {
            Check::Readiness | Check::DriveCycle => {
                let ready = verified.iter().filter(|row| row.value == "Ready").count();
                let incomplete = verified
                    .iter()
                    .filter(|row| row.value == "Not ready")
                    .count();
                let mil_on = verified
                    .iter()
                    .any(|row| row.name == "Malfunction indicator lamp" && row.value == "On");
                attention |= incomplete > 0 || mil_on;
                if replies == 0 {
                    String::new()
                } else {
                    format!(
                        "{ready} ready · {incomplete} incomplete{}",
                        if mil_on { " · MIL on" } else { "" }
                    )
                }
            }
            Check::Vin => {
                let vins: BTreeSet<_> = verified
                    .iter()
                    .filter(|row| valid_vin(&row.value))
                    .map(|row| row.value.trim().to_ascii_uppercase())
                    .collect();
                match vins.len() {
                    0 => String::new(),
                    1 => "VIN received".into(),
                    _ => {
                        attention = true;
                        "ECUs reported different VINs".into()
                    }
                }
            }
            Check::FreezeFrame if replies > 0 => {
                let codes: BTreeSet<_> = verified
                    .iter()
                    .flat_map(|row| codes::identifiers(&row.value))
                    .collect();
                if codes.is_empty() {
                    "No stored frame reported".into()
                } else {
                    format!(
                        "Trigger {}",
                        codes.into_iter().collect::<Vec<_>>().join(", ")
                    )
                }
            }
            _ if replies > 0 => {
                let count = verified
                    .iter()
                    .flat_map(|row| codes::identifiers(&row.value).map(|code| (row.source, code)))
                    .collect::<BTreeSet<_>>()
                    .len();
                attention |= count > 0;
                format!("{count} {}", if count == 1 { "code" } else { "codes" })
            }
            _ => String::new(),
        };
        Self {
            replies,
            issues,
            unsupported,
            finding,
            attention,
        }
    }

    pub(super) fn label(&self) -> String {
        let mut parts = Vec::new();
        if !self.finding.is_empty() {
            parts.push(self.finding.clone());
        }
        if self.replies > 0 {
            parts.push(format!(
                "{} ECU {}",
                self.replies,
                if self.replies == 1 {
                    "reply"
                } else {
                    "replies"
                }
            ));
        }
        if self.issues > 0 {
            parts.push(format!(
                "{} read {}",
                self.issues,
                if self.issues == 1 { "issue" } else { "issues" }
            ));
        }
        if self.unsupported > 0 {
            parts.push(format!("{} ECU unsupported", self.unsupported));
        }
        parts.join(" · ")
    }

    pub(super) fn attention(&self) -> bool {
        self.attention
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scan {
    pub(super) started_ms: u64,
    pub(super) state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) clear_stage: Option<ClearScanStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) ended_ms: Option<u64>,
    results: [Option<CheckResult>; 5],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drive_cycle: Option<CheckResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    freeze_frame: Option<CheckResult>,
}

impl Scan {
    pub(super) fn apply(slot: &mut Option<Self>, sample: &Sample) {
        if matches!(sample.emissions_scan, Some(ScanEvent::Started)) {
            *slot = Some(Self {
                started_ms: sample.at_ms,
                state: State::Running,
                clear_stage: sample.clear_scan_stage,
                ended_ms: None,
                results: Default::default(),
                drive_cycle: None,
                freeze_frame: None,
            });
        }
        let Some(scan) = slot.as_mut().filter(|scan| scan.state == State::Running) else {
            return;
        };
        for check in Check::ALL {
            let rows: Vec<_> = sample
                .rows
                .iter()
                .filter(|row| row.request == check.request())
                .collect();
            if !rows.is_empty() {
                let result = Some(CheckResult::from_rows(check, &rows));
                match check {
                    Check::Readiness => scan.results[0] = result,
                    Check::Stored => scan.results[1] = result,
                    Check::Pending => scan.results[2] = result,
                    Check::Permanent => scan.results[3] = result,
                    Check::Vin => scan.results[4] = result,
                    Check::DriveCycle => scan.drive_cycle = result,
                    Check::FreezeFrame => scan.freeze_frame = result,
                }
            }
        }
        scan.state = match sample.emissions_scan {
            Some(ScanEvent::Finished) => State::Finished,
            Some(ScanEvent::Interrupted) => State::Interrupted,
            _ => State::Running,
        };
        if scan.state != State::Running {
            scan.ended_ms = Some(sample.at_ms);
        }
    }

    pub(super) fn checks(&self) -> impl Iterator<Item = (Check, Option<&CheckResult>)> {
        Check::ALL.into_iter().zip([
            self.results[0].as_ref(),
            self.drive_cycle.as_ref(),
            self.results[1].as_ref(),
            self.results[2].as_ref(),
            self.results[3].as_ref(),
            self.results[4].as_ref(),
            self.freeze_frame.as_ref(),
        ])
    }

    pub(super) fn check_label(&self, result: Option<&CheckResult>) -> String {
        match result {
            Some(result) => result.label(),
            None if self.state == State::Running => "Awaiting reply".into(),
            None => "No result from this scan".into(),
        }
    }
}
