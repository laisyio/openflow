# Desktop experience overhaul — 2026-09-15

Base: upstream main efafabb36365ff18dc5bc18cff556838d67d675b.
Branch: feat/desktop-experience-overhaul. Isolated checkout; existing checkouts untouched.
The former task directory was missing. The iOS work is not part of this change.

## Required rounds
- [x] Design round 1: implementation complete, independent review complete, corrections verified.
- [x] Design round 2: implementation complete, fresh independent review, corrections verified.
- [x] Design round 3: implementation complete, fresh independent review, corrections verified.
- [x] Performance round 1: baseline, bounded improvement, measurement and regression tests.
- [x] Performance round 2: baseline, bounded improvement, measurement and regression tests.
- [x] Performance round 3: baseline, bounded improvement, measurement and regression tests.

## Safety and scope
Native macOS app first; no mobile edits, no microphone activation, no new permission
grants, no personal data reset, no paid inference, no publishing without user request.
Visual live checks and measured performance must be distinguished from static tests.

## Design round 1
Initial baseline: 122 native unit tests + 7 bundle tests passed on unmodified main.
Implementation: native workspace, shared typography/spacing, sidebar identity, clearer
settings headings, route-aware onboarding and enforced private-mode choice.
Actual native AppKit fixtures rendered without Engine, microphone or personal storage:
target/design-round-1-rendered (14 light/dark PNGs). Original transparent captures are
retained as harness debugging evidence, not accepted visual results.

Independent reviewer: desktop_perf_scout (did not implement UI; authored capture harness).
Findings: privacy label misclassifies self-hosted routes; skip-setup leaks shortcut
capture; primary record action lacks hierarchy; welcome headings misaligned;
private setup should navigate straight to Providers. All five corrections verified
by the second independent review, with one additional effective-provider edge case
discovered in the caller (tracked below).
Fresh engine tests passed (129 passed, 1 microphone-dependent test intentionally skipped).
Native tests: 128 passed and 1 new capitalization assertion failed; label corrected.
Subsequent gate passed: 131 native unit tests and 7 bundle tests. Core suite also passed.
No live recording, permissions, production keys, or personal history used.

## Design round 2
Actual AppKit fixtures: target/design-round-2 (30 light/dark PNGs).
Same independent reviewer, new serial review after the first corrections.
Findings: the effective cleanup provider must obey same_provider; Local-only
recovery instructions named the wrong section; private completion needed an
installation checklist; cloud setup lacked an explicit optional-cleanup toggle.
Corrections verified by round 3; gate: 269 tests passed, 2 intentional ignores.
Root also found clipped local-provider privacy captions;
they now use measured wrapping, and the preview uses the real visibility helper.
History and Plugins receive consistent headings and narrow tables can scroll
horizontally instead of concealing columns.

## Design round 3
Actual AppKit fixtures: target/design-round-3 (44 light/dark PNGs).
Independent review found that a second cleanup provider could be enabled without
its own credentials. The conservative correction restricts wizard-enabled cleanup
to the tested transcription endpoint; separate cleanup remains configurable in
Settings. Added explicit accessibility labels to the onboarding privacy choices
and Settings switches. Root found a short subtitle frame in narrow Plugins and
expanded it within the existing header reservation. Final correction gate passed:
271 tests, 2 intentional ignores; Clippy with warnings denied clean. Independent
reviewer rechecked save-time enforcement and tested-key reuse: no further blocker.
Corrected native fixtures: target/design-round-3-verified (46 PNGs).

## Performance round 1 — indexed history
Actual file-backed Rust Database, 50,000 synthetic rows, release build,
31 samples after 3 warm-ups. Latest 1: 14.295334 → 0.008083 ms; latest 50:
44.107500 → 0.032750 ms; latest 500: 66.739125 → 0.251083 ms (medians).
Index creation: 17.626 ms; file growth: 1,687,552 bytes. This measures query,
locking and row conversion, not whole-window or network latency.
The pre-index query-plan regression failed first; post-fix plan, old-schema
migration, tied ordering, search and secure-deletion tests passed. Core gate:
130 passed, 3 ignored; explicit benchmark passed separately. Clippy clean.
See performance-history.md for reproducible command and detailed methodology.

## Performance round 2 — cached table display
Implementation stores text/provider display strings beside each owned full transcript,
replacing the entire row/cache collection on load, search, deletion or error.
Independent review caught stale timezone formatting in the first version. Timestamps
now format live exactly as before; only text/provider cells use the cache. Full raw
and formatted transcripts remain available for copying. Reviewer recheck passed.

Corrected release benchmark: 50 synthetic rows, 20 paints, 3 columns = 3,000 cell
requests. 31 alternating before/after trials after 3 warm-up pairs, cache construction
included, fixture duplication and container teardown excluded symmetrically.
Median: **2.715333 → 0.359834 ms (7.55×)**. This measures Rust cell-data work, not
AppKit drawing or scrolling FPS. Earlier all-column cache timing is discarded.
Native gate: 146 tests passed, 1 opt-in benchmark ignored. Reproduce:

```sh
rtk proxy cargo test -p openflow-native --release --offline benchmark_history_cells_compare -- --ignored --nocapture --test-threads=1
```

Measurements were made on the local Apple M4 / 16 GiB Mac. These are synthetic,
warm-process microbenchmarks, not promises about every user's wall-clock latency.

## Performance round 3 — bounded Dictate previews
The old implementation normalized and counted the entire transcript. The new
character scanner retains at most 220 Unicode scalar values plus an ellipsis
(at most 883 requested output bytes), stopping once it can establish truncation.
Whitespace runs still require scanning when necessary to preserve exact semantics.
An equivalence oracle covers boundaries, long words, Unicode whitespace, emoji,
combining marks and 1,000 deterministic generated inputs. Root independently
reviewed separator/truncation behavior; no remaining correctness finding.

Release medians, 31 alternating-order samples after 3 warm-ups:

| Input | Previous | Bounded |
| --- | ---: | ---: |
| Short ASCII, 68 bytes | 425.2 ns | 112.1 ns |
| Short Unicode, 59 bytes | 311.4 ns | 87.9 ns |
| Synthetic transcript, 1 MiB | 1,565,354.2 ns | 325.0 ns |

These measure preview generation only, not transcription/inference latency.
See performance-preview.md for the exact opt-in reproduction command.
Native gate: 148 passed, 2 opt-in benchmarks ignored; benchmark passed separately.

## Final handoff checks
- Combined core/native suite: **278 passed, 5 ignored**. Three ignored synthetic
  benchmarks passed separately; live microphone and model-install/inference tests
  were intentionally not run.
- Clippy core/native **all targets**, warnings denied: clean.
- Release build and bundle: target/OpenFlow.app. Code signature verifies strictly;
  no local signing identity was available, so this is **ad hoc signed**. macOS may
  request microphone, Accessibility and keychain approval again. No approvals
  were changed automatically.
- Bundled executable `--self-check`: engine up using temporary storage.
- Opened the new bundle through LaunchServices; its bundled process remained
  running at the launch smoke check. No older OpenFlow process was running.
- Release capture: target/desktop-final-layouts, **46 native PNGs**. Decoded bitmap
  comparison against design-round-3-verified: **zero pixel differences**. Encoded
  PNG bytes differed, so the comparison deliberately checked decoded pixels.
- No live spoken dictation, VoiceOver session or paid provider calls were tested.
- No existing checkout or installed app was overwritten; changes remain local on
  feat/desktop-experience-overhaul. Nothing was pushed or published.

## PR preparation — 2026-09-18

The preceding sections record the original September 15 checks, not the current
branch base. The branch was fast-forwarded to upstream main
`73f8c7767e635eb69eed6fe65e50a15b124a3f66` before publication. Upstream iOS changes
are inherited, not part of this desktop diff. The link-first follow-up is recorded
in `link-first-desktop-validation.md`; three additional general-app performance
cycles are recorded in `performance-general-app.md`.

- Fresh complete workspace tests: **287 passed, 8 ignored**. The ignored cases
  comprise six opt-in synthetic benchmarks and two microphone/model-dependent
  tests. The benchmark measurements are documented in their respective ledgers.
- Complete workspace Clippy, all targets and all features, warnings denied: clean.
- Workspace formatting, diff whitespace checks and native release build: passed.
- Final release executable `--self-check`: passed with isolated temporary storage.
- Independent headless read-only review against upstream main: no actionable
  regressions identified. This was source review, not another runtime test.
- Fresh independent core/data, native UI/privacy, and coverage/documentation
  reviews completed. Review found that the onboarding connection check blocked
  loopback services under Local-only protection. It now uses the same loopback
  predicate as Settings and the core request guard, with regression cases for
  localhost, IPv4, IPv6, hosted, LAN and lookalike-host endpoints. The fix was
  independently rechecked. README now names the current “Set up OpenFlow” link.
- Coverage boundaries remain explicit: pure state/policy tests and source-shape
  assertions are not controller-level end-to-end tests. AppKit snapshots render
  real controls with synthetic data, but do not execute save/cancel/rollback,
  late async responses, denied-permission recovery or install retry flows.
  Live spoken dictation, VoiceOver navigation and paid providers were not tested.
