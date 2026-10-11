# 03 — Performance & Reliability Findings

All numbers measured on the reference machine (Intel i7-8565U, 8 threads, Windows) with the
release build, `neomonitor --bench` (25 iterations each) and `--probe`.

## 5. Performance

### Measured per-source sampling cost (release)

Command: `target\release\neomonitor.exe --bench` (2026-10-11).

| Source | ms/call | Gated by | Notes |
|--------|---------|----------|-------|
| `refresh_cpu_usage` | 0.037 | fast | cheap |
| `refresh_memory` | 0.049 | fast | cheap |
| `refresh_cpu_frequency` | 0.094 | slow | cheap |
| `refresh_processes` (lean) | **10.912** | proc | dominates the sys tick |
| `networks.refresh` | 6.753 | net | COM/enumeration |
| `disks.refresh` | 0.129 | slow | cheap |
| `components.refresh` | 4.644 | slow | even with 0 components |
| `gpu refresh` (PDH + WMI) | 8.415 | gpu | mostly the WMI memory query |
| `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` | 200 ms | — | Windows CPU floor |

**P1 — `refresh_processes` dominates (CONFIRMED).** At 10.9 ms per proc tick it is ~97 % of
the sys work that matters. At Balanced (`proc = 1000 ms`) that is ~1.1 % of one core; at
Fast (`proc = 500 ms`) ~2.2 %. It scales with the process count, so machines with 500+
processes will be materially heavier. The refresh kind is already lean
(`ProcessRefreshKind::nothing().with_cpu().with_memory()`, `src/sampler.rs:124`), so the
kernel walk itself is the cost — it cannot be cheapened much within sysinfo.

**P2 — `networks.refresh` (6.75 ms) and `components.refresh` (4.64 ms) are relatively
expensive for the data they yield.** `components.refresh` costs ~4.6 ms to return **zero**
readings on this machine (WMI/ACPI probe that fails). Gating by `slow` (2000–10000 ms)
keeps the average low, but at Fast the components probe is pure overhead when the platform
has no sensors. A one-time capability probe (refresh once; if `list().is_empty()`, stop
polling) would remove it. **SUSPECTED improvement; not a bug.**

**P3 — GPU WMI memory query (~8 ms) is the bulk of `gpu refresh`.** The PDH engine read is
fast (the earlier 338 ms → ~10 ms fix targeted the engine path, where the win really was).
The residual cost is the per-tick WMI `GPUAdapterMemory` COM query (`src/gpu.rs:103`),
which changes slowly and could be moved to PDH or a longer interval without loss.
**CONFIRMED** (the `--bench` label `gpu refresh (pdh)` is itself misleading — see C8).

### Idle cost

- Sampled over 7 s in release with the UI running at Balanced: process CPU ≈ **0.045 cores**
  (~4.5 % of one core, ~0.56 % of the 8-thread machine), working set flat at **~21.6 MB**.
- Working set did not grow across the run → the bounded `VecDeque` histories
  (`src/app.rs:259`) and per-tick snapshot replacement do not leak. **CONFIRMED stable.**

### Rendering cost

- Redraws happen only on new data, input, or the 1 s clock (`src/main.rs:86`), so the terminal
  is not repainted 50×/s. This is the correct design for a monitor and is why idle CPU is low
  despite the 20 ms poll.
- Remaining always-on wake source: the 20 ms `event::poll` (`src/main.rs:93`) → up to 50
  wake-ups/s. It does not repaint, but it does prevent deep CPU idle. **CONFIRMED** (C10).

### Minor allocation findings (no action needed)

- Full process vector is rebuilt and moved into the snapshot every proc tick
  (`src/sampler.rs:291`–`300`). At 274 procs this is small.
- `sort_rows` runs on every snapshot (`src/app.rs:139`) — O(n log n) on ≤274 rows, negligible.
- `ifaces.to_vec()` copies the interface vector each sys tick (`src/sampler.rs:309`). Small.

## 6b. Reliability & risk register

| ID | Risk | Assessment |
|----|------|------------|
| R1 | **Terminal left in raw/mouse mode on panic** | Mitigated: a panic hook restores mouse capture before the default hook runs (`src/main.rs:42`–`46`), and normal exit disables capture + restores (`src/main.rs:50`–`51`). **CONFIRMED handled.** |
| R2 | **Producer threads never stop cleanly** | They exit when `tx.send` fails because the receiver was dropped (`src/sampler.rs:172`, `src/sampler.rs:325`). Detached threads are reclaimed at process exit. **CONFIRMED acceptable**; no join/handshake, so a mid-refresh WMI call may be cut off at exit — harmless. |
| R3 | **WMI unavailable / COM failure** | `WMIConnection::new()` failure is handled (`src/gpu.rs:68`–`81`), and every query falls back to `unwrap_or_default()` (`src/gpu.rs:121`, `src/gpu.rs:132`). GPU column degrades to zeros rather than crashing. **CONFIRMED handled**, though the UI shows `0.0 %` rather than "unavailable". |
| R4 | **Tiny terminal** | Layout uses `Constraint::Min`/percentages (`src/ui.rs:31`, `src/ui.rs:168`); ratatui tolerates zero-size areas, and scroll math uses `saturating_sub` (`src/app.rs:230`, `src/app.rs:244`). **SUSPECTED low risk**; not exercised on a <10-row terminal here. |
| R5 | **Counter wrap / interface reset** | Guarded with `saturating_sub` in net-rate math (`src/sampler.rs:203`). **CONFIRMED handled.** |
| R6 | **Unsigned underflow in disk `used`** | `total.saturating_sub(avail)` (`src/ui.rs:474`). **CONFIRMED handled.** |
| R7 | **No automated tests or CI** | **NOT IMPLEMENTED.** Correctness of parsing (notably `engine_key`, C3) is untested; a regression could ship silently. This is the highest-leverage process gap. |
| R8 | **No `--help` / `--version` / config file** | **NOT IMPLEMENTED.** CLI is discoverable only by pressing keys or reading source. |
| R9 | **Silent preset fallback** | `--speed nonsense` → Balanced with no warning (`src/main.rs:63`). **CONFIRMED** (C7). |
| R10 | **`load_average()` polled every draw** | Called in `draw_network_column` on each redraw (`src/ui.rs:598`), bypassing the sampler cadence; a needless syscall at up to redraw rate. Minor. |
