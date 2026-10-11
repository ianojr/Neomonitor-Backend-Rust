# 09 — Dashboard Engineering

Scope: MASTER PROMPT 04 (dashboard). This records the visual/UX and responsiveness work on
`src/ui.rs`, `src/app.rs`, `src/metrics.rs` and `src/util.rs`. It preserves the existing visual
identity (cyan accent, bordered panels, braille charts, green/yellow/red severity) and changes
only what improves clarity, survivability at small sizes, and honesty about missing data.

Guiding rules followed: no new dependencies; no behaviour change to measurement; no expensive
system calls during rendering; every missing measurement is shown as unavailable, not zero.

---

## 1. Visual hierarchy

The dashboard keeps its three-column shape on large terminals and communicates, in order of
prominence:

| Metric | Where | Severity cue |
|--------|-------|--------------|
| CPU total | top-left gauge | bar + label coloured by 60 %/85 % thresholds |
| Per-core | below CPU | per-core bar + `%` + MHz |
| RAM | left, under cores | gauge + used/total/percent |
| Swap | left | greyed "no swap configured" when absent |
| GPU util + memory | centre | gauge; `n/a` when no backend, `(stale)` when held |
| Temperature/sensors | centre | bar, `peak`/`crit` labels, explicit "no sensors" text |
| Disks | centre | usage %, free, size |
| Network | right | ↓/↑ rates, per-interface totals |
| Processes | bottom (full width) | CPU %-of-machine, memory |
| Health | footer | age, drops, overruns, GPU failures (only when non-zero) |

Units are consistent (`%`, `MHz`, `B/s`/`/s`, `GiB`-style via `human_bytes`) and every panel
uses the same bordered style and title convention.

## 2. Responsive layout

Layout is now decided by a **pure function** `dash_plan(width, body_height)` (`src/ui.rs`) so it
can be unit-tested without a terminal:

| Condition | Result |
|-----------|--------|
| `width >= 110` **and** `body >= 20` | three columns |
| otherwise | single-column **System summary** |
| `body >= 22` | process table shown (height `min(12, body-10)`) |
| columns **and** leftover `>= 16` | history sparklines shown |
| `width < 40` **or** `height < 8` | "terminal too small" message, no layout |

The compact **System summary** replaces "squeeze three columns into a narrow terminal" with a
purpose-built list of one-line gauges in priority order: CPU, MEM, GPU, NET, DISK (worst),
SWAP, TEMP (hottest sensor or an explanation), and a compressed per-core load line. Rows that
do not fit are simply not drawn — nothing overlaps.

Header fields are also trimmed progressively (`arch` below 70 columns, `kernel` below 100, the
clock below 90) so the most important identity information survives first.

## 3. Graphs and history

- **Bounded storage.** All history buffers are `VecDeque` capped at `HISTORY = 120` samples
  (`app.rs`); pushes evict the oldest.
- **Gaps, not zeros.** `gpu_history` is `VecDeque<Option<u64>>`. An unavailable GPU window is
  `None`: the graphs-view `Chart` omits it, producing a visible gap and a "gaps = GPU n/a"
  note; the sparkline (which cannot draw gaps) holds the last known value. This removes the
  previous "hold last value as if current" ambiguity while keeping the line readable.
- **Current vs. average vs. peak.** `calc::series_stat` reduces a window to `{current, avg,
  peak}`. It drives a `cur / avg / peak` line above each graphs-view chart and the
  `avg … peak …` subtitle of every dashboard sparkline, so a sparkline's right edge is no longer
  the only readable value.
- **Scales.** Percentage series use a fixed `0..100` axis; network uses a peak-derived axis
  labelled with `human_bytes`. The x-axis labels come from `calc::span_axis_labels` and track the
  real preset cadence, not a hard-coded span.

## 4. Capability-aware display

The UI distinguishes four states everywhere a value can be missing:

| State | Rendering |
|-------|-----------|
| fresh | value + severity colour |
| stale | value + `(stale)` in yellow |
| unavailable | `n/a` + a short reason (`no backend`, `no sensors reported by the OS`) |
| genuine zero | `0.0%` / `0 B` (a real measurement) |

The help overlay adds a **Capabilities** section that reports GPU and sensor availability at
runtime, and the compact summary explains missing sensors instead of drawing an empty bar.

## 5. Interaction

Shortcuts are unchanged where they worked, with one safe addition (`?`); nothing silently
conflicts.

| Key | Action |
|-----|--------|
| `q`, `Esc` | quit (or close help when the overlay is open) |
| `?` | toggle the help overlay |
| `g` | cycle view Dashboard → Graphs → Investigation → Dashboard |
| `i` | jump to the investigation view (and clear its unseen badge) |
| `d` | (investigation) enable / disable the selected finding's rule |
| `[` / `]` | (investigation) lower / raise the selected rule's threshold |
| `s` | sort processes by CPU ⇄ memory |
| `t` | cycle sampling preset fast/balanced/eco/minimal |
| `Tab` | focus the next panel |
| `j`/`k`, `↑`/`↓` | scroll the focused panel by one row |
| `PgUp`/`PgDn` | scroll by a page |
| `Home`/`End` | jump to the top/bottom of the focused panel |
| mouse wheel | scroll the focused panel |

Process-scoped keys (`s`, `f`, `v`, `/`, `Enter`, `Space`, `x`) are active only while the
process table is on screen — that is, only in the dashboard view. The graphs and investigation
views replace the body, so a stale focus would otherwise let `x` stage a termination for an
off-screen process. The investigation view's own keys (`d`, `[`, `]`, arrows) are documented in
[12-performance-investigator.md](12-performance-investigator.md) §12.6.

The help overlay (`draw_help`) is **modal**: while open, only `?`/`Esc` (close) and `q` (quit)
are handled, so it cannot be confused with panel navigation. It documents every key plus the
capability report. The footer advertises `? help` so the feature is discoverable.

The CLI `--help` lists the same keys.

## 6. Rendering efficiency

- **No system queries in the render path.** Uptime was previously read with `System::uptime()`
  every frame; it is now captured once in `App::new` and advanced locally from a monotonic
  `Instant` (`App::uptime_secs`). Rendering performs no OS calls.
- **Display vs. sample rate stays distinct.** The footer continues to show both `fps` (redraw
  rate) and `Hz` (sample rate); the poll interval is 50 ms and redraws only happen when data
  changed or on the 1 s clock.
- Histories are collected into contiguous `Vec`s only when a widget requires `&[u64]`
  (sparkline/chart), which is inherent to those widget APIs.

## 7. Verification

`cargo test` → **37 passed, 0 failed** (was 27; 10 added). New coverage:

- `calc::series_stat_current_avg_peak` — empty/single, truncation, overflow-safe sum.
- `ui::plan_columns_only_for_wide_and_tall`, `plan_never_over_allocates_height`,
  `plan_handles_degenerate_geometry_without_panicking` — breakpoints and no over-allocation.
- `ui::truncate_is_char_safe_and_bounded` — multi-byte input is never split.
- `ui::renders_at_many_terminal_sizes_without_panicking` — drives the real `draw` through a
  `TestBackend` at 20×5, 40×8, 60×20, 80×24, 100×30, 160×50 and 200×60 across the dashboard,
  graphs view and help overlay.
- `app::help_toggles_and_closes`, `focus_cycles_through_all_panels`, `scroll_clamps_to_content`
  — state transitions.
- `app::gpu_history_marks_gaps_and_reports_stats` — `None` gaps, hold-last sparkline, stats.

`cargo fmt --check` clean. `cargo clippy --all-targets -- -D warnings` clean.

The rendering test is the automated stand-in for manual resize checks; `--probe` and `--bench`
still run clean after the changes.

## 8. Remaining limitations

| Item | Status | Note |
|------|--------|------|
| East-Asian character width | **NEEDS VALIDATION** | `truncate` counts Unicode scalars, not display cells; a CJK-heavy process name may be one cell short/long. No width dependency is added (project rule). |
| Compact summary scope | **BY DESIGN** | It intentionally omits the process table (shown separately when tall enough) and the disk table (only the fullest disk is summarised). |
| Graphs view | **BY DESIGN** | No separate compact variant; it uses flexible (`Fill`) regions and tolerates small sizes, but is most useful on wide terminals. |
| GPU gap honesty | **CONFIRMED limitation** | Sparklines cannot draw gaps, so they hold the last value; the `(stale)`/`n/a` gauge and the graphs-view gap are the authoritative signals. |
| Very small terminals | **BY DESIGN** | Below 40×8 the app shows a message rather than a degraded dashboard. |

No measurement, unit, or threshold was changed to make the interface look better.
