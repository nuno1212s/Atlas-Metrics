use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Utc};
use influxdb::{InfluxDbWriteable, WriteQuery};
use tracing::{debug, info};

use crate::metrics::correlation_ids::CorrelationEventOccurrence;
use crate::metrics::{collect_all_measurements, MetricData};
use crate::{InfluxDBArgs, MetricLevel};
use atlas_common::async_runtime as rt;
use atlas_common::maybe_vec::{MaybeVec, MaybeVecBuilder};

#[derive(InfluxDbWriteable)]
pub struct MetricCounterReading {
    time: DateTime<Utc>,
    #[influxdb(tag)]
    host: String,
    #[influxdb(tag)]
    extra: String,
    value: i64,
}

#[derive(InfluxDbWriteable)]
pub struct MetricDurationReading {
    time: DateTime<Utc>,
    #[influxdb(tag)]
    host: String,
    #[influxdb(tag)]
    extra: String,
    value: f64,
    std_dev: f64,
}

#[derive(InfluxDbWriteable)]
pub struct MetricCountReading {
    time: DateTime<Utc>,
    #[influxdb(tag)]
    host: String,
    #[influxdb(tag)]
    extra: String,
    value: f64,
    std_dev: f64,
}

#[derive(InfluxDbWriteable)]
pub struct MetricCorrelationReading {
    time: DateTime<Utc>,
    #[influxdb(tag)]
    host: String,
    #[influxdb(tag)]
    extra: String,
    correlation_id: String,
    location: String,
    event: String,
}

#[derive(InfluxDbWriteable)]
pub struct MetricCorrelationTimeReading {
    time: DateTime<Utc>,
    #[influxdb(tag)]
    host: String,
    #[influxdb(tag)]
    extra: String,
    correlation_id: String,
    time_taken: i64
}

pub fn launch_metrics(influx_args: InfluxDBArgs, metric_level: MetricLevel) {
    std::thread::spawn(move || {
        metric_thread_loop(influx_args, metric_level);
    });
}

/// The metrics thread. Collects all values from the metrics and sends them to influx db
pub fn metric_thread_loop(influx_args: InfluxDBArgs, metric_level: MetricLevel) {
    let InfluxDBArgs {
        ip,
        db_name,
        user,
        password,
        node_id,
        extra,
    } = influx_args;

    info!("Initializing metrics thread with test name: {:?}", extra);

    let mut client = influxdb::Client::new(ip.to_string(), db_name);

    client = client.with_auth(user, password);

    let host_name = format!("{:?}", node_id);

    let extra = extra.unwrap_or(String::from("None"));

    loop {
        let measurements = collect_all_measurements(&metric_level);

        let time = Utc::now();

        let mut readings = Vec::with_capacity(measurements.len());

        for (metric_name, results) in measurements {
            let query = match results {
                MetricData::Duration(dur) => {
                    if dur.is_empty() {
                        continue;
                    }

                    let duration_avg = mean(&dur[..]).unwrap_or(0.0);

                    let dur = std_deviation(&dur[..]).unwrap_or(0.0);

                    MaybeVec::from_one(
                        MetricDurationReading {
                            time,
                            host: host_name.clone(),
                            extra: extra.clone(),
                            value: duration_avg,
                            std_dev: dur,
                        }
                        .into_query(metric_name),
                    )
                }
                MetricData::Counter(count) => {
                    MaybeVec::from_one(
                        MetricCounterReading {
                            time,
                            host: host_name.clone(),
                            extra: extra.clone(),
                            // Could lose some information, but the driver did NOT like u64
                            value: count.load(Ordering::Relaxed) as i64,
                        }
                        .into_query(metric_name),
                    )
                }
                MetricData::Count(counts) => {
                    if counts.is_empty() {
                        continue;
                    }

                    let count_avg = mean_usize(&counts[..]).unwrap_or(0.0);

                    let dur = std_deviation_usize(&counts[..]).unwrap_or(0.0);

                    MaybeVec::from_one(
                        MetricCountReading {
                            time,
                            host: host_name.clone(),
                            extra: extra.clone(),
                            value: count_avg,
                            std_dev: dur,
                        }
                        .into_query(metric_name),
                    )
                }
                MetricData::Correlation(corr_data) => {
                    corr_data
                        .map
                        .iter_mut()
                        .flat_map(|mut item| {
                            let corr_id = item.key().clone();

                            let guard = item.value_mut();

                            let events = std::mem::take(guard);

                            events
                                .into_iter()
                                .map(|event| {
                                    let CorrelationEventOccurrence {
                                        event,
                                        location,
                                        date,
                                    } = event;

                                    MetricCorrelationReading {
                                        time: date,
                                        host: host_name.clone(),
                                        extra: extra.clone(),
                                        correlation_id: corr_id.to_string(),
                                        location: location.to_string(),
                                        event: format!("{:?}", event),
                                    }
                                        .into_query(metric_name.clone())
                                }).collect::<Vec<_>>()
                        }).collect()
                },
                MetricData::CorrelationDurationTracker(tracker) => {
                    tracker.accumulated
                        .iter_mut()
                        .map(|item| {
                        let corr_id = item.key().clone();

                        MetricCorrelationTimeReading {
                            time,
                            host: host_name.clone(),
                            extra: extra.clone(),
                            correlation_id: corr_id.to_string(),
                            time_taken: item.value().as_nanos() as i64
                        }
                        .into_query(metric_name.clone())
                    }).collect()
                }
            };

            match query {
                MaybeVec::None => {}
                MaybeVec::One(query) => readings.push(query),
                MaybeVec::Mult(mut queries) => readings.append(&mut queries),
            }
        }

        let instant = std::time::Instant::now();

        let result =
            rt::block_on(client.query(readings)).expect("Failed to write metrics to influxdb");

        let time_taken = instant.elapsed();
        debug!(
            "Result of writing metrics: {:?} in {:?}",
            result, time_taken
        );

        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn mean(data: &[u64]) -> Option<f64> {
    let sum = data.iter().sum::<u64>() as f64;
    let count = data.len();

    match count {
        positive if positive > 0 => Some(sum / count as f64),
        _ => None,
    }
}

fn std_deviation(data: &[u64]) -> Option<f64> {
    match (mean(data), data.len()) {
        (Some(data_mean), count) if count > 0 => {
            let variance = data
                .iter()
                .map(|value| {
                    let diff = data_mean - (*value as f64);

                    diff * diff
                })
                .sum::<f64>()
                / count as f64;

            Some(variance.sqrt())
        }
        _ => None,
    }
}

fn mean_usize(data: &[usize]) -> Option<f64> {
    let sum = data.iter().sum::<usize>() as f64;
    let count = data.len();

    match count {
        positive if positive > 0 => Some(sum / count as f64),
        _ => None,
    }
}

fn std_deviation_usize(data: &[usize]) -> Option<f64> {
    match (mean_usize(data), data.len()) {
        (Some(data_mean), count) if count > 0 => {
            let variance = data
                .iter()
                .map(|value| {
                    let diff = data_mean - (*value as f64);

                    diff * diff
                })
                .sum::<f64>()
                / count as f64;

            Some(variance.sqrt())
        }
        _ => None,
    }
}
