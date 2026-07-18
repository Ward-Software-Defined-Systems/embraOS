use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use tokio::time::{Duration, interval};
use tracing::info;

/// Write pressure levels.
const PRESSURE_NORMAL: u8 = 0;
const PRESSURE_HIGH: u8 = 1;

/// Thresholds for pressure detection (in milliseconds).
const HIGH_LATENCY_THRESHOLD_MS: f64 = 5000.0;
const NORMAL_LATENCY_THRESHOLD_MS: f64 = 500.0;
/// Number of intervals to track for pressure detection.
const PRESSURE_WINDOW: usize = 3;

/// Atomic counters for server-wide metrics, reset each reporting interval.
pub struct Metrics {
    /// Total requests in the current interval
    pub request_count: AtomicU64,
    /// Total requests that returned 2xx
    pub success_count: AtomicU64,
    /// Total requests that returned 4xx
    pub client_error_count: AtomicU64,
    /// Total requests that returned 5xx
    pub server_error_count: AtomicU64,

    /// Cumulative request duration in microseconds (current interval)
    pub total_duration_us: AtomicU64,
    /// Min request duration in microseconds (current interval, u64::MAX = no requests)
    pub min_duration_us: AtomicU64,
    /// Max request duration in microseconds (current interval)
    pub max_duration_us: AtomicU64,

    /// Lifetime counters (never reset)
    pub lifetime_requests: AtomicU64,
    pub lifetime_inserts: AtomicU64,
    pub lifetime_queries: AtomicU64,
    pub lifetime_deletes: AtomicU64,

    /// Server start time
    pub started_at: Instant,

    /// Current write pressure level (0 = normal, 1 = high)
    write_pressure: AtomicU8,
    /// Ring buffer of recent average latencies (in ms) for pressure detection
    latency_history: Mutex<Vec<f64>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Metrics {
            request_count: AtomicU64::new(0),
            success_count: AtomicU64::new(0),
            client_error_count: AtomicU64::new(0),
            server_error_count: AtomicU64::new(0),
            total_duration_us: AtomicU64::new(0),
            min_duration_us: AtomicU64::new(u64::MAX),
            max_duration_us: AtomicU64::new(0),
            lifetime_requests: AtomicU64::new(0),
            lifetime_inserts: AtomicU64::new(0),
            lifetime_queries: AtomicU64::new(0),
            lifetime_deletes: AtomicU64::new(0),
            started_at: Instant::now(),
            write_pressure: AtomicU8::new(PRESSURE_NORMAL),
            latency_history: Mutex::new(Vec::with_capacity(PRESSURE_WINDOW)),
        }
    }

    /// Get the current write pressure level as a string.
    pub fn write_pressure(&self) -> &'static str {
        match self.write_pressure.load(Ordering::Relaxed) {
            PRESSURE_HIGH => "high",
            _ => "normal",
        }
    }

    /// Record a completed request.
    pub fn record_request(&self, status: u16, duration_us: u64) {
        self.request_count.fetch_add(1, Ordering::Relaxed);
        self.lifetime_requests.fetch_add(1, Ordering::Relaxed);
        self.total_duration_us
            .fetch_add(duration_us, Ordering::Relaxed);

        if status < 400 {
            self.success_count.fetch_add(1, Ordering::Relaxed);
        } else if status < 500 {
            self.client_error_count.fetch_add(1, Ordering::Relaxed);
        } else {
            self.server_error_count.fetch_add(1, Ordering::Relaxed);
        }

        // Update min (atomic CAS loop)
        let mut current_min = self.min_duration_us.load(Ordering::Relaxed);
        while duration_us < current_min {
            match self.min_duration_us.compare_exchange_weak(
                current_min,
                duration_us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current_min = actual,
            }
        }

        // Update max (atomic CAS loop)
        let mut current_max = self.max_duration_us.load(Ordering::Relaxed);
        while duration_us > current_max {
            match self.max_duration_us.compare_exchange_weak(
                current_max,
                duration_us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current_max = actual,
            }
        }
    }

    pub fn record_insert(&self) {
        self.lifetime_inserts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_bulk_insert(&self, count: u64) {
        self.lifetime_inserts.fetch_add(count, Ordering::Relaxed);
    }

    pub fn record_query(&self) {
        self.lifetime_queries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_delete(&self) {
        self.lifetime_deletes.fetch_add(1, Ordering::Relaxed);
    }

    /// Take a snapshot, reset interval counters, and update write pressure.
    fn snapshot_and_reset(&self) -> IntervalSnapshot {
        let count = self.request_count.swap(0, Ordering::Relaxed);
        let success = self.success_count.swap(0, Ordering::Relaxed);
        let client_err = self.client_error_count.swap(0, Ordering::Relaxed);
        let server_err = self.server_error_count.swap(0, Ordering::Relaxed);
        let total_us = self.total_duration_us.swap(0, Ordering::Relaxed);
        let min_us = self.min_duration_us.swap(u64::MAX, Ordering::Relaxed);
        let max_us = self.max_duration_us.swap(0, Ordering::Relaxed);

        // Compute average latency for this interval
        let avg_ms = if count > 0 {
            (total_us as f64 / count as f64) / 1000.0
        } else {
            0.0 // Idle interval counts as 0ms (no pressure)
        };

        // Update latency ring buffer and compute pressure
        {
            let mut history = self.latency_history.lock();
            if history.len() >= PRESSURE_WINDOW {
                history.remove(0);
            }
            history.push(avg_ms);

            let any_high = history.iter().any(|&ms| ms > HIGH_LATENCY_THRESHOLD_MS);
            let all_normal = history.iter().all(|&ms| ms < NORMAL_LATENCY_THRESHOLD_MS);

            if any_high {
                self.write_pressure.store(PRESSURE_HIGH, Ordering::Relaxed);
            } else if all_normal && history.len() >= PRESSURE_WINDOW {
                self.write_pressure
                    .store(PRESSURE_NORMAL, Ordering::Relaxed);
            }
            // Otherwise: keep current state (hysteresis)
        }

        IntervalSnapshot {
            count,
            success,
            client_err,
            server_err,
            total_us,
            min_us: if min_us == u64::MAX { 0 } else { min_us },
            max_us,
        }
    }
}

struct IntervalSnapshot {
    count: u64,
    success: u64,
    client_err: u64,
    server_err: u64,
    total_us: u64,
    min_us: u64,
    max_us: u64,
}

/// Spawn a background task that logs a stats banner every `interval_secs` seconds.
pub fn spawn_stats_reporter(metrics: Arc<Metrics>, interval_secs: u64) {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(interval_secs));
        // Skip the first immediate tick
        tick.tick().await;

        loop {
            tick.tick().await;

            let snap = metrics.snapshot_and_reset();
            let uptime = metrics.started_at.elapsed().as_secs();
            let lifetime_reqs = metrics.lifetime_requests.load(Ordering::Relaxed);
            let lifetime_inserts = metrics.lifetime_inserts.load(Ordering::Relaxed);
            let lifetime_queries = metrics.lifetime_queries.load(Ordering::Relaxed);
            let lifetime_deletes = metrics.lifetime_deletes.load(Ordering::Relaxed);

            if snap.count == 0 {
                info!(
                    target: "wardsondb::stats",
                    uptime_s = uptime,
                    total_reqs = lifetime_reqs,
                    total_inserts = lifetime_inserts,
                    total_queries = lifetime_queries,
                    "[stats] idle | uptime={uptime}s total_reqs={lifetime_reqs} inserts={lifetime_inserts} queries={lifetime_queries} deletes={lifetime_deletes}"
                );
            } else {
                let rps = snap.count as f64 / interval_secs as f64;
                let avg_ms = if snap.count > 0 {
                    (snap.total_us as f64 / snap.count as f64) / 1000.0
                } else {
                    0.0
                };
                let min_ms = snap.min_us as f64 / 1000.0;
                let max_ms = snap.max_us as f64 / 1000.0;

                info!(
                    target: "wardsondb::stats",
                    reqs = snap.count,
                    rps = format!("{rps:.1}"),
                    ok = snap.success,
                    err_4xx = snap.client_err,
                    err_5xx = snap.server_err,
                    avg_ms = format!("{avg_ms:.2}"),
                    min_ms = format!("{min_ms:.2}"),
                    max_ms = format!("{max_ms:.2}"),
                    uptime_s = uptime,
                    "[stats] reqs={} rps={:.1} ok={} 4xx={} 5xx={} latency avg={:.2}ms min={:.2}ms max={:.2}ms | uptime={uptime}s total_reqs={lifetime_reqs} inserts={lifetime_inserts} queries={lifetime_queries} deletes={lifetime_deletes}",
                    snap.count, rps, snap.success, snap.client_err, snap.server_err,
                    avg_ms, min_ms, max_ms,
                );
            }
        }
    });
}
