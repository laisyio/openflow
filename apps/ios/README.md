# OpenFlow for iPhone

A dictation app that runs the recogniser on the phone. No account, no server, no
analytics, no third-party SDK. The only network request it can make is the
one-time model download, and that lives in one file so you can check the claim
rather than believe it.

This directory is Milestones M1 and M2 of `docs/mobile/PLAN.md`: the core package
with its tests, the four targets with real Swift in them, an XcodeGen spec that
generates a project, and the recogniser. The engine is Moonshine, base-en by
default and tiny-en for older phones, on the CPU where it was measured; the
decision and the numbers behind it are in `docs/mobile/M2-MOONSHINE.md`.

## What iOS lets us build, and what it does not

These three constraints shape everything here, and the plan does not try to work
around them with tricks that get apps rejected:

1. **No system-wide insertion.** A third-party app cannot type into another app.
   The desktop's hotkey-hold-paste loop does not exist on the phone. Text reaches
   other apps through the clipboard, the Share sheet, or our own keyboard
   extension.
2. **Keyboard extensions cannot use the microphone**, and live under a memory cap
   measured in tens of megabytes. The model can never run inside the keyboard.
   The keyboard is a one-key "insert my last dictation" surface and nothing more.
3. **Background execution does not keep a large model resident for free.** A
   suspended app holding hundreds of megabytes is high on jetsam's list, and at
   the 1 GB the original Qwen plan needed it was first. So "runs in the
   background" means the app loads the model on demand, fast, and drops it when
   the system asks, without losing the user's text. Moonshine is what makes that
   cheap: the reload it causes costs about half a second, not three.

The resulting interaction: trigger from the Action Button, Back Tap, a Control
Center control, a Lock Screen widget or the app icon; a small capture sheet
appears with the Dynamic Island showing state; speak; tap stop, or let it stop on
silence. The text is copied to the clipboard and can be inserted with one key
from the OpenFlow keyboard in whatever app you switch to. One trigger, one speak,
one paste.

## Layout

```
apps/ios/
  project.yml                 XcodeGen spec -- the four targets, ids, App Group
  OpenFlow/                   the app (SwiftUI, iOS 18+)
  OpenFlowKeyboard/           keyboard extension: one row, reads the App Group
  OpenFlowWidgets/            Live Activity + ControlWidget
  Packages/
    OpenFlowMobileCore/       the brain: state machine, audio maths, stores
    OpenFlowMoonshineEngine/  the recogniser, on the pinned moonshine-swift
```

## Build and run

```bash
brew install xcodegen
cd apps/ios
xcodegen generate
open OpenFlow.xcodeproj
```

`OpenFlow.xcodeproj` is a build artefact and is not committed. `project.yml` is
the file to review and to change.

**The plists and entitlements are hand-written, and `project.yml` must never
grow an `info:` or `entitlements:` block.** Those keys do not point XcodeGen at
an existing file; they tell it to write one, and it rewrites that path from the
spec on every `xcodegen generate`. That would silently erase the keyboard's
`NSExtension` dict and `RequestsOpenAccess`, `NSMicrophoneUsageDescription`,
`UIBackgroundModes`, `CFBundleURLTypes`, `NSSupportsLiveActivities` and the App
Group in all three entitlements files. `INFOPLIST_FILE`,
`CODE_SIGN_ENTITLEMENTS` and `GENERATE_INFOPLIST_FILE: NO` under each target's
`settings.base` are what point the build at those files, and they are enough on
their own. If a generate ever wipes them, that is why.

### Running with no model, in the Simulator

The Debug configuration defines `OPENFLOW_FAKE_ENGINE`, which swaps in
`FakeEngine`: it loads instantly, returns a canned line, and reports a simulated
420 MB resident, which is base-en's order of magnitude rather than a round
gigabyte. Every screen, the keyboard, the Live Activity and the App Intent
can be exercised end to end before any weights exist. Nothing about the fake is
subtle -- the text it returns says it is the fake, so a fake build cannot be
mistaken for a working one.

To build without it, use the Release configuration or remove
`OPENFLOW_FAKE_ENGINE` from `SWIFT_ACTIVE_COMPILATION_CONDITIONS` in
`project.yml`. The app then runs Moonshine for real, and the download screen is
what stands between a fresh install and the first take.

## Tests

Capture/session cancellation, live engine replacement, download cancellation and
resume, retention, and the Simulator/Release gates are documented in
[PERFORMANCE.md](PERFORMANCE.md). The iOS CI workflow runs core/engine tests and
builds the real-engine Release path as well as the app controller tests.

Both packages build and test with the Command Line Tools alone -- no Xcode, no
simulator, no Metal toolchain:

```bash
cd apps/ios/Packages/OpenFlowMobileCore && swift build && swift test

cd apps/ios/Packages/OpenFlowMoonshineEngine && swift build
OPENFLOW_MOONSHINE_MODEL_DIR="$HOME/Library/Caches/moonshine_voice/download.moonshine.ai/model/base-en/quantized/base-en" \
  swift test
```

The engine package is in that gate because Moonshine ships a prebuilt C++ static
library whose xcframework carries a `macos-arm64_x86_64` slice, so the real
recogniser runs here against the real weights. Its four model-backed tests skip,
loudly, when `OPENFLOW_MOONSHINE_MODEL_DIR` is unset; see
`Packages/OpenFlowMoonshineEngine/README.md`.

That is the gate this milestone was held to. It covers the model manager's
transitions for every trigger in PLAN.md section 2 (driven on a hand-cranked
clock, so a five-minute idle timer costs a microsecond), the silence gate and
resampler against the same vectors as the desktop's Rust tests, the dictionary
post-pass, the transcript store's retention window, the settings defaults and the
model store's checksum verification.

The Xcode gates run on a machine that has Xcode:

```bash
cd apps/ios && xcodegen generate
xcodebuild -scheme OpenFlow -destination 'generic/platform=iOS Simulator' build
```

## The privacy guarantee, and how to check it

The claim: your voice never leaves the phone, and the app talks to exactly one
host, once, to download the recogniser.

How to verify it without trusting the claim:

```bash
# Three files may appear, and nothing else: ModelDownloader.swift, its
# test, and OpenFlowMoonshineEngine/Package.swift, whose https is the
# pinned source of the dependency rather than a host the app calls.
grep -rn "URLSession\|https://" apps/ios --include='*.swift' \
  --exclude-dir=.build
```

`ModelDownloader` is the only type in our code allowed to touch the network. Its
URLs and its six SHA-256 digests are compile-time constants -- there is no
manifest fetch, no redirect chasing, no remote config -- so the host the app can
reach is fixed at build time and visible in the source. A set of files whose
digests do not all match is deleted, not used.

The `--exclude-dir=.build` is not a way of hiding something, and here is what it
hides. `MoonshineVoice` ships an `AssetDownloader` and a `TextToSpeech` that do
use `URLSession`, and both are checked out under `.build` when SwiftPM resolves
the dependency. Neither is called from anything here, on purpose: the app keeps
its own `ModelDownloader` as the only network code in the product, and dead code
stripping is on for every configuration, so nothing that reaches them is linked
into the binary. The check that matters is the one below, on the built app:

```bash
# The hosts a built binary carries. download.moonshine.ai and nothing else.
strings "$APP/OpenFlow" | grep -Eo 'https?://[a-z0-9.-]+' | sort -u
```

Two more things a reviewer can check:

- `OpenFlow/PrivacyInfo.xcprivacy` declares no collected data types, no tracking
  and no tracking domains, and lists only the required-reason APIs the code
  actually calls.
- `OpenFlow/OpenFlow.entitlements` carries the App Group and nothing else: no
  iCloud, no push, no associated domains.

## The keyboard's Allow Full Access

The keyboard extension's `Info.plist` sets `RequestsOpenAccess` to true, and it
is worth being blunt about why. iOS sandboxes a keyboard extension away from the
App Group container unless the user grants Allow Full Access. Without it the
keyboard cannot read the last transcript and has nothing to insert.

That is the only thing it buys OpenFlow. The keyboard target contains no
networking code at all -- there is nothing in it that could send a keystroke
anywhere -- it keeps no record of what you type, and it reads exactly one small
JSON file that this app wrote. If you would rather not grant it, the app still
works: the transcript is on the clipboard, and paste does the same job in one
more tap.

## Where the desktop and the phone agree

Nothing from the desktop Rust is linked into the phone app. What is shared is the
specification, copied deliberately so the two can be cross-checked:

- `SilenceGate` and `AudioResampler` port `speech_level`, `is_silent`,
  `auto_gain` and the FIR `downsample` from `src-tauri/src/audio.rs`, with the
  same constants and the same test vectors. If a test passes there and fails
  here, the implementations have drifted.
- `DictionaryPostPass.capped` reproduces `dictionary_prompt` from
  `src-tauri/src/transcribe.rs`, including the 800-scalar cap, so the same
  dictionary string means the same thing on both platforms.
- The load/unload policy in `ModelManager` is the contract the desktop's local
  runner adopts as well.

## Status

Milestone M2. `MoonshineSpeechEngine` runs base-en and tiny-en, and the pins in
`ModelDownloader` are the six measured digests rather than placeholders. On this
Mac base-en transcribes the 8.5 s reference clip in 0.48 s after a 0.64 s load,
for a 317 MB footprint delta.

What is left of M2 is the phone: load time, the footprint delta shown in
Settings, and a dictation after four hours locked, all on real hardware. The
resident figure the download screen quotes is still an estimate until that
happens. Qwen3-ASR returns later as an "accurate" option behind the same
`SpeechEngine` seam if a phone measurement earns it; WhisperKit is dropped.
