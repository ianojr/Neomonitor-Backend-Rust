//! Platform capability model.
//!
//! NeoMonitor collects from several OS interfaces, and their availability differs by
//! platform and even by machine (a laptop may expose no thermal zone; a desktop may expose
//! several). This module declares, per capability, whether the *platform* can provide it at
//! all, separately from whether a value happens to be available right now.
//!
//! That distinction matters for the UI: "no backend on this OS" and "backend present but
//! this sample failed / no such hardware" must not look the same. Runtime availability is
//! still represented by [`crate::reading::Reading`]; this module describes the ceiling.
//!
//! Extending support means adding an entry here *and* a real backend — the two must agree.

#[cfg(windows)]
pub mod pdh;

/// What a platform can provide for one capability.
///
/// The `&'static str` on the non-`Supported` variants is a short, stable explanation that is
/// safe to print (it never embeds a runtime value).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Support {
    /// A real backend exists and is expected to work without extra software.
    Supported,
    /// A backend exists but has a real limitation (missing hardware, extra software or
    /// privileges required, approximate semantics).
    Partial(&'static str),
    /// No backend exists on this platform; the metric is not collected.
    Unsupported(&'static str),
}

impl Support {
    /// Whether the platform can produce *some* value for the capability.
    pub fn is_available(self) -> bool {
        !matches!(self, Support::Unsupported(_))
    }

    /// Whether the platform can produce a value without caveats.
    pub fn is_full(self) -> bool {
        matches!(self, Support::Supported)
    }

    /// Short status word for tables/probe output.
    pub fn label(self) -> &'static str {
        match self {
            Support::Supported => "yes",
            Support::Partial(_) => "partial",
            Support::Unsupported(_) => "no",
        }
    }

    /// The explanation for a non-full capability.
    pub fn detail(self) -> Option<&'static str> {
        match self {
            Support::Supported => None,
            Support::Partial(reason) | Support::Unsupported(reason) => Some(reason),
        }
    }
}

/// The capability ceiling for the current build/platform.
#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    pub gpu_utilization: Support,
    pub gpu_multiple_adapters: Support,
    pub gpu_memory: Support,
    pub gpu_temperature: Support,
    pub gpu_per_process: Support,
    pub cpu_temperature: Support,
    pub fan_speed: Support,
    pub power_consumption: Support,
    pub clock_speed: Support,
    pub disk_activity: Support,
    pub load_average: Support,
}

impl Capabilities {
    /// Report the ceiling for the OS this binary was built for.
    pub fn detect() -> Self {
        detect_os()
    }

    /// Stable, ordered rows for CLI/diagnostic output.
    pub fn rows(&self) -> [(&'static str, Support); 11] {
        [
            ("GPU utilisation", self.gpu_utilization),
            ("GPU multiple adapters", self.gpu_multiple_adapters),
            ("GPU dedicated/shared memory", self.gpu_memory),
            ("GPU temperature", self.gpu_temperature),
            ("Per-process GPU", self.gpu_per_process),
            ("CPU temperature", self.cpu_temperature),
            ("Fan speed", self.fan_speed),
            ("Power consumption", self.power_consumption),
            ("CPU clock speed", self.clock_speed),
            ("Disk activity (throughput)", self.disk_activity),
            ("Load average", self.load_average),
        ]
    }
}

#[cfg(windows)]
fn detect_os() -> Capabilities {
    Capabilities {
        gpu_utilization: Support::Supported,
        gpu_multiple_adapters: Support::Supported,
        gpu_memory: Support::Supported,
        gpu_temperature: Support::Partial(
            "needs LibreHardwareMonitor; Windows exposes no built-in GPU temperature",
        ),
        gpu_per_process: Support::Unsupported("engine instances are aggregated across processes"),
        cpu_temperature: Support::Partial("ACPI thermal zone only, and often absent"),
        fan_speed: Support::Unsupported("no supported OS interface"),
        power_consumption: Support::Unsupported("needs RAPL or vendor tooling"),
        clock_speed: Support::Supported,
        disk_activity: Support::Supported,
        load_average: Support::Partial("PDH queue-length approximation, not Unix loadavg"),
    }
}

#[cfg(target_os = "linux")]
fn detect_os() -> Capabilities {
    Capabilities {
        gpu_utilization: Support::Unsupported("no GPU backend implemented on Linux"),
        gpu_multiple_adapters: Support::Unsupported("no GPU backend implemented on Linux"),
        gpu_memory: Support::Unsupported("no GPU backend implemented on Linux"),
        gpu_temperature: Support::Unsupported("no GPU backend implemented on Linux"),
        gpu_per_process: Support::Unsupported("no GPU backend implemented on Linux"),
        cpu_temperature: Support::Partial("sysinfo Components/hwmon; availability varies"),
        fan_speed: Support::Unsupported("no backend implemented"),
        power_consumption: Support::Unsupported("RAPL not implemented"),
        clock_speed: Support::Supported,
        disk_activity: Support::Unsupported("no disk-throughput backend implemented on Linux"),
        load_average: Support::Supported,
    }
}

#[cfg(target_os = "macos")]
fn detect_os() -> Capabilities {
    Capabilities {
        gpu_utilization: Support::Unsupported("no GPU backend implemented on macOS"),
        gpu_multiple_adapters: Support::Unsupported("no GPU backend implemented on macOS"),
        gpu_memory: Support::Unsupported("no GPU backend implemented on macOS"),
        gpu_temperature: Support::Unsupported("no GPU backend implemented on macOS"),
        gpu_per_process: Support::Unsupported("no GPU backend implemented on macOS"),
        cpu_temperature: Support::Partial("SMC sensors via sysinfo; availability varies"),
        fan_speed: Support::Unsupported("no backend implemented"),
        power_consumption: Support::Unsupported("no backend implemented"),
        clock_speed: Support::Supported,
        disk_activity: Support::Unsupported("no disk-throughput backend implemented on macOS"),
        load_average: Support::Supported,
    }
}

/// Any other target: nothing beyond sysinfo's portable surface is claimed.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn detect_os() -> Capabilities {
    Capabilities {
        gpu_utilization: Support::Unsupported("no GPU backend implemented on this platform"),
        gpu_multiple_adapters: Support::Unsupported("no GPU backend implemented on this platform"),
        gpu_memory: Support::Unsupported("no GPU backend implemented on this platform"),
        gpu_temperature: Support::Unsupported("no GPU backend implemented on this platform"),
        gpu_per_process: Support::Unsupported("no GPU backend implemented on this platform"),
        cpu_temperature: Support::Partial("sysinfo Components; availability varies"),
        fan_speed: Support::Unsupported("no backend implemented"),
        power_consumption: Support::Unsupported("no backend implemented"),
        clock_speed: Support::Supported,
        disk_activity: Support::Unsupported("no disk-throughput backend implemented"),
        load_average: Support::Partial("sysinfo load average; semantics platform-dependent"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_predicates_and_labels() {
        assert!(Support::Supported.is_available());
        assert!(Support::Supported.is_full());
        assert!(Support::Partial("x").is_available());
        assert!(!Support::Partial("x").is_full());
        assert!(!Support::Unsupported("x").is_available());
        assert_eq!(Support::Supported.label(), "yes");
        assert_eq!(Support::Partial("x").label(), "partial");
        assert_eq!(Support::Unsupported("x").label(), "no");
        assert_eq!(Support::Supported.detail(), None);
        assert_eq!(Support::Partial("why").detail(), Some("why"));
        assert_eq!(Support::Unsupported("why").detail(), Some("why"));
    }

    #[test]
    fn detected_capabilities_are_self_consistent() {
        let caps = Capabilities::detect();
        let rows = caps.rows();
        assert_eq!(rows.len(), 11);
        // Every non-full capability must explain itself; full ones must not.
        for (name, support) in rows {
            if support.is_full() {
                assert!(
                    support.detail().is_none(),
                    "{name} is full but has a reason"
                );
            } else {
                assert!(support.detail().is_some(), "{name} lacks an explanation");
            }
        }
        // Clock speed is expected on every desktop platform we target.
        assert!(caps.clock_speed.is_available());
    }
}
