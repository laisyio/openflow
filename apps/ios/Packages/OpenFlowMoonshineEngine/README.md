# OpenFlowMoonshineEngine

`SpeechEngine` on Moonshine, base-en by default and tiny-en for older phones.
The spec is `docs/mobile/M2-MOONSHINE.md`; this file is how to run it.

## Why this one is not a stub

The two engine packages this replaces were stubs because MLX Swift and WhisperKit
both need the Metal toolchain or the CoreML compiler, and both ship with Xcode
rather than the Command Line Tools. Moonshine ships a prebuilt C++ static library
in an xcframework, and v0.1.5 of that xcframework carries a `macos-arm64_x86_64`
slice next to the `ios-arm64` one that ships. So the real recogniser builds, links
and runs under `swift test` on a machine with no Xcode at all, against the real
weights. The package README upstream still says "iOS only"; it is older than the
release.

## The dependency

Pinned by exact revision, never `from:` or `branch:`:

```swift
.package(
    url: "https://github.com/moonshine-ai/moonshine-swift.git",
    revision: "45a14f9edf1f2a6913d3aff38c1fd4e72d5b7daa"   // v0.1.5, 2026-08-24
)
```

The revision pin matters more here than usual. The package's `binaryTarget`
fetches `Moonshine.xcframework.zip` from a GitHub release by URL, so a floating
pin would let the artefact behind that URL change between two builds of the same
commit of this repo.

Product `MoonshineVoice` links `c++` and is attached to the OpenFlow app target
only. Never to the keyboard extension, which has a memory cap in the tens of
megabytes and cannot run a model, and never to the widgets.

Used from it: `Transcriber`, `ModelArch`, `Transcript`. Deliberately not used:
`AssetDownloader`, `ModelCache`, `MicTranscriber`, `AgentFlow`, `TextToSpeech`,
`VoiceClone`. The app keeps its own `ModelDownloader` as the only network code in
the product, and its own `AudioCapture`.

## Running the tests

Ten tests. Six need no weights. Four load the real model and skip, loudly, when
`OPENFLOW_MOONSHINE_MODEL_DIR` is unset:

```bash
swift test    # six run, four print SKIPPED and say what to set

OPENFLOW_MOONSHINE_MODEL_DIR="$HOME/Library/Caches/moonshine_voice/download.moonshine.ai/model/base-en/quantized/base-en" \
  swift test  # all ten run
```

The directory must hold `encoder_model.ort`, `decoder_model_merged.ort` and
`tokenizer.bin`. Any base-en directory will do; the one above is where the
desktop benchmark's Python client caches them.

The real-model suite is `.serialized`, which is not tidiness: those tests measure
the process footprint, and four of them loading 141 MB into the same process at
once would make every delta a reading of the other three.

The fixture is `Tests/OpenFlowMoonshineEngineTests/Resources/take-say.wav`: 8.5 s,
16 kHz mono Int16, 270 KB, the reference sentence from `M2-MOONSHINE.md` spoken by
the macOS Samantha voice. The assertions are on "leaderboard", "ledger" and
"Thursday", case-insensitively, and never on the brand spelling. base-en writes
"enterol I" for "entro.ly" and "fast pay" for "FastPay"; `DictionaryPostPass` is
what fixes those, and asserting on them here would be testing a correction this
engine does not make.

## What `residentBytes` is

`task_vm_info.phys_footprint` after `load()` minus the same reading before it,
floored at the size of the weights on disk. It is the number iOS itself uses to
decide who jetsam kills, and the one Xcode's memory gauge draws.

It is a delta, with the limits a delta has: it attributes to the model every page
the process gained while the model was loading, and misses anything the allocator
had already reserved. The floor is there because a delta can come out absurdly
low, and "the model costs 4 MB" is a worse answer than "at least the 141 MB it
occupies on disk".

On this Mac, base-en measures a 317 MB delta with a 0.64 s load and 0.48 s of
warm inference on the reference clip. The phone figure replaces it at the M2 gate.

## Key terms

`setKeyterms` is called from the dictionary before a take when the dictionary is
not empty, and its failure is swallowed on purpose. Moonshine applies key terms
only on its streaming architectures, so on base-en and tiny-en the call is
refused and the terms have no effect. `DictionaryPostPass` runs over the finished
transcript either way and is what actually guarantees the spelling, the same
belt-and-braces the desktop uses. The call stays because it costs nothing and
starts working by itself the day a streaming architecture earns its place.
