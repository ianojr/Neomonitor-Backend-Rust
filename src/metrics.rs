use std::time::{Duration, Instant};

use crate::diskio::DiskIoStats;
use crate::gpu::GpuStats;

#[derive(Clone, Default)]
pub struct CoreStat {
    pub usage: f32,
    pub freq: u64,
}

#[derive(Clone, Default)]
pub struct MemStat {
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

#[derive(Clone, Default)]
pub struct DiskStat {
    pub name: String,
    pub mount: String,
    pub fs: String,
    pub total: u64,
    pub avail: u64,
}

#[derive(Clone, Default)]
pub struct NetIface {
    pub name: String,
    pub rx_rate: u64,
    pub tx_rate: u64,
    pub rx_total: u64,
    pub tx_total: u64,
    pub ip: String,
    pub mac: String,
}

#[derive(Clone, Default)]
pub struct CompStat {
    pub label: String,
    pub temp: Option<f32>,
    /// Highest temperature observed so far (sysinfo's running maximum), not a limit.
    pub max: Option<f32>,
    /// Chip/kernel critical threshold, when the platform reports one.
    pub critical: Option<f32>,
}

/// Process execution state.
///
/// Windows only ever reports [`ProcState::Run`] through sysinfo (the field is hard-coded at
/// construction — see the platform notes in `docs/engineering/10-process-explorer.md`), so on
/// this platform the state column carries little information. Other OSes report richer states.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ProcState {
    #[default]
    Unknown,
    Run,
    Sleep,
    Idle,
    Stop,
    Zombie,
    Tracing,
    Dead,
    Wakekill,
    Waking,
    Parked,
    LockBlocked,
    DiskSleep,
    Suspended,
}

impl ProcState {
    pub fn label(self) -> &'static str {
        match self {
            ProcState::Unknown => "?",
            ProcState::Run => "run",
            ProcState::Sleep => "sleep",
            ProcState::Idle => "idle",
            ProcState::Stop => "stop",
            ProcState::Zombie => "zombie",
            ProcState::Tracing => "tracing",
            ProcState::Dead => "dead",
            ProcState::Wakekill => "wakekill",
            ProcState::Waking => "waking",
            ProcState::Parked => "parked",
            ProcState::LockBlocked => "lock",
            ProcState::DiskSleep => "d-sleep",
            ProcState::Suspended => "suspended",
        }
    }
}

/// Cheap per-process row collected on every process tick.
///
/// `pid`, `ppid`, `name` and `start_time` are always retrieved by sysinfo; `cpu`/`mem` require
/// the corresponding refresh kinds. Expensive metadata (executable path, command line, owner,
/// working directory) is *not* collected here — see [`ProcDetail`] and `ProcProbe`.
#[derive(Clone, Default)]
pub struct ProcRow {
    pub pid: u32,
    pub name: String,
    pub cpu: f32,
    pub mem: u64,
    pub ppid: Option<u32>,
    /// Process start time, seconds since the Unix epoch. `None` when the OS did not expose it
    /// (Windows reports `0` for processes whose handle could not be opened).
    pub start_time: Option<u64>,
    pub state: ProcState,
}

/// On-demand, expensive detail for a single process, gathered only for the details view.
#[derive(Clone, Default)]
pub struct ProcDetail {
    pub pid: u32,
    pub name: String,
    pub exe: Option<String>,
    pub cmd: Option<String>,
    pub user: Option<String>,
    pub cwd: Option<String>,
    pub state: ProcState,
    pub ppid: Option<u32>,
    pub start_time: Option<u64>,
    pub run_time: u64,
    pub cpu: f32,
    pub mem: u64,
    pub virtual_mem: u64,
    pub disk_read: u64,
    pub disk_written: u64,
    pub env_count: usize,
}

/// Identity used to revalidate a process before a destructive action, because a PID may be
/// reused by a different process between selection and confirmation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProcIdentity {
    pub pid: u32,
    pub name: String,
    pub start_time: Option<u64>,
}

impl ProcIdentity {
    pub fn of(row: &ProcRow) -> Self {
        Self {
            pid: row.pid,
            name: row.name.clone(),
            start_time: row.start_time,
        }
    }

    /// Whether a freshly observed process is still the same one we selected.
    ///
    /// `start_time` is the strong signal (a reused PID gets a new start time). When either side
    /// lacks a start time we fall back to the name, and refuse only on a clear mismatch.
    pub fn matches(&self, pid: u32, name: &str, start_time: Option<u64>) -> bool {
        if pid != self.pid {
            return false;
        }
        match (self.start_time, start_time) {
            (Some(a), Some(b)) => a == b,
            _ => self.name == name,
        }
    }
}

/// Smoothed queue-length-derived load, refreshed on the slow tick.
///
/// On Windows this is *not* Unix `loadavg`; see `docs/engineering/06-metric-semantics.md`.
#[derive(Clone, Copy, Default)]
pub struct LoadStat {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

#[derive(Clone)]
pub struct SysSnapshot {
    pub cpu_global: f32,
    pub cpu_brand: String,
    pub cores: Vec<CoreStat>,
    pub mem: MemStat,
    pub disks: Vec<DiskStat>,
    /// Machine-wide physical disk throughput (aggregate `_Total`).
    pub disk_io: DiskIoStats,
    pub ifaces: Vec<NetIface>,
    pub net_rx_rate: u64,
    pub net_tx_rate: u64,
    pub net_rx_total: u64,
    pub net_tx_total: u64,
    pub comps: Vec<CompStat>,
    pub procs: Vec<ProcRow>,
    pub total_procs: usize,
    pub load: LoadStat,
    /// Wall-clock instant the snapshot finished collecting; lets the UI report data age.
    pub generated: Instant,
    pub sample_cost: Duration,
}

impl Default for SysSnapshot {
    fn default() -> Self {
        Self {
            cpu_global: 0.0,
            cpu_brand: String::new(),
            cores: Vec::new(),
            mem: MemStat::default(),
            disks: Vec::new(),
            disk_io: DiskIoStats::default(),
            ifaces: Vec::new(),
            net_rx_rate: 0,
            net_tx_rate: 0,
            net_rx_total: 0,
            net_tx_total: 0,
            comps: Vec::new(),
            procs: Vec::new(),
            total_procs: 0,
            load: LoadStat::default(),
            generated: Instant::now(),
            sample_cost: Duration::ZERO,
        }
    }
}

pub enum Update {
    Sys(Box<SysSnapshot>),
    Gpu(GpuStats),
}
