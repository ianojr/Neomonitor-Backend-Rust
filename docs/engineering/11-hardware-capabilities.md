# 11 — Hardware & Platform Capability Engine

Scope: the platform-specific metric acquisition layer (`gpu`, `diskio`, `sampler`) and the
platform-independent capability model (`platform`) and maths (`calc`). This document is the
authoritative, evidence-labelled statement of **what the program can actually measure on each
supported OS**, and **what it deliberately does not claim**.

Evidence labels follow the audit convention: **CONFIRMED**, **SUSPECTED**,
**NOT IMPLEMENTED**, **NEEDS VALIDATION**. Citations use `path:line` / `path:fn`.
All live numbers were taken on the reference machine (Windows 10/11, Intel Core i7-8565U with
Intel UHD Graphics 620, no discrete GPU, no OS thermal sensors).

## 11.1 Layout: what is platform-specific vs. platform-independent

The rule is: **OS acquisition is quarantined; all arithmetic is pure.**

| Layer | File | Responsibility |
|-------|------|----------------|
| Capability ceiling | `src/platform.rs` | Per-OS `Support { Supported, Partial, Unsupported }` for each capability (`Capabilities::detect`, `platform.rs:79`). No runtime values. |
| OS primitive | `src/platform/pdh.rs` | Windows PDH counter wrapper (`PdhCounter`, `pdh.rs:20`), shared by GPU and disk backends. Compiled only on Windows. |
| GPU model + backend | `src/gpu.rs` | Platform-independent types (`GpuKind/GpuAdapter/GpuStats`) plus a `#[cfg(windows)]` backend (`gpu.rs:322`–`633`) and a no-op non-Windows backend (`gpu.rs:632`). |
| Disk I/O model + backend | `src/diskio.rs` | `DiskIoStats` + a `#[cfg(windows)]` PDH backend (`diskio.rs:45`) and a no-op elsewhere. |
| Pure maths | `src/calc.rs` | LUID normalisation, per-adapter engine/memory aggregation, LHM temperature selection, rates, percentiles. Unit-tested with synthetic values. |
| Scheduler | `src/sampler.rs` | Owns the monitors; refreshes each on its interval; degrades to `Stale`/`Unavailable` on failure. |
| Presentation | `src/ui.rs` | Renders readings, and says *which* kind of "n/a" it is (unsupported vs. temporarily missing). |

This separation is what lets the same aggregation code be trusted without a second OS:
`calc` is exercised purely, while `gpu`/`diskio` only convert OS numbers into `Reading`s.

## 11.2 Capability matrix

Legend: **full** = backend exists, expected to work without extra software · **partial** =
backend exists with a real limitation (hardware/extra software/approximate semantics) ·
**no** = no backend; not collected.

| Capability | Windows | Linux | macOS | Method / limitation |
|-----------|---------|-------|-------|---------------------|
| GPU utilisation | full | no | no | PDH `\GPU Engine(*)\Utilization Percentage`, per process then aggregated per adapter. Linux/macOS: **NOT IMPLEMENTED** (`gpu.rs:632`). |
| GPU multiple adapters | full | no | no | DXGI `EnumAdapters1` (`gpu.rs:580`) for identity/name/totals; PDH for live counters. |
| GPU dedicated/shared memory | full | no | no | DXGI `DXGI_ADAPTER_DESC1` totals + PDH `\GPU Adapter Memory(*)` usage. |
| GPU temperature | partial | no | no | No built-in Windows API. Optional LibreHardwareMonitor WMI (`gpu.rs:544`); absent by default. |
| Per-process GPU | no | no | no | PDH engine instances carry `pid_`, but utilisation is intentionally summed across processes for the adapter figure, discarding the pid. |
| CPU temperature | partial | partial | partial | sysinfo `Components`. Windows exposes at most one often-absent ACPI zone; Linux hwmon/macOS SMC availability varies. |
| Fan speed | no | no | no | No supported OS interface; **NOT IMPLEMENTED**. |
| Power consumption | no | no | no | Needs RAPL (Linux) or vendor tooling (Windows); **NOT IMPLEMENTED**. |
| CPU clock speed | full | full | full | sysinfo per-core MHz. |
| Disk activity (throughput) | full | no | no | PDH `\PhysicalDisk(_Total)\Disk {Read,Write} Bytes/sec` (`diskio.rs:45`). Per-volume mapping avoided (see 11.6). |
| Load average | partial | full | full | Windows = sysinfo PDH queue-length approximation, **not** Unix `loadavg`. |

The same data is emitted at runtime by `--probe` (`main.rs`, "Capabilities" section) and in
the help overlay (sensors/GPU/disk/power rows, `ui.rs:1122`–`1161`), so the user can always
see the ceiling rather than guessing from an empty panel.

## 11.3 GPU engine: correctness fixes and per-adapter model

### Identity by adapter LUID
Windows keys GPU adapters by LUID. PDH instance names embed it as
`luid_0x<high>_0x<low>` and DXGI reports the same pair as `DXGI_ADAPTER_DESC1.AdapterLuid`.
`calc::normalize_luid` (`calc.rs:138`) canonicalises both to upper-case `0x<high>_0x<low>`,
and `calc::luid_candidates` (`calc.rs:167`) accepts either half-ordering, so the association
does not depend on an undocumented convention. **CONFIRMED** by matching the two PDH LUIDs on
the reference machine against the two DXGI adapters.

### Per-adapter aggregation (no cross-adapter merging, no engine collapsing)
`calc::gpu_engine_utilization_by_adapter` (`calc.rs:182`) groups by `(LUID, engine key)` —
where the engine key keeps `phys`/`eng`/`engtype` so nine `3D` engines are not summed — then
reports the busiest engine group **per adapter**. `calc::gpu_memory_by_adapter`
(`calc.rs:210`) sums `phys` entries *within* one LUID but never across LUIDs. Both are
unit-tested with synthetic instances.

### No "largest memory = primary" assumption
`gpu::pick_primary` (`gpu.rs:179`) prefers non-software adapters, then highest utilisation,
then largest committed dedicated memory, then enumeration order. A mostly-idle discrete card
does not masquerade as the active GPU just because it has more VRAM.

### Adapter classification
`gpu::classify_adapter` (`gpu.rs:134`) labels an adapter `Integrated` / `Discrete` /
`Software` / `Unknown` from its description, PCI vendor id and the DXGI software flag. Name
rules precede the vendor fallback because one vendor ships both kinds (Intel Arc is discrete;
AMD Radeon RX is discrete while Radeon/Vega graphics on an APU is integrated). The intent is
to be obviously right on common names and otherwise say `Unknown` — never confidently wrong.
**SUSPECTED** for exotic/rebadged parts.

### Live result (reference machine)
```
GPU adapters     : 2 (DXGI + PDH)
  * Intel(R) UHD Graphics 620    integrated util 10.0% ded 0 B shared 239153152 B vram 134217728 B  31.0.101.2135
    Microsoft Basic Render Driver software   util 0.0% ded 0 B shared 8192 B     vram n/a
```
The Intel shared-memory figure matches
`Get-Counter '\GPU Adapter Memory(*)\Shared Usage'` (240 287 744 B at the time of sampling);
DXGI contributes the 128 MiB dedicated total and the friendly names. The Microsoft Basic
Render Driver is correctly classified `Software` and never selected as primary.

## 11.4 Disk activity

`\PhysicalDisk(_Total)\Disk Read Bytes/sec` and `Disk Write Bytes/sec` are exposed through the
shared PDH wrapper (`diskio.rs`). They are rate counters, so the wrapper primes the query at
construction and returns a valid rate on the next `collect` (`pdh.rs:28`, `pdh.rs:51`).
A real idle reading is `Valid(0)`, distinct from `Unavailable` (no backend / no data).

**CONFIRMED** on the reference machine: under a 64 MiB flush/read loop the probe reported
`write 50.9 MB/s`; `Get-Counter` on the same path reported ~464 MB/s at a different instant.

## 11.5 Dynamic hardware and failure handling

- **Adapter changes:** DXGI/WMI adapter metadata is re-enumerated every
  `ADAPTER_REPROBE_TICKS = 20` GPU ticks (`gpu.rs:322`), so a hot-plug, driver reset or newly
  started LibreHardwareMonitor is noticed within a few seconds without per-sample cost.
- **Sensor providers:** the sysinfo component list is re-probed every
  `COMPONENT_REPROBE_TICKS = 300` slow ticks (`sampler.rs`), so a provider started later is
  found instead of disabling sensors for the session.
- **Transient failure:** when a GPU refresh fails, the last good value is republished as
  `Stale` up to one interval, then becomes `Unavailable` (`sampler.rs::gpu_loop`). Stale and
  unavailable are visually distinct (`ui.rs:449`, `ui.rs:461`).
- **Unsupported vs. unavailable:** the UI consults `Capabilities` so an unsupported platform
  reads "n/a (unsupported OS)" (`ui.rs:456`, `ui.rs:989`) while a supported-but-failed metric
  reads plain "n/a" / "(stale)". This is the distinction the prompt requires: absence of a
  capability is not presented as a temporary glitch, and vice versa.
- **Non-Windows:** `GpuMonitor::refresh` returns `None` (`gpu.rs:633`) and `Capabilities`
  reports those rows `Unsupported`; the panel explains *why* rather than showing a silent zero.

## 11.6 Trade-offs and explicit non-goals

- **Per-volume disk I/O: NOT IMPLEMENTED.** Mapping Windows `PhysicalDisk(N X:)` instances to
  `sysinfo` disks is unreliable across RAID/volume-manager layouts; a wrong mapping is worse
  than a machine-wide number, so only `_Total` is reported. **SUSPECTED** that this mapping is
  possible on simple single-disk hosts, but not attempted.
- **GPU temperature: partial.** Windows exposes no built-in GPU temperature; the optional
  LibreHardwareMonitor WMI path (`root\LibreHardwareMonitor`, `gpu.rs:544`, `calc.rs:244`) is
  used when present and otherwise reports `Unavailable`. LibreHardwareMonitor typically needs
  to be installed and run (often elevated). **NEEDS VALIDATION** on a machine that has it.
- **`Dedicated Limit` PDH counter: not relied upon.** On the reference build,
  `Get-Counter -ListSet "GPU Adapter Memory"` listed only `Shared Usage`, `Dedicated Usage`
  and `Total Committed` — **no** `Dedicated Limit`. This is why adapter VRAM total comes from
  DXGI. Counter availability differs across Windows versions. **NEEDS VALIDATION** elsewhere.
- **Per-process GPU: NOT IMPLEMENTED** (see matrix).
- **Fan/power: NOT IMPLEMENTED** on all platforms; no backend is claimed.

## 11.7 Test coverage

Pure logic added or extended in this pass (all run on any host):

| Area | Test |
|------|------|
| LUID canonicalisation (case/spacing/suffixed) | `calc::tests::luid_normalisation_is_source_independent` |
| Both LUID half-orderings | `calc::tests::luid_candidates_cover_both_orderings` |
| Per-adapter engine merge (no cross-adapter, sum-within-engine) | `calc::tests::engine_utilization_is_per_adapter` |
| Memory summed within an adapter only | `calc::tests::memory_is_summed_within_an_adapter_only` |
| LibreHardwareMonitor temperature selection (GPU only, ignores non-finite) | `calc::tests::lhm_temperature_picks_gpu_only` |
| Adapter classification | `gpu::tests::classify_common_adapters` |
| Primary selection (hardware first, then activity, then memory) | `gpu::tests::primary_prefers_hardware_then_activity`, `gpu::tests::primary_falls_back_to_memory_when_idle` |
| Stale propagation keeps adapters/primary | `gpu::tests::stale_preserves_adapters_and_primary` |
| Capability model self-consistency (every non-full row explains itself) | `platform::tests::detected_capabilities_are_self_consistent`, `platform::tests::support_predicates_and_labels` |

Real-hardware validation performed here: `--probe` (2 adapters, LUID association, DXGI totals,
disk bytes/sec) and a release launch smoke test. Everything in 11.3/11.4 that is labelled
**CONFIRMED** is reproduced from these.

## 11.8 What has *not* been validated

- Non-Windows builds compile with the no-op backends but have **no** GPU or disk-activity
  collection; this is stated, not claimed. **NOT IMPLEMENTED** by design.
- LibreHardwareMonitor GPU temperature, DXGI `DedicatedVideoMemory` accuracy on discrete
  cards, and the presence of a `Dedicated Limit` counter on other Windows builds are all
  **NEEDS VALIDATION** and are backed by graceful `Unavailable` fallbacks if absent.
- Fan speed, power consumption and per-process GPU remain **NOT IMPLEMENTED** rather than
  approximated.
