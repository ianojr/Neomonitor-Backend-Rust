//! Performance investigator: deterministic, explainable condition detection.
//!
//! This module is a small rules engine layered on top of the collected metrics. It is
//! deliberately **interpretation only**: it never changes system state, never kills a
//! process, and never talks to the network. Every conclusion is a [`Finding`] that names the
//! rule that fired, the measured values behind it, a confidence/quality judgement, and the
//! limitations of the evidence.
//!
//! Design constraints (see `docs/engineering/12-performance-investigator.md`):
//!
//! * **Deterministic & time-driven.** `observe` takes the sample time explicitly; nothing
//!   reads the clock internally, so the whole lifecycle is unit-testable.
//! * **No diagnosis on missing evidence.** A metric that is [`Reading::Unavailable`] or
//!   [`Reading::Stale`] does not contribute; a stale active finding is retired with a reason
//!   rather than being reinterpreted.
//! * **Sustained vs. spike.** A condition must persist for a configurable window before it
//!   is reported, so brief spikes do not raise findings.
//! * **One finding per active condition.** A continuously active condition updates its
//!   finding in place; it is never re-reported as a new alert. After recovery a cooldown must
//!   elapse before the same rule can fire again.
//! * **Correlation, not causation.** Related measurements are attached as evidence; the text
//!   never asserts why a process or the system is busy.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::gpu::GpuStats;
use crate::metrics::SysSnapshot;
use crate::reading::Reading;

/// Upper bound on the retained finding log (active + recovered).
const MAX_FINDINGS: usize = 64;

/// The fields a rule's `build_*` produces, in the order consumed by [`Investigator::build`]:
/// title, detection rule, measured values, evidence, confidence, confidence note,
/// recommendation, limitations.
type Draft = (
    String,
    String,
    Vec<(String, String)>,
    Vec<String>,
    Confidence,
    String,
    String,
    Vec<String>,
);

/// Which condition a rule watches.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum RuleId {
    CpuSustained,
    MemoryPressure,
    ProcessHog,
    NetworkSurge,
    DiskCapacity,
    Thermal,
}

impl RuleId {
    pub const ALL: [RuleId; 6] = [
        RuleId::CpuSustained,
        RuleId::MemoryPressure,
        RuleId::ProcessHog,
        RuleId::NetworkSurge,
        RuleId::DiskCapacity,
        RuleId::Thermal,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            RuleId::CpuSustained => "Sustained CPU utilisation",
            RuleId::MemoryPressure => "Memory pressure",
            RuleId::ProcessHog => "Dominant process",
            RuleId::NetworkSurge => "Network throughput change",
            RuleId::DiskCapacity => "Disk capacity",
            RuleId::Thermal => "Thermal warning",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            RuleId::CpuSustained => "cpu",
            RuleId::MemoryPressure => "mem",
            RuleId::ProcessHog => "proc",
            RuleId::NetworkSurge => "net",
            RuleId::DiskCapacity => "disk",
            RuleId::Thermal => "temp",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    Notice,
    Warning,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Notice => "notice",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Severity::Notice => "*",
            Severity::Warning => "!",
            Severity::Critical => "!!",
        }
    }

    pub fn rank(self) -> u8 {
        self as u8
    }
}

/// How much the evidence supports the conclusion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn label(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

/// Configurable thresholds and timing windows. All windows are durations so the rules do not
/// depend on a sample count or cadence.
#[derive(Clone, Copy, Debug)]
pub struct Thresholds {
    pub cpu_percent: f32,
    pub cpu_critical: f32,
    pub cpu_persist: Duration,

    pub mem_percent: f32,
    pub mem_critical: f32,
    pub mem_persist: Duration,
    /// Swap/pagefile usage (%-of-total) above which exhaustion is evidenced.
    pub swap_percent: f32,

    pub proc_cpu_percent: f32,
    pub proc_mem_percent: f32,
    pub proc_persist: Duration,

    /// A surge is `current >= baseline * factor` (and above the floor).
    pub net_factor: f32,
    pub net_floor: u64,
    pub net_persist: Duration,
    pub net_baseline_n: usize,

    /// Warn when free space falls to/below this percentage of total.
    pub disk_free_percent: f32,
    pub disk_critical_percent: f32,

    pub temp_celsius: f32,
    pub temp_critical: f32,
    pub temp_persist: Duration,

    /// A condition must stay clear this long before it is marked recovered.
    pub recover: Duration,
    /// After recovery the same rule is suppressed for this long.
    pub cooldown: Duration,
    /// Minimum spacing between notifications while a condition stays active.
    pub renotify: Duration,
    /// If evidence disappears while active, retire the finding after this long.
    pub missing_grace: Duration,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            cpu_percent: 85.0,
            cpu_critical: 95.0,
            cpu_persist: Duration::from_secs(15),

            mem_percent: 90.0,
            mem_critical: 96.0,
            mem_persist: Duration::from_secs(15),
            swap_percent: 50.0,

            proc_cpu_percent: 50.0,
            proc_mem_percent: 25.0,
            proc_persist: Duration::from_secs(10),

            net_factor: 5.0,
            net_floor: 1 << 20,
            net_persist: Duration::from_secs(5),
            net_baseline_n: 30,

            disk_free_percent: 10.0,
            disk_critical_percent: 5.0,

            temp_celsius: 85.0,
            temp_critical: 95.0,
            temp_persist: Duration::from_secs(10),

            recover: Duration::from_secs(10),
            cooldown: Duration::from_secs(30),
            renotify: Duration::from_secs(60),
            missing_grace: Duration::from_secs(30),
        }
    }
}

/// A process contribution kept in a [`Sample`], already trimmed to the likely-useful rows.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcSample {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
}

/// A disk's capacity, used by the capacity rule.
#[derive(Clone, Debug, PartialEq)]
pub struct DiskFree {
    pub name: String,
    pub total: u64,
    pub avail: u64,
}

/// One normalized observation fed to the engine.
///
/// Built by [`Sample::from_snapshot`] on the UI thread so the engine can be exercised with
/// completely synthetic samples in tests.
#[derive(Clone, Debug)]
pub struct Sample {
    pub at: Instant,
    pub cpu: Reading<f32>,
    pub mem_total: u64,
    pub mem_used: u64,
    pub mem_available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub cpu_temp: Reading<f32>,
    pub cpu_temp_label: String,
    pub gpu_temp: Reading<f32>,
    pub gpu_util: Reading<f32>,
    pub net_rx: Reading<u64>,
    pub net_tx: Reading<u64>,
    pub procs: Vec<ProcSample>,
    pub disks: Vec<DiskFree>,
}

impl Sample {
    /// A benign, fully-specified sample (used by tests as a base to tweak).
    #[cfg(test)]
    pub fn quiet(at: Instant) -> Self {
        Self {
            at,
            cpu: Reading::Valid(5.0),
            mem_total: 16 << 30,
            mem_used: 4 << 30,
            mem_available: 12 << 30,
            swap_total: 4 << 30,
            swap_used: 0,
            cpu_temp: Reading::Unavailable,
            cpu_temp_label: String::new(),
            gpu_temp: Reading::Unavailable,
            gpu_util: Reading::Unavailable,
            net_rx: Reading::Valid(1000),
            net_tx: Reading::Valid(1000),
            procs: vec![ProcSample {
                pid: 1,
                name: "idle".to_string(),
                cpu: 1.0,
                mem: 1 << 20,
            }],
            disks: vec![DiskFree {
                name: "C:".to_string(),
                total: 100 << 30,
                avail: 50 << 30,
            }],
        }
    }

    /// Project a collected snapshot (plus the latest GPU stats) into an engine sample.
    pub fn from_snapshot(snap: &SysSnapshot, gpu: &GpuStats, at: Instant) -> Self {
        // Hottest reported component, if any.
        let mut cpu_temp = Reading::Unavailable;
        let mut cpu_temp_label = String::new();
        let mut hottest = f32::MIN;
        for c in &snap.comps {
            if let Some(t) = c.temp
                && t.is_finite()
                && t > hottest
            {
                hottest = t;
                cpu_temp = Reading::Valid(t);
                cpu_temp_label = c.label.clone();
            }
        }

        // Keep the few processes most likely to be relevant, deduplicated: busiest by CPU and
        // largest by memory.
        let mut by_cpu: Vec<&crate::metrics::ProcRow> = snap.procs.iter().collect();
        by_cpu.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        let mut by_mem: Vec<&crate::metrics::ProcRow> = snap.procs.iter().collect();
        by_mem.sort_by(|a, b| b.mem.cmp(&a.mem));
        let mut procs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for p in by_cpu.into_iter().take(8).chain(by_mem.into_iter().take(6)) {
            if seen.insert(p.pid) {
                procs.push(ProcSample {
                    pid: p.pid,
                    name: short_name(&p.name),
                    cpu: p.cpu,
                    mem: p.mem,
                });
            }
        }

        let disks = snap
            .disks
            .iter()
            .map(|d| DiskFree {
                name: d.mount.clone(),
                total: d.total,
                avail: d.avail,
            })
            .collect();

        Self {
            at,
            cpu: Reading::Valid(snap.cpu_global),
            mem_total: snap.mem.total,
            mem_used: snap.mem.used,
            mem_available: snap.mem.available,
            swap_total: snap.mem.swap_total,
            swap_used: snap.mem.swap_used,
            cpu_temp,
            cpu_temp_label,
            gpu_temp: gpu.temperature,
            gpu_util: gpu.utilization,
            net_rx: Reading::Valid(snap.net_rx_rate),
            net_tx: Reading::Valid(snap.net_tx_rate),
            procs,
            disks,
        }
    }
}

fn short_name(name: &str) -> String {
    name.chars().take(40).collect()
}

/// A diagnostic conclusion with everything needed to justify and qualify it.
#[derive(Clone, Debug)]
pub struct Finding {
    pub id: u64,
    pub rule: RuleId,
    pub severity: Severity,
    pub title: String,
    /// When the condition was first confirmed (persistence met).
    pub observed: Instant,
    /// When the condition was last seen breached.
    pub last_seen: Instant,
    /// The exact rule text, e.g. `cpu_global >= 85% for 15s`.
    pub detection: String,
    /// Relevant measured values at the last update.
    pub values: Vec<(String, String)>,
    /// Supporting/correlating observations.
    pub evidence: Vec<String>,
    pub confidence: Confidence,
    pub confidence_note: String,
    pub recommendation: String,
    pub limitations: Vec<String>,
    pub active: bool,
    pub recovered: Option<Instant>,
}

impl Finding {
    /// How long the condition has been (or was) active.
    pub fn duration(&self, now: Instant) -> Duration {
        let end = self.recovered.unwrap_or(now);
        end.saturating_duration_since(self.observed)
    }

    /// Age since the condition was first confirmed.
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.observed)
    }
}

/// Internal per-rule lifecycle state.
#[derive(Default)]
struct RuleRuntime {
    /// Start of the current uninterrupted breach streak.
    breach_since: Option<Instant>,
    /// Start of the current uninterrupted clear streak (only meaningful while active).
    clear_since: Option<Instant>,
    /// Start of the current "no evidence" streak (only meaningful while active).
    missing_since: Option<Instant>,
    /// The active finding's id, if this rule is currently active.
    active_id: Option<u64>,
    cooldown_until: Option<Instant>,
    last_notify: Option<Instant>,
}

enum Outcome {
    Unavailable,
    Clear,
    Breach,
}

/// The engine. Owns all state; fed one [`Sample`] per fast tick.
pub struct Investigator {
    pub thresholds: Thresholds,
    enabled: [bool; RuleId::ALL.len()],
    rules: [RuleRuntime; RuleId::ALL.len()],
    findings: VecDeque<Finding>,
    net_ring: VecDeque<u64>,
    next_id: u64,
    /// Findings produced (or escalated) since the user last looked.
    unseen: u32,
}

impl Default for Investigator {
    fn default() -> Self {
        Self::new()
    }
}

impl Investigator {
    pub fn new() -> Self {
        Self::with_thresholds(Thresholds::default())
    }

    pub fn with_thresholds(thresholds: Thresholds) -> Self {
        Self {
            thresholds,
            enabled: [true; RuleId::ALL.len()],
            rules: Default::default(),
            findings: VecDeque::new(),
            net_ring: VecDeque::new(),
            next_id: 1,
            unseen: 0,
        }
    }

    pub fn findings(&self) -> &VecDeque<Finding> {
        &self.findings
    }

    pub fn is_enabled(&self, rule: RuleId) -> bool {
        self.enabled[rule.index()]
    }

    pub fn set_enabled(&mut self, rule: RuleId, on: bool) {
        self.enabled[rule.index()] = on;
        if !on {
            // Retire any active finding for a disabled rule.
            self.retire(rule, Instant::now(), "rule disabled");
        }
    }

    pub fn toggle(&mut self, rule: RuleId) -> bool {
        let on = !self.is_enabled(rule);
        self.set_enabled(rule, on);
        on
    }

    /// Active findings, newest first.
    pub fn active_count(&self) -> usize {
        self.findings.iter().filter(|f| f.active).count()
    }

    pub fn active_max_severity(&self) -> Option<Severity> {
        self.findings
            .iter()
            .filter(|f| f.active)
            .map(|f| f.severity)
            .max()
    }

    pub fn unseen(&self) -> u32 {
        self.unseen
    }

    pub fn mark_seen(&mut self) {
        self.unseen = 0;
    }

    /// Adjust the selected rule's primary threshold by one step; returns the new value text.
    pub fn adjust_threshold(&mut self, rule: RuleId, up: bool) -> String {
        let d = if up { 1.0 } else { -1.0 };
        match rule {
            RuleId::CpuSustained => {
                self.thresholds.cpu_percent =
                    (self.thresholds.cpu_percent + 5.0 * d as f32).clamp(5.0, 100.0);
                format!("{:.0}%", self.thresholds.cpu_percent)
            }
            RuleId::MemoryPressure => {
                self.thresholds.mem_percent =
                    (self.thresholds.mem_percent + 5.0 * d as f32).clamp(5.0, 100.0);
                format!("{:.0}%", self.thresholds.mem_percent)
            }
            RuleId::ProcessHog => {
                self.thresholds.proc_cpu_percent =
                    (self.thresholds.proc_cpu_percent + 5.0 * d as f32).clamp(5.0, 100.0);
                format!("{:.0}%", self.thresholds.proc_cpu_percent)
            }
            RuleId::NetworkSurge => {
                self.thresholds.net_factor =
                    (self.thresholds.net_factor + 0.5 * d as f32).clamp(1.5, 50.0);
                format!("{:.1}x", self.thresholds.net_factor)
            }
            RuleId::DiskCapacity => {
                self.thresholds.disk_free_percent =
                    (self.thresholds.disk_free_percent + 1.0 * d as f32).clamp(1.0, 50.0);
                format!("{:.0}%", self.thresholds.disk_free_percent)
            }
            RuleId::Thermal => {
                self.thresholds.temp_celsius =
                    (self.thresholds.temp_celsius + 1.0 * d as f32).clamp(40.0, 110.0);
                format!("{:.0} C", self.thresholds.temp_celsius)
            }
        }
    }

    /// Human-readable summary of the selected rule's current threshold.
    pub fn threshold_text(&self, rule: RuleId) -> String {
        match rule {
            RuleId::CpuSustained => format!("{:.0}%", self.thresholds.cpu_percent),
            RuleId::MemoryPressure => format!("{:.0}%", self.thresholds.mem_percent),
            RuleId::ProcessHog => format!("{:.0}%", self.thresholds.proc_cpu_percent),
            RuleId::NetworkSurge => format!("{:.1}x", self.thresholds.net_factor),
            RuleId::DiskCapacity => format!("{:.0}% free", self.thresholds.disk_free_percent),
            RuleId::Thermal => format!("{:.0} C", self.thresholds.temp_celsius),
        }
    }

    /// Feed one observation and advance every enabled rule's lifecycle.
    pub fn observe(&mut self, sample: &Sample) {
        let now = sample.at;
        self.push_net(sample);

        for rule in RuleId::ALL {
            if !self.enabled[rule.index()] {
                continue;
            }
            match self.evaluate(rule, sample) {
                Outcome::Unavailable => self.on_unavailable(rule, now),
                Outcome::Clear => self.on_clear(rule, now),
                Outcome::Breach => self.on_breach(rule, sample),
            }
        }
        self.trim();
    }

    // ----- internals ----------------------------------------------------------

    fn persist(&self, rule: RuleId) -> Duration {
        match rule {
            RuleId::CpuSustained => self.thresholds.cpu_persist,
            RuleId::MemoryPressure => self.thresholds.mem_persist,
            RuleId::ProcessHog => self.thresholds.proc_persist,
            RuleId::NetworkSurge => self.thresholds.net_persist,
            RuleId::DiskCapacity => Duration::ZERO,
            RuleId::Thermal => self.thresholds.temp_persist,
        }
    }

    fn push_net(&mut self, sample: &Sample) {
        if let (Some(rx), Some(tx)) = (fresh(&sample.net_rx), fresh(&sample.net_tx)) {
            self.net_ring.push_back(rx.saturating_add(tx));
            while self.net_ring.len() > self.thresholds.net_baseline_n + 1 {
                self.net_ring.pop_front();
            }
        } else {
            self.net_ring.clear();
        }
    }

    /// Current combined throughput and a baseline (median of the recent window, excluding the
    /// newest sample). `None` until there is enough history to establish a baseline.
    fn net_stats(&self, sample: &Sample) -> Option<(u64, u64)> {
        let rx = fresh(&sample.net_rx)?;
        let tx = fresh(&sample.net_tx)?;
        let total = rx.saturating_add(tx);
        if self.net_ring.len() < 4 {
            return None;
        }
        let mut window: Vec<u64> = self.net_ring.iter().copied().collect();
        window.pop(); // the newest entry is the current sample
        if window.len() < 3 {
            return None;
        }
        window.sort_unstable();
        Some((total, window[window.len() / 2]))
    }

    fn evaluate(&self, rule: RuleId, s: &Sample) -> Outcome {
        match rule {
            RuleId::CpuSustained => match s.cpu {
                Reading::Valid(v) if v.is_finite() => {
                    if v as f64 >= self.thresholds.cpu_percent as f64 {
                        Outcome::Breach
                    } else {
                        Outcome::Clear
                    }
                }
                _ => Outcome::Unavailable,
            },
            RuleId::MemoryPressure => {
                if s.mem_total == 0 {
                    return Outcome::Unavailable;
                }
                if mem_percent(s) >= self.thresholds.mem_percent as f64 {
                    Outcome::Breach
                } else {
                    Outcome::Clear
                }
            }
            RuleId::ProcessHog => {
                if s.procs.is_empty() {
                    return Outcome::Unavailable;
                }
                let hot = s.procs.iter().any(|p| {
                    p.cpu as f64 >= self.thresholds.proc_cpu_percent as f64
                        || (s.mem_total > 0
                            && frac_pct(p.mem, s.mem_total)
                                >= self.thresholds.proc_mem_percent as f64)
                });
                if hot { Outcome::Breach } else { Outcome::Clear }
            }
            RuleId::NetworkSurge => match self.net_stats(s) {
                None => Outcome::Clear, // warm-up: no baseline yet
                Some((total, baseline)) => {
                    let baseline = baseline.max(self.thresholds.net_floor / 4).max(1);
                    let ratio = total as f64 / baseline as f64;
                    if total >= self.thresholds.net_floor
                        && ratio >= self.thresholds.net_factor as f64
                    {
                        Outcome::Breach
                    } else {
                        Outcome::Clear
                    }
                }
            },
            RuleId::DiskCapacity => {
                if s.disks.is_empty() {
                    return Outcome::Unavailable;
                }
                let low = s.disks.iter().any(|d| {
                    d.total > 0
                        && frac_pct(d.avail, d.total) <= self.thresholds.disk_free_percent as f64
                });
                if low { Outcome::Breach } else { Outcome::Clear }
            }
            RuleId::Thermal => match hottest_temp(s) {
                Some(t) if t >= self.thresholds.temp_celsius => Outcome::Breach,
                Some(_) => Outcome::Clear,
                None => Outcome::Unavailable,
            },
        }
    }

    fn severity_for(&self, rule: RuleId, s: &Sample) -> Severity {
        match rule {
            RuleId::CpuSustained => {
                let v = fresh(&s.cpu).unwrap_or(0.0);
                if v >= self.thresholds.cpu_critical {
                    Severity::Critical
                } else {
                    Severity::Warning
                }
            }
            RuleId::MemoryPressure => {
                if mem_percent(s) >= self.thresholds.mem_critical as f64 {
                    Severity::Critical
                } else {
                    Severity::Warning
                }
            }
            RuleId::ProcessHog => {
                let worst = s
                    .procs
                    .iter()
                    .map(|p| {
                        let cpu = p.cpu as f64;
                        let mem = if s.mem_total > 0 {
                            frac_pct(p.mem, s.mem_total)
                        } else {
                            0.0
                        };
                        cpu.max(mem)
                    })
                    .fold(0.0_f64, f64::max);
                if worst >= 80.0 {
                    Severity::Critical
                } else {
                    Severity::Warning
                }
            }
            RuleId::NetworkSurge => Severity::Notice,
            RuleId::DiskCapacity => {
                let min_free = s
                    .disks
                    .iter()
                    .filter(|d| d.total > 0)
                    .map(|d| frac_pct(d.avail, d.total))
                    .fold(f64::INFINITY, f64::min);
                if min_free <= self.thresholds.disk_critical_percent as f64 {
                    Severity::Critical
                } else {
                    Severity::Warning
                }
            }
            RuleId::Thermal => {
                let t = hottest_temp(s).unwrap_or(0.0);
                if t >= self.thresholds.temp_critical {
                    Severity::Critical
                } else {
                    Severity::Warning
                }
            }
        }
    }

    fn on_unavailable(&mut self, rule: RuleId, now: Instant) {
        let idx = rule.index();
        self.rules[idx].breach_since = None;
        let has_active = self.rules[idx].active_id.is_some();
        if has_active {
            let since = *self.rules[idx].missing_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= self.thresholds.missing_grace {
                self.retire(rule, now, "the measurement became unavailable");
            }
        } else {
            self.rules[idx].missing_since = None;
        }
    }

    fn on_clear(&mut self, rule: RuleId, now: Instant) {
        let idx = rule.index();
        self.rules[idx].breach_since = None;
        self.rules[idx].missing_since = None;
        let has_active = self.rules[idx].active_id.is_some();
        if has_active {
            let since = *self.rules[idx].clear_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= self.thresholds.recover {
                self.retire(rule, now, "the condition is no longer met");
            }
        } else {
            self.rules[idx].clear_since = None;
        }
    }

    fn on_breach(&mut self, rule: RuleId, s: &Sample) {
        let now = s.at;
        let idx = rule.index();

        self.rules[idx].clear_since = None;
        self.rules[idx].missing_since = None;
        let since = *self.rules[idx].breach_since.get_or_insert(now);
        let severity = self.severity_for(rule, s);

        if let Some(id) = self.rules[idx].active_id {
            let should_notify = {
                let rt = &self.rules[idx];
                rt.last_notify
                    .is_none_or(|t| s.at.saturating_duration_since(t) >= self.thresholds.renotify)
            };
            let escalated = self
                .find_mut(id)
                .is_some_and(|f| severity.rank() > f.severity.rank());
            self.update_finding(id, s, severity);
            if escalated || should_notify {
                self.unseen = self.unseen.saturating_add(1);
                self.rules[idx].last_notify = Some(now);
            }
            return;
        }

        let persisted = now.saturating_duration_since(since) >= self.persist(rule);
        if !persisted {
            return;
        }
        let in_cooldown = self.rules[idx]
            .cooldown_until
            .is_some_and(|until| now < until);
        // A critical condition overrides the cooldown; anything lesser waits it out.
        if in_cooldown && severity < Severity::Critical {
            return;
        }
        self.activate(rule, s, severity);
    }

    fn activate(&mut self, rule: RuleId, s: &Sample, severity: Severity) {
        let mut finding = self.build(rule, s, severity);
        finding.id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let id = finding.id;
        self.findings.push_front(finding);
        let idx = rule.index();
        self.rules[idx].active_id = Some(id);
        self.rules[idx].last_notify = Some(s.at);
        self.rules[idx].cooldown_until = None;
        self.unseen = self.unseen.saturating_add(1);
    }

    fn update_finding(&mut self, id: u64, s: &Sample, severity: Severity) {
        let rule = match self.find_mut(id) {
            Some(f) => {
                f.last_seen = s.at;
                if severity.rank() > f.severity.rank() {
                    f.severity = severity;
                }
                f.rule
            }
            None => return,
        };
        let fresh = self.build(rule, s, severity);
        if let Some(f) = self.find_mut(id) {
            f.values = fresh.values;
            f.evidence = fresh.evidence;
            f.confidence = fresh.confidence;
            f.confidence_note = fresh.confidence_note;
        }
    }

    fn retire(&mut self, rule: RuleId, now: Instant, reason: &str) {
        let idx = rule.index();
        let Some(id) = self.rules[idx].active_id.take() else {
            return;
        };
        if let Some(f) = self.find_mut(id) {
            f.active = false;
            f.recovered = Some(now);
            f.evidence.push(format!("Resolved: {reason}."));
        }
        self.rules[idx].cooldown_until = Some(now + self.thresholds.cooldown);
        self.rules[idx].breach_since = None;
        self.rules[idx].clear_since = None;
        self.rules[idx].missing_since = None;
    }

    fn find_mut(&mut self, id: u64) -> Option<&mut Finding> {
        self.findings.iter_mut().find(|f| f.id == id)
    }

    fn trim(&mut self) {
        while self.findings.len() > MAX_FINDINGS {
            let removed = self.findings.pop_back();
            if let Some(f) = removed
                && f.active
                && let Some(rt) = self.rules.get_mut(f.rule.index())
                && rt.active_id == Some(f.id)
            {
                rt.active_id = None;
            }
        }
    }

    // ----- finding construction ----------------------------------------------

    fn build(&self, rule: RuleId, s: &Sample, severity: Severity) -> Finding {
        let (title, detection, values, evidence, confidence, confidence_note, rec, limits) =
            match rule {
                RuleId::CpuSustained => self.build_cpu(s),
                RuleId::MemoryPressure => self.build_mem(s),
                RuleId::ProcessHog => self.build_proc(s),
                RuleId::NetworkSurge => self.build_net(s),
                RuleId::DiskCapacity => self.build_disk(s),
                RuleId::Thermal => self.build_temp(s),
            };
        Finding {
            id: 0,
            rule,
            severity,
            title,
            observed: s.at,
            last_seen: s.at,
            detection,
            values,
            evidence,
            confidence,
            confidence_note,
            recommendation: rec,
            limitations: limits,
            active: true,
            recovered: None,
        }
    }

    fn build_cpu(&self, s: &Sample) -> Draft {
        let v = fresh(&s.cpu).unwrap_or(0.0);
        let cores = s.procs.len();
        let mut ev = Vec::new();
        // Correlation: identify the largest observed contributor, if any stands out.
        let top = s.procs.iter().max_by(|a, b| a.cpu.total_cmp(&b.cpu));
        let contributors = top_contributors(s.procs.iter(), 3);
        if !contributors.is_empty() {
            ev.push(format!(
                "Largest observed CPU contributors during the interval: {}.",
                contributors
                    .iter()
                    .map(|p| format!("{} (pid {}, {:.1}%)", p.name, p.pid, p.cpu))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let dominant =
            top.is_some_and(|p| p.cpu as f64 >= self.thresholds.proc_cpu_percent as f64 * 0.5);
        if !dominant {
            ev.push(
                "No single process accounted for a large share of CPU; a broad or kernel-time load is possible."
                    .to_string(),
            );
        }
        ev.push(
            "This identifies who is consuming CPU, not why; process-level attribution does not explain intent."
                .to_string(),
        );
        let confidence = if cores > 0 && dominant {
            Confidence::High
        } else {
            Confidence::Medium
        };
        let note = if dominant {
            "global CPU is a direct measure; a dominant process was also observed"
        } else {
            "global CPU is direct, but process attribution was not conclusive"
        };
        (
            format!("CPU utilisation sustained at {v:.1}%"),
            format!("cpu_global >= {:.0}% for {}s", self.thresholds.cpu_percent, self.thresholds.cpu_persist.as_secs()),
            vec![
                ("cpu_global".to_string(), format!("{v:.1}%")),
                ("threshold".to_string(), format!("{:.0}%", self.thresholds.cpu_percent)),
                ("logical CPUs".to_string(), cores.to_string()),
            ],
            ev,
            confidence,
            note.to_string(),
            "Inspect the named processes' current work; if the load is spread out, check for a background indexer, build, or malware scanner."
                .to_string(),
            vec![
                "CPU utilisation is a busy-fraction over the sampling interval; very short bursts are smoothed.".to_string(),
                "Process CPU is sampled less often than the global figure and may not align exactly.".to_string(),
            ],
        )
    }

    fn build_mem(&self, s: &Sample) -> Draft {
        let pct = mem_percent(s);
        let swap_pct = if s.swap_total > 0 {
            frac_pct(s.swap_used, s.swap_total)
        } else {
            0.0
        };
        let exhausting = swap_pct >= self.thresholds.swap_percent as f64;
        let mut ev = vec![
            format!(
                "Resident memory: {} of {} used ({} available).",
                crate::util::human_bytes(s.mem_used),
                crate::util::human_bytes(s.mem_total),
                crate::util::human_bytes(s.mem_available),
            ),
            format!(
                "Swap/pagefile: {} of {} used ({swap_pct:.0}%).",
                crate::util::human_bytes(s.swap_used),
                crate::util::human_bytes(s.swap_total),
            ),
        ];
        if exhausting {
            ev.push(
                "Elevated swap/pagefile use is evidence of memory exhaustion, not just high utilisation."
                    .to_string(),
            );
        } else {
            ev.push(
                "Little swap/pagefile use: this is high resident usage without evidence of exhaustion."
                    .to_string(),
            );
        }
        if let Some(p) = s.procs.iter().max_by_key(|p| p.mem) {
            ev.push(format!(
                "Largest resident process in the sample: {} (pid {}, {}).",
                p.name,
                p.pid,
                crate::util::human_bytes(p.mem)
            ));
        }
        (
            format!("Memory utilisation at {pct:.0}%"),
            format!("mem.used/mem.total >= {:.0}% for {}s", self.thresholds.mem_percent, self.thresholds.mem_persist.as_secs()),
            vec![
                ("memory used".to_string(), format!("{pct:.0}%")),
                ("swap used".to_string(), format!("{swap_pct:.0}%")),
                ("available".to_string(), crate::util::human_bytes(s.mem_available)),
            ],
            ev,
            Confidence::High,
            "memory totals are direct; the swap signal is a separate, corroborating measurement"
                .to_string(),
            "Identify the largest resident processes; close or investigate them if the machine is paging."
                .to_string(),
            vec![
                "High used-memory is normal for caches; without swap activity it does not prove pressure.".to_string(),
                "Windows also keeps a standby cache; 'available' is the more meaningful free figure.".to_string(),
            ],
        )
    }

    fn build_proc(&self, s: &Sample) -> Draft {
        let top_cpu = s.procs.iter().max_by(|a, b| a.cpu.total_cmp(&b.cpu));
        let top_mem = s.procs.iter().max_by_key(|p| p.mem);
        let name = top_cpu
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "?".to_string());
        let pid = top_cpu.map(|p| p.pid).unwrap_or(0);
        let cpu = top_cpu.map(|p| p.cpu).unwrap_or(0.0);
        let mem = top_mem.map(|p| p.mem).unwrap_or(0);
        let mem_pct = if s.mem_total > 0 {
            frac_pct(mem, s.mem_total)
        } else {
            0.0
        };
        let global = fresh(&s.cpu).unwrap_or(0.0);
        let mut ev = vec![format!(
            "Largest CPU consumer: {} (pid {}), {:.1}% of machine.",
            name, pid, cpu
        )];
        if let Some(m) = top_mem {
            ev.push(format!(
                "Largest resident process: {} (pid {}), {} ({mem_pct:.1}% of RAM).",
                m.name,
                m.pid,
                crate::util::human_bytes(m.mem)
            ));
        }
        // Contradictory-evidence handling: a hot process while the machine is quiet is not a
        // system-wide problem, so say so instead of implying the whole machine is loaded.
        if cpu as f64 > self.thresholds.proc_cpu_percent as f64
            && global < self.thresholds.cpu_percent
        {
            ev.push(format!(
                "Machine-wide CPU was only {global:.1}%; this is process-local, not system-wide saturation."
            ));
        }
        (
            format!("Process {name} (pid {pid}) is a dominant consumer"),
            format!(
                "proc.cpu >= {:.0}% OR proc.mem >= {:.0}% of RAM for {}s",
                self.thresholds.proc_cpu_percent,
                self.thresholds.proc_mem_percent,
                self.thresholds.proc_persist.as_secs()
            ),
            vec![
                ("process".to_string(), format!("{name} (pid {pid})")),
                ("process CPU".to_string(), format!("{cpu:.1}%")),
                ("machine CPU".to_string(), format!("{global:.1}%")),
                ("largest RSS".to_string(), format!("{} ({mem_pct:.1}%)", crate::util::human_bytes(mem))),
            ],
            ev,
            Confidence::Medium,
            "process CPU/memory are sampled on a slower tick than the global figure and may lag"
                .to_string(),
            "Open the process explorer, inspect this PID (Enter), and decide whether its work is expected."
                .to_string(),
            vec![
                "A dominant process is not inherently a problem; some workloads are meant to be busy.".to_string(),
                "Process CPU is a per-interval sample and can miss work between samples.".to_string(),
            ],
        )
    }

    fn build_net(&self, s: &Sample) -> Draft {
        let (total, baseline) = self.net_stats(s).unwrap_or((0, 0));
        let rx = fresh(&s.net_rx).unwrap_or(0);
        let tx = fresh(&s.net_tx).unwrap_or(0);
        let ratio = if baseline > 0 {
            total as f64 / baseline as f64
        } else {
            0.0
        };
        (
            format!(
                "Network throughput rose to {}/s ({ratio:.1}x recent baseline)",
                crate::util::human_rate(total)
            ),
            format!(
                "combined rx+tx >= {:.1}x baseline and >= {}/s for {}s",
                self.thresholds.net_factor,
                crate::util::human_rate(self.thresholds.net_floor),
                self.thresholds.net_persist.as_secs()
            ),
            vec![
                ("download".to_string(), crate::util::human_rate(rx)),
                ("upload".to_string(), crate::util::human_rate(tx)),
                ("baseline".to_string(), crate::util::human_rate(baseline)),
                ("ratio".to_string(), format!("{ratio:.1}x")),
            ],
            vec![
                format!(
                    "Combined throughput {} to {} (baseline).",
                    crate::util::human_rate(total),
                    crate::util::human_rate(baseline)
                ),
                "Pseudo/filter interfaces are excluded from the totals to avoid double counting."
                    .to_string(),
            ],
            Confidence::Low,
            "the baseline is a short rolling median of recent samples, not a long-term profile"
                .to_string(),
            "Correlate with known activity (updates, backups, streaming). If unexpected, inspect network-heavy processes in the process explorer."
                .to_string(),
            vec![
                "A throughput change is not inherently a fault; it may be entirely expected.".to_string(),
                "The baseline adapts quickly, so a sustained new level stops being flagged after the window fills.".to_string(),
            ],
        )
    }

    fn build_disk(&self, s: &Sample) -> Draft {
        let worst = s
            .disks
            .iter()
            .filter(|d| d.total > 0)
            .min_by(|a, b| frac_pct(a.avail, a.total).total_cmp(&frac_pct(b.avail, b.total)));
        let (mount, total, avail, free_pct) = match worst {
            Some(d) => (d.name.clone(), d.total, d.avail, frac_pct(d.avail, d.total)),
            None => (String::from("?"), 0, 0, 0.0),
        };
        (
            format!("Disk {mount} is {free_pct:.0}% free"),
            format!(
                "disk free <= {:.0}% of total",
                self.thresholds.disk_free_percent
            ),
            vec![
                ("volume".to_string(), mount.clone()),
                ("free".to_string(), crate::util::human_bytes(avail)),
                ("total".to_string(), crate::util::human_bytes(total)),
                ("percent free".to_string(), format!("{free_pct:.0}%")),
            ],
            vec![
                "Capacity is a direct measurement; no interpolation is involved.".to_string(),
                "Free space includes the filesystem reserve on some volumes.".to_string(),
            ],
            Confidence::High,
            "capacity is read directly from the filesystem".to_string(),
            "Free space on this volume; check large caches, logs or downloads.".to_string(),
            vec![
                "Filesystem reserve means the usable margin is smaller than the raw percentage."
                    .to_string(),
            ],
        )
    }

    fn build_temp(&self, s: &Sample) -> Draft {
        let cpu = fresh(&s.cpu_temp).filter(|t| t.is_finite());
        let gpu = fresh(&s.gpu_temp).filter(|t| t.is_finite());
        let cpu_label = || {
            if s.cpu_temp_label.is_empty() {
                "CPU".to_string()
            } else {
                s.cpu_temp_label.clone()
            }
        };
        let (t, source) = match (cpu, gpu) {
            (Some(a), Some(b)) if b > a => (b, "GPU".to_string()),
            (Some(a), Some(_)) => (a, cpu_label()),
            (Some(a), None) => (a, cpu_label()),
            (None, Some(b)) => (b, "GPU".to_string()),
            (None, None) => (0.0, "?".to_string()),
        };
        let mut ev = vec![format!("Hottest reported sensor: {source} at {t:.1} C.")];
        if s.gpu_util.is_valid() {
            ev.push(format!(
                "GPU utilisation was {:.1}%.",
                s.gpu_util.value().copied().unwrap_or(0.0)
            ));
        }
        if s.cpu.is_valid() {
            ev.push(format!(
                "CPU utilisation was {:.1}%.",
                s.cpu.value().copied().unwrap_or(0.0)
            ));
        }
        (
            format!("Temperature {t:.1} C"),
            format!("temperature >= {:.0} C for {}s", self.thresholds.temp_celsius, self.thresholds.temp_persist.as_secs()),
            vec![
                ("hottest".to_string(), format!("{t:.1} C")),
                ("source".to_string(), source.clone()),
                ("threshold".to_string(), format!("{:.0} C", self.thresholds.temp_celsius)),
            ],
            ev,
            Confidence::High,
            "a temperature is a direct sensor reading".to_string(),
            "Reduce sustained load or improve cooling if the reading is accurate; verify the sensor source."
                .to_string(),
            vec![
                "Sensor coverage varies: Windows often exposes no thermal zone and GPU temperature needs a provider."
                    .to_string(),
                "A single hot sensor does not necessarily mean the package is at a limit.".to_string(),
            ],
        )
    }
}

fn mem_percent(s: &Sample) -> f64 {
    frac_pct(s.mem_used, s.mem_total)
}

fn frac_pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64) * 100.0
    }
}

/// The value of a reading, but only when it is fresh. Stale and unavailable readings carry no
/// usable evidence for a live decision.
fn fresh<T: Copy>(r: &Reading<T>) -> Option<T> {
    if r.is_valid() {
        r.value().copied()
    } else {
        None
    }
}

fn hottest_temp(s: &Sample) -> Option<f32> {
    let cpu = fresh(&s.cpu_temp).filter(|t| t.is_finite());
    let gpu = fresh(&s.gpu_temp).filter(|t| t.is_finite());
    match (cpu, gpu) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn top_contributors<'a>(
    procs: impl Iterator<Item = &'a ProcSample>,
    n: usize,
) -> Vec<&'a ProcSample> {
    let mut v: Vec<&ProcSample> = procs.filter(|p| p.cpu > 0.1).collect();
    v.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    v.truncate(n);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A quiet sample at `t0 + off` seconds with global CPU overridden.
    fn cpu_at(t0: Instant, off: u64, v: f32) -> Sample {
        let mut s = Sample::quiet(t0 + secs(off));
        s.cpu = Reading::Valid(v);
        s
    }

    fn has_finding(inv: &Investigator, rule: RuleId) -> bool {
        inv.findings().iter().any(|f| f.rule == rule)
    }

    #[test]
    fn sustained_cpu_fires_only_after_persistence() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        // Below the 15 s window: nothing yet.
        for i in 0..15 {
            inv.observe(&cpu_at(t0, i, 90.0));
        }
        assert_eq!(inv.active_count(), 0, "fired before the persistence window");
        // Reaching the window confirms the condition.
        inv.observe(&cpu_at(t0, 15, 90.0));
        assert_eq!(inv.active_count(), 1);
        assert!(has_finding(&inv, RuleId::CpuSustained));
    }

    #[test]
    fn brief_spike_does_not_fire() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        for i in 0..10 {
            inv.observe(&cpu_at(t0, i, 99.0));
        }
        for i in 10..40 {
            inv.observe(&cpu_at(t0, i, 5.0));
        }
        assert_eq!(inv.active_count(), 0);
        assert_eq!(inv.findings().len(), 0);
    }

    #[test]
    fn recovery_marks_finding_recovered() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        for i in 0..=15 {
            inv.observe(&cpu_at(t0, i, 90.0));
        }
        assert_eq!(inv.active_count(), 1);
        // Clear for the recovery window.
        for i in 16..=26 {
            inv.observe(&cpu_at(t0, i, 5.0));
        }
        assert_eq!(inv.active_count(), 0);
        let f = inv
            .findings()
            .iter()
            .find(|f| f.rule == RuleId::CpuSustained)
            .unwrap();
        assert!(!f.active);
        assert!(f.recovered.is_some());
    }

    #[test]
    fn a_missing_metric_retires_an_active_finding() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        for i in 0..=15 {
            inv.observe(&cpu_at(t0, i, 90.0));
        }
        assert_eq!(inv.active_count(), 1);
        // Evidence disappears entirely; after the grace window it is retired, not guessed.
        for i in 16..=46 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.cpu = Reading::Unavailable;
            inv.observe(&s);
        }
        assert_eq!(inv.active_count(), 0);
        let f = inv
            .findings()
            .iter()
            .find(|f| f.rule == RuleId::CpuSustained)
            .unwrap();
        assert!(!f.active);
        assert!(f.evidence.iter().any(|e| e.contains("unavailable")));
    }

    #[test]
    fn stale_measurements_never_breach() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        for i in 0..40 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.cpu = Reading::Stale(99.0);
            inv.observe(&s);
        }
        assert_eq!(inv.active_count(), 0);
        assert_eq!(inv.findings().len(), 0);
    }

    #[test]
    fn a_continuous_condition_yields_exactly_one_finding() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        for i in 0..=60 {
            inv.observe(&cpu_at(t0, i, 92.0));
        }
        assert_eq!(inv.active_count(), 1);
        assert_eq!(inv.findings().len(), 1, "duplicate alerts were produced");
    }

    #[test]
    fn cooldown_suppresses_immediate_re_fire_but_not_forever() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        // Activate.
        for i in 0..=15 {
            inv.observe(&cpu_at(t0, i, 90.0));
        }
        assert_eq!(inv.active_count(), 1);
        // Recover (cooldown starts now, 30 s).
        for i in 16..=26 {
            inv.observe(&cpu_at(t0, i, 5.0));
        }
        assert_eq!(inv.active_count(), 0);
        // Re-breach immediately: still inside the cooldown -> suppressed.
        for i in 27..=45 {
            inv.observe(&cpu_at(t0, i, 92.0));
        }
        assert_eq!(inv.active_count(), 0, "re-fired during cooldown");
        // Persisting past the cooldown finally re-fires.
        for i in 46..=60 {
            inv.observe(&cpu_at(t0, i, 92.0));
        }
        assert_eq!(inv.active_count(), 1);
        assert_eq!(inv.findings().len(), 2);
    }

    #[test]
    fn contradictory_process_evidence_lowers_confidence() {
        let t0 = Instant::now();
        // High global CPU with a clearly dominant process -> high confidence.
        let mut dominant = Investigator::new();
        for i in 0..=15 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.cpu = Reading::Valid(90.0);
            s.procs = vec![ProcSample {
                pid: 9,
                name: "busy".into(),
                cpu: 70.0,
                mem: 1 << 20,
            }];
            dominant.observe(&s);
        }
        let df = dominant
            .findings()
            .iter()
            .find(|f| f.rule == RuleId::CpuSustained)
            .unwrap();
        assert_eq!(df.confidence, Confidence::High);

        // High global CPU with no dominant process -> honest downgrade.
        let mut diffuse = Investigator::new();
        for i in 0..=15 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.cpu = Reading::Valid(90.0);
            s.procs = vec![ProcSample {
                pid: 9,
                name: "spread".into(),
                cpu: 0.5,
                mem: 1 << 20,
            }];
            diffuse.observe(&s);
        }
        let ff = diffuse
            .findings()
            .iter()
            .find(|f| f.rule == RuleId::CpuSustained)
            .unwrap();
        assert_ne!(ff.confidence, Confidence::High);
        assert!(ff.evidence.iter().any(|e| e.contains("No single process")));
    }

    #[test]
    fn memory_pressure_distinguishes_utilisation_from_exhaustion() {
        let t0 = Instant::now();
        // High used RAM but no swap -> utilisation, not evidenced exhaustion.
        let mut util = Investigator::new();
        for i in 0..=15 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.mem_total = 100;
            s.mem_used = 95;
            s.swap_total = 100;
            s.swap_used = 0;
            util.observe(&s);
        }
        let f = util
            .findings()
            .iter()
            .find(|f| f.rule == RuleId::MemoryPressure)
            .unwrap();
        assert!(
            f.evidence
                .iter()
                .any(|e| e.contains("without evidence of exhaustion"))
        );

        // High used RAM with heavy swap -> exhaustion is evidenced.
        let mut exhaust = Investigator::new();
        for i in 0..=15 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.mem_total = 100;
            s.mem_used = 95;
            s.swap_total = 100;
            s.swap_used = 70;
            exhaust.observe(&s);
        }
        let f = exhaust
            .findings()
            .iter()
            .find(|f| f.rule == RuleId::MemoryPressure)
            .unwrap();
        assert!(f.evidence.iter().any(|e| e.contains("memory exhaustion")));
    }

    #[test]
    fn network_surge_relative_to_a_baseline() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        for i in 0..10 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.net_rx = Reading::Valid(1000);
            s.net_tx = Reading::Valid(1000);
            inv.observe(&s);
        }
        for i in 10..=20 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.net_rx = Reading::Valid(10 << 20);
            s.net_tx = Reading::Valid(10 << 20);
            inv.observe(&s);
        }
        assert!(has_finding(&inv, RuleId::NetworkSurge));
    }

    #[test]
    fn disk_capacity_reports_critical_near_full() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        for i in 0..3 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.disks = vec![DiskFree {
                name: "C:".into(),
                total: 100 << 30,
                avail: 4 << 30,
            }];
            inv.observe(&s);
        }
        assert_eq!(inv.active_count(), 1);
        let f = inv
            .findings()
            .iter()
            .find(|f| f.rule == RuleId::DiskCapacity)
            .unwrap();
        assert_eq!(f.severity, Severity::Critical);
    }

    #[test]
    fn thermal_requires_a_fresh_reading() {
        let t0 = Instant::now();
        let mut inv = Investigator::new();
        for i in 0..=10 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.cpu_temp = Reading::Valid(90.0);
            s.cpu_temp_label = "CPU Package".into();
            inv.observe(&s);
        }
        assert!(has_finding(&inv, RuleId::Thermal));

        // A stale GPU temperature must never raise a thermal finding.
        let mut inv2 = Investigator::new();
        for i in 0..30 {
            let mut s = Sample::quiet(t0 + secs(i));
            s.gpu_temp = Reading::Stale(99.0);
            inv2.observe(&s);
        }
        assert!(!has_finding(&inv2, RuleId::Thermal));
    }

    #[test]
    fn disabling_a_rule_stops_it_firing() {
        let mut inv = Investigator::new();
        inv.set_enabled(RuleId::CpuSustained, false);
        assert!(!inv.is_enabled(RuleId::CpuSustained));
        let t0 = Instant::now();
        for i in 0..30 {
            inv.observe(&cpu_at(t0, i, 99.0));
        }
        assert_eq!(inv.active_count(), 0);
        // Re-enabling restores detection.
        inv.set_enabled(RuleId::CpuSustained, true);
        for i in 30..=50 {
            inv.observe(&cpu_at(t0, i, 99.0));
        }
        assert!(has_finding(&inv, RuleId::CpuSustained));
    }

    #[test]
    fn unseen_counter_tracks_new_and_escalated_findings() {
        let mut inv = Investigator::new();
        let t0 = Instant::now();
        for i in 0..=15 {
            inv.observe(&cpu_at(t0, i, 90.0));
        }
        assert!(inv.unseen() >= 1);
        inv.mark_seen();
        assert_eq!(inv.unseen(), 0);
        // Escalation to critical re-notifies once.
        for i in 16..=20 {
            inv.observe(&cpu_at(t0, i, 99.0));
        }
        assert!(inv.unseen() >= 1);
    }

    #[test]
    fn threshold_adjustment_stays_in_range() {
        let mut inv = Investigator::new();
        for _ in 0..50 {
            inv.adjust_threshold(RuleId::CpuSustained, true);
        }
        assert!(inv.thresholds.cpu_percent <= 100.0);
        for _ in 0..50 {
            inv.adjust_threshold(RuleId::CpuSustained, false);
        }
        assert!(inv.thresholds.cpu_percent >= 5.0);
        assert!(inv.threshold_text(RuleId::DiskCapacity).contains("free"));
    }
}
