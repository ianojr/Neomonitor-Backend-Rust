# 01 — Architecture & Module Responsibilities

## 1. Architecture and data flow

NeoMonitor is a single-binary TUI with a three-stage pipeline: **produce → queue →
render**. There is no async runtime; concurrency is two OS threads plus the main thread
communicating over a `std::sync::mpsc` channel.

```
                       ┌───────────────────────────────────────────┐
                       │            main thread (UI)                │
                       │  src/main.rs:run()                          │
                       │                                             │
   arg parse ──────────┤  loop:                                      │
   --probe / --bench    │    drain rx.try_recv()  ──► app.apply()     │
   --speed <preset>     │    app.update_rates()                       │
                       │    if got||dirty||1s: terminal.draw(ui)     │
                       │    event::poll(20ms) ─► input → app state   │
                       └────────────▲───────────────┬──────────────┘
                                    │ Update::Sys    │ Update::Gpu
                                    │ (mpsc)         │ (mpsc)
              ┌─────────────────────┘                └───────────────┐
              │                                                      │
   ┌──────────┴───────────┐                          ┌──────────────┴─────────┐
   │  sys_loop thread      │                          │  gpu_loop thread        │
   │  src/sampler.rs:118   │                          │  src/sampler.rs:321     │
   │                       │                          │                         │
   │  System::new_all()    │                          │  GpuMonitor::new()      │
   │  Disks / Networks /   │                          │   ├─ PDH engine counter │
   │  Components           │                          │   │  (fast path)        │
   │                       │                          │   └─ WMI (static+mem)   │
   │  every fast tick:     │                          │                         │
   │   refresh_cpu_usage   │                          │  every gpu tick:        │
   │   refresh_memory      │                          │   refresh() → GpuStats  │
   │  gated by interval:   │                          └─────────────────────────┘
   │   net / proc / slow   │
   │  build_snapshot → tx  │
   └───────────────────────┘
              ▲
              │  Arc<SamplerConfig> (atomics, shared)
        cycle_preset('t') / --speed
```

### Threading model

- **UI thread** — owns `App`, `Terminal`, and the receiver. Never calls a sysinfo refresh,
  so it cannot stall on a slow WMI/PDH call. Loop at `src/main.rs:78`.
- **sys thread** — owns `System`, `Disks`, `Networks`, `Components`. Persists them across
  ticks, which is required for delta-based metrics (CPU usage, process CPU, net rates).
  Loop at `src/sampler.rs:138`.
- **gpu thread** — owns `GpuMonitor`. Separate because GPU collection is the slowest and
  most failure-prone source. Loop at `src/sampler.rs:323`.

Channel disconnection is the stop signal: producers break when `tx.send(...)` returns
`Err` (`src/sampler.rs:172`, `src/sampler.rs:325`). The threads are detached (`thread::spawn`
handles dropped), so they also die when the process exits. This is simple and adequate.

### Timing model

- `POLL = 20 ms` — max input latency; also the idle wake-up period (`src/main.rs:22`).
- `CLOCK_REDRAW = 1000 ms` — forces a redraw so the header clock ticks even with no data
  (`src/main.rs:23`).
- Redraw happens on new data, on input, or on the clock (`src/main.rs:86`). This keeps the
  terminal quiet while idle and is a deliberate power-saving measure.
- Presets select per-source intervals in milliseconds
  (`src/sampler.rs:42`): `(fast, proc, net, slow, gpu)` =
  Fast `(200, 500, 500, 2000, 500)`, Balanced `(200, 1000, 1000, 3000, 1000)`,
  Eco `(500, 2000, 2000, 5000, 3000)`, Minimal `(1000, 5000, 5000, 10000, 10000)`.
- The interval floor is 100 ms (`src/sampler.rs:82`), which coincidentally matches
  sysinfo's Windows `MINIMUM_CPU_UPDATE_INTERVAL` (see 02).

### State and history

`App` (`src/app.rs:60`) holds the latest `SysSnapshot` and `GpuStats` plus five fixed-length
`VecDeque<u64>` histories (CPU %, RAM %, GPU %, RX, TX) with `HISTORY = 120`
(`src/app.rs:12`). `push` evicts the front when full (`src/app.rs:259`). Scrolling state is
five `offset`/`page` pairs indexed by `Panel` (`src/app.rs:73`).

## 2. Module responsibilities and dependencies

| Module | Responsibility | Depends on |
|--------|----------------|------------|
| `src/main.rs` | Arg parsing, terminal setup/teardown, panic hook, event loop, `--probe`, `--bench` | crossterm, ratatui, all local modules |
| `src/metrics.rs` | Plain data types only: `SysSnapshot`, `GpuStats` re-export, per-metric structs, `Update` enum | `gpu::GpuStats` |
| `src/sampler.rs` | Threads, `Preset`, `SamplerConfig`, interval gating, net-rate math, snapshot assembly | sysinfo, `gpu`, `metrics` |
| `src/gpu.rs` | Windows GPU via PDH + WMI; non-Windows stub | windows, wmi, serde |
| `src/app.rs` | UI-facing state machine: histories, sorting, focus, scrolling, rates | `metrics`, `sampler`, `gpu`, sysinfo (host info) |
| `src/ui.rs` | Pure rendering of `App` into a ratatui `Frame` | ratatui, `app`, `util`, sysinfo (uptime, load) |
| `src/util.rs` | Formatting helpers (`human_bytes`, `human_rate`, `human_duration`, `ratio`, `bar_symbols`) | none |

### Dependency notes

- `metrics.rs` is intentionally dependency-free data; this is good layering.
- `app.rs` and `ui.rs` both call into `sysinfo` directly for **static** facts
  (`App::new` at `src/app.rs:95`–`119`; `System::uptime`/`load_average` at
  `src/ui.rs:137`, `src/ui.rs:598`). That splits "system information" knowledge across
  three modules instead of centralising it in `sampler`. This is a maintainability smell,
  not a defect.
- `windows` and `wmi` are `cfg(windows)`-gated (`Cargo.toml:13`), so non-Windows builds do
  not pull them in. Good.
- No `unsafe` outside the PDH interop block (`src/gpu.rs:152`–`264`). The unsafe block is
  small and the two-call PDH sizing pattern is implemented correctly (see 02).

### External dependency surface

| Crate | Version | Why | Notes |
|-------|---------|-----|-------|
| `sysinfo` | 0.38.4 | CPU/RAM/disk/net/proc/components | Windows impl verified in vendored source (see 02) |
| `ratatui` | 0.30.2 | TUI widgets | |
| `crossterm` | 0.29.0 | Terminal backend + input | |
| `chrono` | 0.4.45 | Header clock | `default-features = false` (good: avoids old tz db) |
| `serde` | 1.0.229 | WMI row deserialisation | |
| `windows` | 0.62.2 | PDH GPU counters | only two features enabled (good: minimal surface) |
| `wmi` | 0.18.4 | GPU static info + memory + fallback | COM/`unsafe` heavy |

No build scripts, no `[features]`, no `[profile]` overrides, no tests, no CI config,
no `rust-toolchain.toml`. The repository is `src/` + `Cargo.toml` + `Cargo.lock` + `.gitignore`
(which ignores only `/target`).
