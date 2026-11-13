use super::codes::identifiers;
use super::model::{Results, Stamped, unavailable};
use crate::diagnostics::Reading;
use std::collections::BTreeMap;

pub(super) const DISCOVERY: &str = "Freeze-frame PID discovery";

pub(super) fn flag_conflicting_triggers(rows: &mut [Reading]) {
    let mut values = BTreeMap::new();
    for row in rows.iter().filter(|row| !unavailable(row)) {
        let value = values.entry(row.source).or_insert(Some(row.value.as_str()));
        if *value != Some(row.value.as_str()) {
            *value = None;
        }
    }
    let conflicts: Vec<_> = values
        .into_iter()
        .filter_map(|(source, value)| value.is_none().then_some(source))
        .collect();
    for row in rows {
        if conflicts.contains(&row.source) && !unavailable(row) {
            row.value = "Unavailable".into();
            row.status = "ECU returned conflicting freeze-frame trigger replies".into();
        }
    }
}

#[derive(Default)]
pub(super) struct Frame<'a> {
    pub trigger: Option<&'a Stamped>,
    pub readings: Vec<&'a Stamped>,
}

#[derive(Default)]
pub(super) struct Frames<'a> {
    pub by_ecu: BTreeMap<(Option<u32>, u8), Frame<'a>>,
    pub discovery: Vec<(u8, &'a Stamped)>,
}

impl<'a> Frames<'a> {
    pub(super) fn from_results(results: &'a Results) -> Self {
        Self::from_rows(
            results
                .rows
                .values()
                .map(|value| &value.latest)
                .chain(results.failures.values()),
        )
    }

    pub(super) fn from_rows(rows: impl Iterator<Item = &'a Stamped>) -> Self {
        let mut frames = Self::default();
        for row in rows {
            let reading = &row.reading;
            let Some(number) = reading.freeze_frame_number() else {
                continue;
            };
            if reading.name == DISCOVERY {
                frames.discovery.push((number, row));
                continue;
            }
            let frame = frames.by_ecu.entry((reading.source, number)).or_default();
            if reading.request.starts_with("0202") && !unavailable(reading) {
                if frame
                    .trigger
                    .is_none_or(|trigger| row.at_ms >= trigger.at_ms)
                {
                    frame.trigger = Some(row);
                }
            } else {
                frame.readings.push(row);
            }
        }
        frames
    }

    fn for_ecu(&self, source: Option<u32>) -> impl Iterator<Item = (u8, &Frame<'a>)> {
        self.by_ecu
            .range((source, 0)..=(source, u8::MAX))
            .map(|(&(_, number), frame)| (number, frame))
    }

    pub(super) fn links(
        &self,
        code_row: &Stamped,
        code: &str,
        verified: impl Fn(&Stamped) -> bool,
    ) -> Links {
        if !verified(code_row) {
            return Links::Unverified;
        }
        let source = code_row.reading.source;
        if source.is_none() {
            return Links::UnknownEcu;
        }
        let mut matches = Vec::new();
        let mut other_code = false;
        let mut empty = Vec::new();
        let mut unverified = false;
        for (number, frame) in self.for_ecu(source) {
            let Some(trigger) = frame.trigger.filter(|trigger| verified(trigger)) else {
                unverified = true;
                continue;
            };
            if trigger.reading.no_freeze_frame() {
                empty.push(number);
            } else if let Some(trigger_code) = trigger_code(&trigger.reading) {
                if trigger_code.eq_ignore_ascii_case(code) {
                    matches.push(MatchingFrame {
                        number,
                        has_values: frame.has_values(trigger, &verified),
                    });
                } else {
                    other_code = true;
                }
            } else {
                unverified = true;
            }
        }
        if !matches.is_empty() {
            Links::Matching(matches)
        } else if unverified {
            Links::Unverified
        } else if other_code {
            Links::OtherCode
        } else if !empty.is_empty() {
            Links::Empty(empty)
        } else if self.by_ecu.is_empty() && self.discovery.is_empty() {
            Links::NotRead
        } else if self.by_ecu.keys().all(|(source, _)| source.is_none()) {
            Links::Unavailable
        } else {
            Links::NoEcuResult
        }
    }
}

impl Frame<'_> {
    pub(super) fn has_values(
        &self,
        trigger: &Stamped,
        verified: impl Fn(&Stamped) -> bool,
    ) -> bool {
        self.readings.iter().any(|row| {
            !row.reading.request.starts_with("0202")
                && !predates_trigger(row, Some(trigger.at_ms))
                && verified(row)
        })
    }
}

pub(super) fn predates_trigger(row: &Stamped, trigger_at: Option<u64>) -> bool {
    trigger_at.is_some_and(|at| row.at_ms < at)
}

pub(super) fn trigger_code(reading: &Reading) -> Option<&str> {
    identifiers(&reading.value).find(|code| *code == reading.value)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct MatchingFrame {
    pub number: u8,
    pub has_values: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Links {
    Matching(Vec<MatchingFrame>),
    Empty(Vec<u8>),
    OtherCode,
    UnknownEcu,
    NoEcuResult,
    Unverified,
    NotRead,
    Unavailable,
}

impl Links {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Matching(_) => "Matching frames",
            Self::Empty(_) => "Frame empty",
            Self::OtherCode => "No matching frame",
            Self::UnknownEcu => "ECU unknown",
            Self::NoEcuResult => "No ECU result",
            Self::Unverified => "Unverified",
            Self::NotRead => "Not read",
            Self::Unavailable => "Unavailable",
        }
    }
}
