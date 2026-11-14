use super::graphs::{ReplayAxis, TimeAxis, Window, signal_title};
use super::model::{Capture, GraphPoint, Results, unit_label};
use crate::diagnostics::Reading;
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

pub(super) struct Signal {
    pub(super) title: String,
    series: Vec<Series>,
}

pub(super) struct Series {
    pub(super) unit: String,
    start_ms: u64,
    points: Vec<GraphPoint>,
}

impl Signal {
    pub(super) fn series_at(&self, cursor: u64) -> &Series {
        let index = self
            .series
            .partition_point(|series| series.start_ms <= cursor);
        &self.series[index.saturating_sub(1)]
    }

    fn append(&mut self, unit: &str, point: GraphPoint) {
        let last = self
            .series
            .last_mut()
            .expect("a signal starts with a numeric reading");
        if point.value.is_some() && last.unit != unit {
            self.series.push(Series {
                unit: unit.to_owned(),
                start_ms: point.at_ms,
                points: vec![point],
            });
        } else {
            last.points.push(point);
        }
    }
}

impl Series {
    pub(super) fn points_in(&self, axis: TimeAxis) -> &[GraphPoint] {
        let first = self
            .points
            .partition_point(|point| point.at_ms < axis.start_ms);
        let last = self
            .points
            .partition_point(|point| point.at_ms <= axis.end_ms);
        &self.points[first..last]
    }
}

#[derive(Default)]
pub(super) struct Graphs {
    pub(super) signals: BTreeMap<String, Signal>,
}

pub(super) struct ReplayCapture {
    capture: Arc<Capture>,
    readings: usize,
    // Build the chart index only when graphs are first shown.
    graphs: OnceLock<Graphs>,
}

impl ReplayCapture {
    pub(super) fn new(capture: Arc<Capture>) -> Self {
        let readings = capture.row_count();
        Self {
            capture,
            readings,
            graphs: OnceLock::new(),
        }
    }

    pub(super) fn graphs(&self) -> &Graphs {
        self.graphs.get_or_init(|| Graphs::new(&self.capture))
    }

    pub(super) fn axis(&self, cursor_ms: u64, window: Window) -> ReplayAxis<'_> {
        let started_ms = self.capture.started_ms;
        let axis = TimeAxis::replay(cursor_ms, window, started_ms, self.capture.end_ms());
        let markers = &self.capture.markers;
        let first = markers.partition_point(|marker| marker.at_ms < axis.start_ms);
        let last = markers.partition_point(|marker| marker.at_ms <= axis.end_ms);
        ReplayAxis {
            axis,
            cursor_ms,
            started_ms,
            markers: &markers[first..last],
        }
    }

    pub(super) fn capture(&self) -> Arc<Capture> {
        self.capture.clone()
    }

    pub(super) fn readings(&self) -> usize {
        self.readings
    }
}

impl std::ops::Deref for ReplayCapture {
    type Target = Capture;

    fn deref(&self) -> &Self::Target {
        &self.capture
    }
}

impl Graphs {
    pub(super) fn new(capture: &Capture) -> Self {
        let mut results = Results::default();
        let mut graphs = Self::default();
        let mut append = |key: &str, reading: &Reading, point: GraphPoint| {
            let unit = unit_label(&reading.unit);
            if let Some(signal) = graphs.signals.get_mut(key) {
                signal.append(unit, point);
            } else if point.value.is_some() {
                graphs.signals.insert(
                    key.to_owned(),
                    Signal {
                        title: signal_title(reading),
                        series: vec![Series {
                            unit: unit.to_owned(),
                            start_ms: point.at_ms,
                            points: vec![point],
                        }],
                    },
                );
            }
        };
        for sample in &capture.samples {
            results.apply_with_graphs(sample, Some(&mut append));
        }
        graphs
    }
}
