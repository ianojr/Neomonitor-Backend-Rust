pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

pub fn human_rate(bytes_per_sec: u64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec))
}

pub fn human_duration(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

pub fn ratio(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64).clamp(0.0, 1.0)
    }
}

pub fn bar_symbols(pct: f32, width: usize) -> String {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * width as f32).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width);
    for i in 0..width {
        s.push(if i < filled { '\u{2588}' } else { '\u{2591}' });
    }
    s
}

/// Descriptive statistics for a set of timing samples.
///
/// Percentiles use linear interpolation between the two nearest ranks (the same convention
/// as `numpy.percentile`), so a single sample reports that sample for every field and an
/// empty set reports zeros rather than panicking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

pub fn summarize(values: &[f64]) -> Summary {
    if values.is_empty() {
        return Summary {
            n: 0,
            mean: 0.0,
            p50: 0.0,
            p95: 0.0,
            max: 0.0,
        };
    }
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return Summary {
            n: 0,
            mean: 0.0,
            p50: 0.0,
            p95: 0.0,
            max: 0.0,
        };
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    let mean = sorted.iter().sum::<f64>() / n as f64;
    Summary {
        n,
        mean,
        p50: percentile(&sorted, 50.0),
        p95: percentile(&sorted, 95.0),
        max: sorted[n - 1],
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    if n == 1 {
        return sorted[0];
    }
    let rank = (p / 100.0) * (n as f64 - 1.0);
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_bytes(1024u64.pow(5)), "1.0 PB");
        assert_eq!(human_bytes(u64::MAX), "16384.0 PB");
    }

    #[test]
    fn ratio_guards_zero_and_clamps() {
        assert_eq!(ratio(1, 0), 0.0);
        assert_eq!(ratio(0, 100), 0.0);
        assert_eq!(ratio(50, 100), 0.5);
        assert_eq!(ratio(100, 100), 1.0);
        // Over/underflow is clamped into 0..=1.
        assert_eq!(ratio(200, 100), 1.0);
    }

    #[test]
    fn human_duration_formats() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(59), "59s");
        assert_eq!(human_duration(60), "1m 0s");
        assert_eq!(human_duration(3661), "1h 1m 1s");
        assert_eq!(human_duration(90_000), "1d 1h 0m");
    }

    #[test]
    fn bar_symbols_is_bounded() {
        assert_eq!(bar_symbols(0.0, 4).chars().count(), 4);
        assert_eq!(bar_symbols(100.0, 4).chars().count(), 4);
        // Out-of-range and NaN must not panic or overflow the bar.
        assert_eq!(bar_symbols(500.0, 4).chars().count(), 4);
        assert_eq!(bar_symbols(f32::NAN, 4).chars().count(), 4);
        assert!(bar_symbols(0.0, 4).starts_with('\u{2591}'));
        assert!(bar_symbols(100.0, 4).starts_with('\u{2588}'));
    }

    #[test]
    fn summarize_empty_and_single() {
        let empty = summarize(&[]);
        assert_eq!(empty.n, 0);
        assert_eq!(empty.mean, 0.0);
        assert_eq!(empty.max, 0.0);

        let one = summarize(&[4.0]);
        assert_eq!(one.n, 1);
        assert_eq!(one.mean, 4.0);
        assert_eq!(one.p50, 4.0);
        assert_eq!(one.p95, 4.0);
        assert_eq!(one.max, 4.0);
    }

    #[test]
    fn summarize_percentiles_interpolate() {
        let s: Vec<f64> = (1..=100).map(|v| v as f64).collect();
        let sum = summarize(&s);
        assert_eq!(sum.n, 100);
        assert_eq!(sum.max, 100.0);
        // 50th percentile of 1..=100 with linear interpolation is 50.5.
        assert!((sum.p50 - 50.5).abs() < 1e-9, "p50={}", sum.p50);
        // 95th percentile is 95.05.
        assert!((sum.p95 - 95.05).abs() < 1e-9, "p95={}", sum.p95);
        assert!((sum.mean - 50.5).abs() < 1e-9, "mean={}", sum.mean);
    }

    #[test]
    fn summarize_ignores_non_finite() {
        let sum = summarize(&[1.0, f64::NAN, 3.0, f64::INFINITY]);
        assert_eq!(sum.n, 2);
        assert_eq!(sum.max, 3.0);
    }
}
