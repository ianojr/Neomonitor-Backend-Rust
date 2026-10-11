# 02 — Metric-by-Metric Correctness Audit

Every metric is traced: **source call → sysinfo/OS semantics → display**. Windows semantics
below were read from the vendored dependency sources under
`C:\Users\Hp\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\sysinfo-0.38.4\src\windows\`,
because this is where correct/incorrect behaviour is actually decided.

---

## CPU

### CPU global usage
- Code: `sys.refresh_cpu_usage()` (`src/sampler.rs:141`) → `sys.global_cpu_usage()`
  (`src/sampler.rs:304`), rendered as a gauge + history (`src/ui.rs:196`, `src/ui.rs:240`).
- Windows semantics (CONFIRMED): sysinfo computes `100 − idle%` from the PDH counter
  `\Processor Information(_Total)\% Idle Time` (`windows/system.rs:118`–`150`). This is a
  time-averaged value over the interval since the previous refresh.
- Interval caveat (CONFIRMED): sysinfo ignores a refresh that arrives sooner than
  `MINIMUM_CPU_UPDATE_INTERVAL = 200 ms` on Windows (`windows/system.rs:41`). The preset
  floor is also 100 ms (`src/sampler.rs:82`), so a sub-200 ms fast interval is accepted by
  the config but silently clamped by sysinfo. **Fast preset Fast=200 ms is exactly at the
  boundary and safe; anything lower (not currently offered) would be a no-op.**
- Verdict: **CONFIRMED correct**. Note the global value is a separate `_Total` counter, not
  the mean of per-core values, so the gauge and the per-core list need not reconcile exactly.

### CPU per-core usage
- Code: `sys.cpus()` → `c.cpu_usage()` (`src/sampler.rs:250`–`251`), rendered per core with a
  bar + colour (`src/ui.rs:216`–`233`).
- Verdict: **CONFIRMED correct**, same 200 ms caveat, same `_Total`-independent basis.

### CPU frequency
- Code: `sys.refresh_cpu_frequency()` (slow-gated, `src/sampler.rs:163`) → `c.frequency()`
  (`src/sampler.rs:252`); the UI shows per-core MHz and an average (`src/ui.rs:197`–`201`,
  `src/ui.rs:228`).
- Windows semantics (CONFIRMED): sysinfo reads `CurrentMhz` via
  `CallNtPowerInformation(ProcessorInformation)` (`windows/cpu.rs:496`–`513`). This is real
  current MHz, not the nominal max.
- Live cross-check (CONFIRMED): probe reported `avg 1792 MHz`; `Win32_Processor.CurrentClockSpeed`
  reported `1792` and `MaxClockSpeed` `1992`. Matches.
- Verdict: **CONFIRMED correct**. Frequency updates only on the `slow` interval, so MHz can
  lag the usage bars by up to `slow` ms (2000–10000 ms). That is a deliberate cost trade-off
  but can look stale at Eco/Minimal.

### Load average
- Code: `System::load_average()` (`src/ui.rs:598`), displayed as "Load 1/5/15" (`src/ui.rs:608`).
- Windows semantics: sysinfo *does* implement this on Windows via the PDH counter
  `\System\Cpu Queue Length` with exponential smoothing (`windows/cpu.rs:44`–`82`). It does
  **not** have Unix run-queue/load semantics; the 1/5/15 labels imply Unix semantics.
- Verdict: **NEEDS VALIDATION / SUSPECTED mislabel**. The number is a queue-length-derived
  smoothed value; presenting it as Unix `loadavg` is misleading. Also `load_average()` is
  called every draw (`src/ui.rs:598`), i.e. up to 50×/s, not gated by preset.

---

## Memory

### RAM total / used / available
- Code: `sys.refresh_memory()` (`src/sampler.rs:142`) → `total_memory`, `used_memory`,
  `available_memory` (`src/sampler.rs:262`–`264`).
- Windows semantics (CONFIRMED): `used_memory = total − available`, and `free_memory()`
  is an alias of `available_memory()` (`windows/system.rs:322`–`349`). So "used" is
  "total minus what the OS says is available", which excludes standby/reclaimable cache —
  a reasonable, Task-Manager-like definition.
- Display: RAM gauge uses `used/total` (`src/ui.rs:252`–`266`); the "Available" row uses
  `available_memory` (`src/ui.rs:605`).
- Verdict: **CONFIRMED correct**.

### Swap
- Code: `total_swap()` / `used_swap()` (`src/sampler.rs:265`–`266`), gauge with a
  "no swap configured" state (`src/ui.rs:286`).
- Windows semantics: backed by the page file / commit charge.
- Verdict: **CONFIRMED** (semantics are pagefile-based, which is the closest Windows
  analogue; label "Swap" is acceptable but technically "page file").

---

## Disks

- Code: `Disks::new_with_refreshed_list()` counted once (`src/sampler.rs:120`), then
  `disks.refresh(true)` on the `slow` interval (`src/sampler.rs:161`) →
  `total_space()` / `available_space()` (`src/sampler.rs:276`–`277`).
- Display: table with a computed used% (`src/ui.rs:472`–`484`).
- Verdict: **CONFIRMED correct**. Only space is collected; there is **no** disk I/O
  throughput (see 04). `total_space`/`available_space` are static until a `slow` refresh.

---

## Network

### Per-interface rates and totals
- Code: `networks.refresh(true)` on the `net` interval (`src/sampler.rs:146`);
  `total_received()`/`total_transmitted()` (cumulative) at `src/sampler.rs:197`–`198`.
- Rate math (CONFIRMED correct in approach): `rx = (total_now − total_prev) / dt` where
  `dt` is the real elapsed time since the previous net refresh (`src/sampler.rs:145`,
  `src/sampler.rs:203`–`204`), with `saturating_sub` guarding counter resets and `dt`
  floored at 1 ms. First tick seeds `prev` with the current totals so the first rate is 0
  (no spurious spike) — `src/sampler.rs:202`.
- Counter semantics (CONFIRMED): sysinfo's `received()`/`transmitted()` delta helpers are
  not used; the code deliberately uses the cumulative totals so it owns the rate window.
  This is the right choice because the net interval is independent of the sys tick.
- Aggregation: `net_rx_rate`/`net_tx_rate` are sums across interfaces
  (`src/sampler.rs:207`–`208`); histories are pushed every **sys** tick from those cached
  values (`src/app.rs:155`–`156`). Because a net refresh only happens every `net` ms, the
  same rate is pushed repeatedly between net refreshes — a **stair-step** in the graph,
  not interpolated. **SUSPECTED visual artifact** (not incorrect, just coarse).
- `prev` keyed by interface name (`src/sampler.rs:205`): if two interfaces had the same
  name, rates would corrupt. Interface names are unique on Windows, so this is
  **CONFIRMED acceptable** but worth noting for exotic platforms.
- Interfaces with `rtx == 0 && ttx == 0` are hidden from the list but **still counted** in
  the aggregate (`src/sampler.rs:199`–`211`). So the header total can include traffic from
  interfaces not shown. **CONFIRMED minor inconsistency** (aggregate vs. list scope).
- **Double-counting of filter-driver interfaces (CONFIRMED, C11).** Live probe showed:

  ```
  NET Wi-Fi-VirtualBox NDIS Light-Weight Filter-0000 rx=262/s tx=342/s
  NET Wi-Fi                                      rx=262/s tx=342/s
  ```

  Windows exposes the same physical link through NDIS filter/lightweight-filter pseudo-
  interfaces that report the *same* cumulative counters. Summing across all interface names
  (`src/sampler.rs:207`–`208`) therefore roughly **doubles** the header RX/TX rate and
  duplicates rows in the interface list. On this machine the true Wi-Fi rate is ~262 B/s
  but the aggregate reports ~524 B/s. This is a correctness bug, not just cosmetic.
- Verdict: rates **CONFIRMED correct**; the hidden-but-counted and stair-step behaviours are
  cosmetic.

---

## Processes

### Identity
- Code: `refresh_processes_specifics(All, true, ProcessRefreshKind::nothing().with_cpu().with_memory())`
  on the `proc` interval (`src/sampler.rs:155`–`156`). `pid`/`name` are stable.
- Verdict: **CONFIRMED correct**.

### Process CPU (`p.cpu_usage()`)
- Code: `src/sampler.rs:297`. Displayed as column "**CPU %**" (`src/ui.rs:635`) and coloured
  by `usage_color` thresholds 60/85 (`src/ui.rs:655`, `src/ui.rs:73`).
- Windows semantics (CONFIRMED): sysinfo returns `100 × (process_delta / system_delta) × ncpus`
  (`windows/process.rs:1045`–`1100`). i.e. **percentage of a single logical core**, so values
  can reach `N×100 %` (800 % here). It also enforces the 200 ms minimum interval internally.
- **This is the defect C5.** On an 8-thread machine a process using two cores shows `~200 %`
  and is coloured red (`usage_color` clamps nothing, so `≥ 85` → red). The column header says
  "CPU %" with no per-core qualifier, and the sort/colour model assumes 0…100.
- Verdict: **CONFIRMED incorrectly labelled/graded** (see C5).

### Process memory (`p.memory()`)
- Code: `src/sampler.rs:298`, displayed with `human_bytes` (`src/ui.rs:656`).
- sysinfo returns resident working set in bytes.
- Verdict: **CONFIRMED correct**.

### Top-N and sorting
- `sort_rows` runs on **every** snapshot (`src/app.rs:139`) and again on `s` (`src/app.rs:167`),
  then truncates to `TOP_N = 200` (`src/app.rs:140`). With ~274 processes on this machine the
  cost is negligible; see 03. NaN-safety: `partial_cmp(...).unwrap_or(Equal)`
  (`src/app.rs:250`–`254`) prevents a panic if `cpu` is NaN. Good.

---

## Components / temperature

- Code: `components.refresh(true)` on the `slow` interval (`src/sampler.rs:162`) →
  `c.temperature()`, `c.max()`, `c.label()` (`src/sampler.rs:284`–`288`).
- Windows semantics (CONFIRMED): sysinfo exposes at most a **single** ACPI thermal zone,
  labelled `"Computer"`, and returns nothing when the firmware does not expose it
  (`windows/component.rs`). `max()` on Windows is the **observed maximum**, not a trip point.
  Live probe reported `Components 0` on this machine. `Component::critical()` (trip
  temperature) is public in sysinfo but is **not used** here (NOT IMPLEMENTED).
- Display: an empty-state explanation is shown when the list is empty (`src/ui.rs:398`–`410`),
  which is good. When non-empty, `max` is displayed as "max …°C" (`src/ui.rs:435`), implying a
  threshold, which is **SUSPECTED misleading** on Windows (it is an observed max).
- Verdict: **CONFIRMED platform-limited**; wording around `max` **SUSPECTED misleading**.

---

## GPU

### Utilisation
- Preferred path: PDH counter `\GPU Engine(*)\Utilization Percentage` via
  `PdhGetFormattedCounterArrayW` (`src/gpu.rs:51`, `src/gpu.rs:186`–`231`). Fallback: WMI
  `Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine` (`src/gpu.rs:120`–`129`).
- Both paths group by `engine_key()` (`src/gpu.rs:142`, `src/gpu.rs:255`), which uses only
  `(luid, engtype)` and drops the `phys_N`/`eng_N` index, then sums per group and takes the
  max (`src/gpu.rs:249`–`251`).
- **This is the defect C3.** Verified live with `typeperf "\GPU Engine(*)\Utilization Percentage"`:
  instance names are `pid_<pid>_luid_0x00000000_0x<LUID>_phys_0_eng_<N>_engtype_<type>`.
  On this machine adapter `…13EC2` exposes **nine** engines all with `engtype_3D`
  (`eng_0` … `eng_8`), and adapter `…13B09` exposes several engines with an **empty**
  `engtype_` (`eng_5,6,7,8,12`). Grouping by `(luid, engtype)` sums all nine 3D engines into
  one group, inflating the reading; the empty-type engines collapse to a single key
  (`"…|"`). Task Manager semantics require the **max over individual engines**, i.e. the key
  must include `phys`+`eng`.
- Verdict: **CONFIRMED incorrect** (see C3).

### VRAM total / used / shared
- Total: `Win32_VideoController.AdapterRAM` of the adapter with the greatest `AdapterRAM`
  (`src/gpu.rs:72`–`76`). Used/shared: sum of `DedicatedUsage`/`SharedUsage` across **all**
  rows of `Win32_PerfFormattedData_GPUPerformanceCounters_GPUAdapterMemory`
  (`src/gpu.rs:131`–`139`).
- Live cross-check (CONFIRMED): `AdapterRAM = 1073741824` (1 GiB) for the Intel UHD 620.
  An integrated GPU has no dedicated VRAM; this value is a reserved/aperture figure, not real
  dedicated memory. The UI therefore shows the misleading "VRAM 0 / 1.0 GB"
  (`src/ui.rs:359`–`368`). PDH `GPUAdapterMemory` reported `DedicatedUsage = 0` for both LUIDs
  and `SharedUsage` of 8 KiB (`…13EC2`) and ~211 MiB (`…13B09`).
- Adapter-identity mismatch: utilisation takes the max over engine groups of **all** adapters
  (`src/gpu.rs:251`), while memory **sums** across all adapters (`src/gpu.rs:135`–`138`). So
  the % and the memory can come from different adapters. On this single-GPU machine they
  happen to agree, but the bases differ. **CONFIRMED design inconsistency** (C4).
- Verdict: total VRAM **CONFIRMED misleading**; used/shared **CONFIRMED** but multi-adapter
  basis **SUSPECTED**.

### Name / driver
- `Win32_VideoController.Name`/`DriverVersion` of the same max-AdapterRAM adapter
  (`src/gpu.rs:74`–`76`). Live: `Intel(R) UHD Graphics 620`, driver `31.0.101.2135`. Correct
  for this machine; on a multi-adapter box the max-AdapterRAM heuristic can pick the "wrong"
  adapter for display. **SUSPECTED**.

### Temperature
- Always `None` (`src/gpu.rs:115`); the label only appends a temp if `Some` (`src/ui.rs:333`).
  **NOT IMPLEMENTED** (see 04).

---

## Rendering-specific correctness

### Graph x-axis and series alignment (C1, C2)
- All histories are plotted by **index** (`src/ui.rs:688`–`705`) against x-bounds
  `[0, n]` (`src/ui.rs:729`) with labels `-120s … now` (`src/ui.rs:730`).
- CPU/RAM/RX/TX are pushed on **every sys tick** (`src/app.rs:150`–`156`); GPU only on **GPU
  ticks** (`src/app.rs:132`). At Balanced (fast 200 ms, gpu 1000 ms) 120 CPU samples span
  ~24 s while 120 GPU samples span ~120 s, yet they share one index axis. **CONFIRMED
  misaligned and mislabelled** (C1, C2).

### `usage_color` colour semantics
- Fixed thresholds 60/85 for all gauges, core bars, process CPU, disk %, RAM, swap, GPU
  (`src/ui.rs:73`–`81`). For RAM/GPU/disk this is fine; for **process CPU** it is wrong
  (0…N×100 scale, C5). For **GPU** it inherits the inflated utilisation (C3).

---

## 6a. Confirmed-bug register

Correctness defects only. Performance/reliability items are in
[03-performance.md](03-performance.md); the full prioritised list is in
[05-roadmap.md](05-roadmap.md).

| ID | Severity | Summary | Evidence |
|----|----------|---------|----------|
| C1 | Medium | Graph x-axis labels claim `-120s … now` but each sample is one **fast** tick, not 1 s; true span ≈24 s at Balanced. | `src/ui.rs:730`, `src/ui.rs:688`, `src/app.rs:12`, `src/sampler.rs:45` |
| C2 | Medium | CPU/RAM/RX/TX (sys cadence) and GPU (gpu cadence) are plotted by index on one axis → not time-aligned. | `src/app.rs:132`, `src/app.rs:150`, `src/ui.rs:700` |
| C3 | High | GPU utilisation grouped by `(luid, engtype)` merges distinct engines (nine `3D` engines on adapter `…13EC2`), inflating the value; can hit the 100 % clamp. | `src/gpu.rs:142`, `src/gpu.rs:255`, `src/gpu.rs:249`, live `typeperf` dump |
| C4 | Medium | GPU % from one adapter (max over all), memory summed over **all** adapters; `AdapterRAM` (1 GiB on an iGPU) shown as VRAM total. | `src/gpu.rs:72`, `src/gpu.rs:135`, `src/ui.rs:359` |
| C5 | High | Process CPU column is per-single-core scale (0…N×100) but labelled "CPU %" and colour-graded 0…100. | `src/sampler.rs:297`, `src/ui.rs:635`, `src/ui.rs:655`, `windows/process.rs:1045` |
| C6 | Low | Sensors `max` shown as if a trip point; Windows `max` is an observed max. `critical()` unused. | `src/ui.rs:435`, `windows/component.rs` |
| C7 | Low | `--speed <invalid>` silently selects Balanced; no `--help`/`--version`. | `src/main.rs:55`–`67` |
| C8 | Low | Bench row `gpu refresh (pdh)` also runs the WMI memory query, so it is ~8 ms WMI, not PDH. | `src/main.rs:242`, `src/gpu.rs:103` |
| C9 | Low | Footer `Hz` counts only sys samples, not GPU updates. | `src/app.rs:158`, `src/ui.rs:815` |
| C10 | Low | 20 ms input poll → up to 50 wake-ups/s even at Minimal preset. | `src/main.rs:22`, `src/main.rs:93` |
| C11 | Medium | Network aggregate sums filter-driver pseudo-interfaces that mirror the physical NIC, ~doubling reported RX/TX and duplicating list rows. | `src/sampler.rs:199`–`208`, live `--probe` |

No open **critical** (crash/data-loss/security) defects were found. There is no code that
could panic on malformed OS data in the sampled paths: numeric formatting is guarded by
`ratio()` (`src/util.rs:36`) and NaN-safe sorts (`src/app.rs:250`).
