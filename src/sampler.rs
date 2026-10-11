//! Background metric collection.
//!
//! Two detached producer threads feed a **bounded** channel that the UI thread drains:
//!
//! * `sys_loop`  – CPU + memory on the fast tick, processes / network / disks + sensors +
//!   frequency + load on their own slower ticks, emits a `SysSnapshot` every fast tick.
//! * `gpu_loop`  – GPU engine + memory on the GPU tick, emits a `GpuStats`.
//!
//! Design notes (see `docs/engineering/08-sampling-performance.md`):
//!
//! * Elapsed time is always measured with a monotonic [`Instant`]; refresh gating uses the
//!   time since the *previous refresh of that source*, never a fixed sleep, so a slow call
//!   cannot cause a catch-up burst.
//! * The channel is bounded (`sync_channel`). If the UI cannot keep up, the newest sample
//!   is dropped and counted rather than growing memory without bound.
//! * Sleeps are sliced and check a shared stop flag, so [`Sampler::shutdown`] joins the
//!   workers promptly instead of waiting out a multi-second interval.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sysinfo::{Components, Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::calc;
use crate::diskio::DiskIoMonitor;
use crate::gpu::{GpuMonitor, GpuStats};
use crate::metrics::{
    CompStat, CoreStat, DiskStat, LoadStat, MemStat, NetIface, ProcRow, ProcState, SysSnapshot,
    Update,
};
use crate::reading::{Freshness, freshness};

/// How many snapshots may queue before new ones are dropped (latest-first behaviour is not
/// needed: the UI drains every poll, and the producer emits far slower than the poll).
const CHANNEL_CAPACITY: usize = 4;
/// Longest single sleep slice; bounds shutdown latency and stop-flag latency.
const NAP_SLICE: Duration = Duration::from_millis(25);
/// Minimum pause after an overrun tick so a persistently slow collector cannot busy-spin.
const OVERRUN_BACKOFF: Duration = Duration::from_millis(5);
/// Re-probe an absent sensor provider roughly this many slow ticks apart (provider may be
/// installed while running), instead of paying the WMI/ACPI probe every slow tick.
const COMPONENT_REPROBE_TICKS: u32 = 300;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Fast,
    Balanced,
    Eco,
    Minimal,
}

impl Preset {
    pub fn label(self) -> &'static str {
        match self {
            Preset::Fast => "fast",
            Preset::Balanced => "balanced",
            Preset::Eco => "eco",
            Preset::Minimal => "minimal",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Preset::Fast => Preset::Balanced,
            Preset::Balanced => Preset::Eco,
            Preset::Eco => Preset::Minimal,
            Preset::Minimal => Preset::Fast,
        }
    }

    /// `(fast, proc, net, slow, gpu)` intervals in milliseconds.
    fn values(self) -> (u64, u64, u64, u64, u64) {
        match self {
            Preset::Fast => (200, 500, 500, 2000, 500),
            Preset::Balanced => (200, 1000, 1000, 3000, 1000),
            Preset::Eco => (500, 2000, 2000, 5000, 3000),
            Preset::Minimal => (1000, 5000, 5000, 10000, 10000),
        }
    }

    /// Fast (CPU/RAM) sampling interval in milliseconds.
    pub fn fast_ms(self) -> u64 {
        self.values().0
    }
}

/// Counters describing collection health, shared between the workers and the UI.
#[derive(Default)]
pub struct SamplerStats {
    pub sys_samples: AtomicU64,
    pub gpu_samples: AtomicU64,
    /// Samples dropped because the consumer (UI) was not draining fast enough.
    pub dropped: AtomicU64,
    /// Ticks whose collection took longer than the fast interval.
    pub overruns: AtomicU64,
    /// GPU ticks with no backend / no readable engine data.
    pub gpu_failures: AtomicU64,
}

impl SamplerStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            sys_samples: self.sys_samples.load(Ordering::Relaxed),
            gpu_samples: self.gpu_samples.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            gpu_failures: self.gpu_failures.load(Ordering::Relaxed),
        }
    }
}

/// A copyable read of [`SamplerStats`] for display / diagnostics.
#[derive(Clone, Copy, Default)]
pub struct StatsSnapshot {
    pub sys_samples: u64,
    pub gpu_samples: u64,
    pub dropped: u64,
    pub overruns: u64,
    pub gpu_failures: u64,
}

pub struct SamplerConfig {
    fast_ms: AtomicU64,
    proc_ms: AtomicU64,
    net_ms: AtomicU64,
    slow_ms: AtomicU64,
    gpu_ms: AtomicU64,
    stop: AtomicBool,
    stats: SamplerStats,
}

impl SamplerConfig {
    pub fn new(preset: Preset) -> Self {
        let (fast, proc, net, slow, gpu) = preset.values();
        Self {
            fast_ms: AtomicU64::new(fast),
            proc_ms: AtomicU64::new(proc),
            net_ms: AtomicU64::new(net),
            slow_ms: AtomicU64::new(slow),
            gpu_ms: AtomicU64::new(gpu),
            stop: AtomicBool::new(false),
            stats: SamplerStats::default(),
        }
    }

    pub fn set(&self, preset: Preset) {
        let (fast, proc, net, slow, gpu) = preset.values();
        self.fast_ms.store(fast, Ordering::Relaxed);
        self.proc_ms.store(proc, Ordering::Relaxed);
        self.net_ms.store(net, Ordering::Relaxed);
        self.slow_ms.store(slow, Ordering::Relaxed);
        self.gpu_ms.store(gpu, Ordering::Relaxed);
    }

    pub fn stats(&self) -> &SamplerStats {
        &self.stats
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn dur(value: &AtomicU64) -> Duration {
        Duration::from_millis(value.load(Ordering::Relaxed).max(100))
    }

    fn fast(&self) -> Duration {
        Self::dur(&self.fast_ms)
    }

    fn proc(&self) -> Duration {
        Self::dur(&self.proc_ms)
    }

    fn net(&self) -> Duration {
        Self::dur(&self.net_ms)
    }

    fn slow(&self) -> Duration {
        Self::dur(&self.slow_ms)
    }

    fn gpu(&self) -> Duration {
        Self::dur(&self.gpu_ms)
    }
}

/// Owner of the worker threads and the receiving half of the update channel.
///
/// Dropping (or explicitly [`shutdown`](Sampler::shutdown)ing) the sampler signals the
/// workers to stop and joins them.
pub struct Sampler {
    pub rx: Receiver<Update>,
    config: Arc<SamplerConfig>,
    handles: Vec<JoinHandle<()>>,
}

impl Sampler {
    /// Signal every worker to stop and wait for it to exit.
    pub fn shutdown(&mut self) {
        self.stop_workers();
    }

    fn stop_workers(&mut self) {
        self.config.request_stop();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop_workers();
    }
}

pub fn spawn(config: Arc<SamplerConfig>) -> Sampler {
    let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);

    let sys_cfg = config.clone();
    let sys_tx = tx.clone();
    let sys_handle = thread::spawn(move || sys_loop(sys_cfg, sys_tx));

    let gpu_cfg = config.clone();
    let gpu_handle = thread::spawn(move || gpu_loop(gpu_cfg, tx));

    Sampler {
        rx,
        config,
        handles: vec![sys_handle, gpu_handle],
    }
}

/// Send a sample without blocking. Returns `false` if the consumer has gone away.
fn publish(tx: &SyncSender<Update>, stats: &SamplerStats, update: Update) -> bool {
    match tx.try_send(update) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            // Consumer is behind; keep memory bounded and record the loss.
            stats.dropped.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// Sleep for `duration`, waking early if stop is requested.
fn nap(config: &SamplerConfig, duration: Duration) {
    let deadline = Instant::now() + duration;
    loop {
        if config.stop_requested() {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        thread::sleep((deadline - now).min(NAP_SLICE));
    }
}

fn sys_loop(cfg: Arc<SamplerConfig>, tx: SyncSender<Update>) {
    let mut sys = System::new_all();
    let mut disks = Disks::new_with_refreshed_list();
    let mut disk_io = DiskIoMonitor::new();
    let mut networks = Networks::new_with_refreshed_list();
    let mut components = Components::new_with_refreshed_list();

    // Capability probe: if the platform exposes no sensors, stop paying the WMI/ACPI probe
    // every slow tick. Re-probe rarely so a provider installed at runtime is still found.
    let mut sensors_supported = !components.list().is_empty();
    let mut reprobe_countdown = COMPONENT_REPROBE_TICKS;

    let process_kind = ProcessRefreshKind::nothing().with_cpu().with_memory();

    let mut prev_net: HashMap<String, (u64, u64)> = HashMap::new();
    let mut ifaces: Vec<NetIface> = Vec::new();
    let mut net_rx_rate = 0u64;
    let mut net_tx_rate = 0u64;
    let mut net_rx_total = 0u64;
    let mut net_tx_total = 0u64;
    let mut load = LoadStat::default();
    let mut disk_stats = crate::diskio::DiskIoStats::default();

    // Start gated timers in the past so every slow source refreshes on the first tick.
    let start0 = Instant::now() - Duration::from_secs(3600);
    let mut last_proc = start0;
    let mut last_net = start0;
    let mut last_slow = start0;

    while !cfg.stop_requested() {
        let start = Instant::now();

        sys.refresh_cpu_usage();
        sys.refresh_memory();

        if last_net.elapsed() >= cfg.net() {
            let dt = last_net.elapsed().as_secs_f64().max(0.001);
            networks.refresh(true);
            let agg = build_ifaces(&networks, &mut prev_net, &mut ifaces, dt);
            net_rx_rate = agg.0;
            net_tx_rate = agg.1;
            net_rx_total = agg.2;
            net_tx_total = agg.3;
            last_net = Instant::now();
        }

        if last_proc.elapsed() >= cfg.proc() {
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, process_kind);
            last_proc = Instant::now();
        }

        if last_slow.elapsed() >= cfg.slow() {
            disks.refresh(true);
            disk_stats = disk_io.refresh();
            if sensors_supported {
                components.refresh(true);
            } else if reprobe_countdown == 0 {
                components.refresh(true);
                sensors_supported = !components.list().is_empty();
                reprobe_countdown = COMPONENT_REPROBE_TICKS;
            } else {
                reprobe_countdown -= 1;
            }
            sys.refresh_cpu_frequency();
            let avg = System::load_average();
            load = LoadStat {
                one: avg.one,
                five: avg.five,
                fifteen: avg.fifteen,
            };
            last_slow = Instant::now();
        }

        let snapshot = build_snapshot(
            &sys,
            &disks,
            &components,
            &ifaces,
            NetAgg {
                rx_rate: net_rx_rate,
                tx_rate: net_tx_rate,
                rx_total: net_rx_total,
                tx_total: net_tx_total,
            },
            load,
            disk_stats.clone(),
            start.elapsed(),
        );

        if !publish(&tx, &cfg.stats, Update::Sys(Box::new(snapshot))) {
            break;
        }
        cfg.stats.sys_samples.fetch_add(1, Ordering::Relaxed);

        let elapsed = start.elapsed();
        let target = cfg.fast();
        if elapsed < target {
            nap(&cfg, target - elapsed);
        } else {
            // Overran the budget: record it and back off briefly instead of busy-spinning.
            cfg.stats.overruns.fetch_add(1, Ordering::Relaxed);
            nap(&cfg, OVERRUN_BACKOFF);
        }
    }
}

fn proc_state(status: sysinfo::ProcessStatus) -> ProcState {
    use sysinfo::ProcessStatus as S;
    match status {
        S::Idle => ProcState::Idle,
        S::Run => ProcState::Run,
        S::Sleep => ProcState::Sleep,
        S::Stop => ProcState::Stop,
        S::Zombie => ProcState::Zombie,
        S::Tracing => ProcState::Tracing,
        S::Dead => ProcState::Dead,
        S::Wakekill => ProcState::Wakekill,
        S::Waking => ProcState::Waking,
        S::Parked => ProcState::Parked,
        S::LockBlocked => ProcState::LockBlocked,
        S::UninterruptibleDiskSleep => ProcState::DiskSleep,
        S::Suspended => ProcState::Suspended,
        _ => ProcState::Unknown,
    }
}

#[derive(Clone, Copy, Default)]
struct NetAgg {
    rx_rate: u64,
    tx_rate: u64,
    rx_total: u64,
    tx_total: u64,
}

fn build_ifaces(
    networks: &Networks,
    prev: &mut HashMap<String, (u64, u64)>,
    ifaces: &mut Vec<NetIface>,
    dt: f64,
) -> (u64, u64, u64, u64) {
    ifaces.clear();
    let mut current: HashMap<String, (u64, u64)> = HashMap::new();
    let mut rx_rate = 0u64;
    let mut tx_rate = 0u64;
    let mut rx_total = 0u64;
    let mut tx_total = 0u64;

    for (name, data) in networks.list() {
        let mac = data.mac_address();
        let ip_networks = data.ip_networks();
        // Skip NDIS filter/switch pseudo-interfaces that mirror a real NIC and would
        // otherwise double-count throughput.
        if calc::is_pseudo_interface(mac.is_unspecified(), !ip_networks.is_empty()) {
            continue;
        }

        let rtx = data.total_received();
        let ttx = data.total_transmitted();
        let (prx, ptx) = prev.get(name).copied().unwrap_or((rtx, ttx));
        let rx = calc::counter_rate(rtx, prx, dt);
        let tx = calc::counter_rate(ttx, ptx, dt);
        current.insert(name.clone(), (rtx, ttx));

        rx_total += rtx;
        tx_total += ttx;
        rx_rate += rx;
        tx_rate += tx;

        if rtx == 0 && ttx == 0 {
            continue;
        }
        let ip = ip_networks
            .iter()
            .find(|n| n.addr.is_ipv4())
            .or_else(|| ip_networks.first())
            .map(|n| n.addr.to_string())
            .unwrap_or_else(|| "-".to_string());
        ifaces.push(NetIface {
            name: name.clone(),
            rx_rate: rx,
            tx_rate: tx,
            rx_total: rtx,
            tx_total: ttx,
            ip,
            mac: mac.to_string(),
        });
    }

    // Drop state for interfaces that have gone away.
    *prev = current;
    ifaces.sort_by(|a, b| (b.rx_rate + b.tx_rate).cmp(&(a.rx_rate + a.tx_rate)));
    (rx_rate, tx_rate, rx_total, tx_total)
}

#[allow(clippy::too_many_arguments)]
fn build_snapshot(
    sys: &System,
    disks: &Disks,
    components: &Components,
    ifaces: &[NetIface],
    net: NetAgg,
    load: LoadStat,
    disk_io: crate::diskio::DiskIoStats,
    cost: Duration,
) -> SysSnapshot {
    let cores: Vec<CoreStat> = sys
        .cpus()
        .iter()
        .map(|c| CoreStat {
            usage: c.cpu_usage(),
            freq: c.frequency(),
        })
        .collect();
    let cpu_brand = sys
        .cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .unwrap_or_default();

    let mem = MemStat {
        total: sys.total_memory(),
        used: sys.used_memory(),
        available: sys.available_memory(),
        swap_total: sys.total_swap(),
        swap_used: sys.used_swap(),
    };

    let disk_stats: Vec<DiskStat> = disks
        .list()
        .iter()
        .map(|d| DiskStat {
            name: d.name().to_string_lossy().into_owned(),
            mount: d.mount_point().to_string_lossy().into_owned(),
            fs: d.file_system().to_string_lossy().into_owned(),
            total: d.total_space(),
            avail: d.available_space(),
        })
        .collect();

    let comps: Vec<CompStat> = components
        .list()
        .iter()
        .map(|c| CompStat {
            label: c.label().to_string(),
            temp: c.temperature(),
            max: c.max(),
            critical: c.critical(),
        })
        .collect();

    let logical = sys.cpus().len().max(1);
    let procs: Vec<ProcRow> = sys
        .processes()
        .values()
        .map(|p| {
            // pid / ppid / name / start_time are always retrieved by sysinfo; cpu and memory
            // come from the process refresh kind requested on the process tick.
            let start = p.start_time();
            ProcRow {
                pid: p.pid().as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cpu: calc::process_cpu_percent(p.cpu_usage(), logical),
                mem: p.memory(),
                ppid: p.parent().map(|p| p.as_u32()),
                start_time: (start != 0).then_some(start),
                state: proc_state(p.status()),
            }
        })
        .collect();
    let total_procs = procs.len();

    SysSnapshot {
        cpu_global: sys.global_cpu_usage(),
        cpu_brand,
        cores,
        mem,
        disks: disk_stats,
        disk_io,
        ifaces: ifaces.to_vec(),
        net_rx_rate: net.rx_rate,
        net_tx_rate: net.tx_rate,
        net_rx_total: net.rx_total,
        net_tx_total: net.tx_total,
        comps,
        procs,
        total_procs,
        load,
        generated: Instant::now(),
        sample_cost: cost,
    }
}

fn gpu_loop(cfg: Arc<SamplerConfig>, tx: SyncSender<Update>) {
    let mut gpu = GpuMonitor::new();
    let mut last_ok: Option<(GpuStats, Instant)> = None;

    if gpu.refresh().is_none() {
        cfg.stats.gpu_failures.fetch_add(1, Ordering::Relaxed);
    }

    while !cfg.stop_requested() {
        let interval = cfg.gpu();
        let stats = match gpu.refresh() {
            Some(stats) => {
                last_ok = Some((stats.clone(), Instant::now()));
                stats
            }
            None => {
                // No backend: report it once per interval rather than silently showing 0 %.
                cfg.stats.gpu_failures.fetch_add(1, Ordering::Relaxed);
                match &last_ok {
                    Some((prev, at)) => match freshness(at.elapsed(), interval) {
                        Freshness::Expired => GpuStats::unavailable(),
                        _ => prev.as_stale(),
                    },
                    None => GpuStats::unavailable(),
                }
            }
        };
        if !publish(&tx, &cfg.stats, Update::Gpu(stats)) {
            break;
        }
        cfg.stats.gpu_samples.fetch_add(1, Ordering::Relaxed);
        nap(&cfg, interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu_update() -> Update {
        Update::Gpu(GpuStats::default())
    }

    #[test]
    fn publish_counts_dropped_when_full() {
        let stats = SamplerStats::default();
        let (tx, rx) = mpsc::sync_channel(1);
        assert!(publish(&tx, &stats, gpu_update()));
        // Channel full -> dropped, but the producer keeps going.
        assert!(publish(&tx, &stats, gpu_update()));
        assert_eq!(stats.snapshot().dropped, 1);
        // Consumer gone -> stop.
        drop(rx);
        assert!(!publish(&tx, &stats, gpu_update()));
    }

    #[test]
    fn nap_stops_early_when_requested() {
        let cfg = SamplerConfig::new(Preset::Minimal);
        cfg.request_stop();
        let t = Instant::now();
        nap(&cfg, Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn sampler_shuts_down_promptly() {
        let cfg = Arc::new(SamplerConfig::new(Preset::Minimal));
        let mut sampler = spawn(cfg.clone());
        // Let the workers start a cycle, then ask them to stop.
        thread::sleep(Duration::from_millis(150));
        cfg.request_stop();
        let t = Instant::now();
        sampler.shutdown();
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "shutdown took {:?}",
            t.elapsed()
        );
    }
}
