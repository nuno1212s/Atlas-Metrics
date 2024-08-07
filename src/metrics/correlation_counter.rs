use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use dashmap::DashMap;
use crate::metrics::{Metric, MetricData};

#[derive(Default)]
pub(crate) struct CorrelationCounterTracker {
    pub(super) counter_track: DashMap<Arc<str>,                                                                                                                                                                                                                                                                                                                                                                                                              AtomicU64>,
}

pub(crate) fn increment_counter(metric: &Metric, correlation: Arc<str>, to_increase: Option<u64>) {
    if let MetricData::CounterCorrelation(tracker) = metric.value().get_metric_data() {
        let entry_ref = if !tracker.counter_track.contains_key(&correlation) {
            tracker.counter_track.entry(correlation.clone()).or_insert_with(|| AtomicU64::new(0));

            tracker.counter_track.get(&correlation).unwrap()
        } else {
            tracker.counter_track.get(&correlation).unwrap()
        };

        entry_ref.value().fetch_add(to_increase.unwrap_or(1), Ordering::Relaxed);
    } else {
        unreachable!("Metric {:?} is not a CounterCorrelation metric", metric)
    }
}

impl Debug for CorrelationCounterTracker {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "CorrelationCounterTracker")
    }
}