# 13. System Integration & Final Review

Review date: 2026-10-11
Reviewer: engineering review (evidence-based)
Scope: whole application after MP01–MP07 — architecture, module boundaries, metric
semantics, end-to-end behaviour, and regression checks. Where a claim can be checked it
carries an evidence label ([README.md](README.md#evidence-labels)) and a `path:line`
citation.

This document is the capstone review. Earlier documents are the per-area audits and the
per-change implementation reports; where they disagree with the current tree, this document
and docs 05–12 (the current-contract set) win.

## 13.1 Method

1. Read every first-party source module in full (`main`, `app`, `ui`, `metrics`, `sampler`,
   `calc`, `reading`, `util`, `procs`, `diagnostics`, `gpu`, `diskio`, `platform`,
   `platform::pdh`).
2. Traced the end-to-end data path producer → channel → App → UI and the second path
   App → diagnostics engine → UI.
3. Cross-checked every metric's unit, sign, aggregation and validity against
   [06-metric-semantics.md](06-metric-semantics.md).
4. Exercised the built binary: `--probe`, `--bench`, and a release TUI smoke run; measured
   working set over time.
5. Ran the full gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo test`.

## 13.2 Integrated data flow — CONFIRMED

Two detached producer threads own all OS handles and never touch UI state
(`src/sampler.rs`). They publish `Update::{Sys,Gpu}` over a bounded `sync_channel`
(capacity 4); dropped counts are surfaced. The UI thread only ever `try_recv`s, so it never
blocks on a sample — the property the original audit called the design's strongest point
([README.md](README.md)).

The diagnostic engine is fed from exactly one place: `App::apply_sys` builds a normalised
`Sample` and calls `inv.observe` (`src/app.rs:210`). The engine is pure w.r.t. its inputs
(no clock, no I/O) — `now` is the snapshot's own `generated` `Instant` (`src/app.rs:210`,
`src/diagnostics.rs:550`). This means a finding's timing is derived from observed data, not
from when the UI happened to render. **CONFIRMED** by construction.

```
        ┌──────────── sampler thread ────────────┐        ┌─ gpu thread ─┐
 OS ───▶│ System/Disks/Networks/Components/PDH    │        │ DXGI+PDH+WMI │
        └───────────────┬────────────────────────┘        └──────┬───────┘
                        │ Update::Sys (sync_channel cap 4)       │ Update::Gpu
                        ▼                                        ▼
                 App::apply_sys —───────────────▶ self.inv.observe(Sample)
                        │                                │
                        ├─ histories (deques, bounded)   └─ findings (≤ 64)
                        ▼
                 ui::draw(app)  ──▶ Dashboard | Graphs | Investigation
```

## 13.3 Module boundaries & ownership — CONFIRMED

- **OS acquisition vs. platform-independent math** is kept separate, as required: raw
  counters and handles live in `sampler.rs`, `diskio.rs`, `gpu.rs`, `platform.rs`,
  `platform::pdh.rs`, `procs::ProcProbe`; pure computations live in `calc.rs` (normalisation,
  counter-rate, series stats, per-adapter/LUID maths, LHM temperature selection) and
  `diagnostics.rs` (rule lifecycle). No `unsafe` outside the `windows` FFI calls, and no
  `unsafe` in the pure modules.
- **Single owner per OS handle.** `ProcProbe` keeps its *own* `System`/user cache for
  on-demand details and termination (`src/procs.rs`), so the sampler's hot `System` is never
  mutated from the UI thread. The sampler owns `Disks`, `Networks`, `Components`,
  `DiskIoMonitor`, `GpuMonitor`. There is no shared mutable state across threads other than
  the bounded channel and atomics in the config.
- **`reading.rs` is the single validity vocabulary.** `Reading{T}` has one meaning project-wide
  (`Valid` / `Stale` / `Unavailable`). The subtlest rule — `value()` includes `Stale`,
  `is_valid()` does not — is honoured consistently: every diagnostic decision goes through the
  `fresh()` helper (`src/diagnostics.rs`), and the GPU history uses `value()` only to draw
  sparklines (which legitimately hold the last value). **CONFIRMED** by full read.

## 13.4 Metric-semantics consistency — CONFIRMED

Cross-checked against [06-metric-semantics.md](06-metric-semantics.md):

| Metric | Unit / aggregation | Verdict |
|--------|--------------------|---------|
| CPU global / per-core | clamped 0–100 %; per-core independent | consistent across probe, dashboard, sample |
| Process CPU % | normalised to 0–100 % using the logical-CPU count passed from `App` | consistent (`calc::process_cpu_percent`, doc 10) |
| Memory used | `total − available` (excludes reclaimable standby) | consistent |
| Disk throughput | `PhysicalDisk(_Total)` = machine-wide, not per-process | documented as aggregate (doc 06) |
| Disk capacity | per-mount used/free; warn ≤10 %, crit ≤5 % free, no persist | consistent |
| Net rate | sum over *real* interfaces; pseudo excluded | consistent (probe shows `4 real, 2 pseudo`) |
| GPU util | DXGI per-adapter, primary-only summary, 0.5 % epsilon, 100 % clamp | consistent |
| GPU memory | dedicated/shared from DXGI; VRAM limit present | consistent (no PDH `Dedicated Limit`) |
| Temperatures | CPU partial (ACPI), GPU from LHM only; `Unavailable` otherwise | consistent (`--probe`: `Components 0`) |

The unavailable-vs-zero distinction is preserved end-to-end: `--probe` prints `GPU
temperature  : n/a` (not `0`), `Components 0  (no sensor provider …)`, and disk `read 0 B/s`
is a genuine zero while temperature is genuinely absent. **CONFIRMED**.

## 13.5 End-to-end validation — CONFIRMED

Full gate after all changes:

- `cargo fmt --check` — clean.
- `cargo clippy --all-targets -- -D warnings` — clean (no warnings).
- `cargo test` — **82 passed, 0 failed** (+1 vs the MP07 baseline of 81; the new test is
  `app::tests::process_keys_enabled_only_on_dashboard`, see §13.6).

`--probe` (release, reference machine) — all sources answered:

```
CPU global usage : 21.2%          Components : 0 (no OS sensor provider)
RAM              : 11.29 / 16.98 GB
GPU adapters     : 2 (DXGI + PDH)  Intel UHD 620 util 13.1%, vram 128 MiB
GPU temperature  : n/a
Disk activity    : read 0 B/s write 0 B/s (backend: PDH PhysicalDisk _Total)
Processes        : 276
```

`--bench` (release, 40 samples/source) — no scheduling pathology:

```
refresh_cpu_usage        mean   0.101  p95   0.207  ms
refresh_memory           mean   0.033  p95   0.057  ms
refresh_processes(lean)  mean  14.767  p95  22.193  ms   (max 131.1)
networks.refresh         mean   7.143  p95   8.978  ms
components.refresh       mean   5.399  p95   7.398  ms
gpu refresh (engine+mem) mean   2.376  p95   2.930  ms
live sampler (balanced, ~3 s):
  sys samples 15 (4.9/s) · gpu 4 (1.3/s) · dropped 0 · overruns 0 · gpu failures 0
  sample cost  mean 13.16  p95 71.98  max 89.90 ms
```

The live rates match the balanced preset exactly (sys every 200 ms ⇒ 5/s; gpu every
1000 ms ⇒ 1/s). Zero drops, zero overruns, zero GPU failures. Release TUI smoke run stayed
alive and responsive for the full window and shut down cleanly on signal. **CONFIRMED**.

## 13.6 Defects found and fixed in this pass

### D1 — Process-scoped keys acted on an off-screen process — FIXED

The process table is drawn on the **dashboard only**: `draw` dispatches to
`draw_graphs` or `draw_investigation`, both of which replace the body
(`src/ui.rs`). But the process keys were gated on `is_focused(Panel::Processes)` and the
default focus *is* `Panel::Processes` (`src/app.rs`), so after switching to Graphs or
Investigation the focus was stale and `s`/`f`/`v`/`/`/`Enter`/`Space`/`x` still acted —
including `x`, which stages a termination for a process the user cannot see.

Fix: a single predicate `App::process_keys_enabled()` returns `view == Dashboard`
(`src/app.rs:501`), and the event loop uses it instead of the inline view checks for
`s`/`f`/`v`/`/` (`src/main.rs:208-211`) and `x`/`Enter`/`Space` (`src/main.rs:213-227`).
The change is a guard, not a rewrite; the modal confirm still names the PID and process, so
even the previous behaviour was never *automatic*. This hardens the pre-existing Graphs case
as well as the new Investigation view. Test:
`app::tests::process_keys_enabled_only_on_dashboard`.

### D2 — In-TUI help was stale — FIXED

The `g` help line read “toggle dashboard / graphs”, omitting the investigation view added in
[12-performance-investigator.md](12-performance-investigator.md). Now
“cycle dashboard / graphs / investigation” (`src/ui.rs:1144`), and a note line states that
process keys act only on the dashboard (`src/ui.rs`). The CLI `--help` was already correct.

### Documentation corrections

- [06-metric-semantics.md](06-metric-semantics.md): the disk-I/O row now names the real
  backend (`\PhysicalDisk(_Total)\Disk {Read,Write} Bytes/sec`, machine-wide aggregate,
  `Reading<u64>`) and the GPU-temperature row now states it comes from optional
  LibreHardwareMonitor WMI and is `Unavailable` when absent (the default here).
- [09-dashboard.md](09-dashboard.md) and [10-process-explorer.md](10-process-explorer.md):
  the interaction notes now say the process keys are active only on the dashboard (the only
  view that draws the table).

## 13.7 Regression investigation (performance & memory)

**Performance — no regression.** Source latencies are flat versus [03-performance.md](03-performance.md)
and the MP07 run; `refresh_processes` remains the dominant cost (lean, ~15 ms mean). The
MP07 learner adds only `inv.observe` on the UI thread — a bounded loop over six rules with
small `Vec` scans — and costs no measurable time in `--bench` (the sampler path is
unchanged). **CONFIRMED**.

**Memory — small, bounded increase; not a leak.** Working set measured over 10 s of live
TUI: **24.7 MB, flat at every 2 s sample** (2/4/6/8/10 s all 24.7 MB). The original audit
recorded ~21.6 MB ([README.md](README.md)). The ~3 MB delta is the retained findings state
and the investigator's fixed buffers, and it is *steady*, so it is a bounded footprint, not
growth. The buffers are provably bounded: findings are capped at `MAX_FINDINGS = 64` and
trimmed every observation (`src/diagnostics.rs:564`); the network baseline ring is capped at
`net_baseline_n + 1 = 31` (`src/diagnostics.rs:583`); the history deques are fixed-length
(`HISTORY = 120`). **CONFIRMED**.

## 13.8 Known limitations / not validated

- **CAPABILITY-unsupported, not a fault.** This machine exposes no OS CPU thermal zone
  (`Components 0`) and no built-in GPU temperature; GPU temperature is therefore `n/a`, which
  is correct rather than missing. Fan and power are `no` by platform. See
  [11-hardware-capabilities.md](11-hardware-capabilities.md). **CONFIRMED**.
- **NEEDS VALIDATION.** Multi-adapter GPU aggregation, per-process GPU, and the
  LibreHardwareMonitor temperature path cannot be exercised on hardware with a single
  integrated adapter and no sensor provider; they are implemented and gated but only
  compile-verified here.
- **Termination is force-only on Windows.** `Signal::Kill` → `taskkill /PID <pid> /F`
  (`src/procs.rs`); there is no graceful path on this platform. Documented in doc 10 and the
  UI.
- **Historical documents** [01](01-architecture.md)–[07](07-correctness-changes.md) describe
  the pre-fix baseline and retain some superseded details (e.g. `POLL = 20 ms`, "37 tests").
  They are intentionally the audit of record at that time; the current contract is docs
  05–12 and this document.

## 13.9 Reproduce

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                     # expect: 82 passed, 0 failed
cargo run --release -- --probe # capability ceiling + raw sources
cargo run --release -- --bench # per-source latency + live sampler health
cargo run --release            # TUI: g cycles views, i investigation, ? help
```

Remove a stale binary first if the build output is locked (`os error 5`):
`Get-Process neomonitor -ErrorAction SilentlyContinue | Stop-Process -Force`.
