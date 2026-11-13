use super::codes::{Category, identifiers};
use super::emissions_scan::State;
use super::model::{
    Snapshot, Stamped, Vehicle, ecu_label, html_escape, unavailable, valid_vin, vehicle_vins,
};
use super::readiness::{self, Period};
use super::recordings::recording_date;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

pub(super) fn same_vehicle(before: &Vehicle, after: &Vehicle) -> bool {
    if valid_vin(&before.vin) && valid_vin(&after.vin) {
        return before.vin.trim().eq_ignore_ascii_case(after.vin.trim());
    }
    let (before_plate, after_plate) = (before.registration.trim(), after.registration.trim());
    !before_plate.is_empty()
        && before_plate.eq_ignore_ascii_case(after_plate)
        && [
            (&before.year, &after.year),
            (&before.make, &after.make),
            (&before.model, &after.model),
        ]
        .into_iter()
        .all(|(a, b)| {
            a.trim().is_empty() || b.trim().is_empty() || a.trim().eq_ignore_ascii_case(b.trim())
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Assessment {
    NewCode,
    NoLongerReported,
    StillReported,
    Changed,
    Unchanged,
    Unverified,
}

impl Assessment {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::NewCode => "Newly reported",
            Self::NoLongerReported => "No longer reported",
            Self::StillReported => "Still reported",
            Self::Changed => "Changed",
            Self::Unchanged => "Unchanged",
            Self::Unverified => "Comparison unavailable",
        }
    }
}

pub(super) struct Change {
    pub(super) source: Option<u32>,
    pub(super) item: String,
    pub(super) before: String,
    pub(super) after: String,
    pub(super) assessment: Assessment,
}

pub(super) struct Section {
    pub(super) title: &'static str,
    pub(super) changes: Vec<Change>,
}

pub(super) struct Comparison {
    pub(super) before_ms: u64,
    pub(super) after_ms: u64,
    pub(super) before_state: State,
    pub(super) after_state: State,
    pub(super) sections: [Section; 3],
}

impl Comparison {
    pub(super) fn new(before: &Snapshot, after: &Snapshot) -> Result<Self, String> {
        let a = before
            .emissions_scan
            .as_ref()
            .ok_or("Choose two saved emissions scans.")?;
        let b = after
            .emissions_scan
            .as_ref()
            .ok_or("Choose two saved emissions scans.")?;
        if a.state == State::Running
            || b.state == State::Running
            || a.started_ms >= b.started_ms
            || before.at_ms > b.started_ms
        {
            return Err("Choose an earlier scan that ended before the later scan began.".into());
        }
        if [before, after].into_iter().any(|snapshot| {
            let (vin, conflict) = vehicle_vins(snapshot.rows.iter());
            conflict
                || vin.is_some_and(|vin| {
                    valid_vin(&snapshot.vehicle.vin)
                        && !vin.eq_ignore_ascii_case(snapshot.vehicle.vin.trim())
                })
        }) {
            return Err("A saved scan contains conflicting VIN information.".into());
        }
        if before.demo != after.demo || !same_vehicle(&before.vehicle, &after.vehicle) {
            return Err(
                "The scans must have a matching VIN or plate and the same data source.".into(),
            );
        }
        Ok(Self {
            before_ms: a.started_ms,
            after_ms: b.started_ms,
            before_state: a.state,
            after_state: b.state,
            sections: [
                Section {
                    title: "Trouble codes",
                    changes: compare_codes(before, after),
                },
                Section {
                    title: "Readiness since clear",
                    changes: compare_readiness(before, after, Period::SinceClear),
                },
                Section {
                    title: "Readiness this drive cycle",
                    changes: compare_readiness(before, after, Period::DriveCycle),
                },
            ],
        })
    }

    pub(super) fn html(&self, vehicle: &Vehicle, demo: bool) -> String {
        let mut html = super::model::printable_header("Emissions scan comparison");
        html.push_str("<h1>Emissions scan comparison</h1>");
        if demo {
            html.push_str("<p>DEMONSTRATION DATA</p>");
        }
        let _ = write!(html, "<h2>{}</h2>", html_escape(&vehicle.label()));
        for (label, value) in [
            ("VIN", &vehicle.vin),
            ("Plate / fleet ID", &vehicle.registration),
        ] {
            if !value.trim().is_empty() {
                let _ = write!(html, "<p>{label} {}</p>", html_escape(value));
            }
        }
        let _ = write!(
            html,
            "<p><strong>Before</strong> {} · {}<br><strong>After</strong> {} · {}</p>",
            recording_date(self.before_ms),
            self.before_state.label(),
            recording_date(self.after_ms),
            self.after_state.label()
        );
        for section in &self.sections {
            let _ = write!(html, "<section><h2>{}</h2>", section.title);
            html.push_str("<table class=\"comparison\"><thead><tr><th>ECU</th><th>Item</th><th>Before</th><th>After</th><th>Assessment</th></tr></thead><tbody>");
            for row in &section.changes {
                let _ = write!(
                    html,
                    "<tr><td>{}</td><th scope=\"row\">{}</th><td>{}</td><td>{}</td><td>{}</td></tr>",
                    ecu_label(row.source),
                    html_escape(&row.item),
                    html_escape(&row.before),
                    html_escape(&row.after),
                    row.assessment.label()
                );
            }
            html.push_str("</tbody></table></section>");
        }
        html.push_str("</body></html>");
        html
    }
}

fn current(snapshot: &Snapshot, row: &Stamped) -> bool {
    !unavailable(&row.reading)
        && !snapshot.predates_clear(row)
        && !snapshot.predates_scan(row)
        && !snapshot.refresh_failed(row)
}

fn unavailable_status(snapshot: &Snapshot, request: &str, source: Option<u32>) -> &'static str {
    let matches = |row: &&Stamped| row.reading.request == request && !snapshot.predates_scan(row);
    if let Some(failure) = snapshot
        .failures
        .iter()
        .filter(matches)
        .filter(|row| row.reading.source == source || row.reading.source.is_none())
        .max_by_key(|row| (row.reading.source == source, row.at_ms))
    {
        return match failure.reading.status.as_str() {
            "ECU reports this code service is unsupported"
            | crate::diagnostics::UNSUPPORTED_REQUEST => "Unsupported",
            text if text.starts_with("Timeout:") || text.starts_with("ECU did not return") => {
                "No reply"
            }
            _ => "Read failed",
        };
    }
    if snapshot
        .rows
        .iter()
        .chain(&snapshot.failures)
        .filter(matches)
        .any(|row| row.reading.source == source)
    {
        "Unverified"
    } else if snapshot
        .rows
        .iter()
        .chain(&snapshot.failures)
        .any(|row| row.reading.request == request && !snapshot.predates_scan(row))
    {
        "No reply"
    } else {
        "Not read"
    }
}

fn sources(before: &Snapshot, after: &Snapshot, request: &str) -> BTreeSet<Option<u32>> {
    let mut sources: BTreeSet<_> = [before, after]
        .into_iter()
        .flat_map(|scan| {
            scan.rows
                .iter()
                .chain(
                    scan.failures
                        .iter()
                        .filter(|row| row.reading.source.is_some()),
                )
                .filter(move |row| row.reading.request == request && !scan.predates_scan(row))
                .map(|row| row.reading.source)
        })
        .collect();
    if sources.is_empty() {
        sources.insert(None);
    }
    sources
}

fn code_list(
    snapshot: &Snapshot,
    category: Category,
    source: Option<u32>,
) -> Option<BTreeSet<&str>> {
    let row = snapshot
        .rows
        .iter()
        .filter(|row| row.reading.source == source && row.reading.request == category.request())
        .max_by_key(|row| row.at_ms)?;
    if !current(snapshot, row) {
        return None;
    }
    let value = row.reading.value.as_str();
    if value == "No codes reported by this ECU" {
        return Some(BTreeSet::new());
    }
    let codes: BTreeSet<_> = identifiers(value).collect();
    (!codes.is_empty() && value.split(", ").all(|code| codes.contains(code))).then_some(codes)
}

fn code_state(
    snapshot: &Snapshot,
    category: Category,
    source: Option<u32>,
    list: Option<&BTreeSet<&str>>,
    code: Option<&str>,
) -> &'static str {
    match (list, code) {
        (Some(codes), Some(code)) if codes.contains(code) => "Reported",
        (Some(_), Some(_)) => "Absent from list",
        (Some(_), None) => "No codes reported",
        (None, _) => unavailable_status(snapshot, category.request(), source),
    }
}

fn compare_codes(before: &Snapshot, after: &Snapshot) -> Vec<Change> {
    let mut changes = Vec::new();
    for category in Category::ALL {
        let request = category.request();
        for source in sources(before, after, request) {
            let old = code_list(before, category, source);
            let new = code_list(after, category, source);
            let empty = BTreeSet::new();
            let old_codes = old.as_ref().unwrap_or(&empty);
            let new_codes = new.as_ref().unwrap_or(&empty);
            let verified = source.is_some() && old.is_some() && new.is_some();
            if old_codes.is_empty() && new_codes.is_empty() {
                changes.push(Change {
                    source,
                    item: format!("{} codes", category.label()),
                    before: code_state(before, category, source, old.as_ref(), None).into(),
                    after: code_state(after, category, source, new.as_ref(), None).into(),
                    assessment: if verified {
                        Assessment::Unchanged
                    } else {
                        Assessment::Unverified
                    },
                });
            }
            for &code in old_codes.union(new_codes) {
                changes.push(Change {
                    source,
                    item: format!("{} {code}", category.label()),
                    before: code_state(before, category, source, old.as_ref(), Some(code)).into(),
                    after: code_state(after, category, source, new.as_ref(), Some(code)).into(),
                    assessment: match (verified, old_codes.contains(code), new_codes.contains(code))
                    {
                        (true, true, true) => Assessment::StillReported,
                        (true, true, false) => Assessment::NoLongerReported,
                        (true, false, true) => Assessment::NewCode,
                        _ => Assessment::Unverified,
                    },
                });
            }
        }
    }
    changes
}

fn readiness_fields(
    snapshot: &Snapshot,
    period: Period,
) -> BTreeMap<Option<u32>, BTreeMap<&str, &str>> {
    readiness::reports(
        snapshot
            .rows
            .iter()
            .filter(|row| !snapshot.predates_scan(row)),
        snapshot
            .failures
            .iter()
            .filter(|row| !snapshot.predates_scan(row)),
        period,
        |row| snapshot.predates_clear(row),
        |row| snapshot.refresh_failed(row),
    )
    .into_iter()
    .filter(|report| report.latest.is_some() && !report.historical && !report.refresh_failed)
    .map(|report| {
        let mut fields: BTreeMap<_, _> = report
            .monitors
            .into_iter()
            .map(|monitor| (monitor.name, monitor.state.completion_label()))
            .collect();
        if let Some(value) = report.mil {
            fields.insert("Check-engine light", value);
        }
        if let Some(value) = report.dtc_count {
            fields.insert("Confirmed code count", value);
        }
        (report.source, fields)
    })
    .collect()
}

fn compare_readiness(before: &Snapshot, after: &Snapshot, period: Period) -> Vec<Change> {
    let old = readiness_fields(before, period);
    let new = readiness_fields(after, period);
    let mut changes = Vec::new();
    let request = period.request();
    for source in sources(before, after, request) {
        let old_fields = old.get(&source);
        let new_fields = new.get(&source);
        let names: BTreeSet<_> = [old_fields, new_fields]
            .into_iter()
            .flatten()
            .flat_map(|fields| fields.keys().copied())
            .collect();
        let old_status = unavailable_status(before, request, source);
        let new_status = unavailable_status(after, request, source);
        if names.is_empty() {
            changes.push(Change {
                source,
                item: "Readiness".into(),
                before: old_status.into(),
                after: new_status.into(),
                assessment: Assessment::Unverified,
            });
        }
        for name in names {
            let a = old_fields.and_then(|fields| fields.get(name)).copied();
            let b = new_fields.and_then(|fields| fields.get(name)).copied();
            changes.push(Change {
                source,
                item: name.into(),
                before: a
                    .unwrap_or(if old_fields.is_some() {
                        "Not reported"
                    } else {
                        old_status
                    })
                    .into(),
                after: b
                    .unwrap_or(if new_fields.is_some() {
                        "Not reported"
                    } else {
                        new_status
                    })
                    .into(),
                assessment: match (source, a, b) {
                    (Some(_), Some(a), Some(b)) if a == b => Assessment::Unchanged,
                    (Some(_), Some(_), Some(_)) => Assessment::Changed,
                    _ => Assessment::Unverified,
                },
            });
        }
    }
    changes
}
