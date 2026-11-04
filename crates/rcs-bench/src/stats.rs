//! Percentiles for reports.

use serde_json::{Value, json};

/// Nearest-rank percentile of sorted values, `p` in 0..=100.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

pub fn summarize(values: &[f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let round = |v: f64| (v * 1000.0).round() / 1000.0;
    json!({
        "count": sorted.len(),
        "min": round(sorted[0]),
        "p50": round(percentile(&sorted, 50.0)),
        "p95": round(percentile(&sorted, 95.0)),
        "p99": round(percentile(&sorted, 99.0)),
        "max": round(sorted[sorted.len() - 1]),
        "mean": round(sorted.iter().sum::<f64>() / sorted.len() as f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank() {
        let values: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&values, 50.0), 50.0);
        assert_eq!(percentile(&values, 95.0), 95.0);
        assert_eq!(percentile(&values, 99.0), 99.0);
        assert_eq!(percentile(&values, 100.0), 100.0);
        assert_eq!(percentile(&[7.0], 99.0), 7.0);
    }

    #[test]
    fn summary_of_unsorted_values() {
        let summary = summarize(&[3.0, 1.0, 2.0]);
        assert_eq!(summary["min"], 1.0);
        assert_eq!(summary["max"], 3.0);
        assert_eq!(summary["p50"], 2.0);
        assert_eq!(summarize(&[]), Value::Null);
    }
}

/// Upper bounds of the histogram buckets in milliseconds; the last bucket takes everything above.
pub const HISTOGRAM_MS: [f64; 15] = [
    0.5, 1.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.7, 20.0, 25.0, 33.4, 50.0, 100.0,
];

pub fn histogram(values: &[f64]) -> Value {
    let mut counts = [0u64; HISTOGRAM_MS.len() + 1];
    for &v in values {
        let bucket = HISTOGRAM_MS
            .iter()
            .position(|&edge| v <= edge)
            .unwrap_or(HISTOGRAM_MS.len());
        counts[bucket] += 1;
    }
    Value::Array(
        counts
            .iter()
            .enumerate()
            .map(|(i, &count)| {
                let le = HISTOGRAM_MS
                    .get(i)
                    .map_or(Value::from("inf"), |&e| Value::from(e));
                json!({ "le_ms": le, "count": count })
            })
            .collect(),
    )
}

#[cfg(test)]
mod histogram_tests {
    use super::*;

    #[test]
    fn buckets_by_upper_bound() {
        let h = histogram(&[0.1, 0.5, 0.6, 9.0, 500.0]);
        assert_eq!(h[0]["count"], 2);
        assert_eq!(h[1]["count"], 1);
        assert_eq!(h[6]["count"], 1);
        assert_eq!(h[HISTOGRAM_MS.len()]["le_ms"], "inf");
        assert_eq!(h[HISTOGRAM_MS.len()]["count"], 1);
    }
}
