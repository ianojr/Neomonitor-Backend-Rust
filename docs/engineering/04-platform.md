# 04 — Platform Support Matrix & Missing Capabilities

> **Update (MP06).** Disk throughput and GPU temperature are now partially implemented, and
> the GPU backend was rebuilt around DXGI + per-adapter LUID association. The authoritative,
> evidence-labelled present-tense matrix lives in
> [11-hardware-capabilities.md](11-hardware-capabilities.md); this file retains the original
> audit view, with the rows below corrected.

## 4. Platform support matrix

The code compiles on all targets but only Windows has a real GPU backend
(`src/gpu.rs` `#[cfg(windows)]` backend; no-op stub for other targets).

| Capability | Windows | Linux | macOS | Notes |
|-----------|---------|-------|-------|-------|
| CPU global / per-core % | ✅ | ✅ | ✅ | sysinfo; Windows = PDH idle counters |
| CPU frequency | ✅ real current MHz | ✅ | ✅ | Windows via `CallNtPowerInformation` |
| RAM total/used/available | ✅ | ✅ | ✅ | `used = total − available` |
| Swap / page file | ✅ | ✅ | ✅ | Windows = pagefile/commit |
| Disks (space) | ✅ | ✅ | ✅ | |
| Disk I/O throughput | ✅ | ❌ | ❌ | machine-wide `\PhysicalDisk(_Total)` (PDH); per-volume not mapped |
| Network rates/totals | ⚠️ | ✅ | ✅ | Windows double-counts filter drivers (C11) |
| Process list CPU/mem | ⚠️ | ✅ | ✅ | Windows process CPU is per-core scale (C5) |
| Temperature / sensors | ⚠️ ~never | ✅ | ✅ | Windows = single ACPI zone, usually absent |
| GPU util / name / memory | ✅ | ❌ | ❌ | DXGI identity/totals + PDH per-adapter, multi-adapter aware |
| GPU temperature | ⚠️ optional LHM | ❌ | ❌ | no built-in Windows API; LibreHardwareMonitor WMI when present |
| Load average | ⚠️ not Unix semantics | ✅ | ✅ | Windows = PDH queue length |

Legend: ✅ works · ⚠️ works with caveats · ❌ not implemented.

### Platform-specific notes

- **Non-Windows GPU (NOT IMPLEMENTED).** `GpuMonitor::refresh` returns `None`, and
  `gpu_loop` substitutes `GpuStats::default()`. The GPU column now reports
  "n/a (unsupported OS)" from `Capabilities` rather than a bare 0.0 %, so the user can tell
  "no GPU support on this OS" from "GPU idle".
- **Windows sensors (CONFIRMED platform-limited).** Even elevated, Windows exposes at most
  one ACPI thermal zone; this machine exposes none (`--probe`: `Components 0`). The UI's empty
  state correctly suggests LibreHardwareMonitor. Load average is likewise a PDH-derived
  approximation, not Unix `loadavg`.

## 6c. Missing capabilities

Ordered by likely user value. Status as of MP06 is noted; the rest remain **NOT IMPLEMENTED**.

| ID | Capability | Why it matters | Feasibility |
|----|-----------|----------------|-------------|
| M1 | **GPU temperature** | A monitor without GPU temp is incomplete for gaming/thermal users. | **Partial**: optional LibreHardwareMonitor WMI path implemented; `Unavailable` when absent. |
| M2 | **Disk I/O throughput** (read/write MB/s) | Disk activity is a core dashboard metric; only capacity was shown. | **Partial**: machine-wide `\PhysicalDisk(_Total)` read/write bytes/s implemented; IOPS/utilisation/per-volume still absent. |
| M3 | **Per-process GPU usage** | Task-Manager parity. | PDH `GPU Engine` carries `pid_`, but utilisation is deliberately summed across processes for the adapter figure. |
| M4 | **CPU package power / energy** | Thermal/power monitoring. | Requires RAPL (Linux) or vendor tooling (Windows). |
| M5 | **`--help` / `--version`** and CLI documentation | Discoverability. | Trivial. |
| M6 | **Settings/config persistence** | Currently everything resets each launch. | Small (serde already present). |
| M7 | **Alerting / thresholds** | Proactive monitoring. | Small; `usage_color` is already the seed. |
| M8 | **History export / logging** (CSV/JSON) | Post-run analysis. | Small. |
| M9 | **Automated tests + CI** | Guards parsing logic (C3). | **Done for the correctness pass**: 65 unit tests; CI wiring still absent. |
| M10 | **Cross-platform GPU backend** | Broader audience. | Large; NVML/ADL/IOKit behind a trait. |

### Architectural gap for M1/M2/M3/M10

The GPU layer is a concrete Windows struct with a no-op stub elsewhere and no trait. Disk I/O
and PDH were factored into `src/diskio.rs` and `src/platform/pdh.rs`, and capabilities into
`src/platform.rs`, so `cfg` logic is now contained. A full cross-platform GPU trait remains
**NOT IMPLEMENTED**. See [11-hardware-capabilities.md](11-hardware-capabilities.md) for the
implemented/partial/unsupported boundary and validation status.
