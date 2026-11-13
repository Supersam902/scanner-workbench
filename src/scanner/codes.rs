use crate::diagnostics::{Query, Reading};
use std::collections::{BTreeMap, BTreeSet};

use super::model::{Results, unavailable};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Category {
    Stored,
    Pending,
    Permanent,
}

impl Category {
    pub(super) const ALL: [Self; 3] = [Self::Stored, Self::Pending, Self::Permanent];

    pub(super) fn from_request(request: &str) -> Option<Self> {
        match request {
            "03" => Some(Self::Stored),
            "07" => Some(Self::Pending),
            "0A" => Some(Self::Permanent),
            _ => None,
        }
    }

    pub(super) fn request(self) -> &'static str {
        match self {
            Self::Stored => "03",
            Self::Pending => "07",
            Self::Permanent => "0A",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Stored => "Stored",
            Self::Pending => "Pending",
            Self::Permanent => "Permanent",
        }
    }
}

pub(super) fn identifiers(value: &str) -> impl Iterator<Item = &str> {
    value.split(", ").filter(|code| {
        matches!(code.as_bytes(), [b'P' | b'C' | b'B' | b'U', b'0'..=b'3', a, b, c]
            if a.is_ascii_hexdigit() && b.is_ascii_hexdigit() && c.is_ascii_hexdigit())
    })
}

fn count_identifiers(value: &str) -> usize {
    identifiers(value).collect::<BTreeSet<_>>().len()
}

pub(super) fn summary(results: &Results, category: Category) -> String {
    let request = category.request();
    let (mut current, mut codes, mut unverified) = (0, 0, 0);
    for row in results.rows.values().map(|value| &value.latest) {
        if row.reading.request != request {
            continue;
        }
        if results.predates_clear(row) || results.refresh_failed(row) {
            unverified += 1;
        } else {
            current += 1;
            codes += count_identifiers(&row.reading.value);
        }
    }
    let issues = results
        .failures
        .values()
        .filter(|row| row.reading.request == request)
        .count();
    let mut text = if current > 0 {
        format!(
            "{current} ECU result{} · {codes} code{}",
            if current == 1 { "" } else { "s" },
            if codes == 1 { "" } else { "s" }
        )
    } else if unverified == 0 && issues == 0 {
        "Not read".into()
    } else {
        "No current results".into()
    };
    if unverified > 0 {
        text.push_str(&format!(" · {unverified} unverified"));
    }
    if issues > 0 {
        text.push_str(&format!(
            " · {issues} read issue{}",
            if issues == 1 { "" } else { "s" }
        ));
    }
    text
}

pub(super) fn confirmed_counts(rows: &[Reading]) -> BTreeMap<Option<u32>, (u8, String)> {
    rows.iter()
        .filter(|r| {
            r.request == "0101"
                && r.name == "Number of DTCs stored in this ECU"
                && r.source.is_some()
                && !unavailable(r)
                && !rows.iter().any(|other| {
                    other.source == r.source
                        && (unavailable(other) || other.name == r.name && other.value != r.value)
                })
        })
        .filter_map(|r| Some((r.source, (r.value.parse::<u8>().ok()?, r.raw.clone()))))
        .collect()
}

fn unscoped_timeout(r: &Reading) -> bool {
    r.source.is_none() && unavailable(r) && r.status.starts_with("Timeout:")
}

/// ISO 9141-2 can omit the Mode 03 reply when the ECU's current PID 01 count is zero.
pub(super) fn complete_legacy_stored(
    query: &Query,
    counts: &BTreeMap<Option<u32>, (u8, String)>,
    expected: &BTreeSet<Option<u32>>,
    rows: &mut Vec<Reading>,
) {
    let mut discrepancies = Vec::new();
    for r in rows.iter().filter(|r| !unavailable(r)) {
        if let Some((count, _)) = counts.get(&r.source) {
            let actual = count_identifiers(&r.value);
            if usize::from(*count) != actual {
                let mut issue = r.clone();
                issue.value = "Unavailable".into();
                issue.status =
                    format!("PID 01 count was {count}. Mode 03 returned {actual} codes.");
                discrepancies.push(issue);
            }
        }
    }
    rows.extend(discrepancies);
    if rows
        .iter()
        .any(|r| r.source.is_none() && !unscoped_timeout(r))
    {
        return;
    }
    let mut inferred = false;
    for (&source, (count, raw)) in counts {
        if *count != 0 || rows.iter().any(|r| r.source == source) {
            continue;
        }
        rows.push(Reading {
            source,
            request: "03".into(),
            name: query.name.clone(),
            value: "No codes reported by this ECU".into(),
            unit: String::new(),
            raw: raw.clone(),
            numeric: None,
            status: "PID 01 reported zero confirmed codes. No Mode 03 list was received.".into(),
            signal_failure: false,
        });
        inferred = true;
    }
    if inferred
        && expected
            .iter()
            .all(|source| rows.iter().any(|r| r.source == *source))
    {
        rows.retain(|r| !unscoped_timeout(r));
    }
}
