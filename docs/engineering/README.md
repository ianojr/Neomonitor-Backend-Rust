# NeoMonitor — Engineering Audit

Audit date: 2026-10-11
Auditor: engineering review (evidence-based)
Scope: entire repository as of commit working tree (no VCS history available)
Binary: `neomonitor` v0.1.0, edition 2024, rustc 1.92.0, Windows 10/11
Reference hardware (the machine all live numbers were taken on): Intel Core i7-8565U
(4C/8T, base 1.80 GHz) with Intel UHD Graphics 620 only, no discrete GPU.

This audit does **not** change application behaviour. It documents what the code does,
what is correct, what is wrong, and what is missing, then recommends a single first fix.

## Evidence labels

Every non-trivial claim carries one of:

| Label | Meaning |
|-------|---------|
| **CONFIRMED** | Reproduced from source, from vendored dependency source, or from a live command on this machine. |
| **SUSPECTED** | Strongly indicated by code/API semantics but not reproduced end-to-end here. |
| **NOT IMPLEMENTED** | The capability does not exist in the codebase. |
| **NEEDS VALIDATION** | Behaviour is platform/version dependent; must be checked before relying on it. |

Citations use `path:line` (and `path:fn`) so claims can be checked by hand.

## Document map

| File | Required-audit section |
|------|------------------------|
| [01-architecture.md](01-architecture.md) | 1. Architecture & data flow · 2. Module responsibilities & dependencies |
| [02-metrics-correctness.md](02-metrics-correctness.md) | 3. Metric-by-metric correctness audit · 6a. Confirmed bugs |
| [03-performance.md](03-performance.md) | 5. Performance & reliability findings · 6b. Risks |
| [04-platform.md](04-platform.md) | 4. Platform support matrix · 6c. Missing capabilities |
| [05-roadmap.md](05-roadmap.md) | 7. Prioritised improvements · 8. Dependencies · 9. Phased roadmap · 10. Verification strategy |
| [06-metric-semantics.md](06-metric-semantics.md) | Metric semantics contract (validity, freshness, units, aggregation) |
| [07-correctness-changes.md](07-correctness-changes.md) | Implementation report: what was fixed, tests, limitations |
| [08-sampling-performance.md](08-sampling-performance.md) | Sampling & performance engine: lifecycle, back-pressure, scheduling, diagnostics |
| [09-dashboard.md](09-dashboard.md) | Dashboard: hierarchy, responsive layout, graphs, interaction, shortcuts, limitations |
| [10-process-explorer.md](10-process-explorer.md) | Process explorer: collected vs. visible data, view pipeline, tree, on-demand details, guarded termination, platform semantics |
| [11-hardware-capabilities.md](11-hardware-capabilities.md) | Hardware & platform capability engine: per-OS matrix, multi-adapter GPU (DXGI+PDH), disk activity, dynamic hardware, validation status |
| [12-performance-investigator.md](12-performance-investigator.md) | Performance investigator: rules, thresholds, lifecycle, confidence/limitations, read-only investigation UI |
| [13-integration-review.md](13-integration-review.md) | System integration & final review: data flow, ownership, metric consistency, e2e validation, defects fixed, perf/memory regression, limitations |

## Executive summary

- The metric collection architecture is sound: two detached producer threads feed an
  `mpsc` channel; the UI thread never blocks on a sample. Gating per source by interval
  is a good design (`src/sampler.rs:118`).
- Sampling cost is acceptable and idle cost is low: ~4.5 % of one core and a flat ~21.6 MB
  working set measured over 7 s (`--bench`, `--probe`). The dominant cost is
  `refresh_processes` (~10.9 ms) — see [03-performance.md](03-performance.md).
- Three correctness defects matter to users:
  1. **Graph time axis is mislabelled** and the three overlaid series are sampled at
     different cadences but plotted by index, so they are not time-aligned
     (`src/ui.rs:686`, `src/app.rs:132`). **CONFIRMED**
  2. **Process "CPU %" is on a per-single-core scale (0…N×100)** but is labelled and
     colour-graded as if it were 0…100 (`src/sampler.rs:297`, `src/ui.rs:635`). **CONFIRMED**
  3. **GPU engine utilisation is grouped by `(luid, engtype)`**, merging multiple physical
     engines — including nine `3D` engines on one adapter here — which inflates the number
     (`src/gpu.rs:142`, `src/gpu.rs:249`). **CONFIRMED** against live PDH counter instances.
- Windows sensor and GPU-temperature support is effectively absent, which is a platform
  limitation rather than a coding error, but the UI should say so more precisely.

## Single highest-impact, lowest-risk first improvement

**Fix the GPU engine grouping key to include the engine index (`phys` + `eng`) so the
reported utilisation matches Task Manager semantics.**

- Impact: high — GPU % is a headline metric and today can be inflated several-fold
  (verified: adapter `…13EC2` exposes nine engines all named `engtype_3D`, all summed into
  one group at `src/gpu.rs:249`). It can also peg at the 100 % clamp (`src/gpu.rs:251`).
- Risk: low — the change is confined to a pure string-parse helper (`engine_key`,
  duplicated at `src/gpu.rs:142` and `src/gpu.rs:255`) plus the two call sites. No API,
  threading, or UI changes. It is a parsing/grouping correction with a clear before/after.
- Effort: < 1 hour.

Details, and the runner-up recommendations, are in [05-roadmap.md](05-roadmap.md).

## Implementation follow-up

The recommended first fix and the rest of the C1–C11 register have since been implemented:
see [06-metric-semantics.md](06-metric-semantics.md) for the resulting metric contract and
[07-correctness-changes.md](07-correctness-changes.md) for the change/test/limitation report.
The sampling pipeline (bounded back-pressure, prompt shutdown, sensor capability probe, overrun
tracking, poll cadence, richer diagnostics) is covered in
[08-sampling-performance.md](08-sampling-performance.md). The dashboard, process explorer,
hardware-capability engine and performance investigator are documented in
[09-dashboard.md](09-dashboard.md), [10-process-explorer.md](10-process-explorer.md),
[11-hardware-capabilities.md](11-hardware-capabilities.md) and
[12-performance-investigator.md](12-performance-investigator.md). The whole system was then
re-verified end to end for integration, metric consistency and regressions — see
[13-integration-review.md](13-integration-review.md).
