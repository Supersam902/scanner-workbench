use super::{Event, Link, ReadMode, live_reads, publish_rows, read_one, unavailable};
use crate::adapter::{DiagnosticAdapter, decode_hex};
use crate::diagnostics::{MONITOR_NOT_COMPLETED, Query, Reading, SupportScan, oxygen_monitor};
use crate::elm327::block_on;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

const OXYGEN_MONITOR: &str = "Oxygen sensor monitoring";

#[derive(Clone, Default)]
pub struct Coverage {
    pub(in crate::scanner) sensor_widths: BTreeMap<u8, Option<u8>>,
    pub(in crate::scanner) tests: BTreeMap<u8, BTreeSet<u8>>,
    pub(in crate::scanner) problem: Option<String>,
}

type Sources = BTreeSet<Option<u32>>;

#[derive(Default)]
pub(super) struct Session {
    coverage: Coverage,
    layouts: BTreeMap<u32, (u8, u8)>,
    advertised: BTreeMap<(u8, u8), Sources>,
    observed: BTreeMap<(u8, u8), Sources>,
    ready: BTreeMap<Option<u32>, bool>,
}

fn query(bytes: Vec<u8>, name: impl Into<String>) -> Query {
    Query {
        bytes,
        target: None,
        name: name.into(),
        signals: Vec::new(),
        verified: false,
    }
}

impl Session {
    pub(super) fn run(
        &mut self,
        adapter: &mut Link,
        id: u64,
        mode: ReadMode,
        pids: &SupportScan,
        cancel: &Arc<AtomicBool>,
        tx: &mpsc::Sender<Event>,
    ) -> String {
        if adapter.connection_info().protocol.contains("CAN") {
            return "Use Mode 06 monitor results on a CAN connection.".into();
        }
        let stopped =
            |adapter: &Link| cancel.load(Ordering::Relaxed) || adapter.failure().is_some();
        self.ready.clear();
        let readiness = read_one(
            adapter,
            &query(vec![1, 1], "Readiness since clear"),
            id,
            false,
            cancel,
            tx,
        );
        for row in readiness
            .iter()
            .filter(|row| row.source.is_some() && row.name == OXYGEN_MONITOR)
        {
            let complete = row.value == "Ready";
            if complete || row.value == "Not ready" {
                self.ready
                    .entry(row.source)
                    .and_modify(|ready| *ready &= complete)
                    .or_insert(complete);
            }
        }
        for row in readiness.iter().filter(|row| unavailable(row)) {
            self.ready.remove(&row.source);
        }
        publish_rows(tx, id, readiness);
        if stopped(adapter) {
            return String::new();
        }

        let (tests, sweep) = match mode {
            ReadMode::OxygenSweep => {
                self.discover(adapter, id, pids, cancel, tx);
                if stopped(adapter) {
                    return String::new();
                }
                (self.advertised.keys().copied().collect::<Vec<_>>(), true)
            }
            ReadMode::OxygenTest { sensor, test } if sensor.is_power_of_two() && test % 32 != 0 => {
                if self.layouts.is_empty() {
                    self.discover_locations(adapter, id, pids, cancel, tx);
                }
                (vec![(sensor, test)], false)
            }
            _ => return "Choose a sensor and a test ID.".into(),
        };
        let _ = tx.send(Event::OxygenSupport(id, self.coverage.clone()));
        let total = tests.len();
        let mut issues = 0;
        for (index, (sensor, test)) in tests.into_iter().enumerate() {
            if stopped(adapter) {
                return String::new();
            }
            let request = query(vec![5, test, sensor], oxygen_monitor::test_name(test));
            let _ = tx.send(Event::Progress(id, index, total, request.name.clone()));
            let rows = self.read_request(adapter, id, &request, cancel, tx, true);
            issues += rows.iter().filter(|row| unavailable(row)).count();
            publish_rows(tx, id, rows);
            let _ = tx.send(Event::Progress(id, index + 1, total, String::new()));
        }
        if sweep && !stopped(adapter) {
            let skipped = self
                .observed
                .iter()
                .filter(|(key, _)| !self.advertised.contains_key(key))
                .flat_map(|(&(sensor, test), sources)| {
                    sources.iter().map(move |&source| Reading {
                        source,
                        request: format!("05{test:02X}{sensor:02X}"),
                        name: oxygen_monitor::test_name(test),
                        value: "Unavailable".into(),
                        unit: String::new(),
                        raw: String::new(),
                        numeric: None,
                        status: "Test was not verified by the latest sensor discovery".into(),
                        signal_failure: false,
                    })
                })
                .collect::<Vec<_>>();
            issues += skipped.len();
            publish_rows(tx, id, skipped);
        }
        if total == 0 && self.coverage.problem.is_some() {
            "No oxygen-test IDs could be discovered. Select a sensor and a known test ID to read it directly.".into()
        } else if total == 0 {
            "The vehicle reported no supported oxygen tests.".into()
        } else if issues > 0 || sweep && self.coverage.problem.is_some() {
            format!("Requested {total} oxygen-sensor tests. Review the results and read issues.")
        } else {
            format!("Read {total} oxygen-sensor tests.")
        }
    }

    fn discover_locations(
        &mut self,
        adapter: &mut Link,
        id: u64,
        pids: &SupportScan,
        cancel: &Arc<AtomicBool>,
        tx: &mpsc::Sender<Event>,
    ) {
        let mut responses = Vec::new();
        for (pid, width) in [(0x13, 4), (0x1d, 2)] {
            if cancel.load(Ordering::Relaxed) || adapter.failure().is_some() {
                return;
            }
            if pid <= pids.support.checked_through && !pids.support.reported.contains(&pid) {
                continue;
            }
            let request = query(vec![1, pid], "Oxygen sensor locations");
            let rows = read_one(adapter, &request, id, false, cancel, tx);
            for row in rows.iter().filter(|row| !unavailable(row)) {
                let Some(source) = row.source else { continue };
                if let Ok(raw) = decode_hex(&row.raw) {
                    if let [0x41, echoed, mask] = raw.as_slice() {
                        if *echoed == pid && *mask != 0 {
                            responses.push((source, width, *mask));
                        }
                    }
                }
            }
        }
        self.layouts.clear();
        self.coverage.sensor_widths.clear();
        for (source, width, mask) in responses {
            self.layouts
                .entry(source)
                .and_modify(|layout| {
                    if layout.0 != width || layout.1 != mask {
                        *layout = (0, layout.1 | mask);
                    }
                })
                .or_insert((width, mask));
        }
        for &(width, mask) in self.layouts.values() {
            for sensor in (0..8)
                .map(|bit| 1u8 << bit)
                .filter(|sensor| mask & sensor != 0)
            {
                let width = (width != 0).then_some(width);
                self.coverage
                    .sensor_widths
                    .entry(sensor)
                    .and_modify(|known| {
                        if *known != width {
                            *known = None;
                        }
                    })
                    .or_insert(width);
            }
        }
    }

    fn discover(
        &mut self,
        adapter: &mut Link,
        id: u64,
        pids: &SupportScan,
        cancel: &Arc<AtomicBool>,
        tx: &mpsc::Sender<Event>,
    ) {
        self.coverage = Coverage::default();
        let previous = std::mem::take(&mut self.advertised);
        self.discover_locations(adapter, id, pids, cancel, tx);
        if self.coverage.sensor_widths.is_empty() {
            self.coverage.problem = Some("No oxygen sensor locations were reported. A known sensor mask and test ID can still be read directly.".into());
            return;
        }
        for &sensor in self.coverage.sensor_widths.keys() {
            let mut scan = SupportScan::new(5, sensor);
            scan.expect_sources(
                previous
                    .iter()
                    .filter(|((known, _), _)| *known == sensor)
                    .flat_map(|(_, sources)| sources.iter().copied()),
            );
            while let Some(base) = scan.support.next_range {
                if cancel.load(Ordering::Relaxed) || adapter.failure().is_some() {
                    return;
                }
                let _ = tx.send(Event::Log(
                    id,
                    format!("Request 05{base:02X}{sensor:02X} / Oxygen-test support"),
                ));
                block_on(scan.step(adapter, 2000));
            }
            if let Some(problem) = &scan.support.problem {
                let _ = tx.send(Event::Log(
                    id,
                    format!("Oxygen-test discovery for sensor {sensor:02X}. {problem}"),
                ));
                self.coverage.problem.get_or_insert_with(|| format!("Some sensors did not supply a complete test list. Select a known test ID to read it directly. {problem}"));
            }
            for &test in &scan.support.reported {
                let sources = scan.pid_sources(test).cloned().unwrap_or_default();
                self.advertised.insert((sensor, test), sources);
                self.coverage.tests.entry(sensor).or_default().insert(test);
            }
        }
    }

    pub(super) fn read_request(
        &mut self,
        adapter: &mut Link,
        id: u64,
        query: &Query,
        cancel: &Arc<AtomicBool>,
        tx: &mpsc::Sender<Event>,
        fresh_readiness: bool,
    ) -> Vec<Reading> {
        let mut rows = read_one(adapter, query, id, false, cancel, tx);
        let [5, test, sensor] = query.bytes.as_slice() else {
            return rows;
        };
        if *test % 32 == 0 || !sensor.is_power_of_two() {
            return rows;
        }
        let key = (*sensor, *test);
        let seen = self.observed.entry(key).or_default();
        seen.extend(
            rows.iter()
                .filter(|row| row.source.is_some() && !unavailable(row))
                .map(|row| row.source),
        );
        let mut expected = seen.clone();
        if let Some(advertised) = self.advertised.get(&key) {
            expected.extend(advertised.iter().copied().filter(Option::is_some));
        }
        rows = live_reads::individual_rows(query, &expected, rows);
        let mut complete = BTreeMap::new();
        let mut conflicts = BTreeSet::new();
        for row in rows.iter().filter(|row| !unavailable(row)) {
            if let Some(previous) = complete.insert(row.source, row.raw.as_str()) {
                if previous != row.raw {
                    conflicts.insert(row.source);
                }
            }
        }
        for row in rows
            .iter_mut()
            .filter(|row| conflicts.contains(&row.source))
        {
            row.value = "Unavailable".into();
            row.numeric = None;
            row.status = "ECU sent conflicting oxygen-test results".into();
        }
        for row in rows.iter_mut().filter(|row| !unavailable(row)) {
            let width = row
                .source
                .and_then(|source| self.layouts.get(&source))
                .filter(|(width, mask)| *width != 0 && *mask & *sensor != 0)
                .map(|&(width, _)| width);
            row.name = format!(
                "{} / {}",
                oxygen_monitor::sensor_label(*sensor, width),
                row.name
            );
            if fresh_readiness && row.status == oxygen_monitor::COMPLETION_UNKNOWN {
                match self.ready.get(&row.source) {
                    Some(false) => row.status = MONITOR_NOT_COMPLETED.into(),
                    Some(true) => {
                        if let Ok(raw) = decode_hex(&row.raw) {
                            if let [0x45, _, _, value, min, max] = raw.as_slice() {
                                row.status = if (min..=max).contains(&value) {
                                    "PASS"
                                } else {
                                    "FAIL"
                                }
                                .into();
                            }
                        }
                    }
                    None => {}
                }
            }
        }
        rows
    }
}
