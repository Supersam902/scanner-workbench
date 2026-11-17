use super::model::{GraphPoint, RecordingMarker};
use crate::diagnostics::Reading;

pub(super) fn signal_title(reading: &Reading) -> String {
    match reading.source {
        Some(source) => format!("{} · ECU {source:X}", reading.name),
        None => reading.name.clone(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Window {
    TenSeconds,
    ThirtySeconds,
    Minute,
    TwoMinutes,
}

impl Window {
    pub(super) const ALL: [Self; 4] = [
        Self::TenSeconds,
        Self::ThirtySeconds,
        Self::Minute,
        Self::TwoMinutes,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::TenSeconds => "10 s",
            Self::ThirtySeconds => "30 s",
            Self::Minute => "60 s",
            Self::TwoMinutes => "120 s",
        }
    }

    pub(super) fn milliseconds(self) -> u64 {
        match self {
            Self::TenSeconds => 10_000,
            Self::ThirtySeconds => 30_000,
            Self::Minute => 60_000,
            Self::TwoMinutes => 120_000,
        }
    }
}

pub(super) struct Trace {
    pub(super) runs: Vec<Vec<GraphPoint>>,
    pub(super) range: Option<(f64, f64)>,
}

impl Trace {
    pub(super) fn new(
        points: impl IntoIterator<Item = impl std::borrow::Borrow<GraphPoint>>,
        start: u64,
        end: u64,
    ) -> Self {
        let mut runs = Vec::new();
        let mut run = Vec::new();
        let mut low = f64::INFINITY;
        let mut high = f64::NEG_INFINITY;
        for point in points
            .into_iter()
            .map(|point| *point.borrow())
            .filter(|p| (start..=end).contains(&p.at_ms))
        {
            if let Some(number) = point.value {
                low = low.min(number);
                high = high.max(number);
                run.push(point);
            } else if !run.is_empty() {
                runs.push(std::mem::take(&mut run));
            }
        }
        if !run.is_empty() {
            runs.push(run);
        }
        Self {
            runs,
            range: low.is_finite().then_some((low, high)),
        }
    }

    pub(super) fn point_near(&self, at_ms: u64, tolerance_ms: u64) -> Option<GraphPoint> {
        self.runs
            .iter()
            .flatten()
            .copied()
            .min_by_key(|point| point.at_ms.abs_diff(at_ms))
            .filter(|point| point.at_ms.abs_diff(at_ms) <= tolerance_ms)
    }
}

#[derive(Clone, Copy)]
pub(super) struct TimeAxis {
    pub(super) start_ms: u64,
    pub(super) end_ms: u64,
    span_ms: u64,
}

impl TimeAxis {
    pub(super) fn live(now: u64, window: Window) -> Self {
        let span_ms = window.milliseconds();
        Self {
            start_ms: now.saturating_sub(span_ms),
            end_ms: now,
            span_ms,
        }
    }

    pub(super) fn replay(cursor: u64, window: Window, start: u64, end: u64) -> Self {
        let duration = window.milliseconds();
        let left = cursor.saturating_sub(duration / 2);
        let start_ms = left.clamp(start, end.saturating_sub(duration).max(start));
        let end_ms = start_ms.saturating_add(duration).min(end);
        Self {
            start_ms,
            end_ms,
            span_ms: end_ms.saturating_sub(start_ms).max(1),
        }
    }

    pub(super) fn fraction(self, at_ms: u64) -> f32 {
        1.0 - self.end_ms.saturating_sub(at_ms) as f32 / self.span_ms as f32
    }

    pub(super) fn time_at(self, fraction: f32) -> u64 {
        self.end_ms.saturating_sub(
            ((1.0 - fraction.clamp(0.0, 1.0)) as f64 * self.span_ms as f64).round() as u64,
        )
    }

    pub(super) fn duration(self) -> u64 {
        self.span_ms
    }
}

#[derive(Clone, Copy)]
pub(super) struct ReplayAxis<'a> {
    pub(super) axis: TimeAxis,
    pub(super) cursor_ms: u64,
    pub(super) started_ms: u64,
    pub(super) markers: &'a [RecordingMarker],
}

impl<'a> ReplayAxis<'a> {
    pub(super) fn marker_near(self, at_ms: u64, tolerance_ms: u64) -> Option<&'a RecordingMarker> {
        let next = self.markers.partition_point(|marker| marker.at_ms < at_ms);
        let previous = next.checked_sub(1).and_then(|idx| self.markers.get(idx));
        previous
            .into_iter()
            .chain(self.markers.get(next))
            .min_by_key(|marker| marker.at_ms.abs_diff(at_ms))
            .filter(|marker| marker.at_ms.abs_diff(at_ms) <= tolerance_ms)
    }
}
