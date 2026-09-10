# Local runner benchmark: Qwen3-ASR on Apple silicon (MLX)

Measured 2026-09-02 on a MacBook Air M4, 16 GB, macOS 26.5, mlx 0.32.2, mlx-audio 0.5.1, Python 3.12. Clip: `take.wav`, 8.7 s of speech, the same clip used for the Groq and LAN timings in PR #7. Method: `bench_qwen.py` (load, one cold run, median of five warm runs, `mx.get_peak_memory`), then `bench_load.py` in a fresh process with the weights already on disk.

## Results

| | Qwen3-ASR-0.6B-8bit | Qwen3-ASR-1.7B-8bit | whisper.cpp large-v3-turbo q5_0 (Metal) | LAN box 1.7B-8bit (M-series, over Wi-Fi) | Groq whisper-large-v3-turbo + dictionary |
|---|---|---|---|---|---|
| Warm inference, 8.7 s clip | 0.40 s (min 0.395) | 0.98 s (min 0.954) | 1.8 s wall (encode 1.22 s, load 0.16 s) | 1.1 s | 1.7 s |
| First inference after load (Metal compile) | 0.68 s | 1.23 s | 2.15 s | n/a | n/a |
| Load weights from disk, fresh process | 2.98 s | 2.54 s | 0.16 s (mmap) | n/a | n/a |
| Active memory while resident | 1.03 GB | 2.49 GB | 870 MB RSS | remote | remote |
| Process max RSS | 518 MB | 1.37 GB | 870 MB | remote | remote |
| Weights on disk | 969 MB | 699 MB (plus tokenizer, total under 2 GB) | 574 MB | | |
| "the entro.ly leaderboard" | "the intro dot lie leaderboard" | "the intro.ly leaderboard" | "the intro.ly leaderboard" (prompt not honored for this term) | "the intro.ly leaderboard" | "the Entro.LY leaderboard" |
| "fast pay ledger" | "fast pay ledger" | "fast pay ledger" | "FastPay ledger" (prompt honored) | "fast pay ledger" | "FastPay ledger" |
| Filler words | keeps "Um", "Ah" | keeps "ah", drops "Um" | keeps both, lowercases everything | same as local 1.7B | drops both |

## What this means for the runner

- Speed: 0.6B is 2.4 times faster than 1.7B on the same machine and 4 times faster than Groq end to end. For a 10 s dictation the user-visible wait drops from about 1.7 s to about 0.4 s, and there is no network variance.
- Accuracy: 0.6B loses proper nouns that 1.7B keeps. Neither Qwen size honors the Whisper `prompt`, so the Dictionary setting does nothing on the local runner; a post-pass (the existing cleanup step, or a deterministic dictionary replace) has to carry the spellings. 1.7B is the safer default for a user who dictates product names; 0.6B is the right choice when speed matters more than names, and the cleanup step is on.
- Memory: a resident 0.6B sidecar holds about 1 GB of unified memory, 1.7B about 2.5 GB. That is not "nothing", so the runner must unload after an idle window (the reload penalty is about 3.7 s for 0.6B, 3.8 s for 1.7B, paid on the first dictation after the window) and the Settings UI must show the resident cost.
- Runtime: MLX needs Python and about 600 MB of packages. Bundling that into OpenFlow.app roughly triples the download. whisper.cpp (large-v3-turbo, q5_0, Metal) needs no runtime and honors the prompt for plain words (FastPay), but it is 4.5 times slower than Qwen 0.6B and no faster than Groq: whisper's encoder always processes a 30 s window, so short dictations pay a fixed 1.2 s encode. It loads in 0.16 s because the weights are memory-mapped, which is the reload behavior the runner wants regardless of engine.

## Recommendation

Build the runner abstraction engine-agnostic (PLAN.md section 7) and ship the MLX Qwen sidecar as the engine: 1.7B as the default model (keeps proper nouns), 0.6B offered as "fast" in Settings, the dictionary applied as a deterministic post-pass since neither Qwen size honors a prompt, prewarm the model when recording starts so the 3 s load hides behind the user speaking, and unload after 10 minutes idle. whisper.cpp is ruled out as the primary engine on speed: it matches the cloud round trip it was meant to beat. It stays a candidate for a no-Python build later, at the cost of the 4x speed gain.

## Addendum 2026-09-03: Moonshine and Cohere Transcribe

Same machine, same `take.wav` clip, same method (one cold run, median of five warm runs). Reference transcript: "Um, so the entro.ly leaderboard, ah, needs the FastPay ledger fixed. Scratch that, needs the FastPay ledger checked before Thursday."

### Moonshine (moonshine-voice 0.1.5, ONNX Runtime 1.23.2 on CPU, MIT for the English models)

Measured with `bench_moonshine.py` (`Transcriber.transcribe_without_streaming`) and `mem_moonshine.py` (one model per fresh process, `ru_maxrss`).

| Model | Params | Warm inference | Cold (first call) | Load from disk | Process max RSS | Weights on disk | "entro.ly" | "FastPay" | "before Thursday" | Fillers |
|---|---|---|---|---|---|---|---|---|---|---|
| tiny-en | 27M | 0.30 s (min 0.288) | 0.26 s | 0.12 s | 246 MB | 42 MB | "intro.ly" | "fast pay" | correct | keeps "Um", "uh" |
| base-en | 62M | 0.44 s (min 0.430) | 0.40 s | 0.19 s | 562 MB | 142 MB | "intro.ly" | "fast pay" | correct | keeps "Um", "ah" |
| tiny-streaming-en (2026) | | 0.38 s | 0.33 s | 0.03 s | 324 MB | 47 MB | "intro.ly" | "fast pay" | wrong ("4th of April") | garbles "Um" as "On" |
| small-streaming-en (2026) | | 0.89 s | 0.79 s | 0.06 s | 722 MB | 139 MB | "intro.ly" | "fast pay" | wrong ("30 days") | keeps both |
| medium-streaming-en (2026, catalog default) | 245M | 1.28 s | 1.22 s | 0.13 s | 1151 MB | 269 MB | "intro.ly" | "fast pay" | wrong ("30") | keeps both |

Notes.

- Moonshine base-en matches Qwen 0.6B on speed (0.44 s against 0.40 s) on the CPU alone, with no Metal, about half the resident memory (562 MB RSS against 1.03 GB active) and a 142 MB download against 969 MB. On this clip it is also more accurate than Qwen 0.6B: "intro.ly" instead of "intro dot lie", and the same "fast pay" that every local engine produces. The dictionary post-pass fixes both spellings.
- tiny-en at 27M parameters is the fastest engine measured, 0.30 s, and got every content word of the clip right. It keeps filler words; the cleanup step or a filler post-pass has to drop them.
- The 2026 streaming line is slower per clip in this offline call and all three sizes got the trailing date wrong. They are built for incremental captions, not for a hold-to-talk take. Do not pick them for the runner on the basis of the vendor WER tables.
- Runtime shape: the package is a 9.7 MB `libmoonshine.dylib` plus a 26 MB ONNX Runtime, with a plain C API (`moonshine_load_transcriber_from_files`, `moonshine_transcribe_without_streaming`). The Python layer is ctypes glue. That means a Moonshine engine can be linked into the Rust binary directly, with no Python sidecar and no 600 MB of MLX packages. The CoreML execution provider is not compiled into this build; everything ran on CPU.
- Load is near instant (0.03 to 0.19 s), so prewarm and idle unload matter much less than they do for Qwen (3 s reload).
- Licensing: the English models are MIT; the non-English models are under the non-commercial Moonshine Community License, which the downloader warns about. English only is fine for OpenFlow; a multilingual local mode would need Qwen or Cohere.
- Memory caveat: `ru_maxrss` after one transcription; the first-loaded model in a process also pays the ONNX Runtime and Python overhead (about 28 MB before load).

### Cohere Transcribe 03-2026 (2B, Apache-2.0): measured 2026-09-10

Through the canonical `CohereLabs/cohere-transcribe-03-2026` weights (gated on Hugging Face; Titan accepted the terms), mlx-audio 0.5.3, same M4. The community MLX checkpoints tried on 2026-09-03 were laid out for other runtimes and loaded with a third of the parameters missing; that failure is recorded in the CHANGELOG of PR #27 and is why the canonical weights were needed.

Clip: `take-say.wav`, the reference sentence spoken by the macOS Samantha voice (the original human clip was lost to a temp-directory purge), 8.5 s, 16 kHz. Qwen numbers below are re-measured on the same clip in the same run, so the comparison is like for like; Moonshine base-en on this clip is 0.43 s through the Python package.

| | Cohere Transcribe bf16 (as published) | Cohere Transcribe 8-bit (`mlx.nn.quantize`, group 64, in memory) | Qwen3-ASR-0.6B-8bit | Qwen3-ASR-1.7B-8bit |
|---|---|---|---|---|
| Warm inference, 8.5 s clip | 0.48 s (min 0.475) | 0.46 s (min 0.426) | 0.46 s (min 0.444) | 1.10 s (min 1.054) |
| First inference after load | 12.7 s (Metal compile of 48 encoder layers) | 1.31 s (compile already done in that process) | 0.96 s | 1.14 s |
| Load, fresh process, files cached | 3.8 s | not measured (needs a saved 8-bit checkpoint) | 2.3 s | 2.4 s |
| Active memory while resident | 4.18 GB | 2.48 GB | 1.7 GB peak in this run | 3.3 GB peak in this run |
| Weights on disk | 4.13 GB bf16 | about 2.1 GB once saved | 969 MB | 699 MB |
| Transcript | "the Entro Lie leaderboard, ah, needs the FastPay Ledger fixed. Scratch that, needs the FastPay Ledger checked before Thursday." | identical to bf16 | "the entro lie leaderboard. Ah, needs the fast pay ledger fixed." | "the entroli leaderboard ah needs the fast pay ledger fixed." |

What that says.

- Cohere at 8-bit costs what Qwen 1.7B costs (2.5 GB resident) and runs at Qwen 0.6B speed, 2.4 times faster than the 1.7B it would replace in the accurate tier.
- It is the only local engine that produced "FastPay" with its casing, and the only one that heard "entro" rather than "intro". Its punctuation and capitalisation are the closest to the Groq output. The dictionary post-pass still has to turn "Entro Lie" into "entro.ly".
- The first-inference Metal compile is 12.7 s in a fresh process. The runner's prewarm-on-record hides a 3 s load; it does not hide 12.7 s. Shipping Cohere means either a warm-up inference right after load (the runner does one at prewarm time, so the user pays it once per load, while speaking) or accepting a slow first take. Measure the compile once a saved 8-bit checkpoint exists; it may be shorter with quantized layers.
- The published weights are bf16 and the repo is gated. For the runner to download them, the user would need a Hugging Face login that has accepted Cohere's terms, which is not a dictation-app onboarding step. The licence is Apache-2.0, so the fix is to publish an 8-bit conversion in mlx-audio's own layout under an ungated repo of ours and pin it, the way the Qwen repos are pinned today. That conversion is the first task of any Cohere tier, and this measurement is the reason to do it.

## Recommendation, revised (2026-09-10)

- Light tier: Moonshine base-en (0.44 s, 562 MB RSS, 142 MB download, MIT), tiny-en as the floor. Because Moonshine is a C library, this tier can drop the Python sidecar by linking `libmoonshine` from `openflow-core`.
- Accurate tier: Cohere Transcribe at 8-bit replaces Qwen 1.7B: same resident cost, 2.4 times faster, better proper nouns and casing. Blocked on publishing our own ungated 8-bit conversion and on measuring its first-inference compile; until then Qwen 1.7B stays.
- Qwen 0.6B drops out: Moonshine base matches its speed at half the memory and beats it on names.
- For the iPhone app the decision was taken on 2026-09-10: Moonshine (`docs/mobile/M2-MOONSHINE.md`).
