//! Platform-independent metric calculations.
//!
//! Everything here is a pure function of its inputs so it can be unit-tested with
//! synthetic values and reused by any acquisition backend. OS-specific collection lives
//! in `sampler` (sysinfo) and `gpu` (PDH/WMI); it should call into here for the maths.

use std::collections::HashMap;

/// Clamp an arbitrary metric value to the inclusive `0..=100` percent range.
///
/// Non-finite inputs (NaN, ±∞) are treated as zero so they can never leak into the UI.
pub fn normalize_percent(value: f64) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 100.0) as f32
}

/// Convert a sysinfo process CPU value *relative to one logical CPU* into a percentage of
/// the whole machine.
///
/// sysinfo's Windows backend returns `100 * process_delta / system_delta * ncpus`, i.e. it
/// can reach `ncpus * 100`. Dividing by the logical CPU count yields the Task-Manager-style
/// "percent of total CPU" and keeps the value in `0..=100`.
///
/// `logical_cpus == 0` or a non-finite `raw` yields `0.0`.
pub fn process_cpu_percent(raw: f32, logical_cpus: usize) -> f32 {
    if !raw.is_finite() || logical_cpus == 0 {
        return 0.0;
    }
    (raw / logical_cpus as f32).clamp(0.0, 100.0)
}

/// Instantaneous throughput from two cumulative counters and a monotonic elapsed time.
///
/// Returns bytes/second. A counter going backwards (reset/rollover) yields `0` rather than
/// underflowing, and a non-positive or non-finite `dt_secs` yields `0`.
pub fn counter_rate(now: u64, prev: u64, dt_secs: f64) -> u64 {
    if !dt_secs.is_finite() || dt_secs <= 0.0 {
        return 0;
    }
    let delta = now.saturating_sub(prev) as f64;
    (delta / dt_secs).round() as u64
}

/// Whether a network interface is a pseudo/filter interface that mirrors another one.
///
/// Windows exposes NDIS lightweight-filter and switch-extension adapters through the same
/// MIB table as real NICs; they carry no MAC and no IP of their own but often report the
/// same octet counters, which double-counts throughput when summed naively. A real hardware
/// interface always has a MAC, even when it currently has no IP, so an interface with
/// neither is treated as pseudo.
pub fn is_pseudo_interface(mac_unspecified: bool, has_ip: bool) -> bool {
    mac_unspecified && !has_ip
}

/// Stable grouping key for one GPU engine instance.
///
/// PDH/WMI instance names look like
/// `pid_12176_luid_0x00000000_0x00013B09_phys_0_eng_0_engtype_3D`. The `pid_` part is
/// intentionally dropped because utilisation is summed across processes; `phys` and `eng`
/// are *kept* because they identify a distinct physical engine. Merging by `engtype` alone
/// (the previous behaviour) collapsed nine `3D` engines on one adapter into a single summed
/// bucket, inflating the reported utilisation.
///
/// If the name does not match the expected shape it is returned unchanged, so unknown
/// formats never collide with each other.
pub fn gpu_engine_key(name: &str) -> String {
    let luid = name
        .split("luid_")
        .nth(1)
        .and_then(|rest| rest.split("_phys_").next())
        .unwrap_or(name);
    let phys = segment(name, "_phys_");
    let eng = segment(name, "_eng_");
    let engtype = name.split("engtype_").nth(1).unwrap_or("");

    match (phys, eng) {
        (Some(phys), Some(eng)) => format!("{luid}|phys_{phys}_eng_{eng}|{engtype}"),
        _ => name.to_string(),
    }
}

/// Return the token following `key` up to the next `_`.
fn segment(name: &str, key: &str) -> Option<String> {
    name.split(key)
        .nth(1)
        .map(|rest| rest.split('_').next().unwrap_or("").to_string())
}

/// Aggregate raw GPU engine utilisation readings into a single `0..=100` value.
///
/// Values are grouped by [`gpu_engine_key`], summed within a group (across processes), and
/// the busiest engine group is returned. The result is clamped to `100` because a single
/// engine's summed utilisation can occasionally exceed 100. Non-finite values are ignored.
/// Returns `None` when there is no usable data.
pub fn gpu_engine_utilization(items: &[(String, f64)]) -> Option<f32> {
    gpu_engine_utilization_by_adapter(items)
        .into_values()
        .reduce(f32::max)
}

/// Select the memory footprint of the single busiest GPU adapter.
///
/// PDH reports memory per adapter instance (e.g. `luid_0x00000000_0x00013B09_phys_0`).
/// Summing every instance would merge unrelated adapters and double-count a GPU that
/// exposes multiple LUIDs, so the adapter with the largest committed footprint
/// (`dedicated + shared`) is chosen, mirroring the WMI fallback. Non-finite/negative
/// samples are ignored. Returns `None` when nothing usable is present.
pub fn adapter_memory(dedicated: &[(String, f64)], shared: &[(String, f64)]) -> Option<(u64, u64)> {
    let mut map: HashMap<&str, (f64, f64)> = HashMap::new();
    for (name, value) in dedicated {
        if value.is_finite() && *value >= 0.0 {
            map.entry(name.as_str()).or_insert((0.0, 0.0)).0 += *value;
        }
    }
    for (name, value) in shared {
        if value.is_finite() && *value >= 0.0 {
            map.entry(name.as_str()).or_insert((0.0, 0.0)).1 += *value;
        }
    }
    map.into_values()
        .max_by(|a, b| (a.0 + a.1).total_cmp(&(b.0 + b.1)))
        .map(|(ded, shared)| (ded.max(0.0) as u64, shared.max(0.0) as u64))
}

/// Canonicalise a GPU adapter LUID segment to a stable, comparable key.
///
/// Windows identifies GPU adapters by LUID. PDH instance names embed it as
/// `luid_0x<high>_0x<low>` (lower-case hex, e.g. `luid_0x00000000_0x00013b09`), while DXGI
/// reports it as a [`windows::Win32::Foundation::LUID`] of `(high, low)` u32 halves. Because
/// the two sources may differ in case (and the halves could in principle be ordered
/// differently), we canonicalise to upper-case `0x<high>_0x<low>` so equal adapters compare
/// equal regardless of source formatting.
///
/// Accepts either a full instance name (anything before `luid_` is ignored, anything from
/// `_phys_` onward is ignored) or a bare segment. Returns `None` when no hex halves survive.
pub fn normalize_luid(segment: &str) -> Option<String> {
    let part = segment.split("luid_").nth(1).unwrap_or(segment);
    let body = part.split("_phys_").next().unwrap_or(part);
    let mut halves = Vec::new();
    for token in body.split('_') {
        let token = token
            .strip_prefix("0x")
            .or_else(|| token.strip_prefix("0X"))
            .unwrap_or(token);
        let value = u32::from_str_radix(token, 16).ok()?;
        halves.push(value);
    }
    match halves.as_slice() {
        [high, low] => Some(format!("0x{high:08X}_0x{low:08X}")),
        [only] => Some(format!("0x{only:08X}")),
        _ => None,
    }
}

/// The adapter a PDH GPU instance name belongs to, if it names one.
pub fn instance_luid(name: &str) -> Option<String> {
    normalize_luid(name)
}

/// Both canonical orderings of a DXGI `LUID` half-pair, upper-cased.
///
/// We accept either ordering when matching a DXGI adapter against a PDH instance name so the
/// association does not depend on an undocumented high/low convention. A genuinely
/// ambiguous collision is impossible in practice because the two halves are `(high, low)`.
pub fn luid_candidates(high: u32, low: u32) -> [String; 2] {
    [
        format!("0x{high:08X}_0x{low:08X}"),
        format!("0x{low:08X}_0x{high:08X}"),
    ]
}

/// Aggregate raw GPU engine utilisation into one `0..=100` value **per adapter**.
///
/// Values are grouped first by adapter (LUID) and by [`gpu_engine_key`] within it, summed
/// across processes, and the busiest engine group of each adapter is reported. This keeps
/// two adapters from being merged into one number (the previous single-value helper picked
/// the global maximum, so a busy integrated GPU could mask a discrete one or vice-versa).
/// Non-finite or negative samples are ignored. The returned map is keyed by
/// [`normalize_luid`]-style adapter keys.
pub fn gpu_engine_utilization_by_adapter(items: &[(String, f64)]) -> HashMap<String, f32> {
    let mut engines: HashMap<(String, String), f64> = HashMap::new();
    for (name, value) in items {
        if !value.is_finite() || *value < 0.0 {
            continue;
        }
        let luid = instance_luid(name).unwrap_or_else(|| name.clone());
        let engine = gpu_engine_key(name);
        *engines.entry((luid, engine)).or_insert(0.0) += *value;
    }
    let mut per_adapter: HashMap<String, f64> = HashMap::new();
    for ((luid, _engine), value) in engines {
        per_adapter
            .entry(luid)
            .and_modify(|max| *max = max.max(value))
            .or_insert(value);
    }
    per_adapter
        .into_iter()
        .map(|(luid, value)| (luid, normalize_percent(value)))
        .collect()
}

/// Per-adapter GPU memory usage, summed within each LUID but never across adapters.
///
/// PDH reports memory per adapter instance (`luid_..._phys_...`). A single physical adapter
/// can expose more than one `phys` entry (linked display adapters); those are summed, while
/// distinct LUIDs stay separate. Returns `(dedicated, shared)` bytes per adapter key.
pub fn gpu_memory_by_adapter(
    dedicated: &[(String, f64)],
    shared: &[(String, f64)],
) -> HashMap<String, (u64, u64)> {
    let mut map: HashMap<String, (f64, f64)> = HashMap::new();
    let add = |map: &mut HashMap<String, (f64, f64)>, name: &str, value: f64, slot: usize| {
        if value.is_finite() && value >= 0.0 {
            let key = instance_luid(name).unwrap_or_else(|| name.to_string());
            let entry = map.entry(key).or_insert((0.0, 0.0));
            if slot == 0 {
                entry.0 += value;
            } else {
                entry.1 += value;
            }
        }
    };
    for (name, value) in dedicated {
        add(&mut map, name, *value, 0);
    }
    for (name, value) in shared {
        add(&mut map, name, *value, 1);
    }
    map.into_iter()
        .map(|(luid, (ded, shared))| (luid, (ded.max(0.0) as u64, shared.max(0.0) as u64)))
        .collect()
}

/// Pick the hottest GPU temperature from a list of `(name, sensor_type, value)` sensors.
///
/// This is fed by the optional LibreHardwareMonitor WMI provider, whose temperature sensors
/// are reported as `SensorType = "Temperature"` and Celsius values, with names such as
/// `GPU Core` or `GPU Hot Spot`. We keep only temperature sensors whose name mentions the
/// GPU (and not the CPU), ignore non-finite values, and return the maximum. `None` when no
/// matching sensor produced a usable value.
pub fn lhm_gpu_temperature(sensors: &[(String, String, f32)]) -> Option<f32> {
    sensors
        .iter()
        .filter(|(name, kind, _)| {
            kind.eq_ignore_ascii_case("temperature") && {
                let n = name.to_ascii_lowercase();
                n.contains("gpu") && !n.contains("cpu")
            }
        })
        .map(|(_, _, value)| *value)
        .filter(|value| value.is_finite())
        .reduce(f32::max)
}

/// Summary of a history series: the latest value plus the window average and peak.
///
/// The UI uses this to distinguish the *current* reading from the average and peak of the
/// visible window, rather than implying a sparkline's right edge is the whole story.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeriesStat {
    pub current: u64,
    pub avg: u64,
    pub peak: u64,
}

/// Reduce a contiguous series to [`SeriesStat`]. Empty input yields all zeros.
///
/// The average is the plain arithmetic mean of the window (rounded down); this is a
/// *window* average, not a time-weighted or exponentially-smoothed value.
pub fn series_stat(values: &[u64]) -> SeriesStat {
    if values.is_empty() {
        return SeriesStat::default();
    }
    let sum: u128 = values.iter().map(|v| *v as u128).sum();
    let avg = (sum / values.len() as u128).min(u64::MAX as u128) as u64;
    let peak = values.iter().copied().max().unwrap_or(0);
    SeriesStat {
        current: values[values.len() - 1],
        avg,
        peak,
    }
}

/// Five x-axis labels spanning `span_secs` back from "now", oldest first.
pub fn span_axis_labels(span_secs: u64) -> Vec<String> {
    let s = span_secs.max(1);
    vec![
        format!("-{s}s"),
        format!("-{}s", s * 3 / 4),
        format!("-{}s", s / 2),
        format!("-{}s", s / 4),
        "now".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_percent_clamps_and_sanitizes() {
        assert_eq!(normalize_percent(0.0), 0.0);
        assert_eq!(normalize_percent(42.5), 42.5);
        assert_eq!(normalize_percent(100.0), 100.0);
        assert_eq!(normalize_percent(150.0), 100.0);
        assert_eq!(normalize_percent(-5.0), 0.0);
        assert_eq!(normalize_percent(f64::NAN), 0.0);
        assert_eq!(normalize_percent(f64::INFINITY), 0.0);
        assert_eq!(normalize_percent(f64::NEG_INFINITY), 0.0);
    }

    #[test]
    fn process_cpu_is_machine_relative() {
        // 8-thread machine: a process using four cores (~400 raw) is 50% of the machine.
        assert_eq!(process_cpu_percent(400.0, 8), 50.0);
        assert_eq!(process_cpu_percent(100.0, 8), 12.5);
        assert_eq!(process_cpu_percent(0.0, 8), 0.0);
        // Over-subscribed value is clamped.
        assert_eq!(process_cpu_percent(900.0, 8), 100.0);
        // Degenerate inputs.
        assert_eq!(process_cpu_percent(50.0, 0), 0.0);
        assert_eq!(process_cpu_percent(f32::NAN, 8), 0.0);
        // Non-finite input is treated as invalid, not as saturation.
        assert_eq!(process_cpu_percent(f32::INFINITY, 8), 0.0);
    }

    #[test]
    fn counter_rate_basic_and_edges() {
        assert_eq!(counter_rate(1000, 0, 1.0), 1000);
        assert_eq!(counter_rate(1000, 500, 0.5), 1000);
        assert_eq!(counter_rate(0, 0, 1.0), 0);
        // Counter reset / rollover -> no negative, no panic.
        assert_eq!(counter_rate(10, 1000, 1.0), 0);
        // Invalid elapsed.
        assert_eq!(counter_rate(1000, 0, 0.0), 0);
        assert_eq!(counter_rate(1000, 0, -1.0), 0);
        assert_eq!(counter_rate(1000, 0, f64::NAN), 0);
        // First sample (prev == now) yields zero.
        assert_eq!(counter_rate(42, 42, 2.0), 0);
    }

    #[test]
    fn pseudo_interface_detection() {
        assert!(is_pseudo_interface(true, false));
        assert!(!is_pseudo_interface(false, false));
        assert!(!is_pseudo_interface(true, true));
        assert!(!is_pseudo_interface(false, true));
    }

    #[test]
    fn engine_key_keeps_engine_identity() {
        let a = "pid_12176_luid_0x00000000_0x00013EC2_phys_0_eng_3_engtype_3D";
        let b = "pid_400_luid_0x00000000_0x00013EC2_phys_0_eng_4_engtype_3D";
        // Same adapter + engine type but different engines must NOT collide.
        assert_ne!(gpu_engine_key(a), gpu_engine_key(b));
        assert_eq!(gpu_engine_key(a), "0x00000000_0x00013EC2|phys_0_eng_3|3D");
        // Same engine across two processes must collide (summed).
        let a2 = "pid_999_luid_0x00000000_0x00013EC2_phys_0_eng_3_engtype_3D";
        assert_eq!(gpu_engine_key(a), gpu_engine_key(a2));
        // Empty engine type is preserved distinctly.
        let blank = "pid_4_luid_0x00000000_0x00013B09_phys_0_eng_5_engtype_";
        assert_eq!(gpu_engine_key(blank), "0x00000000_0x00013B09|phys_0_eng_5|");
        // Unparseable names fall back to the raw name and never collide.
        assert_eq!(gpu_engine_key("weird"), "weird");
    }

    #[test]
    fn engine_utilization_does_not_sum_distinct_engines() {
        // Nine 3D engines at 10% each must report 10%, not 90%.
        let items: Vec<(String, f64)> = (0..9)
            .map(|e| {
                (
                    format!("pid_1_luid_0x0_0x13EC2_phys_0_eng_{e}_engtype_3D"),
                    10.0,
                )
            })
            .collect();
        assert_eq!(gpu_engine_utilization(&items), Some(10.0));
    }

    #[test]
    fn engine_utilization_sums_within_an_engine() {
        // The same engine (same phys/eng/type) from three processes sums then caps at 100.
        let items = vec![
            (
                "pid_1_luid_0x0_0xABC_phys_0_eng_0_engtype_3D".to_string(),
                40.0,
            ),
            (
                "pid_2_luid_0x0_0xABC_phys_0_eng_0_engtype_3D".to_string(),
                40.0,
            ),
            (
                "pid_3_luid_0x0_0xABC_phys_0_eng_0_engtype_3D".to_string(),
                40.0,
            ),
        ];
        assert_eq!(gpu_engine_utilization(&items), Some(100.0));
    }

    #[test]
    fn engine_utilization_handles_empty_and_invalid() {
        assert_eq!(gpu_engine_utilization(&[]), None);
        let items = vec![("a".to_string(), f64::NAN), ("b".to_string(), -1.0)];
        assert_eq!(gpu_engine_utilization(&items), None);
        // A single adapter's busiest engine wins.
        let items = vec![
            (
                "pid_1_luid_0x0_0xAAA_phys_0_eng_0_engtype_3D".to_string(),
                5.0,
            ),
            (
                "pid_1_luid_0x0_0xBBB_phys_0_eng_0_engtype_3D".to_string(),
                70.0,
            ),
        ];
        assert_eq!(gpu_engine_utilization(&items), Some(70.0));
    }

    #[test]
    fn adapter_memory_selects_busiest_and_ignores_invalid() {
        let dedicated = vec![
            ("a".to_string(), 100.0),
            ("b".to_string(), 500.0),
            ("c".to_string(), f64::NAN),
            ("d".to_string(), -1.0),
        ];
        let shared = vec![("a".to_string(), 50.0), ("b".to_string(), 0.0)];
        // b has the largest committed footprint (500 + 0).
        assert_eq!(adapter_memory(&dedicated, &shared), Some((500, 0)));
        // Only invalid samples -> no usable adapter.
        assert_eq!(adapter_memory(&[("x".to_string(), f64::NAN)], &[]), None);
        assert_eq!(adapter_memory(&[], &[]), None);
    }

    #[test]
    fn luid_normalisation_is_source_independent() {
        // PDH instance name (lower-case, with pid/phys suffixes) -> canonical upper-case.
        assert_eq!(
            instance_luid("pid_1_luid_0x00000000_0x00013b09_phys_0_eng_0_engtype_3D").as_deref(),
            Some("0x00000000_0x00013B09")
        );
        // Bare adapter-memory instance.
        assert_eq!(
            instance_luid("luid_0x00000000_0x00013EC2_phys_0").as_deref(),
            Some("0x00000000_0x00013EC2")
        );
        // A DXGI half-pair formats to the same key regardless of input case.
        assert_eq!(
            normalize_luid("luid_0x00000000_0x00013b09").unwrap(),
            normalize_luid("0x00000000_0x00013B09").unwrap()
        );
        // Names with no LUID are not misparsed.
        assert_eq!(instance_luid("weird"), None);
    }

    #[test]
    fn luid_candidates_cover_both_orderings() {
        let [a, b] = luid_candidates(0, 0x13EC2);
        assert_eq!(a, "0x00000000_0x00013EC2");
        assert_eq!(b, "0x00013EC2_0x00000000");
    }

    #[test]
    fn engine_utilization_is_per_adapter() {
        let items = vec![
            // Adapter A: one 3D engine at 20% from two processes -> 40% (summed).
            (
                "pid_1_luid_0x0_0xAAAA_phys_0_eng_0_engtype_3D".to_string(),
                20.0,
            ),
            (
                "pid_2_luid_0x0_0xAAAA_phys_0_eng_0_engtype_3D".to_string(),
                20.0,
            ),
            // Adapter B: a single 3D engine at 65%.
            (
                "pid_3_luid_0x0_0xBBBB_phys_0_eng_0_engtype_3D".to_string(),
                65.0,
            ),
        ];
        let per = gpu_engine_utilization_by_adapter(&items);
        assert_eq!(per.get("0x00000000_0x0000AAAA"), Some(&40.0));
        assert_eq!(per.get("0x00000000_0x0000BBBB"), Some(&65.0));
        // The single-value helper still reports the busiest adapter.
        assert_eq!(gpu_engine_utilization(&items), Some(65.0));
    }

    #[test]
    fn memory_is_summed_within_an_adapter_only() {
        let dedicated = vec![
            // Two phys entries on the same LUID are summed (linked display adapters).
            ("luid_0x0_0xAAAA_phys_0".to_string(), 100.0),
            ("luid_0x0_0xAAAA_phys_1".to_string(), 150.0),
            ("luid_0x0_0xBBBB_phys_0".to_string(), 900.0),
            ("luid_0x0_0xBBBB_phys_0".to_string(), f64::NAN),
        ];
        let shared = vec![
            ("luid_0x0_0xAAAA_phys_0".to_string(), 10.0),
            ("luid_0x0_0xAAAA_phys_1".to_string(), 20.0),
        ];
        let map = gpu_memory_by_adapter(&dedicated, &shared);
        assert_eq!(map.get("0x00000000_0x0000AAAA"), Some(&(250, 30)));
        assert_eq!(map.get("0x00000000_0x0000BBBB"), Some(&(900, 0)));
    }

    #[test]
    fn lhm_temperature_picks_gpu_only() {
        let sensors = vec![
            ("CPU Package".to_string(), "Temperature".to_string(), 55.0),
            ("GPU Core".to_string(), "Temperature".to_string(), 48.0),
            ("GPU Hot Spot".to_string(), "Temperature".to_string(), 61.0),
            ("GPU Fan".to_string(), "Control".to_string(), 40.0),
            ("GPU Load".to_string(), "Load".to_string(), 90.0),
            (
                "GPU Memory".to_string(),
                "Temperature".to_string(),
                f32::NAN,
            ),
        ];
        assert_eq!(lhm_gpu_temperature(&sensors), Some(61.0));
        // CPU-only sensors never masquerade as GPU temperature.
        assert_eq!(
            lhm_gpu_temperature(&[("CPU Package".into(), "Temperature".into(), 70.0)]),
            None
        );
        assert_eq!(lhm_gpu_temperature(&[]), None);
    }

    #[test]
    fn axis_labels_track_real_span() {
        assert_eq!(
            span_axis_labels(24),
            vec!["-24s", "-18s", "-12s", "-6s", "now"]
        );
        assert_eq!(
            span_axis_labels(120),
            vec!["-120s", "-90s", "-60s", "-30s", "now"]
        );
        // Degenerate span still produces five labels.
        assert_eq!(span_axis_labels(0).len(), 5);
    }

    #[test]
    fn series_stat_current_avg_peak() {
        assert_eq!(series_stat(&[]), SeriesStat::default());
        let s = series_stat(&[10, 20, 30, 40]);
        assert_eq!(s.current, 40);
        assert_eq!(s.avg, 25);
        assert_eq!(s.peak, 40);
        // Single value: current == avg == peak.
        assert_eq!(
            series_stat(&[7]),
            SeriesStat {
                current: 7,
                avg: 7,
                peak: 7
            }
        );
        // Average truncates toward zero.
        assert_eq!(series_stat(&[1, 2]).avg, 1);
        // Large magnitudes do not overflow the sum (uses u128 internally).
        let big = [u64::MAX, u64::MAX];
        assert_eq!(series_stat(&big).avg, u64::MAX);
    }
}
