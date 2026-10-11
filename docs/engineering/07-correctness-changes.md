# 07 — Correctness Changes Report

Scope: MASTER PROMPT 02 (metric correctness engine). This records what was changed, the
evidence for each change, test results, and what still needs validation on real hardware. It
resolves items **C1–C11** from `02-metrics-correctness.md`.

Guiding rules followed: fix verified defects rather than rewriting correct logic; keep OS
acquisition separate from platform-independent arithmetic; add no new dependencies; document
anything that could not be verified.

---

## 1. New modules

| File | Purpose |
|------|---------|
| `src/reading.rs` | `Reading<T>` validity type + `Freshness`/`freshness`. Makes "unknown" distinct from "0". |
| `src/calc.rs` | All platform-independent arithmetic, pure and unit-tested. |

No new crates were added to `Cargo.toml`.

---

## 2. Bug register — resolution

| ID | Issue | Resolution |
|----|-------|-----------|
| **C1** | Graph x-axis labels hard-coded to `-120s`, wrong for most presets. | `calc::span_axis_labels` + `App::history_span_secs()` derive labels from the real cadence (`src/ui.rs`, `src/app.rs`). |
| **C2** | CPU/RAM and GPU series plotted by index but sampled at different rates, so the shared x-axis was misleading. | GPU history is now pushed once per **system** tick, holding the last value when unavailable; all series share one cadence (`src/app.rs`). |
| **C3** | GPU utilization grouped by `(luid, engtype)`, merging physically distinct engines. | `calc::gpu_engine_key` keeps `phys_`/`eng_`; `gpu_engine_utilization` reports the busiest engine (`src/gpu.rs`, `src/calc.rs`). Tested: 9×`3D` no longer summed. |
| **C4** | GPU % vs memory basis mismatch; `AdapterRAM` shown as true VRAM. | Memory now comes from PDH `GPU Adapter Memory` per adapter (WMI fallback); UI labels the total "adapter" and only shows a fill ratio when a real total exists (`src/gpu.rs`, `src/ui.rs`). |
| **C5** | Process CPU was per-single-core (could exceed 100) but presented like a whole-machine %. | `calc::process_cpu_percent` normalises by logical CPU count (`src/sampler.rs`). |
| **C6** | Sensor `max()` (observed peak) used as the bar denominator, as if a limit. | Bar scales against `critical()` (fallback 100 °C); text shows `peak` and `crit` separately (`src/metrics.rs`, `src/sampler.rs`, `src/ui.rs`). |
| **C7** | `--speed <invalid>` silently fell back; no `--help`/`--version`. | Unknown/missing `--speed` now warns on stderr; `-h/--help` and `-V/--version` added (`src/main.rs`). |
| **C8** | Bench row "gpu refresh (pdh)" secretly included the WMI memory query. | Row renamed "gpu refresh (engine+mem)"; the WMI round-trip is gone when PDH memory is available. |
| **C9** | Footer Hz counted only system updates. | (Tracked; see §4 — not in this pass.) |
| **C10** | 20 ms poll → ~50 wakeups/s. | (Tracked; see §4 — not in this pass.) |
| **C11** | Network aggregate double-counted NDIS filter pseudo-NICs. | `calc::is_pseudo_interface` excludes MAC-less/IP-less interfaces from the list and the aggregate (`src/sampler.rs`). Confirmed live: LWF no longer counted. |

Also improved while in scope:
- Counter deltas use `calc::counter_rate` (returns 0 on counter reset/rollover, invalid elapsed,
  or non-finite `dt`) instead of raw subtraction (`src/sampler.rs`).
- Stale network-interface state is pruned each sample (`*prev = current`), so a hot-plugged NIC
  does not leave stale rates behind.
- The GPU loop keeps the last good sample but marks it `Stale`, and drops to `Unavailable` once
  the reading is older than twice the GPU interval (`src/sampler.rs`).
- GPU constructors treat *either* PDH or WMI as a valid backend (`refresh` returns `None` only
  when neither exists), and non-Windows builds return `None` (documented stub).

---

## 3. Verification

### Tests

`cargo test` → **21 passed, 0 failed**. Coverage includes the exact bugs fixed:

- `calc::gpu_engine_key` keeps engine identity; nine `3D` engines are not collapsed.
- `gpu_engine_utilization` sums within an engine, picks the busiest, ignores NaN/negative, and
  returns `None` for empty input.
- `process_cpu_percent` is machine-relative and treats non-finite input as invalid.
- `is_pseudo_interface` for all four MAC/IP combinations.
- `counter_rate` for normal, reset/rollover, zero and non-finite elapsed.
- `adapter_memory` selects the busiest adapter and ignores invalid samples.
- `span_axis_labels` tracks the requested span.
- `Reading`/`Freshness` behaviour, incl. `Default == Unavailable`.
- `util` helpers (`human_bytes`, `human_duration`, `ratio`, `bar_symbols`).

`cargo clippy --all-targets -- -D warnings` → clean. `cargo fmt --check` → clean.

### Live probes (this machine)

`--probe` now shows pseudo-interfaces tagged `[pseudo: excluded from totals]`; the real `Wi-Fi`
interface and the `Wi-Fi-VirtualBox LWF` both report ~392/430 B/s but only the real one feeds
the aggregate — the double count (C11) is gone.

### Performance (`--bench`, release)

| Source | Before | After |
|--------|--------|-------|
| `refresh_processes(lean)` | 10.9 ms | 10.9 ms |
| `networks.refresh` | 6.8 ms | 6.4 ms |
| `components.refresh` | 4.6 ms | 6.0 ms |
| **`gpu refresh (engine+mem)`** | 8.4 ms (WMI, mislabelled) → 47.8 ms (WMI mem per sample) | **1.07 ms** (PDH engine + PDH memory) |

The GPU path is now ~8× faster than the audited figure and uses no per-sample WMI query when
the PDH counter set is present. `AdapterRAM` (the unreliable aperture value) is still read
**once** at startup for the adapter name/driver.

---

## 4. Remaining limitations / known-unverifiable metrics

| Item | Status | Note |
|------|--------|------|
| GPU temperature | **NOT IMPLEMENTED** | No OS thermal provider on this machine (`Components 0`); field is always `Unavailable`, never a fake 0. |
| GPU `AdapterRAM` total | **NEEDS VALIDATION** | On integrated GPUs this is a shared-memory aperture (live: bogus `1073741824`). Shown labelled "adapter" and never used as a fill denominator unless `dedicated_used` exists. Correct on a discrete card is unverified. |
| GPU per-adapter selection | **SUSPECTED** | Chooses the busiest adapter by committed footprint. Correct on a single-GPU machine; multi-GPU association with the WMI adapter name is unverified. |
| Load average | **SUSPECTED mislabel** | Windows value is queue-length-derived, not Unix `loadavg`; the 1/5/15 presentation overstates its meaning. Not changed in this pass. |
| CPU freq lag | **CONFIRMED trade-off** | MHz updates on the slow tick and can lag up to 10 s at Minimal. |
| C9 footer Hz, C10 20 ms poll | **NOT ADDRESSED** | Performance/UX items, tracked for the next pass. |

### Needs physical-hardware validation

- Discrete NVIDIA/AMD GPU: engine grouping, per-adapter memory, and `AdapterRAM` as a real total.
- Any machine exposing thermal sensors: sensor `peak`/`crit` rendering and the critical fallback.
- A machine with multiple physical NICs and active filter drivers: aggregate correctness and the
  pseudo-interface heuristic under load.
