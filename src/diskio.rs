//! Machine-wide physical disk throughput.
//!
//! Only the aggregate `_Total` rate is collected. Per-volume throughput would require
//! mapping Windows `PhysicalDisk(N X:)` instance names back to `sysinfo` disks, which is not
//! reliable across RAID/volume-manager layouts; reporting a wrong mapping would be worse than
//! reporting none, so the machine-wide figure is what we stand behind.

use crate::reading::Reading;

/// Instantaneous disk throughput, in bytes/second.
#[derive(Clone, Default)]
pub struct DiskIoStats {
    pub read_rate: Reading<u64>,
    pub write_rate: Reading<u64>,
}

pub struct DiskIoMonitor {
    imp: imp::Impl,
}

impl DiskIoMonitor {
    pub fn new() -> Self {
        Self {
            imp: imp::Impl::new(),
        }
    }

    /// Whether a disk-throughput backend exists on this platform.
    pub fn is_supported(&self) -> bool {
        self.imp.is_supported()
    }

    pub fn refresh(&mut self) -> DiskIoStats {
        self.imp.refresh()
    }
}

#[cfg(windows)]
mod imp {
    use crate::platform::pdh::PdhCounter;
    use crate::reading::Reading;

    use super::DiskIoStats;

    const READ_PATH: &str = "\\PhysicalDisk(_Total)\\Disk Read Bytes/sec";
    const WRITE_PATH: &str = "\\PhysicalDisk(_Total)\\Disk Write Bytes/sec";

    pub struct Impl {
        read: Option<PdhCounter>,
        write: Option<PdhCounter>,
    }

    impl Impl {
        pub fn new() -> Self {
            Self {
                read: PdhCounter::new(READ_PATH),
                write: PdhCounter::new(WRITE_PATH),
            }
        }

        pub fn is_supported(&self) -> bool {
            self.read.is_some() || self.write.is_some()
        }

        pub fn refresh(&mut self) -> DiskIoStats {
            DiskIoStats {
                read_rate: counter_bytes(self.read.as_ref()),
                write_rate: counter_bytes(self.write.as_ref()),
            }
        }
    }

    /// A rate counter: `Some(0.0)` is a real idle reading, `None` means "no data".
    fn counter_bytes(counter: Option<&PdhCounter>) -> Reading<u64> {
        match counter.and_then(PdhCounter::collect_sum) {
            Some(value) if value.is_finite() && value >= 0.0 => Reading::Valid(value as u64),
            _ => Reading::Unavailable,
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::DiskIoStats;

    pub struct Impl;

    impl Impl {
        pub fn new() -> Self {
            Self
        }

        pub fn is_supported(&self) -> bool {
            false
        }

        pub fn refresh(&mut self) -> DiskIoStats {
            DiskIoStats::default()
        }
    }
}
