use super::model::{Results, Stamped};
use std::collections::BTreeMap;

const IGNITION: &str = "Ignition monitor type";
const MIL: &str = "Malfunction indicator lamp";
const DTC_COUNT: &str = "Number of DTCs stored in this ECU";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(super) enum Period {
    #[default]
    SinceClear,
    DriveCycle,
}

impl Period {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::SinceClear => "Since clear",
            Self::DriveCycle => "This drive cycle",
        }
    }

    pub(super) fn request(self) -> &'static str {
        match self {
            Self::SinceClear => "0101",
            Self::DriveCycle => "0141",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum MonitorState {
    NotReady,
    Disabled,
    DisabledOrUnknown,
    Ready,
    Unsupported,
}

impl MonitorState {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::NotReady => "Not ready",
            Self::Disabled => "Disabled this cycle",
            Self::DisabledOrUnknown => "Disabled or unsupported",
            Self::Ready => "Ready",
            Self::Unsupported => "Not supported",
        }
    }

    pub(super) fn completion_label(self) -> &'static str {
        match self {
            Self::Ready => "Complete",
            Self::NotReady => "Incomplete",
            other => other.label(),
        }
    }
}

pub(super) struct Monitor<'a> {
    pub(super) name: &'a str,
    pub(super) state: MonitorState,
}

pub(super) struct EcuReport<'a> {
    pub(super) source: Option<u32>,
    pub(super) latest: Option<&'a Stamped>,
    pub(super) historical: bool,
    pub(super) refresh_failed: bool,
    pub(super) mil: Option<&'a str>,
    pub(super) dtc_count: Option<&'a str>,
    pub(super) ignition: Option<&'a str>,
    pub(super) monitors: Vec<Monitor<'a>>,
}

fn latest_replies<'a>(
    rows: impl Iterator<Item = &'a Stamped>,
    period: Period,
) -> BTreeMap<Option<u32>, Vec<&'a Stamped>> {
    let mut replies: BTreeMap<Option<u32>, Vec<&Stamped>> = BTreeMap::new();
    for stamped in rows {
        let reading = &stamped.reading;
        if reading.request == period.request() {
            replies.entry(reading.source).or_default().push(stamped);
        }
    }
    for rows in replies.values_mut() {
        // All the decoded fields from one reply share its timestamp and raw bytes.
        // The ignition row also identifies the newer reply if a replay reuses a timestamp.
        if let Some(newest) = rows
            .iter()
            .copied()
            .max_by_key(|row| (row.at_ms, row.reading.name == IGNITION))
        {
            rows.retain(|row| row.at_ms == newest.at_ms && row.reading.raw == newest.reading.raw);
        }
    }
    replies
}

fn reading_named<'a>(rows: &[&'a Stamped], name: &str) -> Option<&'a Stamped> {
    rows.iter().copied().find(|row| row.reading.name == name)
}

pub(super) fn ecu_reports(results: &Results, period: Period) -> Vec<EcuReport<'_>> {
    reports(
        results.rows.values().map(|value| &value.latest),
        results.failures.values(),
        period,
        |row| results.predates_clear(row),
        |row| results.refresh_failed(row),
    )
}

pub(super) fn reports<'a>(
    rows: impl Iterator<Item = &'a Stamped> + Clone,
    failures: impl Iterator<Item = &'a Stamped>,
    period: Period,
    predates_clear: impl Fn(&Stamped) -> bool,
    refresh_failed: impl Fn(&Stamped) -> bool,
) -> Vec<EcuReport<'a>> {
    let mut replies = latest_replies(rows.clone(), period);
    for failure in failures {
        let reading = &failure.reading;
        if reading.request == period.request() && reading.source.is_some() {
            replies.entry(reading.source).or_default();
        }
    }
    let since_clear =
        (period == Period::DriveCycle).then(|| latest_replies(rows, Period::SinceClear));
    let fresh = |row: &Stamped| !predates_clear(row) && !refresh_failed(row);

    replies
        .into_iter()
        .map(|(source, rows)| {
            let latest = rows.first().copied();
            let historical = latest.is_some_and(&predates_clear);
            let refresh_failed = rows.is_empty() || rows.iter().any(|row| refresh_failed(row));
            let ignition = reading_named(&rows, IGNITION)
                .map(|row| row.reading.value.as_str())
                .filter(|value| matches!(*value, "Spark" | "Compression"));
            let support = since_clear.as_ref().and_then(|replies| {
                let support = replies.get(&source)?;
                let support_ignition = reading_named(support, IGNITION)?;
                (source.is_some()
                    && !historical
                    && !refresh_failed
                    && fresh(support_ignition)
                    && ignition == Some(support_ignition.reading.value.as_str()))
                .then_some(support)
            });
            let mut monitors: Vec<_> = rows
                .iter()
                .filter_map(|row| {
                    let reading = &row.reading;
                    let name = reading.name.as_str();
                    if matches!(name, IGNITION | MIL | DTC_COUNT) {
                        return None;
                    }
                    let state = match reading.value.as_str() {
                        "Ready" => MonitorState::Ready,
                        "Not ready" => MonitorState::NotReady,
                        "Not supported" if period == Period::SinceClear => {
                            MonitorState::Unsupported
                        }
                        // Older recordings used "Not supported" for a clear PID 41 enable bit.
                        "Disabled or unsupported" | "Not supported"
                            if period == Period::DriveCycle =>
                        {
                            match support
                                .and_then(|rows| reading_named(rows, name))
                                .filter(|row| fresh(row))
                                .map(|row| row.reading.value.as_str())
                            {
                                Some("Ready" | "Not ready") => MonitorState::Disabled,
                                Some("Not supported") => MonitorState::Unsupported,
                                _ => MonitorState::DisabledOrUnknown,
                            }
                        }
                        _ => return None,
                    };
                    Some(Monitor { name, state })
                })
                .collect();
            monitors.sort_unstable_by_key(|monitor| (monitor.state, monitor.name));
            let (mil, dtc_count) = if period == Period::SinceClear {
                (
                    reading_named(&rows, MIL).map(|row| row.reading.value.as_str()),
                    reading_named(&rows, DTC_COUNT).map(|row| row.reading.value.as_str()),
                )
            } else {
                (None, None)
            };
            EcuReport {
                source,
                latest,
                historical,
                refresh_failed,
                mil,
                dtc_count,
                ignition,
                monitors,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Reading;
    use crate::scanner::model::{Capture, Sample, Vehicle};

    fn row(source: Option<u32>, period: Period, name: &str, value: &str, raw: &str) -> Reading {
        Reading {
            source,
            request: period.request().into(),
            name: name.into(),
            value: value.into(),
            unit: String::new(),
            raw: raw.into(),
            numeric: None,
            status: "ECU responded".into(),
            signal_failure: false,
        }
    }

    fn reply(source: u32, period: Period, raw: &str, fields: &[(&str, &str)]) -> Vec<Reading> {
        fields
            .iter()
            .map(|(name, value)| row(Some(source), period, name, value, raw))
            .collect()
    }

    fn sample(at_ms: u64, rows: Vec<Reading>) -> Sample {
        Sample {
            at_ms,
            rows,
            ..Default::default()
        }
    }

    #[test]
    fn readiness_counts_and_mil_stay_with_each_ecu() {
        let mut results = Results::default();
        let period = Period::SinceClear;
        let mut rows = reply(
            0x7e8,
            period,
            "41010007E5E5",
            &[
                (IGNITION, "Spark"),
                (MIL, "Off"),
                (DTC_COUNT, "0"),
                ("Catalyst monitoring", "Not ready"),
                ("Evaporative system monitoring", "Not ready"),
                ("Oxygen sensor monitoring", "Not ready"),
                ("Oxygen sensor heater monitoring", "Not ready"),
                ("EGR/VVT monitoring", "Not ready"),
                ("Comprehensive component monitoring", "Ready"),
                ("Fuel system monitoring", "Ready"),
                ("Misfire monitoring", "Ready"),
                ("A/C refrigerant monitoring", "Not supported"),
            ],
        );
        rows.extend(reply(
            0x7ea,
            period,
            "410181040000",
            &[
                (IGNITION, "Spark"),
                (MIL, "On"),
                (DTC_COUNT, "1"),
                ("Comprehensive component monitoring", "Ready"),
                ("Catalyst monitoring", "Not supported"),
            ],
        ));
        results.apply(&sample(1, rows));

        let reports = ecu_reports(&results, period);
        assert_eq!(reports.len(), 2);
        let [engine, other] = &reports[..] else {
            unreachable!();
        };
        assert_eq!(
            (engine.source, engine.mil, engine.dtc_count),
            (Some(0x7e8), Some("Off"), Some("0"))
        );
        assert_eq!(
            (other.source, other.mil, other.dtc_count),
            (Some(0x7ea), Some("On"), Some("1"))
        );
        assert_eq!(
            engine
                .monitors
                .iter()
                .filter(|monitor| monitor.state == MonitorState::Ready)
                .count(),
            3
        );
        assert_eq!(
            engine
                .monitors
                .iter()
                .filter(|monitor| monitor.state == MonitorState::NotReady)
                .count(),
            5
        );
        assert!(
            engine.monitors[..5]
                .iter()
                .all(|monitor| monitor.state == MonitorState::NotReady)
        );
    }

    #[test]
    fn older_cycle_recordings_resolve_disabled_bits_with_fresh_matching_support() {
        let since = Period::SinceClear;
        let cycle = Period::DriveCycle;
        let mut capture = Capture::new(Vehicle::default(), false, 0);
        let mut support = reply(
            0x7e8,
            since,
            "410100076504",
            &[
                (IGNITION, "Spark"),
                ("Catalyst monitoring", "Ready"),
                ("Secondary air monitoring", "Not supported"),
            ],
        );
        support.extend(reply(
            0x7ea,
            since,
            "410100080001",
            &[(IGNITION, "Compression"), ("EGR/VVT monitoring", "Ready")],
        ));
        capture.samples.push(sample(1, support));
        let mut reports = reply(
            0x7e8,
            cycle,
            "414100046400",
            &[
                (IGNITION, "Spark"),
                ("Catalyst monitoring", "Not supported"),
                ("Secondary air monitoring", "Not supported"),
            ],
        );
        reports.extend(reply(
            0x7ea,
            cycle,
            "414100000000",
            &[(IGNITION, "Spark"), ("EGR/VVT monitoring", "Not supported")],
        ));
        capture.samples.push(sample(2, reports));
        capture.samples.push(sample(
            3,
            vec![row(Some(0x7e8), since, "Readiness", "Unavailable", "")],
        ));
        capture.samples.push(Sample {
            at_ms: 4,
            clear_requested: true,
            ..Default::default()
        });
        let decoded: Capture =
            serde_json::from_str(&serde_json::to_string(&capture).unwrap()).unwrap();
        decoded.validate().unwrap();

        let results = decoded.replay(2);
        let reports = ecu_reports(&results, cycle);
        assert_eq!(reports[0].monitors[0].state, MonitorState::Disabled);
        assert_eq!(reports[0].monitors[1].state, MonitorState::Unsupported);
        assert_eq!(
            reports[1].monitors[0].state,
            MonitorState::DisabledOrUnknown
        );
        assert_eq!((reports[0].mil, reports[0].dtc_count), (None, None));
        let results = decoded.replay(3);
        assert!(
            ecu_reports(&results, cycle)[0]
                .monitors
                .iter()
                .all(|monitor| monitor.state == MonitorState::DisabledOrUnknown)
        );
        let results = decoded.replay(4);
        assert!(ecu_reports(&results, cycle)[0].historical);
    }

    #[test]
    fn newest_reply_excludes_old_monitors_and_preserves_scoped_failures() {
        let mut results = Results::default();
        let period = Period::SinceClear;
        results.apply(&sample(
            1,
            reply(
                0x7e8,
                period,
                "410100070100",
                &[(IGNITION, "Spark"), ("Catalyst monitoring", "Ready")],
            ),
        ));
        let mut newest = reply(
            0x7e8,
            period,
            "410100080101",
            &[
                (IGNITION, "Compression"),
                ("NMHC catalyst monitoring", "Not ready"),
            ],
        );
        newest.push(row(Some(0x7ea), period, "Readiness", "Unavailable", ""));
        newest.push(row(None, period, "Readiness", "Unavailable", ""));
        results.apply(&sample(2, newest));

        let reports = ecu_reports(&results, period);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].ignition, Some("Compression"));
        assert_eq!(reports[0].monitors.len(), 1);
        assert_eq!(reports[0].monitors[0].name, "NMHC catalyst monitoring");
        assert!(!reports[0].refresh_failed);
        assert!(reports[1].refresh_failed);
        assert!(reports[1].latest.is_none());
    }
}
