# 06 — Metric Semantics Contract

This document is the authoritative definition of **what each number on screen means**, how it
is measured, and — critically — how the program distinguishes *"the value is zero"* from
*"we do not have a value"*. It complements `02-metrics-correctness.md` (the audit of where the
values come from) by fixing the contract the rest of the code is written against.

All platform-independent arithmetic lives in `src/calc.rs` (pure, unit-tested). OS acquisition
lives in `src/sampler.rs` and `src/gpu.rs`. The UI (`src/ui.rs`) only formats and colours; it
must never invent a number.

---

## 1. Validity model — `Reading<T>`

Source: `src/reading.rs`.

```
enum Reading<T> { Valid(T), Stale(T), Unavailable }
```

| State | Meaning | UI obligation |
|-------|---------|---------------|
| `Valid(v)` | Fresh measurement for the current interval. | Render `v` normally. |
| `Stale(v)` | Last good measurement that could not be refreshed (source busy/failed, or the sampler has not produced a newer value yet). | Render `v` but flag it (e.g. `(stale)`); never silently equal to fresh. |
| `Unavailable` | No measurement exists (unsupported platform, missing hardware, first sample, error). | Render `n/a`; **never** `0`. |

`Default = Unavailable`, so a freshly constructed snapshot is honestly empty rather than zeroed.
Helpers: `is_available`, `is_valid`, `value`, `into_value`, `map`, `as_stale`, `unwrap_or`.

### Freshness

`Freshness { Current, Stale, Expired }` + `freshness(age, budget)` (`src/reading.rs:86`–`110`):
`Current` while `age <= budget`, `Stale` up to `2 × budget`, `Expired` beyond. Used by the GPU
thread to keep showing the last good sample but stop treating it as current.

---

## 2. Metric contract table

| Metric | Source | Basis / units | Validity | Freshness budget |
|--------|--------|---------------|----------|------------------|
| CPU global | `sysinfo` PDH `% Idle Time` (`100−idle`) | % of whole machine, time-averaged since previous refresh | always numeric (≥200 ms apart) | sys tick |
| CPU per-core | `sysinfo` per logical CPU | % per logical CPU | always numeric | sys tick |
| CPU frequency | `CallNtPowerInformation` `CurrentMhz` | MHz, real current clock | numeric | slow tick |
| RAM used/total | `sysinfo` | bytes; `used = total − available` (excludes reclaimable standby) | numeric | sys tick |
| Swap used/total | `sysinfo` pagefile/commit | bytes | numeric; 0 when no pagefile | sys tick |
| Process CPU % | `sysinfo` process counter | **% of whole machine** (see §3) | numeric | sys tick |
| Process memory | `sysinfo` working set | bytes | numeric | sys tick |
| Disk usage | `sysinfo` `Disks` | used/total bytes; mount point is not "volume name" | numeric | slow tick |
| Disk I/O | PDH `\PhysicalDisk(_Total)\Disk {Read,Write} Bytes/sec` (`src/diskio.rs`) | bytes/s, **machine-wide aggregate**; `Valid(0)` = idle, `Unavailable` = no backend/data | `Reading<u64>`; unsupported off Windows | slow tick |
| Network rate | `sysinfo` per-interface cumulative counters | bytes/s (delta ÷ elapsed) | 0 on reset | net tick |
| Network aggregate | sum over **physical** interfaces | bytes/s | as above | net tick |
| GPU utilization | PDH `\GPU Engine(*)\Utilization Percentage`, WMI fallback | % of the **busiest single engine** (see §4) | `Reading<f32>` | GPU tick |
| GPU memory | PDH `\GPU Adapter Memory(*)\{Dedicated,Shared} Usage`, WMI fallback | bytes of the **busiest adapter** | `Reading<u64>` | GPU tick |
| GPU temperature | optional LibreHardwareMonitor WMI (`root\LibreHardwareMonitor`), selected by `calc::lhm_gpu_temperature` | °C, hottest GPU-named temperature sensor | `Reading<f32>`; `Unavailable` when LHM is absent (the default on Windows) | GPU tick |
| Sensors | `sysinfo` `Components` | °C; `peak` = observed max, `crit` = threshold (see §5) | numeric when a provider exists | slow tick |

---

## 3. Process CPU is a share of the whole machine

`src/calc.rs:27` `process_cpu_percent(raw, logical_cpus) = clamp(raw / logical_cpus, 0, 100)`.

sysinfo's Windows backend returns `100 * process_delta / system_delta * ncpus`, i.e. it can reach
`ncpus × 100` (a fully-busy 8-thread box can show 800). Dividing by the logical CPU count yields
the Task-Manager-style "percent of total CPU" and keeps every value in `0..=100`. The count used
is **logical** CPUs (`sys.cpus().len()`), which is the correct denominator for this formula; it
is **not** an assertion that logical == physical cores.

Non-finite input → `0.0` (treated as invalid, never saturated).

---

## 4. GPU utilization is the busiest engine, not a sum of unrelated engines

`src/calc.rs:68` `gpu_engine_key` normalises a PDH/WMI instance name

```
pid_<pid>_luid_0x00000000_0x<LUID>_phys_0_eng_<N>_engtype_<type>
```

by dropping `pid_` (usage is summed across processes) but **keeping** `luid`, `phys_` and `eng_`,
because they identify a physically distinct engine. Merging by `engtype` alone (the previous
behaviour) collapsed nine `3D` engines on one adapter into one bucket and inflated the reading.

`gpu_engine_utilization` sums per engine, then reports the **maximum** engine's load
(`src/calc.rs:97`). Rationale: consumer GPUs report load per engine type; "GPU load" is the
busiest engine. The total is clamped to `0..=100`. Non-finite/negative samples are ignored;
empty input → `None` → `Reading::Unavailable`.

## 5. Sensor `peak` and `crit` are different things

sysinfo `Component::max()` is a **running observed maximum** (it rises when the temperature
rises), while `Component::critical()` is the chip/kernel halt threshold. The UI previously used
`max` as the bar's denominator, which made the bar meaningless. Now (`src/ui.rs` `draw_sensors`):

- bar scale = `critical` when available, else a fixed `100 °C`;
- text shows `peak <observed>` and `crit <threshold>` separately.

---

## 6. Network aggregation excludes pseudo-interfaces

`src/calc.rs:53` `is_pseudo_interface(mac_unspecified, has_ip)`.

Windows exposes NDIS lightweight-filter / switch-extension adapters through the same MIB table
as real NICs. They have **no MAC and no IP** but mirror a real interface's counters, so summing
all interfaces double-counts throughput (observed live: `Wi-Fi` and
`Wi-Fi-VirtualBox NDIS Light-Weight Filter-0000` both reported the same rate). The aggregate and
the interface list therefore skip any interface with neither a MAC nor an IP. A real hardware
interface always has a MAC even with no IP, so this rule does not hide real idle NICs.

---

## 7. Graph time axis reflects the real sampling cadence

`src/calc.rs:138` `span_axis_labels(span_secs)` derives the labels from
`App::history_span_secs()` (`HISTORY × preset fast interval`). The axis is no longer hard-coded
to `-120s`, which was wrong for every preset except the one it was written for.

GPU is drawn on the same time axis as CPU/RAM/network by pushing one GPU sample per **system**
tick (`App::apply_sys`), holding the last value when the current GPU reading is unavailable.
This keeps all four series aligned even though the GPU source runs on a slower cadence.
