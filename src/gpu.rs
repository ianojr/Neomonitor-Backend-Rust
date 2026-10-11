use crate::reading::Reading;

/// What kind of adapter a GPU is, as best we can tell.
///
/// The distinction drives which adapter the dashboard treats as "the" GPU and lets the UI
/// explain a laptop's integrated/discrete split. Classification is heuristic (see
/// [`classify_adapter`]) and conservative: anything we cannot identify stays `Unknown`
/// rather than being guessed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GpuKind {
    #[default]
    Unknown,
    /// Integrated into the CPU/SoC; shares system memory.
    Integrated,
    /// A discrete card with its own memory.
    Discrete,
    /// A software/virtual renderer (e.g. Microsoft Basic Render Driver).
    Software,
}

impl GpuKind {
    pub fn label(self) -> &'static str {
        match self {
            GpuKind::Unknown => "unknown",
            GpuKind::Integrated => "integrated",
            GpuKind::Discrete => "discrete",
            GpuKind::Software => "software",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            GpuKind::Unknown => "gpu",
            GpuKind::Integrated => "iGPU",
            GpuKind::Discrete => "dGPU",
            GpuKind::Software => "swGPU",
        }
    }
}

/// Telemetry for one adapter, keyed by a stable identity (`id`).
///
/// Each field is a [`Reading`] so "this adapter does not expose engine utilisation" stays
/// distinct from "this adapter is idle".
#[derive(Clone, Default)]
pub struct GpuAdapter {
    /// Stable identity. On Windows this is the canonical adapter LUID; elsewhere it may be a
    /// name. Used to keep associations correct across samples.
    pub id: String,
    pub name: String,
    pub kind: GpuKind,
    pub driver: String,
    pub utilization: Reading<f32>,
    pub dedicated_used: Reading<u64>,
    pub shared_used: Reading<u64>,
    pub dedicated_total: Reading<u64>,
    pub shared_total: Reading<u64>,
    pub temperature: Reading<f32>,
}

impl GpuAdapter {
    fn as_stale(&self) -> Self {
        Self {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind,
            driver: self.driver.clone(),
            utilization: self.utilization.as_stale(),
            dedicated_used: self.dedicated_used.as_stale(),
            shared_used: self.shared_used.as_stale(),
            dedicated_total: self.dedicated_total.as_stale(),
            shared_total: self.shared_total.as_stale(),
            temperature: self.temperature.as_stale(),
        }
    }
}

/// A complete snapshot of GPU telemetry.
///
/// The scalar fields mirror [`GpuStats::primary_adapter`] for the common single-GPU case;
/// [`GpuStats::adapters`] always holds the full per-adapter list so multiple GPUs are not
/// collapsed into one number. Everything is a [`Reading`] because GPU data can be genuinely
/// unavailable (unsupported platform, no adapter, WMI/PDH failure): `0.0 %` and `Unavailable`
/// must never be confused.
#[derive(Clone, Default)]
pub struct GpuStats {
    pub name: String,
    pub driver: String,
    pub utilization: Reading<f32>,
    pub dedicated_used: Reading<u64>,
    pub shared_used: Reading<u64>,
    pub dedicated_total: Reading<u64>,
    pub temperature: Reading<f32>,
    /// Every adapter found, best (non-software, busiest) first.
    pub adapters: Vec<GpuAdapter>,
    /// Index into `adapters` of the adapter the scalar fields describe.
    pub primary: Option<usize>,
}

impl GpuStats {
    /// All fields unavailable.
    pub fn unavailable() -> Self {
        Self::default()
    }

    /// Return a copy with all available values demoted to stale.
    pub fn as_stale(&self) -> Self {
        Self {
            name: self.name.clone(),
            driver: self.driver.clone(),
            utilization: self.utilization.as_stale(),
            dedicated_used: self.dedicated_used.as_stale(),
            shared_used: self.shared_used.as_stale(),
            dedicated_total: self.dedicated_total.as_stale(),
            temperature: self.temperature.as_stale(),
            adapters: self.adapters.iter().map(GpuAdapter::as_stale).collect(),
            primary: self.primary,
        }
    }

    /// The adapter the scalar fields mirror, if any.
    pub fn primary_adapter(&self) -> Option<&GpuAdapter> {
        self.primary.and_then(|i| self.adapters.get(i))
    }
}

/// Heuristically classify an adapter from its description, PCI vendor id and the DXGI
/// "software adapter" flag.
///
/// Name matching runs before the vendor fallback because a vendor can ship both integrated
/// and discrete parts (Intel Arc is discrete; AMD Radeon RX is discrete while Radeon/Vega
/// graphics on an APU is integrated). The intent is to be obviously right on common names
/// and to prefer [`GpuKind::Unknown`] otherwise, never to be confidently wrong.
pub fn classify_adapter(name: &str, vendor_id: u32, software: bool) -> GpuKind {
    if software {
        return GpuKind::Software;
    }
    let n = name.to_ascii_lowercase();
    if n.contains("basic render") || n.contains("virtual") || n.contains("remote display") {
        return GpuKind::Software;
    }
    // Discrete-only product lines.
    if n.contains("geforce")
        || n.contains("nvidia")
        || n.contains("quadro")
        || n.contains("rtx")
        || n.contains("tesla")
        || n.contains("radeon rx")
        || n.contains("radeon pro")
        || n.contains("intel arc")
        || n.contains("arc a")
        || n.contains("arc b")
        || n.contains("arc(")
    {
        return GpuKind::Discrete;
    }
    // Integrated signals.
    if n.contains("intel") && (n.contains("uhd") || n.contains("hd graphics") || n.contains("iris"))
    {
        return GpuKind::Integrated;
    }
    if (n.contains("radeon") || n.contains("vega") || n.contains("apu")) && !n.contains("rx ") {
        return GpuKind::Integrated;
    }
    match vendor_id {
        0x10DE => GpuKind::Discrete, // NVIDIA
        0x8086 => GpuKind::Integrated,
        0x1002 | 0x1022 => GpuKind::Integrated, // AMD/AMD; discrete variants matched above
        _ => GpuKind::Unknown,
    }
}

/// Choose the adapter the scalar fields should describe.
///
/// Preference order: non-software before software, then highest engine utilisation, then
/// largest committed dedicated memory, then enumeration order. Utilisation leads because
/// "the GPU doing work" matches user intent; memory is only a tie-break, since the adapter
/// with the largest allocation is not necessarily the one in use.
pub fn pick_primary(adapters: &[GpuAdapter]) -> Option<usize> {
    let rank = |a: &GpuAdapter| {
        (
            !matches!(a.kind, GpuKind::Software),
            a.utilization.value().copied().unwrap_or(f32::MIN),
            a.dedicated_used.value().copied().unwrap_or(0),
        )
    };
    let mut best: Option<usize> = None;
    for (i, adapter) in adapters.iter().enumerate() {
        match best {
            None => best = Some(i),
            Some(b) if rank(adapter) > rank(&adapters[b]) => best = Some(i),
            _ => {}
        }
    }
    best
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;

    use serde::Deserialize;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
    };
    use wmi::WMIConnection;

    use crate::calc;
    use crate::platform::pdh::PdhCounter;
    use crate::reading::Reading;

    use super::{GpuAdapter, GpuKind, GpuStats, classify_adapter, pick_primary};

    const ENGINE_PATH: &str = "\\GPU Engine(*)\\Utilization Percentage";
    const MEM_DEDICATED_PATH: &str = "\\GPU Adapter Memory(*)\\Dedicated Usage";
    const MEM_SHARED_PATH: &str = "\\GPU Adapter Memory(*)\\Shared Usage";

    const CONTROLLER_QUERY: &str =
        "SELECT Name, DriverVersion, AdapterRAM FROM Win32_VideoController";
    const ENGINE_QUERY: &str = "SELECT Name, UtilizationPercentage FROM \
         Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine";
    const MEM_QUERY: &str = "SELECT DedicatedUsage, SharedUsage FROM \
         Win32_PerfFormattedData_GPUPerformanceCounters_GPUAdapterMemory";
    // LibreHardwareMonitor exposes its sensors under this namespace; absent unless the user
    // has installed and run it, hence optional.
    const LHM_SENSOR_QUERY: &str = "SELECT Name, SensorType, Value FROM Sensor";

    /// Refresh the DXGI/WMI adapter list every this many GPU ticks (nominal ~20 s at the
    /// default cadence) so hot-plug, driver resets and newly installed LHM are noticed
    /// without paying the enumeration cost on every sample.
    const ADAPTER_REPROBE_TICKS: u32 = 20;

    #[derive(Deserialize)]
    struct VideoController {
        #[serde(rename = "Name")]
        name: Option<String>,
        #[serde(rename = "DriverVersion")]
        driver_version: Option<String>,
        #[serde(rename = "AdapterRAM")]
        adapter_ram: Option<u64>,
    }

    #[derive(Deserialize)]
    struct GpuEngine {
        #[serde(rename = "Name")]
        name: Option<String>,
        #[serde(rename = "UtilizationPercentage")]
        utilization: Option<u64>,
    }

    #[derive(Deserialize)]
    struct GpuAdapterMemory {
        #[serde(rename = "DedicatedUsage")]
        dedicated_usage: Option<u64>,
        #[serde(rename = "SharedUsage")]
        shared_usage: Option<u64>,
    }

    #[derive(Deserialize)]
    struct LhmSensor {
        #[serde(rename = "Name")]
        name: Option<String>,
        #[serde(rename = "SensorType")]
        sensor_type: Option<String>,
        #[serde(rename = "Value")]
        value: Option<f32>,
    }

    /// One DXGI adapter: identity and totals that PDH does not provide.
    struct AdapterInfo {
        id: String,
        alt_id: String,
        name: String,
        kind: GpuKind,
        driver: String,
        dedicated_total: Option<u64>,
        shared_total: Option<u64>,
    }

    pub struct GpuMonitor {
        conn: Option<WMIConnection>,
        engine: Option<PdhCounter>,
        mem_dedicated: Option<PdhCounter>,
        mem_shared: Option<PdhCounter>,
        /// Optional LibreHardwareMonitor connection for GPU temperature.
        lhm: Option<WMIConnection>,
        adapters: Vec<AdapterInfo>,
        /// Fallback identity used when DXGI enumeration fails (name from WMI).
        fallback_name: String,
        fallback_driver: String,
        fallback_total: Reading<u64>,
        ticks: u32,
    }

    impl GpuMonitor {
        pub fn new() -> Self {
            // WMI first: establishing it also initialises COM on this thread, which DXGI and
            // PDH lean on.
            let conn = WMIConnection::new().ok();
            let engine = PdhCounter::new(ENGINE_PATH);
            let mem_dedicated = PdhCounter::new(MEM_DEDICATED_PATH);
            let mem_shared = PdhCounter::new(MEM_SHARED_PATH);
            let lhm = open_lhm();

            let mut monitor = Self {
                conn,
                engine,
                mem_dedicated,
                mem_shared,
                lhm,
                adapters: Vec::new(),
                fallback_name: String::new(),
                fallback_driver: String::new(),
                fallback_total: Reading::Unavailable,
                ticks: ADAPTER_REPROBE_TICKS,
            };
            monitor.reprobe();
            monitor
        }

        /// (Re)enumerate adapters and refresh the cached name/driver/total metadata.
        fn reprobe(&mut self) {
            self.ticks = 0;
            if self.lhm.is_none() {
                self.lhm = open_lhm();
            }

            let drivers = self.driver_map();
            let dxgi = enumerate_dxgi();
            let mut adapters = Vec::new();
            if let Some(cards) = &dxgi {
                for card in cards {
                    let driver = drivers
                        .get(&card.name.to_ascii_lowercase())
                        .cloned()
                        .unwrap_or_default();
                    adapters.push(AdapterInfo {
                        id: card.id.clone(),
                        alt_id: card.alt_id.clone(),
                        name: card.name.clone(),
                        kind: classify_adapter(&card.name, card.vendor_id, card.software),
                        driver,
                        dedicated_total: (card.dedicated_video > 0).then_some(card.dedicated_video),
                        shared_total: (card.shared_system > 0).then_some(card.shared_system),
                    });
                }
            }
            adapters.sort_by_key(|a| matches!(a.kind, GpuKind::Software));
            self.adapters = adapters;

            self.fallback_name.clear();
            self.fallback_driver.clear();
            self.fallback_total = Reading::Unavailable;
            if self.adapters.is_empty()
                && let Some(conn) = &self.conn
                && let Ok(cards) = conn.raw_query::<VideoController>(CONTROLLER_QUERY)
                && let Some(best) = cards.iter().max_by_key(|c| c.adapter_ram.unwrap_or(0))
            {
                self.fallback_name = best.name.clone().unwrap_or_default();
                self.fallback_driver = best.driver_version.clone().unwrap_or_default();
                let ram = best.adapter_ram.unwrap_or(0);
                if cards.len() == 1 && ram > 0 {
                    // Adapter-reported aperture: labelled as such by the UI.
                    self.fallback_total = Reading::Valid(ram);
                }
            }
        }

        fn driver_map(&self) -> HashMap<String, String> {
            let Some(conn) = &self.conn else {
                return HashMap::new();
            };
            conn.raw_query::<VideoController>(CONTROLLER_QUERY)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|c| {
                    let name = c.name?.to_ascii_lowercase();
                    Some((name, c.driver_version.unwrap_or_default()))
                })
                .collect()
        }

        /// Returns `None` only when no GPU backend is available at all.
        pub fn refresh(&mut self) -> Option<GpuStats> {
            if self.conn.is_none()
                && self.engine.is_none()
                && self.mem_dedicated.is_none()
                && self.adapters.is_empty()
                && self.fallback_name.is_empty()
            {
                return None;
            }
            self.ticks = self.ticks.saturating_add(1);
            if self.ticks >= ADAPTER_REPROBE_TICKS {
                self.reprobe();
            }

            let engine_items = self.engine_items();
            let util_by_adapter = calc::gpu_engine_utilization_by_adapter(&engine_items);
            let ded = self
                .mem_dedicated
                .as_ref()
                .map(PdhCounter::collect)
                .unwrap_or_default();
            let shar = self
                .mem_shared
                .as_ref()
                .map(PdhCounter::collect)
                .unwrap_or_default();
            let mem_by_adapter = calc::gpu_memory_by_adapter(&ded, &shar);
            let temperature = self.gpu_temperature();

            if self.adapters.is_empty() {
                return Some(self.fallback_stats(engine_items, temperature));
            }

            let mut adapters: Vec<GpuAdapter> = self
                .adapters
                .iter()
                .map(|info| {
                    let util = lookup(&util_by_adapter, &info.id, &info.alt_id)
                        .map(Reading::Valid)
                        .unwrap_or(Reading::Unavailable);
                    let (ded_used, shared_used) = lookup(&mem_by_adapter, &info.id, &info.alt_id)
                        .map(|(d, s)| (Reading::Valid(d), Reading::Valid(s)))
                        .unwrap_or((Reading::Unavailable, Reading::Unavailable));
                    GpuAdapter {
                        id: info.id.clone(),
                        name: info.name.clone(),
                        kind: info.kind,
                        driver: info.driver.clone(),
                        utilization: util,
                        dedicated_used: ded_used,
                        shared_used,
                        dedicated_total: info
                            .dedicated_total
                            .map(Reading::Valid)
                            .unwrap_or(Reading::Unavailable),
                        shared_total: info
                            .shared_total
                            .map(Reading::Valid)
                            .unwrap_or(Reading::Unavailable),
                        temperature: Reading::Unavailable,
                    }
                })
                .collect();

            let primary = pick_primary(&adapters);
            if let Some(idx) = primary {
                // Attach the (best-effort, provider-level) temperature to the primary adapter.
                if let Some(t) = temperature {
                    adapters[idx].temperature = Reading::Valid(t);
                }
            }
            let primary_adapter = primary.map(|i| adapters[i].clone());

            let mut stats = GpuStats {
                adapters,
                primary,
                ..GpuStats::default()
            };
            if let Some(p) = &primary_adapter {
                stats.name = p.name.clone();
                stats.driver = p.driver.clone();
                stats.utilization = p.utilization;
                stats.dedicated_used = p.dedicated_used;
                stats.shared_used = p.shared_used;
                stats.dedicated_total = p.dedicated_total;
                stats.temperature = p.temperature;
            }
            Some(stats)
        }

        /// Single-adapter aggregate used only when DXGI enumeration is unavailable.
        fn fallback_stats(
            &self,
            engine_items: Vec<(String, f64)>,
            temperature: Option<f32>,
        ) -> GpuStats {
            let utilization = calc::gpu_engine_utilization(&engine_items)
                .map(Reading::Valid)
                .unwrap_or(Reading::Unavailable);
            let (dedicated_used, shared_used) = self.memory_fallback();
            GpuStats {
                name: self.fallback_name.clone(),
                driver: self.fallback_driver.clone(),
                utilization,
                dedicated_used,
                shared_used,
                dedicated_total: self.fallback_total,
                temperature: temperature
                    .map(Reading::Valid)
                    .unwrap_or(Reading::Unavailable),
                adapters: Vec::new(),
                primary: None,
            }
        }

        /// Raw `(instance_name, utilisation_percent)` pairs from PDH, falling back to WMI.
        fn engine_items(&self) -> Vec<(String, f64)> {
            if let Some(engine) = &self.engine {
                let items = engine.collect();
                if !items.is_empty() {
                    return items;
                }
            }
            match &self.conn {
                Some(conn) => {
                    let engines: Vec<GpuEngine> = conn.raw_query(ENGINE_QUERY).unwrap_or_default();
                    engines
                        .into_iter()
                        .filter_map(|e| Some((e.name?, e.utilization.unwrap_or(0) as f64)))
                        .collect()
                }
                None => Vec::new(),
            }
        }

        /// Per-adapter memory via PDH, falling back to a single busiest WMI adapter.
        fn memory_fallback(&self) -> (Reading<u64>, Reading<u64>) {
            if let (Some(ded), Some(shared)) = (&self.mem_dedicated, &self.mem_shared)
                && let Some((d, s)) = calc::adapter_memory(&ded.collect(), &shared.collect())
            {
                return (Reading::Valid(d), Reading::Valid(s));
            }
            match &self.conn {
                Some(conn) => {
                    let mem: Vec<GpuAdapterMemory> = conn.raw_query(MEM_QUERY).unwrap_or_default();
                    match mem.iter().max_by_key(|m| {
                        m.dedicated_usage.unwrap_or(0) + m.shared_usage.unwrap_or(0)
                    }) {
                        Some(m) => (
                            Reading::Valid(m.dedicated_usage.unwrap_or(0)),
                            Reading::Valid(m.shared_usage.unwrap_or(0)),
                        ),
                        None => (Reading::Unavailable, Reading::Unavailable),
                    }
                }
                None => (Reading::Unavailable, Reading::Unavailable),
            }
        }

        /// Best-effort GPU temperature from LibreHardwareMonitor, if it is installed.
        fn gpu_temperature(&self) -> Option<f32> {
            let conn = self.lhm.as_ref()?;
            let sensors: Vec<LhmSensor> = conn.raw_query(LHM_SENSOR_QUERY).ok()?;
            let rows: Vec<(String, String, f32)> = sensors
                .into_iter()
                .filter_map(|s| Some((s.name?, s.sensor_type?, s.value.unwrap_or(f32::NAN))))
                .collect();
            calc::lhm_gpu_temperature(&rows)
        }
    }

    fn lookup<T: Copy>(map: &HashMap<String, T>, a: &str, b: &str) -> Option<T> {
        map.get(a).or_else(|| map.get(b)).copied()
    }

    /// Open a connection to LibreHardwareMonitor's namespace, if present.
    fn open_lhm() -> Option<WMIConnection> {
        WMIConnection::with_namespace_path("ROOT\\LibreHardwareMonitor").ok()
    }

    /// A DXGI adapter description, normalised into platform-independent fields.
    struct DxgiCard {
        id: String,
        alt_id: String,
        name: String,
        vendor_id: u32,
        software: bool,
        dedicated_video: u64,
        shared_system: u64,
    }

    /// Enumerate hardware and software adapters through DXGI.
    ///
    /// DXGI is the same interface Task Manager and games use, and it exposes the adapter LUID
    /// that ties a card to its PDH counters, plus dedicated/shared memory totals that PDH
    /// does not report. Returns `None` (not an empty list) when DXGI itself is unavailable.
    fn enumerate_dxgi() -> Option<Vec<DxgiCard>> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let mut cards = Vec::new();
            let mut index = 0u32;
            loop {
                let Ok(adapter) = factory.EnumAdapters1(index) else {
                    break;
                };
                index += 1;
                let Ok(desc) = adapter.GetDesc1() else {
                    continue;
                };
                cards.push(card_from_desc(&desc));
            }
            Some(cards)
        }
    }

    fn card_from_desc(desc: &DXGI_ADAPTER_DESC1) -> DxgiCard {
        let high = desc.AdapterLuid.HighPart as u32;
        let low = desc.AdapterLuid.LowPart;
        let [id, alt_id] = calc::luid_candidates(high, low);
        DxgiCard {
            id,
            alt_id,
            name: wide_to_string(&desc.Description),
            vendor_id: desc.VendorId,
            software: (desc.Flags as i32) & DXGI_ADAPTER_FLAG_SOFTWARE.0 != 0,
            dedicated_video: desc.DedicatedVideoMemory as u64,
            shared_system: desc.SharedSystemMemory as u64,
        }
    }

    fn wide_to_string(wide: &[u16]) -> String {
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        String::from_utf16_lossy(&wide[..end])
    }
}

#[cfg(not(windows))]
mod imp {
    use super::GpuStats;

    pub struct GpuMonitor;

    impl GpuMonitor {
        pub fn new() -> Self {
            Self
        }

        /// No GPU backend exists on this platform; the capability model reports it as
        /// unsupported rather than this appearing as a transient failure.
        pub fn refresh(&mut self) -> Option<GpuStats> {
            None
        }
    }
}

pub use imp::GpuMonitor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_common_adapters() {
        assert_eq!(
            classify_adapter("Intel(R) UHD Graphics 620", 0x8086, false),
            GpuKind::Integrated
        );
        assert_eq!(
            classify_adapter("NVIDIA GeForce RTX 3060", 0x10DE, false),
            GpuKind::Discrete
        );
        assert_eq!(
            classify_adapter("AMD Radeon RX 6800 XT", 0x1002, false),
            GpuKind::Discrete
        );
        assert_eq!(
            classify_adapter("AMD Radeon(TM) Graphics", 0x1002, false),
            GpuKind::Integrated
        );
        assert_eq!(
            classify_adapter("Intel(R) Arc(TM) A770 Graphics", 0x8086, false),
            GpuKind::Discrete
        );
        // The DXGI software flag wins regardless of the name.
        assert_eq!(
            classify_adapter("NVIDIA GeForce RTX 4090", 0x10DE, true),
            GpuKind::Software
        );
        assert_eq!(
            classify_adapter("Microsoft Basic Render Driver", 0x1414, false),
            GpuKind::Software
        );
        // Vendor fallback only when the name is uninformative.
        assert_eq!(
            classify_adapter("Mystery 3D", 0xDEAD, false),
            GpuKind::Unknown
        );
        assert_eq!(
            classify_adapter("Mystery 3D", 0x10DE, false),
            GpuKind::Discrete
        );
    }

    fn adapter(kind: GpuKind, util: Option<f32>, ded: Option<u64>) -> GpuAdapter {
        GpuAdapter {
            kind,
            utilization: util.map(Reading::Valid).unwrap_or(Reading::Unavailable),
            dedicated_used: ded.map(Reading::Valid).unwrap_or(Reading::Unavailable),
            ..GpuAdapter::default()
        }
    }

    #[test]
    fn primary_prefers_hardware_then_activity() {
        let adapters = vec![
            adapter(GpuKind::Software, Some(99.0), Some(0)),
            adapter(GpuKind::Integrated, Some(10.0), Some(100)),
            adapter(GpuKind::Discrete, Some(40.0), Some(50)),
        ];
        // Software adapters never win even with high utilisation; the discrete card is
        // busier, so it leads despite the integrated card holding more memory.
        assert_eq!(pick_primary(&adapters), Some(2));
    }

    #[test]
    fn primary_falls_back_to_memory_when_idle() {
        let adapters = vec![
            adapter(GpuKind::Integrated, None, Some(100)),
            adapter(GpuKind::Discrete, None, Some(4000)),
        ];
        assert_eq!(pick_primary(&adapters), Some(1));
        assert_eq!(pick_primary(&[]), None);
    }

    #[test]
    fn stale_preserves_adapters_and_primary() {
        let mut stats = GpuStats {
            name: "GPU".into(),
            adapters: vec![adapter(GpuKind::Discrete, Some(25.0), Some(10))],
            primary: Some(0),
            ..GpuStats::default()
        };
        stats.utilization = Reading::Valid(25.0);
        let stale = stats.as_stale();
        assert_eq!(stale.primary, Some(0));
        assert_eq!(stale.adapters.len(), 1);
        assert_eq!(stale.utilization, Reading::Stale(25.0));
        assert_eq!(stale.adapters[0].utilization, Reading::Stale(25.0));
        assert_eq!(stale.adapters[0].dedicated_used, Reading::Stale(10));
        // Unavailable values stay unavailable when demoted.
        assert_eq!(stale.adapters[0].temperature, Reading::Unavailable);
    }
}
