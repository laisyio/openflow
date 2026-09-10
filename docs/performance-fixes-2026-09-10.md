# Performance sweep: implementation and verification

This work starts from main `efafabb` plus the pending iOS changes through
`5fa1c76`. It addresses the numbered D1–D10/M1–M8 engineering findings. The
separate feature roadmap (Android/iPad, meetings, full onboarding redesign,
destination-aware paste, etc.) is not represented as completed work.

## Desktop

| Finding | Change | Regression evidence |
|---|---|---|
| D1 | One coalesced off-main health/device/credential refresh; cached first paint; stale-generation and per-field dirty/loading guards | Refresh coalescing, stale health/prewarm response, credential edits/placeholder races |
| D2 | All capture controls share ordered background dispatch; a separate one-in-flight insertion worker rejects repeated pastes | Slow-operation ordering and 1,000 rejected-request burst tests |
| D3 | `(created_at,id)` index and deterministic keyset API; lazy async history, debounced serialized searches, cached tray recents | Query plan, identical timestamps, insert-between-pages, stale search tests |
| D4 | Local job identity/cancel, bounded uploads/connections/inference, final priority, obsolete-preview replacement and stale-publication guards | Slow fake inference, preview/final/cancel ordering and bounded queue tests |
| D5 | Multipart offsets/memoryviews; release upload before inference | Byte-preservation/parser tests; synthetic 49 MiB body peaks at 49.002 MiB versus ~245 MiB previously |
| D6 | Plugin stages off async workers, per-take manifest snapshot, live authorization checks, total ten-second budget, cancellation, Unix process-group cleanup on all exits | Cancellation, cumulative budget and exited-parent/inherited-pipe regressions |
| D7 | Preallocated capture block pool, nonblocking handoff, single DSP owner, incremental FIR/upsampling, one gate/gain percentile, WAV preallocation | Batch equivalence over six rates and three block sizes; bounded callback/pool tests |
| D8 | Digest-named staged runtimes, exact lock/package verification/model revisions, active+rollback retention | Lock/snapshot/activation tests, interrupted activation, safe deletion targets and live-child leases |
| D9 | One bounded playback session; sequence/byte/bookkeeping checks; stale-event rejection; object URL/listener disposal; bounded missing-chunk/attachment/drain deadlines | 23 playback tests, including cancellation, early completion, late events, fallback, closed/never-open/stalled MSE and timer interactions |
| D10 | Raw speech bytes for native, four-chunk async ingress, anonymous spool for decoder seeking, cached overlay text layout | Decode genuine MP3 before EOF; cancellation under backpressure; 10 MiB spool; 600 unchanged frames |

The multipart figure is isolated parser allocation, not total application RSS
or model memory. No whole-app percentage speedup is claimed. Frontend production
JS is approximately 244 KB raw / 74 KB gzip; the work primarily removes blocking
and unbounded/repeated work rather than reducing bundle size.

Important boundaries:

- One already-running MLX decode cannot be safely preempted. Queued cancelled
  jobs never decode; active cancelled results are discarded. A final take may
  still wait for that one active decode, not an obsolete preview backlog.
- Native playback can use anonymous temporary disk storage. The file is
  account-only and unlinked before audio is written; playback does not retain a
  complete compressed clip in application heap. Browser replay has its own
  buffers beyond the application's payload/accounting limits.
- Substring history search is still non-FTS. Keyset pagination exists in the
  core API; this is not a full transcript editor/paginated history product UI.
- Existing native credential **writes** on end-editing remain synchronous;
  the repeated slow reads, device enumeration and health checks moved off-main.
- Unix ordinary plugin descendants are cleaned up; Windows still kills the
  direct plugin child only. Job Objects remain required for equivalent Windows
  descendant cleanup. Plugins are trusted executables, not sandboxed code.
- On Unix, runtime cleanup keeps the active and one previous verified generation.
  Shared cross-process leases protect running sidecars, setup and probes;
  cleanup defers until use ends. Unknown, legacy, corrupt or symlinked directories
  are preserved, not guessed to be disposable. Interrupted activation fails
  closed. Non-Unix pruning is disabled rather than deleting without equivalent
  process leases. There is no new rollback/model-removal UI.

## Mobile

M1–M8 are implemented in the iOS code and test gates:

- Hard sample ceiling works independently of stop-on-silence and keeps the
  beginning of a take.
- Explicit capture leases own model residency; abandoned prewarm unloads;
  cancellation and session generations reject stale recognition/clipboard work.
- Base/Tiny replacement waits for current work and unloads the old engine.
- Downloads have owned cancellation, per-model admission, unique staging,
  verified resume/reuse, previous-install rollback and installed-state checks.
- Stateful low-rate upsampling, once-per-block FIR compaction and one percentile
  calculation avoid repeated audio work.
- History persistence/cache/retention are serialized off the main actor.
- Cancellation and persistence share an atomic commit-admission boundary. A
  cancellation that wins before admission writes neither last transcript nor
  history; an already-admitted commit may finish. Disk I/O does not hold the
  admission lock, and clipboard publication still checks the live session.
- Real controller tests run from a reproducible host harness. CI generates the
  app/keyboard/widget project, compiles Release with the real engine, and tests
  the controller on Simulator.

See [mobile regression and device gates](../apps/ios/PERFORMANCE.md). Background
execution is finite. Retry retains stopped audio only while the process lives;
this is not durable recovery after iOS termination. Physical phone footprint,
energy, thermal behavior, interruptions and keyboard round trips require device
validation. This host has Command Line Tools, not Xcode; CI is configured but
the Release/Simulator gate has not run locally.

## Evaluation

[Methodology and commands](../evals/README.md),
[raw baseline](../evals/baselines/2026-09-10-m4/comparison.md),
[dictionary baseline](../evals/baselines/2026-09-10-m4/comparison-postpass.md).

The full run used 48 clips (571.1 seconds), 12 human readers, four local models,
three warm repeats per clip and one first-inference anchor per model. All 576
warm recognition calls completed. Exact audio/model hashes, versions, raw
transcripts, errors, timings and memory metrics are retained.

The strict raw clean-human WER was Tiny 7.88%, Base 3.38%, Qwen 0.6B 2.44%, Qwen
1.7B 2.44%. The synthetic English WER was 19.32%, 16.48%, 9.66%, 2.84%; the
predeclared dictionary changes those to 17.05%, 15.34%, 9.66%, 2.84%. This does
not justify replacing raw scores with corrected scores or choosing a model
based on one brand spelling. The 12-reader uncertainty intervals and missing
real-dictation/phone coverage are disclosed, not hidden in a pooled score.

## Local verification

Commands run from the repository root:

```sh
npm test
npm run check
cargo fmt --all -- --check
cargo test --workspace --all-features --offline
cargo clippy --workspace --all-targets --all-features --offline -- -D warnings
python3.14 -m unittest discover -s crates/openflow-native/runner -v
python3.12 -m unittest discover -s evals -p 'test_*.py' -v
swift test --package-path apps/ios/Packages/OpenFlowMobileCore
python3 apps/ios/scripts/test-controller-host.py
cargo build -p openflow-core --example eval_postpass --offline
```

At the final Rust gate: 284 tests passed; the two explicitly ignored tests need
hardware/model integration. Native tests are included (139), not inferred from
the empty non-macOS target. The optimized native release binary built and its
`--self-check` reported `engine=up`. Frontend: 23 tests and strict TypeScript/build
passed. Python: 12 runner and 15 evaluation tests passed. Swift: 109 MobileCore,
6 actual-controller tests, and all 10 Moonshine engine tests passed, including
the four real-weight tests against the cached Base model. Those model tests ran
on macOS, not an iPhone. Package/controller gates were run separately.
The hardened eval harness also completed a fresh two-human-clip/four-model smoke
run after supervision/reporting changes. That smoke run is not the baseline.

The pre-PR coverage audit also records these integration gaps, not demonstrated
production failures: actual-controller tests use no persistence repository, so
the controller-to-repository cancellation handoff is not tested end-to-end;
download tests verify completed-file reuse but not real URLSession resume tickets
and HTTP Range responses; and frontend playback tests simulate MediaSource rather
than exercising an installed WebView. Package tests and the release self-check
do not substitute for those integration gates. No instrumented coverage
percentage is claimed.

No hosted provider charges, user recordings, credential changes, microphone
capture or application installation were used for these checks. The test results
do not claim that every supported OS/device or provider has been exercised.

## Independent review closure

Parallel reviews and an independent headless review drove additional regression
fixes before handoff: runtime-generation growth, stale queued pastes, permanently
stalled completed playback, and a default eval requiring ignored generated audio.
Follow-up review added protection for interrupted activation, runtime directory
symlinks and failed monitor-thread creation. Cancellation-versus-history delivery
and overlapping playback timers were reproduced separately before being fixed.
These are resolved findings, not a claim that review proves the absence of bugs.
