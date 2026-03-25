//! Lock-free metric primitives for Prometheus-compatible instrumentation.
//!
//! All types are safe to share across threads without external synchronization.
//! Counters and gauges use single atomic operations; histograms use per-bucket
//! atomics with a CAS loop only for the float sum accumulator.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::RwLock;

const ORD: Ordering = Ordering::Relaxed;

/// Atomically add `val` to an [`AtomicU64`] that stores `f64` bits.
fn atomic_f64_add(slot: &AtomicU64, val: f64) {
    let mut old = slot.load(ORD);
    loop {
        let new_val = f64::from_bits(old) + val;
        match slot.compare_exchange_weak(old, new_val.to_bits(), ORD, ORD) {
            Ok(_) => break,
            Err(actual) => old = actual,
        }
    }
}

// ── Counter ─────────────────────────────────────────────────────────────────

/// Monotonically increasing 64-bit counter.
pub struct Counter(AtomicU64);

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

impl Counter {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    #[inline]
    pub fn inc(&self) {
        self.0.fetch_add(1, ORD);
    }

    #[inline]
    pub fn get(&self) -> u64 {
        self.0.load(ORD)
    }
}

// ── Gauge ───────────────────────────────────────────────────────────────────

/// Signed gauge that can increase or decrease.
pub struct Gauge(AtomicI64);

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

impl Gauge {
    pub const fn new() -> Self {
        Self(AtomicI64::new(0))
    }

    #[inline]
    pub fn inc(&self) {
        self.0.fetch_add(1, ORD);
    }

    #[inline]
    pub fn dec(&self) {
        self.0.fetch_sub(1, ORD);
    }

    #[inline]
    pub fn get(&self) -> i64 {
        self.0.load(ORD)
    }
}

// ── Histogram ───────────────────────────────────────────────────────────────

/// Fixed-bucket histogram.  Bucket boundaries are defined at construction time
/// and cannot change.  All operations are lock-free.
pub struct Histogram {
    bounds: &'static [f64],
    /// Per-bucket counts.  Length = `bounds.len() + 1` (last slot is +Inf).
    buckets: Box<[AtomicU64]>,
    /// Running sum stored as `f64::to_bits`.
    sum_bits: AtomicU64,
    /// Total observation count.
    count: AtomicU64,
}

impl Histogram {
    pub fn new(bounds: &'static [f64]) -> Self {
        let n = bounds.len() + 1;
        let buckets: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(0)).collect();
        Self {
            bounds,
            buckets: buckets.into_boxed_slice(),
            sum_bits: AtomicU64::new(0f64.to_bits()),
            count: AtomicU64::new(0),
        }
    }

    /// Record a single observation (seconds).
    pub fn observe(&self, val: f64) {
        let idx = self.bounds.partition_point(|&b| b < val);
        self.buckets[idx].fetch_add(1, ORD);
        atomic_f64_add(&self.sum_bits, val);
        self.count.fetch_add(1, ORD);
    }

    /// Take a point-in-time snapshot for rendering.
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut cumulative = Vec::with_capacity(self.bounds.len());
        let mut running = 0u64;
        for (i, &bound) in self.bounds.iter().enumerate() {
            running += self.buckets[i].load(ORD);
            cumulative.push((bound, running));
        }
        running += self.buckets[self.bounds.len()].load(ORD);
        HistogramSnapshot {
            buckets: cumulative,
            count: running,
            sum: f64::from_bits(self.sum_bits.load(ORD)),
        }
    }
}

/// Immutable snapshot of a [`Histogram`] at a point in time.
pub struct HistogramSnapshot {
    /// `(upper_bound, cumulative_count)` pairs — does **not** include +Inf.
    pub buckets: Vec<(f64, u64)>,
    /// Total observation count (equals the +Inf bucket).
    pub count: u64,
    /// Sum of all observed values.
    pub sum: f64,
}

// ── CounterVec ──────────────────────────────────────────────────────────────

/// Labeled counter family.
///
/// Keys are pipe-delimited label values, e.g. `"POST|/v1/memories|2xx|mcp"`.
/// The read-lock fast path avoids contention once a label combination is seen.
pub struct CounterVec {
    values: RwLock<HashMap<String, AtomicU64>>,
}

impl Default for CounterVec {
    fn default() -> Self {
        Self::new()
    }
}

impl CounterVec {
    pub fn new() -> Self {
        Self {
            values: RwLock::new(HashMap::new()),
        }
    }

    /// Increment the counter for `key` (pipe-delimited label values).
    pub fn inc(&self, key: &str) {
        // Fast path: read lock
        {
            let map = self.values.read().unwrap();
            if let Some(c) = map.get(key) {
                c.fetch_add(1, ORD);
                return;
            }
        }
        // Slow path: write lock, insert if absent
        let mut map = self.values.write().unwrap();
        map.entry(key.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, ORD);
    }

    /// Snapshot all label→value pairs (unsorted).
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let map = self.values.read().unwrap();
        map.iter()
            .map(|(k, v)| (k.clone(), v.load(ORD)))
            .collect()
    }
}

// ── HistogramVec ────────────────────────────────────────────────────────────

/// Labeled histogram family.  Same key convention as [`CounterVec`].
pub struct HistogramVec {
    bounds: &'static [f64],
    values: RwLock<HashMap<String, Histogram>>,
}

impl HistogramVec {
    pub fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            values: RwLock::new(HashMap::new()),
        }
    }

    /// Record an observation for the given label combination.
    pub fn observe(&self, key: &str, val: f64) {
        // Fast path
        {
            let map = self.values.read().unwrap();
            if let Some(h) = map.get(key) {
                h.observe(val);
                return;
            }
        }
        // Slow path
        let mut map = self.values.write().unwrap();
        map.entry(key.to_string())
            .or_insert_with(|| Histogram::new(self.bounds))
            .observe(val);
    }

    /// Snapshot all labeled histograms.
    pub fn snapshot(&self) -> Vec<(String, HistogramSnapshot)> {
        let map = self.values.read().unwrap();
        map.iter()
            .map(|(k, h)| (k.clone(), h.snapshot()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_inc() {
        let c = Counter::new();
        assert_eq!(c.get(), 0);
        c.inc();
        c.inc();
        assert_eq!(c.get(), 2);
    }

    #[test]
    fn gauge_up_down() {
        let g = Gauge::new();
        g.inc();
        g.inc();
        g.dec();
        assert_eq!(g.get(), 1);
    }

    #[test]
    fn histogram_buckets() {
        static BOUNDS: &[f64] = &[0.01, 0.1, 1.0];
        let h = Histogram::new(BOUNDS);
        h.observe(0.005); // bucket 0 (le=0.01)
        h.observe(0.05);  // bucket 1 (le=0.1)
        h.observe(0.5);   // bucket 2 (le=1.0)
        h.observe(5.0);   // bucket 3 (+Inf)
        let snap = h.snapshot();
        assert_eq!(snap.count, 4);
        assert_eq!(snap.buckets, vec![(0.01, 1), (0.1, 2), (1.0, 3)]);
        assert!((snap.sum - 5.555).abs() < 0.001);
    }

    #[test]
    fn counter_vec_labeled() {
        let cv = CounterVec::new();
        cv.inc("a|b");
        cv.inc("a|b");
        cv.inc("c|d");
        let mut snap = cv.snapshot();
        snap.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(snap, vec![("a|b".into(), 2), ("c|d".into(), 1)]);
    }
}
