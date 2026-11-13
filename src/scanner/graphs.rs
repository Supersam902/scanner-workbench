use super::model::{GraphPoint, ValueState};

pub(super) const MAX_GRAPHS: usize = 6;

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
    pub(super) fn new(value: &ValueState, start: u64, end: u64) -> Self {
        let mut runs = Vec::new();
        let mut run = Vec::new();
        let mut low = f64::INFINITY;
        let mut high = f64::NEG_INFINITY;
        for &point in value
            .history
            .iter()
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
