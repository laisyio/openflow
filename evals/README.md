# Speech recognition evaluation

A repeatable comparison of **raw recognizers**, with a separate pass through
OpenFlow's real Rust dictionary correction. No reference transcript, dictionary
or brand hints are sent to the recognizers. No cloud calls, model downloads or
user recordings happen implicitly.

## Corpus

The first full run has 48 clips, 571.1 seconds total:

| Stratum | Clips | Purpose |
|---|---:|---|
| Human clean | 24, from 12 readers | Seeded LibriSpeech selection; two 3–16 second utterances per reader |
| Human derived | 6 | Noise at 10/5 dB, quiet speech, hard clipping, leading/trailing silence |
| Human stitched | 2 | Whole utterances joined to exceed 60/180 seconds; not natural conversation |
| Synthetic dictation | 13 | Ten English prompts with US/UK/Indian voices, plus Spanish, French and Mandarin |
| Non-speech | 3 | Silence, noise and a tone, all with empty references |

Original prompts cover addresses, dates, budgets, negation, email, technical
terms, disfluencies, ordinary short commands and multiple product names. ENTRO.LY
is not the selection criterion. Synthetic and derived cases never count as
additional independent human readers.

The 24 human WAVs and `corpus/human-manifest.json` are checked in (~5.6 MiB).
They come from [LibriSpeech test-clean](https://www.openslr.org/12/), distributed
under CC BY 4.0. Attribution: Vassil Panayotov, Guoguo Chen, Daniel Povey and
Sanjeev Khudanpur, and the LibriVox readers named per clip in the manifest.
See `corpus/LIBRISPEECH-LICENSE.txt`. Each source FLAC member and resulting WAV
has a SHA-256; the only clean-clip transformation is decoding FLAC to PCM WAV.
The archive is verified against the publisher's MD5 and its SHA-256 recorded.

Generated audio is ignored. In particular macOS voice output stays local and
is not redistributed as an application fixture. OS voice versions can change;
regenerated synthetic hashes must be reported, not assumed identical.

## Run on an Apple silicon Mac

Use Python **3.12**, not macOS's system Python 3.9. Run from the repository root.
The eval virtualenv is separate from OpenFlow's installed runtime.

```sh
python3.12 -m venv .eval-venv
.eval-venv/bin/python -m pip install -r evals/requirements-macos-py312.txt
cp evals/models.example.json evals/models.local.json
# Edit paths to models you have already installed. IDs/versions are recorded.
.eval-venv/bin/python evals/run.py --repeats 3
```

The default is the checked-in human-only manifest: no corpus download or voice
generator is needed. The larger manifest references local generated files. To
build and explicitly select it:

```sh
curl --fail --location --output /tmp/openflow-test-clean.tar.gz https://www.openslr.org/resources/12/test-clean.tar.gz
.eval-venv/bin/python evals/prepare.py --librispeech-archive /tmp/openflow-test-clean.tar.gz
.eval-venv/bin/python evals/run.py --manifest evals/corpus/manifest.json --repeats 3 --out evals/results/my-run
```

`--no-synthetic` builds the 35 human/derived/non-speech clips without macOS
voices. `--human-only` rebuilds just the redistributable human manifest/WAVs,
preserving the full manifest. `--model ID` selects one model; `--limit 2` is a
smoke check, **not** a valid model ranking. Never compare differently hashed
manifests as if they were the same experiment.

Models run sequentially in fresh processes, offline, with exact weight-file
hashes and package versions saved. Qwen uses the shipped runner's path-based
`generate` API with temperature zero; Moonshine uses non-streaming Tiny/Base
through its official Python bindings. A local `command` adapter can receive
WAV path and language and return JSON `{"text":"..."}`. Adapter commands are
explicitly trusted local code, not a sandbox or permission to use cloud APIs.

Default deadlines are 300 seconds for loading, 120 seconds per decode and
1,800 seconds per model; configure `load_timeout_s`, `clip_timeout_s` and
`suite_timeout_s` per model if needed. A hung native call terminates its worker,
preserving completed rows and marking the missing coverage. Workers checkpoint
after each completed clip; partially completed repeat sets are not ranked.
There is no automatic resume; rerun failed models with `--model` into a new
output directory. The command adapter also has a 120-second default deadline.
Ordinary descendants are cleaned up on timeout, exit or interruption; these
trusted adapters must not daemonize/escape their process groups.

## What the report measures

- WER with substitution/deletion/insertion counts; CER for character-level
  errors and languages such as Mandarin. Scores may exceed 100% with insertions.
- Fixed normalization: NFKC, case folding and punctuation separation. Numbers
  are **not** rewritten to hide formatting errors; fillers are retained.
- Predeclared critical-term aliases, scored separately from strict WER.
- Non-speech false positives, not division by zero or an invented WER.
- Per-clip three-repeat outputs and agreement; p50/p95 of the per-clip median
  latency, audio-weighted real-time factor, and explicit failure coverage.
- Fresh-process load and first inference, sampled RSS every 10 ms, OS peak RSS,
  MLX allocated peak, model disk size, and RSS before/after unload. These are
  different metrics: **never add RSS and Metal or substitute either for iPhone
  physical footprint**. Command-adapter child memory is not measured.
- Paired 95% speaker-cluster bootstrap intervals over the same successfully
  decoded human clips only. Twelve readers are not population-level proof.

Model hashing reads weights before the load timer: the OS file cache is warm.
This is **not disk-cold startup**. `non_warm_overhead_s` includes imports,
hashing, first inference, scoring, checkpoints and unload, not just startup.
Adapter timing includes audio preparation; it excludes capture, HTTP queues,
cleanup, clipboard/paste, Swift UI and app startup. Fixed model order and any
other work on the host can bias latency. Use an otherwise idle machine and
repeat sessions with reversed model order before making performance decisions.

## Raw versus corrected text

```sh
cargo build -p openflow-core --example eval_postpass
.eval-venv/bin/python evals/rescore.py evals/results/my-run/comparison.json \
  --executable target/debug/examples/eval_postpass --dictionary evals/dictionary.txt
```

The example calls the production Rust post-pass, not a Python approximation.
Raw reports stay unchanged; the separate corrected JSON preserves raw text,
scores and repeat agreement, and hashes its executable/scoring code. Timings
remain raw recognition timings; only the first output of each repeat set is
post-processed. Already-corrected inputs are rejected to avoid chained rules.
Choose the dictionary before evaluation; tuning against errors makes the set
a development set and needs a new held-out test set.

## Checked-in baseline

[September 10 M4 results](baselines/2026-09-10-m4/comparison.md) include all raw
transcripts in the adjacent JSON, plus a separate dictionary report. All four
models completed all 48 clips and 576 warm recognition calls.

| Model | Human-clean WER | Human-clean p50 | Synthetic English WER | English critical-term recall |
|---|---:|---:|---:|---:|
| Moonshine Tiny | 7.88% | 0.209 s | 19.32% | 90.00% |
| Moonshine Base | 3.38% | 0.247 s | 16.48% | 93.33% |
| Qwen 0.6B 8-bit | 2.44% | 0.448 s | 9.66% | 93.33% |
| Qwen 1.7B 8-bit | 2.44% | 1.007 s | 2.84% | 100.00% |

This supports keeping the lightweight English and accurate/multilingual tiers,
not declaring a universal winner. The Qwen human-clean WER difference is zero
on this small set; 1.7B does better on synthetic terms and stress cases. Tiny
emits text on one of three non-speech cases. English-only Moonshine's non-English
scores are deliberately visible and should not be pooled into English quality.
The three non-English synthetic clips are smoke coverage, not a multilingual
benchmark. There is no iPhone measurement here.

## Regression gate and next corpus additions

```sh
python3.12 -m unittest discover -s evals -p 'test_*.py' -v
```

CI runs this dependency-free gate, covering edit counts, no hidden number/filler
errors, CJK, empty references, corpus integrity, failure visibility, paired
sampling, deadlines, descendant cleanup and interrupt handling. Real-model
inference is an explicit opt-in and must not be a silently skipped CI success.

Next: consented spontaneous human dictation, more accents/languages, several
real microphones/rooms, naturally long recordings, per-language speaker counts,
and physical iPhone energy/thermal/interruption traces. Add explicit speaker,
license/consent, reference, transformations and audio hashes to every new clip.
Never check in private user recordings or provider keys.
