use super::codes::{Category, identifiers};
use super::freeze_frames::{Frames, Links, predates_trigger, trigger_code};
use super::model::{Snapshot, Stamped, ecu_label, html_escape, unit_label};
use super::readiness::{self, EcuReport, MonitorState, Period};
use crate::diagnostics::Reading;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

pub(super) fn is_scan_reading(reading: &Reading) -> bool {
    Category::from_request(&reading.request).is_some()
        || matches!(reading.request.as_str(), "0101" | "0141")
        || super::model::is_monitor_result(reading)
        || reading.freeze_frame_number().is_some()
}

pub(super) fn qualification(snapshot: &Snapshot, row: &Stamped) -> Option<&'static str> {
    if let Some(evidence) = snapshot.evidence_issue(row) {
        Some(match evidence {
            super::model::EvidenceIssue::BeforeClear => "Before clear request · reread needed",
            super::model::EvidenceIssue::RefreshFailed => "Refresh failed · result unverified",
        })
    } else if snapshot.predates_scan(row) {
        Some("Before latest scan")
    } else {
        None
    }
}

fn report_note(html: &mut String, text: &str) {
    let _ = write!(
        html,
        "<small class=\"result-note\">{}</small>",
        html_escape(text)
    );
}

fn verified(snapshot: &Snapshot, row: &Stamped) -> bool {
    !super::model::unavailable(&row.reading) && qualification(snapshot, row).is_none()
}

pub(super) fn append(html: &mut String, snapshot: &Snapshot, describe: &impl Fn(&str) -> String) {
    let captures = Frames::from_rows(snapshot.rows.iter().chain(&snapshot.failures));
    append_codes(html, snapshot, describe, &captures);
    append_readiness(html, snapshot);
    append_frames(html, snapshot, captures);
    append_monitor_results(html, snapshot);
}

fn frame_links(
    snapshot: &Snapshot,
    code_row: &Stamped,
    code: &str,
    captures: &Frames<'_>,
) -> String {
    match captures.links(code_row, code, |row| verified(snapshot, row)) {
        Links::Matching(frames) => {
            let ecu = ecu_label(code_row.reading.source);
            frames
                .iter()
                .map(|frame| {
                    let number = frame.number;
                    let values = if frame.has_values {
                        ""
                    } else {
                        "<br>Trigger only"
                    };
                    format!("<a href=\"#frame-{ecu}-{number}\">Frame {number}</a>{values}")
                })
                .collect::<Vec<_>>()
                .join("<br>")
        }
        Links::Empty(frames) => frames
            .iter()
            .map(|number| format!("Frame {number} empty"))
            .collect::<Vec<_>>()
            .join("<br>"),
        state => state.label().into(),
    }
}

fn append_codes(
    html: &mut String,
    snapshot: &Snapshot,
    describe: &impl Fn(&str) -> String,
    captures: &Frames<'_>,
) {
    html.push_str("<section><h2>Trouble codes</h2><table class=\"codes\"><thead><tr><th>ECU</th><th>Category</th><th>Code</th><th>Description or result</th><th>Freeze frame</th></tr></thead><tbody>");
    for category in Category::ALL {
        let mut read = false;
        for row in snapshot
            .rows
            .iter()
            .filter(|row| row.reading.request == category.request())
        {
            read = true;
            let reading = &row.reading;
            let ecu = ecu_label(reading.source);
            let state = qualification(snapshot, row);
            let codes: BTreeSet<_> = identifiers(&reading.value).collect();
            if codes.is_empty() {
                let _ = write!(
                    html,
                    "<tr><td>{ecu}</td><td>{}</td><td>—</td><td>{}",
                    category.label(),
                    html_escape(&reading.value)
                );
                if !reading.status.is_empty() && reading.status != "ECU responded" {
                    report_note(html, &reading.status);
                }
                if let Some(state) = state {
                    report_note(html, state);
                }
                html.push_str("</td><td>—</td></tr>");
            }
            for code in codes {
                let description = describe(code);
                let _ = write!(
                    html,
                    "<tr><td>{ecu}</td><td>{}</td><th scope=\"row\">{code}</th><td>{}",
                    category.label(),
                    html_escape(&description)
                );
                if let Some(state) = state {
                    report_note(html, state);
                }
                let _ = write!(
                    html,
                    "</td><td>{}</td></tr>",
                    frame_links(snapshot, row, code, captures)
                );
            }
        }
        for failure in snapshot
            .failures
            .iter()
            .filter(|row| row.reading.request == category.request())
        {
            read = true;
            let _ = write!(
                html,
                "<tr><td>{}</td><td>{}</td><td>—</td><td colspan=\"2\">{}",
                ecu_label(failure.reading.source),
                category.label(),
                html_escape(&failure.reading.status)
            );
            if let Some(state) = qualification(snapshot, failure) {
                report_note(html, state);
            }
            html.push_str("</td></tr>");
        }
        if !read {
            let _ = write!(
                html,
                "<tr><td>—</td><td>{}</td><td colspan=\"3\">Not read</td></tr>",
                category.label()
            );
        }
    }
    html.push_str("</tbody></table></section>");
}

fn append_frames(html: &mut String, snapshot: &Snapshot, Frames { by_ecu, discovery }: Frames<'_>) {
    html.push_str("<section><h2>Freeze frame</h2>");
    if by_ecu.is_empty() && discovery.is_empty() {
        html.push_str("<p>Freeze frame has not been read.</p>");
    }
    for (frame, row) in discovery {
        let _ = write!(
            html,
            "<p>Frame {frame} · {}</p>",
            html_escape(&row.reading.status)
        );
    }
    for ((source, frame), capture) in by_ecu {
        let ecu = ecu_label(source);
        let _ = write!(
            html,
            "<h3 id=\"frame-{ecu}-{frame}\">Frame {frame} · ECU {ecu}</h3>"
        );
        if let Some(trigger) = capture.trigger {
            html.push_str("<p>");
            if trigger.reading.no_freeze_frame() {
                html.push_str("ECU reported no stored freeze frame.");
            } else if let Some(code) = trigger_code(&trigger.reading) {
                let _ = write!(html, "<strong>Trigger code {code}</strong>");
                if verified(snapshot, trigger)
                    && !capture.has_values(trigger, |row| verified(snapshot, row))
                {
                    report_note(html, "No verified freeze-frame values captured.");
                }
            } else {
                html.push_str("Trigger code unavailable.");
            }
            if let Some(state) = qualification(snapshot, trigger) {
                report_note(html, state);
            }
            html.push_str("</p>");
        } else {
            html.push_str("<p>Trigger code unavailable.</p>");
        }
        if capture.readings.is_empty() {
            continue;
        }
        html.push_str("<table class=\"frame-values\"><thead><tr><th>Parameter</th><th>Captured value</th></tr></thead><tbody>");
        for row in capture.readings {
            let reading = &row.reading;
            let value = reading
                .value
                .parse::<f64>()
                .ok()
                .filter(|_| reading.numeric.is_some())
                .map_or_else(
                    || reading.value.clone(),
                    |number| {
                        let rounded = format!("{number:.2}");
                        if number != 0.0 && rounded.parse() == Ok(0.0) {
                            reading.value.clone()
                        } else {
                            rounded
                                .trim_end_matches('0')
                                .trim_end_matches('.')
                                .to_owned()
                        }
                    },
                );
            let _ = write!(
                html,
                "<tr><th scope=\"row\">{}</th><td>{} {}",
                html_escape(&reading.name),
                html_escape(&value),
                html_escape(unit_label(&reading.unit))
            );
            if super::model::unavailable(reading) {
                report_note(html, &reading.status);
            }
            if let Some(state) = qualification(snapshot, row) {
                report_note(html, state);
            } else if verified(snapshot, row)
                && predates_trigger(row, capture.trigger.map(|trigger| trigger.at_ms))
            {
                report_note(html, "Before latest trigger read");
            } else if capture
                .trigger
                .is_some_and(|trigger| !verified(snapshot, trigger))
            {
                report_note(html, "Frame trigger unverified");
            }
            html.push_str("</td></tr>");
        }
        html.push_str("</tbody></table>");
    }
    html.push_str("</section>");
}

fn append_readiness(html: &mut String, snapshot: &Snapshot) {
    html.push_str("<section><h2>Emissions readiness</h2>");
    let reports = |period| {
        readiness::reports(
            snapshot.rows.iter(),
            snapshot.failures.iter(),
            period,
            |row| snapshot.predates_clear(row) || snapshot.predates_scan(row),
            |row| snapshot.refresh_failed(row),
        )
        .into_iter()
        .map(|report| (report.source, report))
        .collect::<BTreeMap<_, _>>()
    };
    let since = reports(Period::SinceClear);
    let cycle = reports(Period::DriveCycle);
    let sources: BTreeSet<_> = since.keys().chain(cycle.keys()).copied().collect();
    let mut failures = snapshot
        .failures
        .iter()
        .filter_map(|row| {
            let period = match row.reading.request.as_str() {
                "0101" => Period::SinceClear,
                "0141" => Period::DriveCycle,
                _ => return None,
            };
            Some((period, row))
        })
        .peekable();
    if sources.is_empty() && failures.peek().is_none() {
        html.push_str("<p>Readiness has not been read.</p></section>");
        return;
    }

    for source in sources {
        let since_clear = since.get(&source);
        let drive_cycle = cycle.get(&source);
        let _ = write!(html, "<h3>ECU {}</h3>", ecu_label(source));
        if let Some(report) = since_clear.filter(|report| report.latest.is_some()) {
            let _ = write!(
                html,
                "<p><strong>MIL {}</strong> · Confirmed DTCs {}</p>",
                html_escape(report.mil.unwrap_or("unavailable")),
                html_escape(report.dtc_count.unwrap_or("unavailable"))
            );
        }
        let _ = write!(
            html,
            "<table class=\"readiness\"><thead><tr><th>Monitor</th><th>{}</th><th>{}</th></tr></thead><tbody>",
            Period::SinceClear.label(),
            Period::DriveCycle.label(),
        );
        let status = |report: Option<&EcuReport<'_>>| match report {
            Some(report) => report.latest.map_or_else(
                || "Unavailable".into(),
                |row| {
                    let summary = if report.monitors.is_empty() {
                        "No monitor states reported".into()
                    } else {
                        let mut parts = vec![
                            format!("{} complete", monitor_count(report, MonitorState::Ready)),
                            format!(
                                "{} incomplete",
                                monitor_count(report, MonitorState::NotReady)
                            ),
                        ];
                        for (state, label) in [
                            (MonitorState::Disabled, "disabled"),
                            (MonitorState::DisabledOrUnknown, "disabled or unsupported"),
                            (MonitorState::Unsupported, "not supported"),
                        ] {
                            let count = monitor_count(report, state);
                            if count != 0 {
                                parts.push(format!("{count} {label}"));
                            }
                        }
                        parts.join(" · ")
                    };
                    let mut result = format!("<strong>{summary}</strong>");
                    if let Some(state) = qualification(snapshot, row) {
                        report_note(&mut result, state);
                    }
                    result
                },
            ),
            None => "No result".into(),
        };
        let _ = write!(
            html,
            "<tr class=\"read-status\"><th scope=\"row\">Read result</th><td>{}</td><td>{}</td></tr>",
            status(since_clear),
            status(drive_cycle)
        );

        let mut monitors = BTreeMap::new();
        for monitor in since_clear.into_iter().flat_map(|report| &report.monitors) {
            monitors.entry(monitor.name).or_insert((None, None)).0 = Some(monitor.state);
        }
        for monitor in drive_cycle.into_iter().flat_map(|report| &report.monitors) {
            monitors.entry(monitor.name).or_insert((None, None)).1 = Some(monitor.state);
        }
        for (name, (since, cycle)) in monitors {
            if since == Some(MonitorState::Unsupported)
                && matches!(cycle, None | Some(MonitorState::Unsupported))
            {
                continue;
            }
            let _ = write!(
                html,
                "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td></tr>",
                html_escape(name),
                monitor_status(since),
                monitor_status(cycle)
            );
        }
        html.push_str("</tbody></table>");
    }
    for (period, failure) in failures {
        let _ = write!(
            html,
            "<p>{} · ECU {} · {}",
            period.label(),
            ecu_label(failure.reading.source),
            html_escape(&failure.reading.status)
        );
        if let Some(state) = qualification(snapshot, failure) {
            report_note(html, state);
        }
        html.push_str("</p>");
    }
    html.push_str("<p>The ECU reports completion status. Whether a monitor is actively running is unknown.</p></section>");
}

fn monitor_count(report: &EcuReport<'_>, state: MonitorState) -> usize {
    report
        .monitors
        .iter()
        .filter(|monitor| monitor.state == state)
        .count()
}

fn monitor_status(state: Option<MonitorState>) -> &'static str {
    match state {
        Some(state) => state.completion_label(),
        None => "No result",
    }
}

fn append_monitor_results(html: &mut String, snapshot: &Snapshot) {
    let mut results = snapshot
        .rows
        .iter()
        .chain(&snapshot.failures)
        .filter(|row| super::model::is_monitor_result(&row.reading))
        .peekable();
    if results.peek().is_none() {
        return;
    }
    html.push_str("<section><h2>On-board monitor test results</h2><table><thead><tr><th>ECU / test</th><th>Measured value and limits</th><th>Outcome</th></tr></thead><tbody>");
    for row in results {
        let corrected = super::model::correct_recorded_monitor(&row.reading);
        let reading = corrected.as_ref().unwrap_or(&row.reading);
        let name = if let Some((_, sensor)) =
            crate::diagnostics::oxygen_monitor::request_parts(&reading.request)
        {
            if reading.name.contains(" / ") {
                reading.name.clone()
            } else {
                format!(
                    "{} / {}",
                    crate::diagnostics::oxygen_monitor::sensor_label(sensor, None),
                    reading.name
                )
            }
        } else {
            reading.name.clone()
        };
        let _ = write!(
            html,
            "<tr><th scope=\"row\">{} / {}</th><td>{} {}</td><td>{}",
            ecu_label(reading.source),
            html_escape(&name),
            html_escape(&reading.value),
            html_escape(unit_label(&reading.unit)),
            html_escape(&reading.status)
        );
        if let Some(state) = qualification(snapshot, row) {
            report_note(html, state);
        }
        html.push_str("</td></tr>");
    }
    html.push_str("</tbody></table></section>");
}
