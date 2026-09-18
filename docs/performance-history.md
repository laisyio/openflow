# Desktop performance round 1: bounded history reads

Measured 2026-09-15 in the release build, bundled SQLite 3.45.0.

## Change

The native tray, Dictate and History pages synchronously request recent rows.
`ORDER BY created_at DESC LIMIT ?` previously scanned the entire table and built
a temporary sort even for the single newest result. A timestamp-only descending
index now serves those reads. No transcript text is duplicated into the index.

The idempotent migration runs after the existing one-time deleted-content scrub.
`secure_delete`, retention, search semantics and the 500-row cap are unchanged.
Existing databases pay the index construction cost once, then reopen without
rebuilding it.

## Reproduce

From the repository root:

```sh
rtk proxy cargo test -p openflow-core --release --offline db::tests::benchmark_history_index_compare -- --exact --ignored --nocapture
```

Select this exact ignored test; do not run all ignored tests, since a separate
audio test opens the microphone.

The test creates a fresh UUID-named temporary file-backed database with 50,000
synthetic rows. It inserts ascending one-second RFC3339 timestamps, 232 bytes of
raw text and 180 bytes of formatted text per row. It calls the actual
`Database::get_history` method, including mutex acquisition, statement preparation,
row conversion and allocations. Each limit has three warm-up calls followed by
31 timed calls; the median is reported. Results are passed to `black_box`.

In the same fixture, the test compares the original no-index state against the
indexed state, checks the identical newest 500 row IDs and order, and measures
index construction and database file growth. Index removal occurs only inside
this owned synthetic fixture. The fixture is removed after the test.

## Results

| Rows requested | Before median | Indexed median |
| --- | ---: | ---: |
| 1 | 14.295334 ms | 0.008083 ms |
| 50 | 44.107500 ms | 0.032750 ms |
| 500 | 66.739125 ms | 0.251083 ms |

Index construction: **17.626 ms**. File size: 27,078,656 → 28,766,208 bytes,
an increase of **1,687,552 bytes** for this fixture.

Query plan before: `SCAN transcriptions; USE TEMP B-TREE FOR ORDER BY`.
Query plan after: `SCAN transcriptions USING INDEX transcriptions_created_at_desc`.

The initial paired run before enabling production migration independently showed
17.368417 → 0.008208 ms, 55.305875 → 0.032792 ms, and 69.766666 → 0.252167 ms
for the same respective limits. Timing variation is expected. These are local
warm-query measurements, not app-launch, microphone or transcription latency
measurements; a fresh empty history does not gain the same absolute savings.
The benchmark does not claim faster arbitrary substring searches. CI asserts
query-plan and data correctness, never hardware-dependent timing thresholds.

## Regression evidence

- The index query-plan regression failed before production migration with the
  original full scan and temporary sort.
- Tests verify opening a legacy schema twice creates one index, preserves
  timestamp/tie order and settings, and keeps `secure_delete` enabled.
- Search across raw/formatted text, zero-limit, delete, clear and the 500-row cap
  retain their behavior. Existing deleted-content scrub tests remain in the suite.
- Full core suite: **130 passed, 3 ignored**. The ignored cases are live microphone,
  model installation/inference, and this opt-in benchmark. The benchmark passed
  separately. The suite needed permission for its temporary loopback servers and
  fixture subprocess inspection; the initial sandboxed run denied those operations.
