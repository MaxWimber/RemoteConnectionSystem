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
