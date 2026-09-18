# General desktop performance: three follow-up cycles

Requested 2026-09-18. This is a new, sequential optimization pass beyond the
earlier history index, history-cell formatting and bounded home-preview work.
Existing uncommitted desktop/onboarding changes are preserved.

## Method and scope

Each cycle follows baseline → implementation → regression/equivalence tests →
release measurement → independent review → corrections. The benchmark skill's
baseline/comparison principle is adapted to native Rust paths; browser vitals
are inapplicable and no browser performance grade is claimed.

Machine: Apple M4, 16 GiB RAM, macOS 26.5.2 (25F84), Rust 1.98.0. Synthetic data
only: no microphone, personal history, API credentials, provider inference or
paid requests. Release-mode opt-in benchmarks retain previous implementations
as reference oracles and compare both in one process, alternating order.
Timing assertions are deliberately not CI gates because machine load varies.

These are component CPU timings, not end-to-end dictation or model latency.
No audio quality trade-off, reduced privacy checks or new network access is
part of the optimization. The three cycles must close serially, not be counted
as three repetitions of the same benchmark.

## Cycle 1 — completed: live-preview/final transcript comparison

`agreement.rs` previously ran the full Levenshtein dynamic-programming grid even
when preview and take matched or differed only at the end. It now removes equal
prefix/suffix tokens and sizes working rows to the shorter remaining sequence.
The exact distance and original take-length score denominator are unchanged.
Unrelated text retains the quadratic worst case; this is not an approximation.
The engine performs this comparison after the pipeline completes and before
emitting result/history events. The saved work reduces that reporting cost;
it is not a claim of faster provider inference or earlier text insertion.

Baseline before production changes: identical 1,600-token input took 3.951812 ms
in the reference and 3.947042 ms in the then-current implementation. This checked
that the oracle and production baseline measured the same work.

Final same-process comparison, median of nine measured batches after warm-up:

| Synthetic comparison | Previous | Updated |
|---|---:|---:|
| Short, 6 vs 7 tokens | 1.638 µs | 1.426 µs |
| Identical, 1,600 vs 1,600 | 4.045334 ms | 0.181938 ms |
| Prefix, 1,440 vs 1,600 | 3.557125 ms | 0.162083 ms |
| One correction, 1,600 vs 1,600 | 3.950667 ms | 0.172666 ms |
| Unrelated, 1,440 vs 400 | 0.808979 ms | 0.809771 ms |

Tests: original examples plus empty/prefix/suffix/repeated-token/CJK/case score
cases and 1,200 generated token pairs, each checked against the original exact
distance in both orientations. Six tests pass; the benchmark passes separately.
Independent review confirmed correctness and requested clearer case dimensions
and a true median. Both reporting corrections were applied and rerun.

Reproduce:

```sh
cargo test -p openflow-core --release --offline agreement::
cargo test -p openflow-core --release --offline agreement::performance_tests::benchmark_agreement_compare -- --ignored --exact --nocapture
```

## Cycle 2 — completed: audio level/gain preparation

`audio.rs` used to calculate the same 95th-percentile sample magnitude for the
silence gate and again for auto-gain, then allocate a new gained-sample vector.
Both stop and live-preview paths now compute that level once and adjust the
already-owned resampled buffer in place. Stop's encoder is factored out so
tests exercise the actual production errors and bytes without a microphone.
The silence threshold, gain curve, clamp and PCM encoder are unchanged.

This removes one magnitude-scratch allocation/selection and one gained-vector
allocation per audible take/preview. For three minutes at 16 kHz, each vector
contains 2,880,000 `f32` samples (11.52 MB). This is allocation-volume reasoning,
not a measured process-RSS reduction: native capture and WAV storage still exist,
and the two avoided temporaries were not necessarily alive at the same time.

Baseline was recorded before production edits. Frozen reference functions keep
the old level/gain/error behavior. Equivalence tests cover exact finite sample
bits, silence and gain thresholds, empty/short recordings, exact WAV bytes,
duration, source immutability and in-place buffer reuse. Synthetic cases include
quiet/normal/transient/silent input at 1/30/180 seconds and 16/48 kHz.

Independent review passed source correctness and measurement fairness: no work
moved into the capture callback or capture lock. Nine-sample reported tail values
are only observed maxima, so the report uses medians, not a tail-latency claim.
Core suite after cycle 2: 133 passed, 5 intentionally ignored. An initial run in
the default sandbox denied existing loopback/process tests; rerunning with the
required local-test permissions passed. No test behavior was weakened.

Median times in milliseconds, paired original → updated. Two warm-up batches
and nine measured batches per implementation, alternating order. Post-resample
measurements exclude identical input cloning on both sides; full preview includes
resampling and encoding. Two complete post-change runs are shown because the
second experienced substantial scheduling noise:

| Case | First run | Repeat run |
|---|---:|---:|
| 30 s normal, 16 kHz, full preview | 1.901959 → 1.497708 | 1.975875 → 1.512709 |
| 30 s normal, post-resample stage | 1.895250 → 1.419041 | 3.692375 → 2.890167 |
| 180 s quiet, 16 kHz, full preview | 13.796584 → 10.304167 | 20.391458 → 15.379292 |
| 180 s normal, 16 kHz, full preview | 14.483334 → 11.132209 | 25.694875 → 25.341667 |
| 180 s normal, post-resample stage | 12.953500 → 9.495583 | 27.947542 → 21.037958 |
| 30 s normal, 48 kHz, full preview | 15.565375 → 15.072209 | 25.256833 → 27.354708 |

The 16 kHz stage removes measurable work. Do not claim a reliable whole-pipeline
48 kHz improvement from this cycle alone: the effect is small relative to the
observed noise. Silence already performed one p95 pass and gets no structural
speedup; its measured ratios also fluctuated. The original pre-change baseline
was measured separately (30 s normal 16 kHz preview: 1.894 ms; 180 s normal:
12.992917 ms) and reference/current control timings mostly agreed within 1%.

Reproduce:

```sh
cargo test -p openflow-core --offline audio::
cargo test -p openflow-core --release --offline audio::gain_perf_tests::benchmark_audio_gain_compare -- --ignored --exact --nocapture
```

## Cycle 3 — completed: FIR resampling

The 63-tap low-pass filter previously checked both input boundaries for every
tap of every output sample. Most windows are entirely inside the recording.
`downsample` now checks that window once and iterates its bounded slice together
with the filter taps. Windows at either end use the original guarded loop.
Filter design, sample positions, left-to-right `f32` operations, upsampling and
output duration are unchanged. There is no SIMD/FMA reassociation or unsafe
indexing and no extra buffer is allocated.

Pre-change control at 48 kHz/180 s: original resampler 82.713208 ms, then-current
82.175416 ms; full preparation 91.130833 vs 91.391750 ms. Both then used the
same old loop. Final comparison holds cycle-2 gain changes identical on both
sides, so the earlier gain improvement is not counted again.

After-change medians in milliseconds, nine measured samples after warm-up:

| Rate / duration | Resampler previous → updated | Full preparation previous → updated |
|---|---:|---:|
| 44.1 kHz / 1 s | 0.442958 → 0.180583 | 0.503125 → 0.228875 |
| 44.1 kHz / 30 s | 13.698291 → 5.399708 | 15.139125 → 6.830375 |
| 44.1 kHz / 180 s | 82.257917 → 32.485250 | 91.394208 → 41.534917 |
| 48 kHz / 1 s | 0.457792 → 0.180375 | 0.506167 → 0.229417 |
| 48 kHz / 30 s | 13.674834 → 5.392834 | 15.150500 → 6.870541 |
| 48 kHz / 180 s | 82.026541 → 32.380042 | 91.171625 → 41.432167 |
| 96 kHz / 1 s | 0.461875 → 0.184125 | 0.510167 → 0.232292 |
| 96 kHz / 30 s | 13.698209 → 5.453667 | 15.170625 → 6.929084 |
| 96 kHz / 180 s | 82.062458 → 32.643833 | 92.599250 → 43.641334 |

Full preparation is actual preview encoding: resampling, minimum-length check,
silence detection, gain and WAV encoding. The capture buffer copy, device I/O,
network and model work are excluded. Both variants include output destruction.
Synthetic deterministic finite samples are created before timing.

Exact bit comparisons cover zero/equal/upsampling rates as well as 44.1/48/96
kHz downsampling, lengths 0–128 and threshold/longer cases, impulses at each
edge/center and signed zero. WAV bytes and errors are compared separately.
Existing speech-band preservation and anti-alias rejection tests also pass.
Focused audio suite: 26 passed, 3 intentionally ignored.

Independent review confirmed safe checked windows, unchanged boundary behavior
and arithmetic, and independently reran all 26 audio tests. No fixes requested.
The repeated benchmark confirmed the effect: 48 kHz/30 s full preparation
15.165958 → 6.872500 ms; 48 kHz/180 s 92.094708 → 42.469417 ms.
At 44.1 kHz/180 s it was 91.152084 → 41.390709 ms and at 96 kHz/180 s
91.349250 → 41.914625 ms. These improvements remained clear despite the
machine-load variability observed during cycle 2.

Reproduce:

```sh
cargo test -p openflow-core --release --offline audio::
cargo test -p openflow-core --release --offline audio::resample_perf_tests::benchmark_resample_compare -- --ignored --exact --nocapture
```

## Final verification and artifacts

- All three sequential implementation/review cycles completed.
- Core/native suite: 286 passed, 8 intentionally ignored. Only exact named
  synthetic benchmarks were opted in; ignored microphone/model tests were not run.
- All-target core/native Clippy with warnings denied: passed. A test-only
  negated floating comparison was clarified with `partial_cmp` after the first
  lint run, preserving the exact predicate.
- Optimized release build, strict bundle signature verification and packaged
  isolated `--self-check`: passed.
- No UI changes were made in this pass. Existing desktop/onboarding work,
  local-only restrictions, provider behavior and cancellation remain unchanged.
- App: `target/OpenFlow.app`; previous bundle retained at
  `target/OpenFlow-before-general-performance.app`. No installed application,
  permissions, credentials or user data was replaced. Ad-hoc signing may require
  the user to grant macOS microphone/accessibility permission again.
- The optimization pass ended without commits, pushes or a pull request; publication is a separate follow-up.

## Separate follow-up observation

Read-only scouting noticed nonstreaming speech responses are size-checked after
`response.bytes()` has buffered them. A bounded response collector deserves a
separate robustness fix. It is not counted as a completed optimization here.
