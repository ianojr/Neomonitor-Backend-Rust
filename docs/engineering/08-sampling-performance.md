# 08 — Sampling & Performance Engine

Scope: MASTER PROMPT 03 (sampling and performance). This is the implementation report for the
collection pipeline: scheduling, lifecycle, back-pressure, resource control, and diagnostics. It
builds on the audit in `03-performance.md` and the metric contract in `06-metric-semantics.md`.

Guiding rules followed: fix verified defects rather than rewriting correct logic; no new
dependencies; no micro-optimisation that trades correctness for a bench number; document what
could not be verified.

---

## 1. Pipeline model

Two detached producer threads feed one channel that the UI thread drains:

| Thread | Emits | Cadence |
|--------|-------|---------|
| `sys_loop` (`src/sampler.rs`) | `Update::Sys(SysSnapshot)` | every **fast** tick (CPU + RAM); processes / network / disks+sensors+freq+load on their own slower ticks |
| `gpu_loop` (`src/sampler.rs`) | `Update::Gpu(GpuStats)` | every **gpu** tick |

The UI thread runs a `crossterm` event loop that drains the channel non-blocking, then redraws
only when something changed (`src/main.rs`).

### Invariants enforced

- **Monotonic timing.** Every gate compares `Instant::now()` against the time of the previous
  refresh *of that source* (`last_net`, `last_proc`, `last_slow`). A slow call therefore delays
  the next one instead of causing a catch-up burst, and no fixed `sleep` can drift.
- **First-tick priming.** All gates start an hour in the past so every source refreshes on the
  first iteration; no metric is blank for one interval at startup.
- **OS read vs. arithmetic split.** Threads only acquire; all derived maths stays in
  `src/calc.rs` (pure, tested), per the correctness contract.

---

## 2. Changes

### 2.1 Back-pressure: bounded channel + drop counter (`src/sampler.rs`)

Previously the channel was `mpsc::channel` (unbounded). If the UI thread ever stalled, the
producers could grow the queue without bound.

- The channel is now `mpsc::sync_channel(4)`. Four is enough for the tallest burst (fast + gpu
  ticks arriving within one 50 ms poll) while capping memory at four snapshots.
- `publish()` uses `try_send`: on `Full` the newest sample is **dropped and counted**
  (`SamplerStats::dropped`); on `Disconnected` the producer exits. Producers never block on the
  consumer.
- A latest-value-cell refactor was considered and rejected as premature complexity for a stream
  that is already produced far slower than it is drained.

### 2.2 Lifecycle: stop flag + sliced sleep + joinable handle (`src/sampler.rs`)

Previously the workers exited only by failing to send after sleeping up to one full interval
(up to 10 s at Minimal), and the process relied on `main` returning.

- `SamplerConfig` carries an `AtomicBool` stop flag.
- `nap()` sleeps in ≤25 ms slices and returns early when stop is requested, so shutdown latency
  is bounded regardless of preset.
- `spawn()` now returns a `Sampler { rx, config, handles }`. `Sampler::shutdown()` signals and
  **joins** both threads; `Drop` does the same, so no worker outlives the process and no join
  handle is leaked.
- `main` calls `sampler.shutdown()` before restoring the terminal, so the terminal is never left
  in raw mode by a worker after exit.

### 2.3 Efficiency: sensor capability probe (`src/sampler.rs`)

`components.refresh()` costs ~5 ms even when the OS exposes **zero** sensors (this machine:
`Components 0`; evidence in `06-metric-semantics.md`). It is now gated by a capability flag:

- If the initial list is empty, the slow tick skips the probe.
- An absent provider is re-probed every `COMPONENT_REPROBE_TICKS = 300` slow ticks (≈15 min at
  Balanced), so a sensor provider installed at runtime is still discovered.
- If a provider was present at startup it is refreshed normally.

### 2.4 Scheduling: load average off the render path (`src/sampler.rs`, `src/ui.rs`)

`load_average()` is cheap (a global mutex clone, verified against `sysinfo` 0.38.4) but was
called every frame on the UI thread. It is now read once per **slow** tick and carried in
`SysSnapshot.load: LoadStat`, so the render path performs no sysinfo calls at all.

### 2.5 Overrun detection (`src/sampler.rs`, `src/ui.rs`)

Each `sys_loop` tick measures its own duration (`SysSnapshot.sample_cost`). If it exceeds the
fast budget the tick is counted in `SamplerStats::overruns` and the loop backs off a minimum of
5 ms (`OVERRUN_BACKOFF`) instead of busy-spinning. This makes scheduling pressure observable
rather than hidden.

### 2.6 Poll cadence (`src/main.rs`)

`POLL` reduced **20 ms → 50 ms** (resolves C10). Input latency stays ≤50 ms — well within
interactive tolerance — while halving the wake-up rate of an idle UI. The loop still redraws
immediately when data arrives, so rendering latency is unchanged.

### 2.7 Diagnostics

- **`--bench`** now reports **n / mean / p50 / p95 / max** per source (`util::summarize`) instead
  of a single mean, and adds a **live sampler** run (balanced, ~3 s) reporting system/GPU sample
  rates, drops, overruns, GPU failures and the sample-cost distribution. This measures the real
  pipeline, not just isolated calls.
- **`--probe`** annotates a `Components 0` result with "(no sensor provider exposed by the OS)"
  and reports real vs. pseudo network interfaces.
- **Footer** shows data age and, only when non-zero, the health counters (`drop`, `ovr`,
  `gpufail`), keeping the bar quiet in the normal case.

No new crates were added to `Cargo.toml`.

---

## 3. Verification

### Tests

`cargo test` → **27 passed, 0 failed** (was 21; 6 added):

- `sampler::publish_counts_dropped_when_full` — a full channel increments `dropped` and the
  producer continues; a disconnected consumer stops it.
- `sampler::nap_stops_early_when_requested` — a 5 s nap returns in <200 ms once stop is set.
- `sampler::sampler_shuts_down_promptly` — `shutdown()` joins the live workers in well under 5 s.
- `util::summarize_*` — empty/single input, interpolated percentiles (1..=100 → p50 50.5,
  p95 95.05), and non-finite filtering.

`cargo clippy --all-targets -- -D warnings` → clean. `cargo fmt --check` → clean.

### Live `--bench` (release, this machine)

Per-source latency, 40 samples, mean (ms):

| Source | Audit (`03`) | Now |
|--------|--------------|-----|
| `refresh_cpu_usage` | 0.043 | 0.079 |
| `refresh_memory` | 0.043 | 0.045 |
| `refresh_cpu_frequency` | 0.071 | 0.072 |
| `refresh_processes(lean)` | 10.9 | 10.0 |
| `networks.refresh` | 6.4 | 7.0 |
| `disks.refresh` | 0.2 | 0.19 |
| `components.refresh` | 6.0 | 4.8 |
| `gpu refresh (engine+mem)` | 1.07 | 1.02 |

Live pipeline (balanced, ~3 s): `sys` 4.9/s (expected 1/200 ms = 5/s), `gpu` 1.3/s (1/s + tick),
**0 dropped, 0 overruns, 0 GPU failures**; sample cost mean 6.4 ms, p95 32.5 ms (the
p95 spike is the tick that also refreshes processes + network + disks + sensors together — still
far below the 200 ms budget). The pipeline meets its schedule on this hardware.

Network/CPU micro-timings vary by a few tenths of a ms run-to-run and are within noise; the GPU
path remains the ~8× win recorded in `07-correctness-changes.md`.

---

## 4. Remaining limitations / needs validation

| Item | Status | Note |
|------|--------|------|
| Fast tick below 200 ms | **N/A by design** | `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL = 200ms`; faster polling returns no new CPU data. All presets use ≥200 ms. |
| Process refresh cost (~10 ms) | **CONFIRMED, accepted** | Dominates the cost but is gated (≥500 ms) and fell by luck on this run; the lean `ProcessRefreshKind` is already used. Revisit only if a preset needs <500 ms processes. |
| Slow-tick jitter | **SUSPECTED** | A tick that coincides with processes+network+disks+sensors can reach ~35 ms; harmless at ≥200 ms budget, but a machine with many disks/NICs could overrun more often — now visible via `overruns`. |
| Drop counter under load | **NEEDS VALIDATION** | Drains need a UI thread starved for >200 ms to exercise; unit test covers the code path, not a real backlog. |
| Multi-processor scheduling | **NEEDS VALIDATION** | Timer resolution and wake-up coalescing differ on other Windows builds; `overruns` is the mechanism to detect it. |

No behavioural metric was changed to improve any number here.
