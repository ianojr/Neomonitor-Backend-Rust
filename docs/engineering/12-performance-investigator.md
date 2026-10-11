# 12 — Performance Investigator

Scope: the deterministic, explainable condition-detection engine (`src/diagnostics.rs`) and its
dedicated UI (`ui.rs`, `draw_investigation`). This document states **what the engine detects**,
**how it decides**, **what it refuses to claim**, and **what is deliberately out of scope**.

Evidence labels follow the audit convention: **CONFIRMED**, **SUSPECTED**,
**NOT IMPLEMENTED**, **NEEDS VALIDATION**. Citations use `path:line` / `path:fn`.

## 12.1 Purpose and safety contract

The investigator turns raw samples into a small number of *findings*. It is **interpretation
only**: `diagnostics.rs:1`–`22` states the design constraints, and the code never mutates system
state, never kills a process, and never performs I/O beyond what the sampler already collected.

Guarantees, each backed by the engine loop (`Investigator::observe`, `diagnostics.rs:550`):

- **Deterministic and time-driven.** `observe` takes the sample time from `Sample.at`, which is
  the snapshot's `generated` `Instant` (`Sample::from_snapshot`, `diagnostics.rs:294`). The engine
  reads no clock internally, so a whole lifecycle can be replayed in a unit test.
- **No diagnosis on missing evidence.** A metric carries a `Reading` (`reading.rs:14`). The engine
  uses a value only when it is `Valid`; `Stale` and `Unavailable` contribute nothing
  (`fresh`, `diagnostics.rs:1243`). Corroborating values embedded in a finding are likewise
  fresh-only.
- **One finding per active condition.** A continuously breaching rule updates its finding in place
  (`update_finding`, `diagnostics.rs:819`); it is never re-reported as a new alert.
- **Correlate, do not cause.** Evidence strings describe co-occurring measurements; none assert
  *why* the machine is busy (e.g. the process rule explicitly separates process-local from
  system-wide load, `build_proc`, `diagnostics.rs`).

## 12.2 What is observed

`Sample` (`diagnostics.rs:243`) is a normalised projection of `SysSnapshot` plus the latest
`GpuStats` (`from_snapshot`, `diagnostics.rs:294`):

| Field | Source | Type |
|-------|--------|------|
| `cpu` | `snap.cpu_global` | `Reading<f32>` (%) |
| `mem_total/used/available`, `swap_total/used` | `snap.mem` | `u64` bytes |
| `cpu_temp`, `gpu_temp`, `gpu_util` | `snap.comps` / GPU stats | `Reading<f32>` |
| `net_rx`, `net_tx` | `snap` interface totals | `Reading<u64>` (B/s) |
| `procs` | top CPU + top memory rows, deduplicated | `Vec<ProcSample>` |
| `disks` | `snap.disks` | `Vec<DiskFree>` |

The process set is *trimmed*: the busiest 8 by CPU and largest 6 by memory, de-duplicated
(`diagnostics.rs:312`). This bounds cost and is sufficient for the process rule; it is **not** a
complete process catalogue (that is the explorer's job, doc 10).

## 12.3 Rules, thresholds and severity

Thresholds default at `diagnostics.rs:185`; the selected rule's primary threshold is adjustable at
runtime with `[` / `]` (`adjust_threshold`, `diagnostics.rs:501`).

| Rule | Fires when | Persist | Warning / Critical | Confidence |
|------|-----------|---------|--------------------|-----------|
| `CpuSustained` | global CPU ≥ 85 % | 15 s | ≥85 % / ≥95 % | depends on process attribution |
| `MemoryPressure` | used ≥ 90 % of total | 15 s | ≥90 % / ≥96 % | High; swap is corroborating |
| `ProcessHog` | one process ≥ 50 % CPU **or** ≥ 25 % of RAM | 10 s | a single hot process | Medium (process tick lags) |
| `NetworkSurge` | throughput ≥ 5× rolling baseline, and ≥ 1 MiB/s | 5 s | relative jump | Low (short baseline) |
| `DiskCapacity` | any volume ≤ 10 % free | none (immediate) | ≤10 % / ≤5 % | High |
| `Thermal` | hottest fresh sensor ≥ 85 °C | 10 s | ≥85 °C / ≥95 °C | High; only a fresh sensor counts |

Severity is a three-level ladder `Notice < Warning < Critical` (`Severity`, `diagnostics.rs:97`);
`severity_for` (`diagnostics.rs:675`) maps the measured value onto the warning/critical pair. A
rising severity re-notifies; a falling one does not de-escalate a still-active finding downward
without recovery.

`ProcessHog` deliberately handles **contradictory evidence**: a hot process on an otherwise idle
machine is reported as process-local, not as system-wide saturation (`build_proc`).

## 12.4 Lifecycle

`RuleRuntime` (`diagnostics.rs:403`) tracks, per rule, the last outcome, a continuous
breach/clear streak since a timestamp, and a cooldown deadline.

```
Unavailable ───────────────► (no finding; retire after missing_grace)
Clear ──(persist)──► Breach ──(persist window)──► ACTIVATE (new finding)
                                       │                │
                          still breaching ──► UPDATE in place
                                       │
Clear ──(recover window)──► retire + cooldown(30s) ──► can fire again
```

- A condition must persist for its window before activation, so a brief spike is ignored
  (`on_breach`, `diagnostics.rs:766`).
- A breach that ends must stay clear for `recover` (10 s) before the finding is retired
  (`on_clear`, `diagnostics.rs:751`).
- After retirement the rule is suppressed for `cooldown` (30 s); a **Critical** severity overrides
  the cooldown so a genuinely severe, re-arming condition is not hidden.
- If evidence disappears (`Unavailable`/`Stale`) while active, the finding is not silently
  reinterpreted; after `missing_grace` (30 s) it is retired with an explicit reason
  (`retire`, `diagnostics.rs:839`).
- The retained log is capped at `MAX_FINDINGS = 64` (`diagnostics.rs:32`).

## 12.5 Confidence and limitations

Every `Finding` (`diagnostics.rs:365`) carries a confidence level and an explicit limitations
list, because the honesty of the output matters more than the confidence of the tone:

- **High** — the metric is direct (memory %, disk capacity, a fresh sensor).
- **Medium** — a supporting metric is sampled on a slower tick and may lag (process figures).
- **Low** — the conclusion rests on a short rolling baseline, not a long-term profile (network).

Known limitations stated in the UI and encoded in evidence:

- **No OS thermal sensors here.** The reference machine reports `Components 0` (doc 11), so on
  this hardware the thermal rule can only aggregate what sensors exist; with none, it stays
  `Unavailable` and never fires. **CONFIRMED** (`--probe`).
- **No per-process GPU.** Adapter utilisation is summed across processes (doc 11), so GPU load is
  never attributed to a process.
- **Short network baseline.** `net_baseline_n = 30` samples, median of the recent window excluding
  the newest (`net_stats`, `diagnostics.rs:593`); it is a local burst detector, not a long-term
  profile.
- **Trimmed process set.** Only the busiest/largest rows are considered, so a process that is hot
  only briefly may not be in the sample.
- **Absolute thresholds are configurable heuristics**, not universal definitions of "slow"
  (memory %, CPU %, free disk %). The UI shows each rule's current threshold and lets the user
  change it.

## 12.6 UI and interaction

`View::Investigation` is the third view (cycle with `g`, or open with `i`;
`app.rs` `toggle_view` / `open_investigation`). It is read-only.

- **List** (`draw_inv_list`): severity glyph, rule, title and age, with the active/logged counts
  in the title and a scrollbar. Recovered findings are dimmed.
- **Evidence** (`draw_inv_detail`): title, state, observed age, how long active, the detection
  rule, every measured value, the evidence bullets, confidence + note, the recommended next
  investigation, every limitation, and the selected rule's enabled state and threshold.
- **Empty state** explains why nothing is shown and lists every rule with its on/off state and
  current threshold (`rule_legend`).
- **Keys:** `i` investigation · `↑`/`↓`/`j`/`k`/PgUp/PgDn/Home/End select a finding · `d` enable
  or disable the selected rule · `[` / `]` lower or raise the selected rule's threshold.
- The **footer** shows an alert badge with the active count and the maximum severity, plus a
  `(+n)` marker for findings raised since the view was last seen (`unseen`, `diagnostics.rs:492`).

## 12.7 Verification

- Unit tests (`diagnostics.rs`, `#[cfg(test)] mod tests`): sustained vs. brief spike, recovery +
  cooldown, retiring on missing evidence, stale never breaching, duplicate suppression,
  contradictory process evidence, memory utilisation vs. exhaustion (swap), network surge vs.
  baseline, disk capacity critical, thermal fresh-only, disabling a rule, the unseen counter, and
  threshold adjustment.
- UI test `ui::tests::renders_investigation_view_with_findings` drives real findings through the
  view at narrow and wide geometries, including selection, rule toggle and threshold changes,
  without panicking.
- Full gate: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
  (81 tests) all clean; `--probe` and `--bench` unchanged on the reference machine.

## 12.8 Out of scope

- **No remediation.** The engine never kills, throttles, cleans or reconfigures anything.
- **No network access, no persistence, no telemetry.** Findings live in memory for the session.
- **No root-cause claims.** It reports *what happened with which evidence and confidence*, and
  states the limitations; the interpretation is the user's.
