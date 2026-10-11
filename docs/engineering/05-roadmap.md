# 05 — Prioritised Improvements, Dependencies, Roadmap & Verification

## 7. Prioritised improvements

Impact and risk are judged against the evidence in 01–04. "Risk" = chance the change breaks
something that currently works. IDs reference the registers (C = correctness, P = performance,
R = reliability, M = missing capability).

| Rank | Improvement | Ref | Impact | Risk | Effort |
|------|-------------|-----|--------|------|--------|
| 1 | **Fix GPU engine grouping key** (include `phys`+`eng`) | C3 | High | Low | < 1 h |
| 2 | **Relabel/scale process CPU** (per-core ⇒ show cores or ÷N; fix colour) | C5 | High | Low | < 1 h |
| 3 | **Deduplicate network interfaces** (drop filter/mirror pseudo-NICs) | C11 | Medium | Low | 1–2 h |
| 4 | **Fix graph time base** (store timestamps; label real span; align series) | C1,C2 | Medium | Medium | 3–5 h |
| 5 | **Stop polling components when the platform has none** | P2 | Medium | Low | 1 h |
| 6 | **Honest GPU memory/identity** (don't label iGPU aperture as VRAM; mark "unavailable") | C4 | Medium | Low | 1–2 h |
| 7 | **Add `--help`/`--version`; reject invalid `--speed`** | C7,M5,R9 | Low | Low | < 1 h |
| 8 | **Add unit tests + CI** for parsers (`engine_key`, rate math, `human_bytes`) | R7,M9 | High (long-term) | Low | 2–4 h |
| 9 | **Idle-friendly poll** (adaptive/blocking-with-timeout) | C10,P* | Low | Medium | 2–3 h |
| 10 | Move GPU memory to PDH / slow interval; fix bench label | P3,C8 | Low | Low | 2 h |
| 11 | Cross-platform / disk-I/O / GPU-temp backends | M1,M2,M3,M10 | High | High | weeks |

## 8. Improvement dependencies

- **#8 (tests/CI) unlocks safe execution of everything else.** The confirmed `engine_key` bug
  is exactly the kind of pure-function defect a unit test would have caught. Recommended to
  do #8 first *or* alongside #1 (a test for the new key is the natural proof of #1).
- **#1 → #3-style per-pid work (M3).** Once engines are keyed correctly, per-process GPU
  utilisation is a small extension (same instances already carry `pid_`).
- **#4 depends on #1 and #3 conceptually** only in that all three touch "how many streams of
  data exist"; the time-base fix should be done after the sampling streams are settled so the
  timestamps model is chosen once.
- **#6 depends on deciding adapter-identity semantics** (which LUID is "the" GPU). Without
  that, per-adapter VRAM is still ambiguous. Low blocker; can ship as a labelling fix first.
- **#9 (poll) is independent** but should not precede #8, because changing the event loop
  without tests is exactly the "medium risk" scenario.
- **#11 requires a provider abstraction** (see 04) and should not start before the Windows
  correctness backlog (#1–#6) is cleared.

## 9. Phased roadmap

### Phase 0 — Safety net (do first, ~half day)
- Add `#[cfg(test)]` unit tests for `engine_key`, net-rate math, and `human_bytes`.
- Add a minimal CI job (`cargo build`, `cargo clippy -- -D warnings`, `cargo test`).

### Phase 1 — Correctness, high-confidence (1–2 days)
- #1 GPU engine grouping key (both PDH and WMI copies — ideally unify into one helper).
- #2 Process CPU scale/label/colour.
- #3 Network interface dedup.
- #7 CLI `--help`/`--version` + reject invalid `--speed`.

### Phase 2 — Correctness, structural (3–5 days)
- #4 Graph time base: capture `Instant` (or monotonic ms) per sample; compute real span;
  align or separately window CPU/RAM vs GPU series.
- #5 Stop polling empty components.
- #6 Honest GPU memory/identity labelling.

### Phase 3 — Efficiency & polish (2–4 days)
- #9 Adaptive poll / blocking read with timeout; keep redraw-on-change.
- #10 Move GPU memory to PDH or slower cadence; correct the bench label; maybe split the
  bench into PDH vs WMI rows.

### Phase 4 — Capability (ongoing, larger)
- #11 provider abstraction; disk I/O (M2); GPU temperature (M1); per-process GPU (M3);
  config persistence (M6); alerts (M7).

## 10. Verification strategy

Per-change verification, mapped to the change. All are runnable here.

| Change | How to verify |
|--------|---------------|
| #1 GPU key | Unit test parsing the live instance format `pid_4_luid_0x0_0x13EC2_phys_0_eng_3_engtype_3D`. Integration: compare `neomonitor --probe` GPU % against Task Manager; must be ≤ single-engine max, never the 9× sum. Cross-check vs `typeperf "\GPU Engine(*)\Utilization Percentage"` (max over engines). |
| #2 Process CPU | `neomonitor --probe`/TUI: a 2-core busy process should read ~200 % (or be shown as "200 % (2.0 cores)"). Colour test with a spinner task; assert it is not auto-red. |
| #3 Net dedup | `neomonitor --probe` on this machine must no longer list `Wi-Fi` and `Wi-Fi-VirtualBox NDIS…` with identical counters; header aggregate must match the physical NIC rate. |
| #4 Graph time base | Instrument the span; label must equal `samples × fast_interval`. With Balanced (200 ms × 120) the axis must read ~24 s, not 120 s. GPU and CPU curves must share a wall-clock x axis. |
| #5 Components | On this machine `components.refresh` must be called once, not per `slow` tick; idle CPU should drop slightly. Verify by a counter/log or by the bench. |
| #6 GPU memory | `--probe` must not print `VRAM 0 / 1073741824` for the iGPU; it should say "integrated / shared" or "unavailable". |
| #7 CLI | `--speed bogus` prints a warning and/or exits non-zero; `--help` lists options; `--version` prints `0.1.0`. |
| #8 Tests/CI | `cargo test` green; CI runs on push. |
| #9 Poll | Idle CPU measured over 7 s must drop below the current ~0.045 cores at Minimal; input latency unchanged. |
| #10 GPU mem | `--bench` must report PDH engine and WMI memory as separate rows with correct labels. |

Regression guard for every phase: re-run `--bench` and confirm no source regresses beyond
~10 %, and re-run `--probe` to confirm identical values except the intentionally changed metric.

---

## Recommended single first improvement

**Do #1: fix the GPU engine grouping key** (`src/gpu.rs:142` and the duplicate at
`src/gpu.rs:255`).

Reasoning:

- **Highest impact among low-risk fixes.** GPU utilisation is a headline number; today it is
  computed by summing engines that must not be summed. Verified live: adapter `…13EC2`
  presents **nine** engines all named `engtype_3D`; the current key collapses them into a
  single bucket (OLD groups = 1) and sums them (`src/gpu.rs:249`), then clamps at 100
  (`src/gpu.rs:251`). Corrected keying yields the intended 9 buckets. Likewise adapter
  `…13B09` goes from 7 buckets to 13, removing the accidental merge of empty-`engtype`
  engines. So the displayed value can be inflated several-fold and can falsely peg at 100 %.
- **Lowest risk.** The blast radius is one pure string parser plus its two call sites: no
  threading, channel, snapshot, or UI changes. If the new key were somehow wrong, the worst
  case is a different (still bounded 0–100) number, not a crash.
- **Cheap and testable.** It is a pure function over an `&str`, so it is the perfect first
  subject for the Phase-0 unit test — simultaneously closing part of R7.
- **Unlocks M3** (per-process GPU) as a follow-on.

Concretely, include `phys` and `eng` in the key, e.g. transform
`…_phys_0_eng_3_engtype_3D` to `0x00000000_0x00013EC2|phys_0_eng_3|3D`, and collapse the two
copies of `engine_key` into one shared function so the PDH and WMI paths cannot drift.
