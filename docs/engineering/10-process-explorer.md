# 10 — Process Explorer

Scope: MASTER PROMPT 05 (process explorer). This records the work that turns the old
top-N process table into a reliable explorer: richer metadata, an on-demand details view, a
search/filter/sort/tree view pipeline, a selection cursor, and an explicitly-confirmed
termination action with identity revalidation.

Guiding rules followed: no new dependencies; keep OS acquisition separate from
platform-independent view math; never collect expensive per-process data on the collection
cadence; never terminate automatically; show unavailable values as unavailable; document
platform differences instead of hiding them.

---

## 1. Collected vs. visible data

The central change is a separation between **what is collected** and **what is shown**:

- `SysSnapshot.procs: Vec<ProcRow>` is the *collected* set — **every** process, in
  collection order, with only cheap fields.
- `App.view_rows: Vec<ViewRow>` is the *visible* set — indices into `snap.procs` plus a tree
  depth, produced by the pure pipeline in `src/procs.rs`. It is recomputed only when the
  collected data or a view option changes (`App::rebuild_proc_view`), **never during a
  redraw**.

Previously `apply_sys` sorted and truncated to `TOP_N = 200` and the UI read that list
directly. The cap meant a search could not see processes it had dropped; it has been removed.
At the reference machine's ~274 processes the full list is trivial to hold and sort.

`ProcRow` now carries `ppid`, `start_time` and a platform-neutral `ProcState`, all of which
sysinfo returns for free (see §3). `cpu`/`mem` come from the process refresh kind already
requested on the process tick (`src/sampler.rs`).

## 2. View pipeline (pure, tested)

`src/procs.rs` contains only platform-independent logic:

| Function | Responsibility |
|----------|----------------|
| `matches_query(row, q)` | case-insensitive name substring **or** PID substring; empty query matches all |
| `Filter::matches` | `All` / `Active` (cpu ≥ 0.1 %) / `Memory` (≥ 32 MiB) |
| `cmp_rows(a, b, key)` | deterministic ordering: primary key, then name (case-insensitive), then PID |
| `ordered_indices` | filter + sort indices |
| `build_view` | `ordered_indices` plus optional tree flattening → `Vec<ViewRow>` |

Sort keys: `CPU`/`MEM`/`AGE` are descending (busiest/newest first), `NAME`/`PID` ascending.
NaN CPU values never panic (`partial_cmp` fallback) and unknown start times sort **last** so
they cannot masquerade as the newest.

### Tree construction

- Roots are processes with no live parent in the filtered set (missing parent, self-parent, or
  parent filtered out).
- A node shows its children only when it is in the `expanded` set.
- Traversal is an **iterative** DFS, so a deep parent/child chain does not grow the call stack;
  `visited` prevents infinite recursion on cycles.
- Nodes left unreachable because of a parent/child cycle are detected with `reaches_root`
  (a parent-pointer walk that fails on a cycle) and emitted as roots, so nothing silently
  disappears. Nodes merely hidden under a *collapsed* ancestor are correctly skipped.

## 3. Platform semantics (evidence from vendored sysinfo 0.38.4)

| Fact | Evidence | Consequence |
|------|----------|-------------|
| `pid`, `ppid`, `name`, `start_time` are always retrieved, even with `ProcessRefreshKind::nothing()` | `sysinfo/src/common/system.rs:2356` | added to every `ProcRow` at no extra cost |
| `cpu`/`memory` require `with_cpu()`/`with_memory()` | `common/system.rs:2395` | unchanged; already requested on the process tick |
| `exe`/`cmd`/`user`/`cwd`/`environ` are `UpdateKind` and may be unavailable | `common/system.rs:2399` | gathered **on demand** only (details view) |
| Windows `status` is hard-coded to `ProcessStatus::Run` | `sysinfo/src/windows/process.rs:278` | the State column is uninformative on Windows; documented, not faked |
| Windows supports only `Signal::Kill` | `windows/system.rs:32` | no graceful signal exists |
| Windows `kill_with` runs `taskkill.exe /PID <pid> /F` — forceful, by PID only | `windows/process.rs:365` | termination is force-only; PID reuse must be revalidated by us |

## 4. On-demand probe

`ProcProbe` (`src/procs.rs`) owns its **own** `System` and `Users`, so a details refresh or a
termination never races the sampler thread. It is created lazily on first use
(`App::probe_mut`).

- `detail(pid, logical)` refreshes exactly that PID with
  `ProcessRefreshKind::everything().without_tasks()`, then reads executable, command line,
  owner (mapped through `Users`), working directory, status, start/run time, CPU, memory,
  virtual memory, total I/O and environment-variable count into a `ProcDetail`.
- CPU is normalised to 0–100 % with the logical CPU count passed in from the App, matching the
  table.

## 5. Termination safety

Termination is deliberately conservative:

1. **Staged, not immediate.** `x` (or `x` from the details overlay) stages a `ProcIdentity`
   (`pid`, `name`, `start_time`); nothing is killed. This app's own PID is refused up front.
2. **Confirmed.** A modal asks for an explicit `y`; `n`/`Esc` cancels. The modal states that
   Windows performs a forceful kill.
3. **Revalidated.** `ProcProbe::terminate` re-refreshes the PID and compares `ProcIdentity`
   before acting. A reused PID (matching pid but a different start time, or a different name
   when a start time is unavailable) yields `Refused` and **no** kill is attempted.
4. **Outcome reported.** `Sent` / `Failed` (access denied) / `Gone` (already exited) /
   `Refused` is shown in the footer status line; if the killed process's details were open,
   they close.

Termination never happens automatically and never targets a process it cannot re-identify.

## 6. Interaction

| Key | Action |
|-----|--------|
| `/` | open the search box (name or PID); `Enter`/`Esc` apply, `Backspace` edits, `Ctrl+U` clears |
| `f` | cycle filter: all → active → memory |
| `v` | toggle the process tree |
| `Space` | expand / collapse the selected tree node |
| `Enter` | open process details |
| `x` | stage a termination (asks for confirmation) |
| `s` | cycle sort: CPU → MEM → NAME → PID → AGE |
| `j`/`k`, `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | move the **selection** when the Processes panel is focused; otherwise scroll that panel as before |
| mouse wheel | still scrolls the focused panel |

The selection **follows a process across refresh** by PID: when new data reorders the list, the
cursor stays on the same process rather than the same row. If the process disappears the
selection falls back to a valid index.

These keys act only while the process table is visible. The dashboard is the only view that
draws it; the graphs and investigation views replace the body, so the process-scoped keys are
disabled there to prevent a stale focus from staging a termination or opening details for an
off-screen process.

Modal precedence in the event loop: help → termination confirmation → details → search input →
normal keys. While a modal is open the mouse is ignored.

## 7. UI

- The process table gains a `State` column, tree indentation in the name column, a highlighted
  selected row (only when the panel is focused), and a title that reports the current
  sort / filter / tree / query.
- Empty results show "no process matches the current query or filter".
- `draw_proc_details` and `draw_confirm_kill` are centered modal overlays; the footer shows the
  transient status message and, while searching, an inline search box.

## 8. Verification

`cargo test` → **54 passed, 0 failed** (was 37; 17 added). New coverage:

- `procs::` — empty list, name/PID query matching, active/memory filters, deterministic sort
  with tie-breaks, NaN and missing-start-time comparison, tree nesting/expand, missing parent +
  self-parent + cycle handling, a 5000-node chain (linear, complete, no stack growth), and
  `ProcIdentity` PID-reuse rejection.
- `app::selection_follows_pid_across_refresh`, `selection_survives_a_process_disappearing`,
  `query_and_filter_reduce_view_without_touching_collected_rows`,
  `tree_mode_nests_and_expands`, `self_process_cannot_be_killed`,
  `confirm_without_pending_is_a_noop`.
- `ui::renders_modal_overlays_without_panicking`, `renders_tree_and_search_states`.

`cargo fmt --check` clean. `cargo clippy --all-targets -- -D warnings` clean.

## 9. Remaining limitations

| Item | Status | Note |
|------|--------|------|
| Windows process state | **CONFIRMED limitation** | sysinfo always reports `Run`; the State column is meaningful only on Linux/macOS/BSD. |
| Graceful termination | **NOT IMPLEMENTED (platform)** | Windows exposes only a forceful kill via `taskkill /F`; a clean shutdown request is not available through sysinfo. |
| Owner / command line | **NEEDS VALIDATION** | On Windows these can require elevation; when absent the details view shows `n/a` / `n/a (needs privilege)`. |
| Termination tests | **BY DESIGN** | Tests cover staging, self-refusal, identity revalidation and the no-pending no-op; they never kill a live process. |
| Tree on every process | **BY DESIGN** | Children are grouped from cheap `ppid` data only; there is no per-node OS call. |
| Selection vs. mouse | **BY DESIGN** | The wheel scrolls the view (offset) rather than moving the selection; keyboard navigation drives the cursor. |
