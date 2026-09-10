# Milestone M2: the Moonshine engine

Decided by Titan on 2026-09-10 after the desktop benchmark (`docs/native-port/local-runner-benchmark.md`, addendum) and the lightweight audit (`PLAN.md` section 8): the phone's engine is Moonshine, base-en by default and tiny-en for older phones. Qwen3-ASR stays a later "accurate" option; WhisperKit is dropped from the plan. This file is the spec an implementer builds from; `PLAN.md` sections 3, 5 and 7 are rewritten to match.

## Why Moonshine

| | Qwen3-ASR-0.6B on MLX (old plan) | Moonshine base-en | Moonshine tiny-en |
|---|---|---|---|
| Parameters | 600M | 62M | 27M |
| Weights to download | about 700 MB | 141 MB | 44 MB |
| Resident on an M4 (process RSS, one transcription) | about 1 GB active | 562 MB | 246 MB |
| Warm inference, 8.7 s clip, M4 | 0.40 s (GPU) | 0.44 s (CPU) | 0.30 s (CPU) |
| Load from disk | 3 s | 0.19 s | 0.12 s |
| "entro.ly", "FastPay" on the reference clip | "intro dot lie", "fast pay" | "intro.ly", "fast pay" | "intro.ly", "fast pay" |
| Toolchain | MLX Swift, Metal, Xcode to build | prebuilt C++ static library, official Swift package | same |
| Licence | Apache-2.0 | MIT (English models) | MIT (English models) |

The plan's load-and-unload machine (`PLAN.md` section 2) exists because a suspended app holding 1 GB is the first thing jetsam kills. At 250 to 560 MB the app is one of the crowd. Every unload rule stays, but the reload it causes now costs a fifth of a second, not three.

The old rule "never CPU inference, fail loudly" (`PLAN.md` sections 5 and 7) was written to stop a GPU engine degrading silently. Moonshine is CPU-native by design, so the rule becomes: the engine runs where it was measured to run, and a Core ML execution provider is a later, opt-in measurement, never a silent change.

## The package

- Dependency: `https://github.com/moonshine-ai/moonshine-swift`, pinned by exact revision `45a14f9edf1f2a6913d3aff38c1fd4e72d5b7daa` (tag v0.1.5, 2026-08-24). Never `from:` or `branch:`; the desktop's pin rule (`crates/openflow-core/src/runner.rs` header) applies.
- The package is a `binaryTarget` (`Moonshine.xcframework.zip`, sha256 `6bc7fb4b6d3a470a2ae2d681299975f3ba9d710753786d1cd7e8beabaad066e8`) plus a Swift wrapper `MoonshineVoice`. Slices in v0.1.5: `ios-arm64` (34 MB static library, the one that ships), `ios-arm64_x86_64-simulator`, and `macos-arm64_x86_64` (112 MB). The macOS slice is what lets this Mac build and test the engine with the Command Line Tools alone; the package README that says "iOS only" is older than the release.
- Product `MoonshineVoice` is a static library that links `c++`. It is attached to the OpenFlow app target only. Never to the keyboard extension (60 MB memory cap) or the widgets.
- API used, verbatim from `Sources/MoonshineVoice`:
  - `Transcriber(modelPath: String, modelArch: ModelArch = .base, options: [TranscriberOption]? = nil, spellingModelPath: String? = nil) throws`
  - `transcribeWithoutStreaming(audioData: [Float], sampleRate: Int32 = 16000, flags: UInt32 = 0) throws -> Transcript`, where `Transcript.lines[].text` carries the words
  - `setKeyterms(_ keyterms: [String]) throws`, the phone-side equivalent of the desktop's dictionary prompt
  - `close()`
  - `ModelArch.tiny` (0) and `.base` (1). The streaming architectures are not used: on the reference clip all three misheard the trailing date and were slower per offline take.
- Not used, on purpose: `AssetDownloader`, `ModelCache`, `MicTranscriber`, `AgentFlow`, `TextToSpeech`, `VoiceClone`. The app keeps its own `ModelDownloader` as the only network code (`PLAN.md` section 0), and its own `AudioCapture`.

## Weights and pins

Three files per model, served by Moonshine's CDN at stable paths. Sizes and digests measured 2026-09-10 from the files the desktop benchmark downloaded; the CDN answers HEAD with the same content lengths.

base-en, directory `moonshine/base-en`:

| File | Bytes | SHA-256 |
|---|---|---|
| `encoder_model.ort` | 31326816 | `7c66495948d0d08ec1af454cd4b5514862ae6511e94712a60e6d83eaec8dc8cf` |
| `decoder_model_merged.ort` | 109424400 | `d9d7b333af34bc552580576ddcf248a1c6c839e0d3b43b09afb9376ed009899d` |
| `tokenizer.bin` | 249974 | `6884b35fd6377d4c4d32336a0bc152f36b64d1e45b6503683cdc238250a8472d` |

URL prefix: `https://download.moonshine.ai/model/base-en/quantized/base-en/`

tiny-en, directory `moonshine/tiny-en`:

| File | Bytes | SHA-256 |
|---|---|---|
| `encoder_model.ort` | 13281600 | `94e90a4654fc45cdfedb77c4c08e1739f48862998e58fada384b25118134f221` |
| `decoder_model_merged.ort` | 30412256 | `cf524c4862d36e9e5ab032eddc73637efd822d70e868ac575cf1a46e1e4708a0` |
| `tokenizer.bin` | 249974 | `6884b35fd6377d4c4d32336a0bc152f36b64d1e45b6503683cdc238250a8472d` |

URL prefix: `https://download.moonshine.ai/model/tiny-en/quantized/tiny-en/`

`ModelDownloader.Pin` becomes a set of files per engine. A model is installed only when every file is present and verified; a partial set is removed. Progress is reported across the set (bytes of all files), so the download screen's bar rises once.

## Code changes

1. `apps/ios/Packages/OpenFlowMoonshineEngine` (new): `MoonshineSpeechEngine: SpeechEngine`, an actor. `identifier` is `moonshine-base-en` or `moonshine-tiny-en`. `load()` constructs `Transcriber` from the model directory and is idempotent. `unload()` calls `close()` and drops it; because `transcribeWithoutStreaming` never suspends, actor serialisation makes an unload during a take safe, exactly the case the `SpeechEngine` doc comment describes, and the comment is updated to say this engine is the one-unbroken-stretch kind. `transcribe(samples16k:)` joins `lines[].text` with a space, trims, throws `noSpeechRecognised` on empty, and reports `latencySeconds`. `setKeyterms` is called with the dictionary's entries before each take when the dictionary is non-empty; the deterministic `DictionaryPostPass` still runs after, the same belt-and-braces the desktop uses.
   - `residentBytes`: measured, not guessed. Read the task's physical footprint (`task_vm_info.phys_footprint`) before and after `load()` and report the delta, floored at the weights' size on disk. Document that it is a footprint delta.
   - Delete `OpenFlowQwenEngine` and `OpenFlowWhisperEngine` from the tree and from `project.yml`. The Qwen option returns when it is built, against this seam; carrying two stubs that need Xcode helps nobody.
   - The engine package is in the Command Line Tools gate: `swift build` and `swift test` inside it on this Mac, using the macOS slice. Its test loads the real base-en files from a directory named by `OPENFLOW_MOONSHINE_MODEL_DIR` (skip with a message when unset) and transcribes the fixture clip `take-say.wav` (8.5 s, 16 kHz mono Int16, 270 KB, the reference sentence spoken by the macOS Samantha voice, committed under the package's test resources), asserting the transcript contains "leaderboard", "ledger" and "Thursday" and that latency is under 2 s. On this Mac the model files are at `~/Library/Caches/moonshine_voice/download.moonshine.ai/model/base-en/quantized/base-en/`.
2. `OpenFlowMobileCore`:
   - `EngineChoice`: `moonshineBase` (default) and `moonshineTiny`. Remove `qwen06` and `whisper`; `SettingsStore.Defaults.engine = .moonshineBase`. Reading a stored value that no longer parses falls back to the default; test it.
   - `ModelDownloader`: multi-file pins per the tables above, `pin(for:)` returns the set; `placeholderDigest` machinery stays for any future pin. `ModelStore` gains a per-engine subdirectory.
   - `EngineProfile`: download bytes are the sum of the set; resident estimate is download bytes times three (base about 420 MB, tiny about 130 MB), labelled an estimate until the M2 gate on a phone replaces it with a measured number. The copy stays "about N MB".
   - `SpeechEngineError.acceleratorUnavailable` is kept but documented as unused by this engine.
   - `FakeEngine` reports a simulated 420 MB, not 1 GB.
3. App target (not compilable here; keep the diff small): `OpenFlowApp.swift` constructs `MoonshineSpeechEngine(choice:store:)` where `UnavailableEngine` is today; `SettingsView` engine picker shows the two Moonshine choices; `ModelDownloadView` copy says what is downloaded ("three files, about 141 MB"). `project.yml`: add the package under `packages:` with `url:` and `revision:`, attach `MoonshineVoice` to the OpenFlow target only. No `info:` or `entitlements:` keys.
4. Docs: `PLAN.md` sections 3, 5, 7 rewritten (done in this commit); `apps/ios/README.md` engine paragraphs, the "1 GB" sentence, the package list and the M2 milestone text; `CHANGELOG.md` entry.

## Gates

- On this Mac: `swift build` and `swift test` in both packages, all green, with the real-model test running (not skipped) and its transcript printed in the report. Mutation checks on the pin verification (flip one digest, see the install refuse) and on the empty-transcript rule.
- On Titan's Mac (Xcode): `xcodegen generate && xcodebuild -scheme OpenFlow -destination 'generic/platform=iOS Simulator' build`, then on a phone: first dictation after install, measured load time and the footprint delta shown in Settings, and a dictation after four hours locked (the idle rule fires, reload is under half a second).

## Not in M2

The Core ML execution provider, streaming partials in the capture sheet, the Qwen accurate option, and any non-English model (those are under the non-commercial Moonshine Community License, which a paid app cannot use).
