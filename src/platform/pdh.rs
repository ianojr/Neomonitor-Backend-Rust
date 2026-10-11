//! Minimal Windows PDH (Performance Data Helper) wrapper.
//!
//! This is the single home for OS-specific *collection primitives* that several metric
//! sources share (GPU engine/memory today, physical-disk throughput too). Keeping it here,
//! rather than inside one metric module, is what lets `gpu.rs` and `diskio.rs` stay focused
//! on turning raw counters into platform-independent readings.
//!
//! The wrapper owns a PDH query per counter and collects a formatted snapshot on demand.
//! Rate counters (bytes/sec, utilisation) are valid once two samples have been collected;
//! the constructor takes the first, and every [`PdhCounter::collect`] takes the next.

use windows::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::core::PCWSTR;

/// One PDH counter, kept open for the lifetime of the metrics backend.
pub struct PdhCounter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

impl PdhCounter {
    /// Open `path` as an English (locale-independent) counter. Returns `None` when the
    /// counter does not exist on this machine, so callers can degrade to `Unavailable`.
    pub fn new(path: &str) -> Option<Self> {
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);
        unsafe {
            let mut query = PDH_HQUERY(std::ptr::null_mut());
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER(std::ptr::null_mut());
            if PdhAddEnglishCounterW(query, PCWSTR(wide.as_ptr()), 0, &mut counter) != 0 {
                let _ = PdhCloseQuery(query);
                return None;
            }
            // Prime the counter so the first `collect` can already produce a rate.
            let _ = PdhCollectQueryData(query);
            Some(Self { query, counter })
        }
    }

    /// Collect `(instance_name, double_value)` pairs for all valid instances.
    ///
    /// An empty result means the counter had no usable data this tick; it is *not* an error
    /// and must be distinguished from a genuine `0.0` value by the caller.
    pub fn collect(&self) -> Vec<(String, f64)> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return Vec::new();
            }
            let mut size = 0u32;
            let mut count = 0u32;
            let status = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                None,
            );
            if (status != PDH_MORE_DATA && status != 0) || count == 0 || size == 0 {
                return Vec::new();
            }
            let words = (size as usize).div_ceil(std::mem::size_of::<u64>());
            let mut buffer = vec![0u64; words];
            let status = PdhGetFormattedCounterArrayW(
                self.counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                Some(buffer.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W),
            );
            if status != 0 {
                return Vec::new();
            }
            let items = std::slice::from_raw_parts(
                buffer.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
                count as usize,
            );
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                let status = it.FmtValue.CStatus;
                if status != PDH_CSTATUS_VALID_DATA && status != PDH_CSTATUS_NEW_DATA {
                    continue;
                }
                let value = it.FmtValue.Anonymous.doubleValue;
                let name = it.szName.to_string().unwrap_or_default();
                out.push((name, value));
            }
            out
        }
    }

    /// Sum every valid instance of a counter (e.g. `\_Total` style aggregation).
    ///
    /// Returns `None` when no instance produced a valid sample, `Some(sum)` otherwise —
    /// including a legitimate `Some(0.0)`.
    pub fn collect_sum(&self) -> Option<f64> {
        let items = self.collect();
        if items.is_empty() {
            None
        } else {
            Some(items.iter().map(|(_, v)| *v).sum())
        }
    }
}

impl Drop for PdhCounter {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}
