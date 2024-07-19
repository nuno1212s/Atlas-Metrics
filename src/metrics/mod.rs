use std::cell::Cell;
use std::fmt::{Debug, Formatter};
use std::iter;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::Utc;
use getset::Getters;
use thread_local::ThreadLocal;
use tracing::error;

use atlas_common::globals::Global;

use crate::metrics::correlation_ids::{
    encapsulate_correlation_id, end_correlation_id, pass_correlation_id, register_correlation_id,
    CorrelationTracker,
};
use crate::{MetricLevel, MetricRegistry, MetricRegistryInfo};
use crate::metrics::correlation_time::CorrelationTimeTracker;

mod correlation_ids;
pub mod metrics_thread;
pub(super) mod os_mon;
mod correlation_time;

static mut METRICS: Global<Metrics> = Global::new();

/// Metrics for the general library.
pub struct Metrics {
    live_indexes: Vec<usize>,
    metrics: Vec<Option<Metric>>,
    current_level: MetricLevel,
}

/// A metric statistic to be used. This will be collected every X seconds and sent to InfluxDB
#[derive(Getters)]
#[get = "pub"]
pub struct Metric {
    name: String,
    value: SafeMetricData,
    additional_data: Mutex<AdditionalMetricData>,
    metric_level: MetricLevel,
    metric_type: MetricKind,
}

/// The data for a metric
enum SafeMetricData {
    Sequential(Vec<Mutex<MetricData>>, ThreadLocal<Cell<usize>>),
    ThreadSafe(AtomicPtr<MetricData>),
}

/// Data for a given metric
#[derive(Debug)]
pub(super) enum MetricData {
    Duration(Vec<u64>),
    /// A counter is a metric that is incremented by X every time it is called and in the end is combined
    Counter(AtomicU64),
    /// A vector that stores the counts of things and then averages their values to feed to influx db
    /// This is useful for stuff like the batch size, which we don't want to add together.
    Count(Vec<usize>),

    Correlation(CorrelationTracker),
    
    CorrelationDurationTracker(CorrelationTimeTracker)
}

#[derive(Debug)]
/// Additional data that might be useful for a metric
/// For example, duration metrics need to know when they started for
/// when we are measuring durations that are not in the same scope (without having to
/// pass ugly arguments and such around)
pub enum AdditionalMetricData {
    Duration(Option<Instant>),
    // Counter does not need any additional data storage
    Counter,
    Count,
    Correlation,
}

/// The possible kinds of metrics
#[derive(Debug)]
pub enum MetricKind {
    Duration,
    /// A counter is a metric that is incremented by X every time it is called and in the end is combined
    Counter,
    /// A count is to be used to store various independent counts and then average them together
    Count,

    Correlation,
    
    CorrelationTracker
}

impl Metrics {
    fn new(
        registered_metrics: Vec<MetricRegistry>,
        metric_level: MetricLevel,
        concurrency: usize,
    ) -> Self {
        let mut largest_ind = 0;

        // Calculate the necessary size for the vector
        for info in &registered_metrics {
            largest_ind = std::cmp::max(largest_ind, info.index);
        }

        let mut metrics = iter::repeat_with(|| None)
            .take(largest_ind + 1)
            .collect::<Vec<_>>();

        let mut live_indexes = Vec::with_capacity(registered_metrics.len());

        for metric in registered_metrics {
            let index = metric.index;

            metrics[index] = Some(Metric::new(
                metric.name,
                metric.kind,
                metric.level,
                metric.concurrency_override.unwrap_or(concurrency),
            ));

            live_indexes.push(index);
        }

        Self {
            live_indexes,
            metrics,
            current_level: metric_level,
        }
    }
}

impl MetricKind {
    fn gen_metric_type(&self, concurrency: usize) -> SafeMetricData {
        match self {
            MetricKind::Duration | MetricKind::Count => {
                let buckets = iter::repeat_with(|| Mutex::new(self.gen_metric_type_internal()))
                    .take(concurrency)
                    .collect();

                SafeMetricData::Sequential(buckets, ThreadLocal::new())
            }
            _ => {
                let metric = self.gen_metric_type_internal();

                let metrics_ptr = Box::into_raw(metric.into());

                SafeMetricData::ThreadSafe(AtomicPtr::from(metrics_ptr))
            }
        }
    }

    fn gen_metric_type_internal(&self) -> MetricData {
        match self {
            MetricKind::Duration => MetricData::Duration(Vec::new()),
            MetricKind::Counter => MetricData::Counter(AtomicU64::new(0)),
            MetricKind::Count => MetricData::Count(Vec::new()),
            MetricKind::Correlation => MetricData::Correlation(Default::default()),
            MetricKind::CorrelationTracker => MetricData::CorrelationDurationTracker(Default::default())
        }
    }

    fn gen_additional_data(&self) -> AdditionalMetricData {
        match self {
            MetricKind::Duration => AdditionalMetricData::Duration(None),
            MetricKind::Counter => AdditionalMetricData::Counter,
            MetricKind::Count => AdditionalMetricData::Count,
            MetricKind::Correlation => AdditionalMetricData::Correlation,
            MetricKind::CorrelationTracker => AdditionalMetricData::Correlation
        }
    }
}

impl Metric {
    fn new(name: String, kind: MetricKind, level: MetricLevel, concurrency: usize) -> Self {
        Self {
            name,
            value: kind.gen_metric_type(concurrency),
            additional_data: Mutex::new(kind.gen_additional_data()),
            metric_level: level,
            metric_type: kind,
        }
    }

    fn take_values(&self) -> Vec<MetricData> {
        match &self.value {
            SafeMetricData::Sequential(data_buckets, _) => {
                let mut collected_values = Vec::with_capacity(data_buckets.len());

                data_buckets.iter().for_each(|data| {
                    let metric_type = {
                        let mut value = data.lock().unwrap();

                        let mt = match &*value {
                            MetricData::Duration(vals) => {
                                MetricData::Duration(Vec::with_capacity(vals.len()))
                            }
                            MetricData::Counter(_) => MetricData::Counter(AtomicU64::new(0)),
                            MetricData::Count(vals) => {
                                MetricData::Count(Vec::with_capacity(vals.len()))
                            }
                            MetricData::Correlation(_) => {
                                MetricData::Correlation(Default::default())
                            }
                            MetricData::CorrelationDurationTracker(_) => {
                                MetricData::CorrelationDurationTracker(Default::default())
                            }
                        };
                        
                        let prev_value = std::mem::replace(&mut *value, mt);
                        
                        if let MetricData::CorrelationDurationTracker(tracker) = prev_value {
                            if let MetricData::CorrelationDurationTracker(curr_tracker) = &mut *value {

                                let CorrelationTimeTracker {
                                    time_track,
                                    accumulated
                                } = tracker;

                                curr_tracker.time_track = time_track;

                                MetricData::CorrelationDurationTracker(CorrelationTimeTracker {
                                    time_track: Default::default(),
                                    accumulated
                                })
                            } else {
                                unreachable!("How this metric data be replaced by something else?")
                            }
                        } else {
                            prev_value
                        }
                    };
                    
                    collected_values.push(metric_type);
                });

                collected_values
            }
            SafeMetricData::ThreadSafe(reference) => {
                let previous_value = reference.swap(
                    Box::into_raw(self.metric_type.gen_metric_type_internal().into()),
                    Ordering::Relaxed,
                );

                let previous_value = *unsafe { Box::from_raw(previous_value) };
                
                let previous_value = if let MetricData::CorrelationDurationTracker(tracker) = previous_value {
                    
                    // Pass the time track to the new value as we want to keep the pending ones
                    let CorrelationTimeTracker {
                        time_track,
                        accumulated
                    } = tracker;
                    
                    let new_value = MetricData::CorrelationDurationTracker(CorrelationTimeTracker {
                        time_track,
                        accumulated: Default::default()
                    });

                    destroy_pointer(reference.swap(
                        Box::into_raw(new_value.into()),
                        Ordering::Relaxed,
                    ));
                    
                    MetricData::CorrelationDurationTracker(CorrelationTimeTracker {
                        time_track: Default::default(),
                        accumulated
                    })
                } else {
                    previous_value
                };
                
                vec![previous_value]
            }
        }
    }
}

fn destroy_pointer<T>(ptr: *mut T) {
    if !ptr.is_null() {
        unsafe {
            let _ = Box::from_raw(ptr);
        }
    }
}

impl MetricData {
    fn merge(&mut self, other: Self) {
        match (self, other) {
            (MetricData::Duration(dur), MetricData::Duration(mut dur2)) => {
                dur.append(&mut dur2);
            }
            (MetricData::Counter(c), MetricData::Counter(c2)) => {
                c.fetch_add(c2.load(Ordering::Relaxed), Ordering::Relaxed);
            }
            (MetricData::Count(count), MetricData::Count(mut count2)) => {
                count.append(&mut count2);
            }
            _ => panic!("Can't merge metrics of different types"),
        }
    }
}

impl SafeMetricData {
    fn get_round_robin(&self) -> usize {
        match self {
            SafeMetricData::Sequential(_, round_robin) => {
                let cell = round_robin.get_or(|| Cell::new(0));

                let current_round_robin = cell.get();

                cell.set(current_round_robin + 1);

                current_round_robin
            }
            SafeMetricData::ThreadSafe(_) => 0,
        }
    }

    pub fn locked_metric_data(&self) -> MutexGuard<MetricData> {
        match self {
            SafeMetricData::Sequential(data, _) => {
                let round_robin = self.get_round_robin();

                data[round_robin % data.len()].lock().unwrap()
            }
            SafeMetricData::ThreadSafe(_) => {
                unreachable!("Cannot get a lock on a thread safe metric")
            }
        }
    }

    pub fn get_metric_data(&self) -> &MetricData {
        match self {
            SafeMetricData::ThreadSafe(data) => unsafe { &*data.load(Ordering::Relaxed) },
            SafeMetricData::Sequential(_, _) => {
                unreachable!("Cannot get a reference to a sequential metric")
            }
        }
    }
}

/// Initialize the metrics module
pub(super) fn init(
    registered_metrics: Vec<MetricRegistryInfo>,
    concurrency: usize,
    level: MetricLevel,
) {
    unsafe {
        METRICS.set(Metrics::new(registered_metrics, level, concurrency));
    }
}

/// Enqueue a duration measurement
#[inline]
fn enqueue_duration_measurement(metric: &Metric, duration: u64) {
    let mut values = metric.value().locked_metric_data();

    if let MetricData::Duration(ref mut v) = &mut *values {
        v.push(duration);
    }
}

#[inline]
fn enqueue_counter_measurement(metric: &Metric, count: usize) {
    let mut values = metric.value().locked_metric_data();

    if let MetricData::Count(ref mut v) = *values {
        v.push(count);
    }
}

#[inline]
fn start_duration_measurement(metric: &Metric) {
    let mut metric_guard = metric.additional_data.lock().unwrap();

    match &mut *metric_guard {
        AdditionalMetricData::Duration(start) => {
            start.replace(Instant::now());
        }
        AdditionalMetricData::Counter => {}
        AdditionalMetricData::Count => {}
        AdditionalMetricData::Correlation => {}
    }
}

#[inline]
fn end_duration_measurement(metric: &Metric) {
    let start_instant = {
        let mut metric_guard = metric.additional_data.lock().unwrap();

        match &mut *metric_guard {
            AdditionalMetricData::Duration(start) => start.take(),
            AdditionalMetricData::Counter => None,
            AdditionalMetricData::Count => None,
            AdditionalMetricData::Correlation => None,
        }
    };

    if let Some(start_instant) = start_instant {
        enqueue_duration_measurement(metric, start_instant.elapsed().as_nanos() as u64);
    }
}

/// Enqueue a counter measurement
#[inline]
fn increment_counter_measurement(metric: &Metric, to_add: Option<u64>) {
    let values = metric.value().get_metric_data();

    if let MetricData::Counter(counter) = values {
        counter.fetch_add(to_add.unwrap_or(1), Ordering::Relaxed);
    } else {
        panic!("Metric is not a counter")
    }
}

/// Collect all measurements from a given metric
fn collect_measurements(metric: &Metric) -> Vec<MetricData> {
    metric.take_values()
}

/// Collect all measurements from all metrics
fn collect_all_measurements(level: &MetricLevel) -> Vec<(String, MetricData)> {
    match unsafe { METRICS.get() } {
        Some(metrics) => {
            let mut collected_metrics = Vec::with_capacity(metrics.metrics.len());

            for index in &metrics.live_indexes {
                let metric = metrics.metrics[*index].as_ref().unwrap();

                if metric.metric_level < *level {
                    continue;
                }

                collected_metrics.push((metric.name.clone(), collect_measurements(metric)));
            }

            let mut final_metric_data = Vec::with_capacity(collected_metrics.len());

            for (name, metric_data) in collected_metrics {
                let joined_metrics = join_metrics(metric_data);

                final_metric_data.push((name, joined_metrics));
            }

            final_metric_data
        }
        None => panic!("Metrics were not initialized"),
    }
}

/// Join all metrics into a single metric so we can push them into InfluxDB
fn join_metrics(mut metrics: Vec<MetricData>) -> MetricData {
    if metrics.is_empty() {
        panic!("No metrics to join")
    }

    let mut first = metrics.swap_remove(0);

    while let Some(metric) = metrics.pop() {
        first.merge(metric)
    }

    first
}

fn metric_at_index<'a>(metrics: &&'a Metrics, index: usize) -> Option<&'a Metric> {
    metrics.metrics[index].as_ref()
}

#[inline]
pub fn metric_local_duration_start() -> Instant {
    Instant::now()
}

#[inline]
pub fn metric_local_duration_end(metric: usize, start: Instant) {
    metric_duration(metric, start.elapsed());
}

#[inline]
pub fn metric_duration_start(m_index: usize) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            start_duration_measurement(metric)
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_duration_end(m_index: usize) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            end_duration_measurement(metric)
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_duration(m_index: usize, duration: Duration) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            enqueue_duration_measurement(metric, duration.as_nanos() as u64);
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_increment(m_index: usize, counter: Option<u64>) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            increment_counter_measurement(metric, counter);
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_store_count(m_index: usize, amount: usize) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            enqueue_counter_measurement(metric, amount);
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_initialize_correlation_id(
    m_index: usize,
    correlation_id: impl AsRef<str>,
    location: Arc<str>,
) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            let correlation_id = Arc::from(correlation_id.as_ref());

            register_correlation_id(metric, correlation_id, location);
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_correlation_id_ended(
    m_index: usize,
    correlation_id: impl AsRef<str>,
    location: Arc<str>,
) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            let correlation_id = Arc::from(correlation_id.as_ref());

            end_correlation_id(metric, correlation_id, location);
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_correlation_id_passed(
    m_index: usize,
    correlation_id: impl AsRef<str>,
    location: Arc<str>,
) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            let correlation_id = Arc::from(correlation_id.as_ref());

            pass_correlation_id(metric, correlation_id, location);
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_correlation_id_encapsulated(
    m_index: usize,
    correlation_id: impl AsRef<str>,
    location: Arc<str>,
    (metric_id, encap_corr_id): (usize, impl AsRef<str>),
) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            let correlation_id = Arc::from(correlation_id.as_ref());

            encapsulate_correlation_id(
                metric,
                correlation_id,
                location,
                (metric_id, Arc::from(encap_corr_id.as_ref())),
            );
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_decapsulate_correlation_id(
    m_index: usize,
    correlation_id: impl AsRef<str>,
    location: Arc<str>,
) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            let correlation_id = Arc::from(correlation_id.as_ref());

            correlation_ids::decapsulate_correlation_id(metric, correlation_id, location);
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_correlation_time_start(m_index: usize, correlation_id: impl AsRef<str>) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            let correlation_id = Arc::from(correlation_id.as_ref());

            correlation_time::start_correlation_time_tracker(metric, correlation_id);
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

#[inline]
pub fn metric_correlation_time_end(m_index: usize, correlation_id: impl AsRef<str>) {
    if let Some(ref metrics) = unsafe { METRICS.get() } {
        let metric = metric_at_index(metrics, m_index);

        if let Some(metric) = metric {
            if metric.metric_level < metrics.current_level {
                return;
            }

            let correlation_id = Arc::from(correlation_id.as_ref());

            correlation_time::end_correlation_time_tracker(metric, correlation_id);
        } else {
            error!(
                "Failed to get metric by index {}. It is probably not registered",
                m_index
            );
        }
    }
}

impl Debug for Metric {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metric")
            .field("name", &self.name)
            .field("metric_level", &self.metric_level)
            .finish()
    }
}
