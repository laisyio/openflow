# OpenFlow ASR + dictionary comparison

Corpus SHA-256: `4af7d2160bb10fa98faaa859693d8945aa8afa74d18cbced731c90afd5508d83`

Local offline recognition, not app end-to-end latency. Synthetic/derived clips are separate strata, not extra independent readers.

| Model | Stratum | Coverage | WER | CER | p50 / p95 seconds | RTF |
|---|---|---:|---:|---:|---:|---:|
| moonshine-tiny | human-clean/en | 24/24 | 7.88% | 3.31% | 0.209 / 0.423 | 0.030 |
| moonshine-tiny | human-derived/en | 6/6 | 23.64% | 12.45% | 0.182 / 0.274 | 0.025 |
| moonshine-tiny | human-stitched/en | 2/2 | 7.28% | 2.47% | 3.525 / 5.178 | 0.028 |
| moonshine-tiny | non-speech/en | 3/3 | n/a | n/a | 0.026 / 0.030 | 0.006 |
| moonshine-tiny | synthetic-dictation/en | 10/10 | 17.05% | 14.60% | 0.187 / 0.234 | 0.028 |
| moonshine-tiny | synthetic-dictation/es | 1/1 | 82.35% | 32.00% | 0.197 / 0.197 | 0.030 |
| moonshine-tiny | synthetic-dictation/fr | 1/1 | 200.00% | 92.50% | 0.195 / 0.195 | 0.034 |
| moonshine-tiny | synthetic-dictation/zh | 1/1 | 1400.00% | 322.58% | 0.189 / 0.189 | 0.026 |
| moonshine-base | human-clean/en | 24/24 | 3.38% | 1.91% | 0.247 / 0.506 | 0.040 |
| moonshine-base | human-derived/en | 6/6 | 18.18% | 9.66% | 0.235 / 0.382 | 0.034 |
| moonshine-base | human-stitched/en | 2/2 | 3.28% | 1.89% | 4.764 / 7.301 | 0.038 |
| moonshine-base | non-speech/en | 3/3 | n/a | n/a | 0.029 / 0.029 | 0.006 |
| moonshine-base | synthetic-dictation/en | 10/10 | 15.34% | 13.90% | 0.251 / 0.328 | 0.038 |
| moonshine-base | synthetic-dictation/es | 1/1 | 82.35% | 26.67% | 0.219 / 0.219 | 0.033 |
| moonshine-base | synthetic-dictation/fr | 1/1 | 100.00% | 66.25% | 0.213 / 0.213 | 0.037 |
| moonshine-base | synthetic-dictation/zh | 1/1 | 1800.00% | 535.48% | 0.307 / 0.307 | 0.042 |
| qwen-0.6b | human-clean/en | 24/24 | 2.44% | 1.06% | 0.448 / 0.776 | 0.060 |
| qwen-0.6b | human-derived/en | 6/6 | 10.00% | 3.00% | 0.362 / 0.615 | 0.057 |
| qwen-0.6b | human-stitched/en | 2/2 | 2.43% | 0.77% | 7.872 / 11.740 | 0.063 |
| qwen-0.6b | non-speech/en | 3/3 | n/a | n/a | 0.175 / 0.182 | 0.038 |
| qwen-0.6b | synthetic-dictation/en | 10/10 | 9.66% | 7.53% | 0.408 / 0.461 | 0.062 |
| qwen-0.6b | synthetic-dictation/es | 1/1 | 64.71% | 24.00% | 0.486 / 0.486 | 0.074 |
| qwen-0.6b | synthetic-dictation/fr | 1/1 | 0.00% | 0.00% | 0.378 / 0.378 | 0.066 |
| qwen-0.6b | synthetic-dictation/zh | 1/1 | 0.00% | 0.00% | 0.444 / 0.444 | 0.061 |
| qwen-1.7b | human-clean/en | 24/24 | 2.44% | 1.44% | 1.007 / 1.832 | 0.143 |
| qwen-1.7b | human-derived/en | 6/6 | 8.18% | 2.58% | 0.909 / 1.498 | 0.136 |
| qwen-1.7b | human-stitched/en | 2/2 | 2.00% | 0.64% | 16.948 / 24.912 | 0.136 |
| qwen-1.7b | non-speech/en | 3/3 | n/a | n/a | 0.404 / 0.406 | 0.085 |
| qwen-1.7b | synthetic-dictation/en | 10/10 | 2.84% | 2.43% | 0.936 / 1.060 | 0.142 |
| qwen-1.7b | synthetic-dictation/es | 1/1 | 0.00% | 0.00% | 1.100 / 1.100 | 0.168 |
| qwen-1.7b | synthetic-dictation/fr | 1/1 | 0.00% | 0.00% | 0.919 / 0.919 | 0.160 |
| qwen-1.7b | synthetic-dictation/zh | 1/1 | 0.00% | 0.00% | 0.936 / 0.936 | 0.129 |

## Completion and failures

- moonshine-tiny: completed; 0 unsuccessful clips.
- moonshine-base: completed; 0 unsuccessful clips.
- qwen-0.6b: completed; 0 unsuccessful clips.
- qwen-1.7b: completed; 0 unsuccessful clips.

## Fresh-process startup and memory

File hashes are verified before loading, warming the OS file cache. These are not disk-cold startup measurements. RSS is not Metal allocated memory.

| Model | Load s | First inference s | Loaded RSS MiB | OS peak RSS MiB | After unload RSS MiB |
|---|---:|---:|---:|---:|---:|
| moonshine-tiny | 0.245 | 0.224 | 196.2 | 508.5 | 188.4 |
| moonshine-base | 2.223 | 0.308 | 471.5 | 763.0 | 276.5 |
| qwen-0.6b | 1.376 | 0.369 | 1122.0 | 1277.3 | 520.1 |
| qwen-1.7b | 1.830 | 1.088 | 657.0 | 880.0 | 136.5 |

Metal allocated peaks during warm inference (not total footprint; not additive with RSS):

- qwen-0.6b: 3089.7 MiB.
- qwen-1.7b: 4551.6 MiB.

## Critical terms and non-speech

Aliases are fixed before recognition; they do not replace the stricter WER score.

| Model | Stratum | Term recall | Non-speech false positives |
|---|---|---:|---:|
| moonshine-tiny | non-speech/en | n/a | 33.33% |
| moonshine-tiny | synthetic-dictation/en | 90.00% | n/a |
| moonshine-tiny | synthetic-dictation/es | 50.00% | n/a |
| moonshine-tiny | synthetic-dictation/fr | 0.00% | n/a |
| moonshine-tiny | synthetic-dictation/zh | 0.00% | n/a |
| moonshine-base | non-speech/en | n/a | 0.00% |
| moonshine-base | synthetic-dictation/en | 93.33% | n/a |
| moonshine-base | synthetic-dictation/es | 50.00% | n/a |
| moonshine-base | synthetic-dictation/fr | 50.00% | n/a |
| moonshine-base | synthetic-dictation/zh | 0.00% | n/a |
| qwen-0.6b | non-speech/en | n/a | 0.00% |
| qwen-0.6b | synthetic-dictation/en | 93.33% | n/a |
| qwen-0.6b | synthetic-dictation/es | 50.00% | n/a |
| qwen-0.6b | synthetic-dictation/fr | 100.00% | n/a |
| qwen-0.6b | synthetic-dictation/zh | 100.00% | n/a |
| qwen-1.7b | non-speech/en | n/a | 0.00% |
| qwen-1.7b | synthetic-dictation/en | 100.00% | n/a |
| qwen-1.7b | synthetic-dictation/es | 100.00% | n/a |
| qwen-1.7b | synthetic-dictation/fr | 100.00% | n/a |
| qwen-1.7b | synthetic-dictation/zh | 100.00% | n/a |

## Paired human accuracy uncertainty

WER difference, with 95% speaker-cluster bootstrap interval; negative favors the first model. Small corpus, not population-level proof.

- moonshine-tiny minus moonshine-base: 4.50% [1.86%, 8.54%], 12 readers, 24 successful shared clips (complete-case comparison).
- moonshine-tiny minus qwen-0.6b: 5.44% [2.88%, 9.59%], 12 readers, 24 successful shared clips (complete-case comparison).
- moonshine-tiny minus qwen-1.7b: 5.44% [2.69%, 9.80%], 12 readers, 24 successful shared clips (complete-case comparison).
- moonshine-base minus qwen-0.6b: 0.94% [-0.42%, 2.77%], 12 readers, 24 successful shared clips (complete-case comparison).
- moonshine-base minus qwen-1.7b: 0.94% [-0.35%, 2.08%], 12 readers, 24 successful shared clips (complete-case comparison).
- qwen-0.6b minus qwen-1.7b: 0.00% [-1.16%, 1.14%], 12 readers, 24 successful shared clips (complete-case comparison).

See JSON for raw transcripts, critical-term recall, non-speech hallucinations, per-repeat timings, file hashes, versions and failures.

Do not use pooled averages to hide unsupported languages, number/name mistakes or failed clips. WER retains fillers and does not equate digit formatting with written numbers; critical-term aliases are scored separately. Model selection needs real dictation and phone measurements before changing defaults.

## Post-pass scope

Scoring only. All timings remain RAW recognition; dictionary is never provided to models. Raw outputs and agreement are preserved in JSON; only the first output per clip is corrected.

## Measurement notes

- Exploratory Apple M4 / 16 GiB / macOS 26.5.2 run, 48 clips (571.1s), 3 warm repeats per model/clip, 4 models, 576 warm recognition calls plus 4 first-inference anchors.
- Model order was Tiny, Base, Qwen 0.6B, Qwen 1.7B. Other engineering work occurred on the host; latency comparisons are not a dedicated-idle randomized crossover benchmark.
- SHA-256 artifact verification warmed the filesystem cache before each loader timer. Fresh-process startup here is not disk-cold startup.
- Qwen Metal allocations are not fully represented by RSS. In particular the smaller loaded RSS for Qwen 1.7B does not imply a smaller model footprint.
- The original run completed every clip before checkpoint supervision was added. Recognition calls and scoring were unchanged; executed harness hash is preserved separately from report renderer hashes.
- Moonshine Python timing includes WAV decoding and conversion to a Python float list; Qwen timing uses its path-based decoder. These are local adapter timings, not Swift app or end-to-end pipeline timings.
