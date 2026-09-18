# Performance round 3: bounded Dictate previews

Measured on macOS 26.5.2 (25F84), arm64, Rust 1.98.0, release profile.
The change is confined to the Dictate result preview; it does not change UI,
capture, provider routing, the full copy payload, or transcription performance.

## Change and bounds

The former implementation split the entire transcript into a temporary vector,
joined the whole text, took 220 Unicode scalar values, and counted the entire
normalized string again. The replacement normalizes as it walks the input,
retains at most 220 scalars plus an ellipsis, and stops when the next normalized
scalar proves truncation. It requests at most 883 bytes for its single output
allocation (220 × 4 UTF-8 bytes + 3 bytes for the ellipsis).

This is bounded output storage, **not bounded input scanning in every case**.
Arbitrarily long leading whitespace, whitespace between visible words, or
trailing whitespace after an exactly-at-limit result must still be inspected.
Those scans preserve the old whitespace/truncation semantics. Ordinary long
transcripts and long unbroken words stop after the needed prefix.

## Reproduction

Run only this exact ignored benchmark; do not enable every ignored test:

```sh
rtk proxy cargo test -p openflow-native --offline --release ui::dictate::tests::benchmark_dictate_preview_compare -- --ignored --exact --nocapture --test-threads=1
```

The benchmark retains the previous production function as the baseline, checks
output equality, uses `black_box` on inputs and outputs, performs 3 warmups and
31 measured samples, and alternates old/new order. Each short-case sample makes
10,000 calls; each 1 MiB sample makes 10 calls. Values below are per-call times
derived from those batches. No machine-sensitive speed threshold is imposed.
All inputs are synthetic; no microphone, credentials, or personal history are
accessed. Warm inputs and repeated allocators make this a microbenchmark, not
an end-to-end cold-start or UI responsiveness measurement.

| Synthetic case | Old median | New median | Old p95 | New p95 | Median ratio |
| --- | ---: | ---: | ---: | ---: | ---: |
| ASCII, 68 bytes | 425.2 ns | 112.1 ns | 513.7 ns | 113.4 ns | 3.79× |
| Unicode, 59 bytes | 311.4 ns | 87.9 ns | 322.4 ns | 88.9 ns | 3.54× |
| Ordinary words, 1,048,576 bytes | 1,565,354.2 ns | 325.0 ns | 1,639,408.4 ns | 412.5 ns | 4,816.47× |

The large ratio reflects removing full-input work for a tiny visible prefix.
The absolute large-input saving is about 1.565 ms per preview on this machine;
it does not imply an equivalent speedup for the application or speech engine.

## Correctness and gates

Equivalence tests compare against the former function for empty strings,
leading/trailing Unicode whitespace, newlines, multibyte characters, emoji and
combining marks, 219/220/221-scalar boundaries, boundary separators, a 1 MiB
unbroken word, long whitespace runs, and 1,000 deterministically generated
mixed-text cases. Tests also check the bounded output size/allocation capacity.

- Focused Dictate tests: 17 passed, 1 benchmark ignored.
- Entire native suite: 141 unit + 7 integration tests passed; 2 benchmarks ignored.
- Exact opt-in release benchmark: 1 passed.

No other ignored tests, including microphone tests, were run.
