//! Prometheus text exposition format rendering helpers.
//!
//! These utilities convert metric snapshots into the `text/plain; version=0.0.4`
//! wire format that Prometheus expects.  No external crate required.

use super::types::HistogramSnapshot;
use std::fmt::Write;

// ── Low-level helpers ───────────────────────────────────────────────────────

/// Write `# HELP` and `# TYPE` header lines.
pub fn header(out: &mut String, name: &str, help: &str, metric_type: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {metric_type}");
}

/// Write a single metric line:  `name{labels} value\n`.
pub fn line(out: &mut String, name: &str, labels: &str, value: impl std::fmt::Display) {
    if labels.is_empty() {
        let _ = writeln!(out, "{name} {value}");
    } else {
        let _ = writeln!(out, "{name}{{{labels}}} {value}");
    }
}

// ── High-level renderers ────────────────────────────────────────────────────

/// Render a single counter (no labels).
pub fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    header(out, name, help, "counter");
    line(out, name, "", value);
}

/// Render a single gauge (no labels).
pub fn gauge(out: &mut String, name: &str, help: &str, value: i64) {
    header(out, name, help, "gauge");
    line(out, name, "", value);
}

/// Render a [`CounterVec`](super::types::CounterVec) snapshot.
///
/// `label_names` maps positional pipe-delimited key segments to Prometheus label names.
///
/// # Example
/// ```text
/// label_names = ["method", "route", "status", "source"]
/// key         = "POST|/v1/memories|2xx|mcp"
/// → memoria_http_requests_total{method="POST",route="/v1/memories",status="2xx",source="mcp"} 42
/// ```
pub fn counter_vec(
    out: &mut String,
    name: &str,
    help: &str,
    label_names: &[&str],
    entries: &mut [(String, u64)],
) {
    if entries.is_empty() {
        return;
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    header(out, name, help, "counter");
    for (key, val) in entries.iter() {
        let labels = pipe_to_labels(label_names, key);
        line(out, name, &labels, val);
    }
}

/// Render a [`HistogramVec`](super::types::HistogramVec) snapshot.
pub fn histogram_vec(
    out: &mut String,
    name: &str,
    help: &str,
    label_names: &[&str],
    entries: &mut [(String, HistogramSnapshot)],
) {
    if entries.is_empty() {
        return;
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    header(out, name, help, "histogram");

    let bucket_name = format!("{name}_bucket");
    let sum_name = format!("{name}_sum");
    let count_name = format!("{name}_count");

    for (key, snap) in entries.iter() {
        let base = pipe_to_labels(label_names, key);
        // Finite buckets
        for &(bound, cumulative) in &snap.buckets {
            let le = fmt_le(bound);
            let labels = if base.is_empty() {
                format!("le=\"{le}\"")
            } else {
                format!("{base},le=\"{le}\"")
            };
            line(out, &bucket_name, &labels, cumulative);
        }
        // +Inf bucket
        let inf_labels = if base.is_empty() {
            "le=\"+Inf\"".to_string()
        } else {
            format!("{base},le=\"+Inf\"")
        };
        line(out, &bucket_name, &inf_labels, snap.count);
        line(out, &sum_name, &base, fmt_f64(snap.sum));
        line(out, &count_name, &base, snap.count);
    }
}

// ── Internal helpers ────────────────────────────────────────────────────────

/// Convert a pipe-delimited key to Prometheus label pairs.
fn pipe_to_labels(names: &[&str], key: &str) -> String {
    let parts: Vec<&str> = key.split('|').collect();
    names
        .iter()
        .zip(parts.iter())
        .map(|(n, v)| format!("{n}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(",")
}

/// Format a bucket boundary for the `le` label.
fn fmt_le(v: f64) -> String {
    if v == f64::INFINITY {
        "+Inf".to_string()
    } else {
        fmt_f64(v)
    }
}

/// Format an f64 for Prometheus (strip unnecessary trailing zeros but keep at
/// least one decimal digit for clarity).
fn fmt_f64(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.1}")
    } else {
        // Use default Display which avoids scientific notation for reasonable ranges.
        format!("{v}")
    }
}

// ── Composite renderer ──────────────────────────────────────────────────────

/// Render all process-level metrics from the global registry.
///
/// Called by the `/metrics` endpoint after DB-based metrics are rendered.
pub fn render_process_metrics(out: &mut String) {
    let reg = super::registry();

    out.push('\n');

    // ── HTTP layer ──────────────────────────────────────────────────────
    counter_vec(
        out,
        "memoria_http_requests_total",
        "Total HTTP requests by method, route, status class, and traffic source.",
        &["method", "route", "status", "source"],
        &mut reg.http.requests_total.snapshot(),
    );
    histogram_vec(
        out,
        "memoria_http_request_duration_seconds",
        "HTTP request duration in seconds by route and traffic source.",
        &["route", "source"],
        &mut reg.http.request_duration.snapshot(),
    );
    gauge(
        out,
        "memoria_http_requests_inflight",
        "Number of HTTP requests currently being processed.",
        reg.http.inflight.get(),
    );

    // ── Security ────────────────────────────────────────────────────────
    counter(
        out,
        "memoria_auth_failures_total",
        "Total authentication failures (401/403).",
        reg.security.auth_failures.get(),
    );
    counter(
        out,
        "memoria_sensitivity_blocks_total",
        "Total requests blocked by the content sensitivity filter.",
        reg.security.sensitivity_blocks.get(),
    );

    // ── Worker / async pipeline ─────────────────────────────────────────
    counter(
        out,
        "memoria_entity_extraction_drops_total",
        "Entity extraction jobs dropped because the queue was full.",
        reg.worker.entity_extraction_drops(),
    );

    // ── Embedding sub-system ─────────────────────────────────────────────
    histogram_vec(
        out,
        "memoria_embedding_duration_seconds",
        "Embedding call duration in seconds by provider and operation (single/batch).",
        &["provider", "operation"],
        &mut reg.embedding.duration_seconds.snapshot(),
    );
    counter_vec(
        out,
        "memoria_embedding_errors_total",
        "Embedding calls that returned an error, by provider and operation.",
        &["provider", "operation"],
        &mut reg.embedding.errors_total.snapshot(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_rendering() {
        let mut out = String::new();
        counter(&mut out, "my_counter", "A test counter.", 42);
        assert!(out.contains("# HELP my_counter A test counter."));
        assert!(out.contains("# TYPE my_counter counter"));
        assert!(out.contains("my_counter 42"));
    }

    #[test]
    fn pipe_to_labels_basic() {
        let labels = pipe_to_labels(&["method", "route"], "POST|/v1/memories");
        assert_eq!(labels, "method=\"POST\",route=\"/v1/memories\"");
    }

    #[test]
    fn counter_vec_rendering() {
        let mut entries = vec![
            ("POST|/v1/memories|2xx|mcp".into(), 10),
            ("GET|/v1/memories|2xx|api".into(), 5),
        ];
        let mut out = String::new();
        counter_vec(
            &mut out,
            "test_total",
            "Test.",
            &["method", "route", "status", "source"],
            &mut entries,
        );
        assert!(out.contains("method=\"GET\""));
        assert!(out.contains("source=\"mcp\""));
    }

    #[test]
    fn histogram_vec_rendering() {
        use super::super::types::Histogram;
        static BOUNDS: &[f64] = &[0.1, 1.0];
        let h = Histogram::new(BOUNDS);
        h.observe(0.05);
        h.observe(0.5);
        let mut entries = vec![("test|mcp".into(), h.snapshot())];
        let mut out = String::new();
        histogram_vec(
            &mut out,
            "test_duration_seconds",
            "Test.",
            &["route", "source"],
            &mut entries,
        );
        assert!(out.contains("le=\"0.1\""));
        assert!(out.contains("le=\"+Inf\""));
        assert!(out.contains("test_duration_seconds_count"));
        assert!(out.contains("test_duration_seconds_sum"));
    }
}
