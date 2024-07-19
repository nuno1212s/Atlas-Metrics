use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use std::time::{Duration, Instant};
use dashmap::DashMap;
use crate::metrics::{Metric, MetricData};

#[derive(Default)]
pub(super) struct CorrelationTimeTracker {
    pub(super) time_track: DashMap<Arc<str>, Instant>,
    pub(super) accumulated: DashMap<Arc<str>, Duration>,
}


pub(crate) fn start_correlation_time_tracker(
    metric: &Metric,
    id: Arc<str>,
) {
    if let MetricData::CorrelationDurationTracker(tracker) = metric.value().get_metric_data() {
        tracker
            .time_track
            .insert(id, Instant::now());
    } else {
        unreachable!("Metric {:?} is not a CorrelationTime metric", metric)
    }
}

pub(crate) fn end_correlation_time_tracker(
    metric: &Metric,
    id: Arc<str>,
) {
    if let MetricData::CorrelationDurationTracker(tracker) = metric.value().get_metric_data() {
        
        if let Some((correlation, init_time)) = tracker.time_track.remove(&id) {
            tracker.accumulated.insert(correlation, init_time.elapsed());
        }
    } else {
        unreachable!("Metric {:?} is not a CorrelationTime metric", metric)
    }
}

impl Debug for CorrelationTimeTracker {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "CorrelationTimeTracker")
    }
}