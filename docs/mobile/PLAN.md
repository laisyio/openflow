# OpenFlow for iPhone: fully local dictation

Status: plan v1, 2026-09-02. Author: Titan (with Claude). Base: origin/main 9c8b67e. Price point under design: US$9.99 paid up front, no account, no server.

## 0. The offer, and what iOS lets us build

The pitch: your voice never leaves the phone. Recognition runs on the device with Moonshine (section 3), the app has no backend, and the only network request it ever makes is the one-time model download. The cost is honesty about overhead: the model needs a few hundred MB of memory while loaded and 141 MB on disk (44 MB for the tiny model), and the app says so on the download screen.

iOS changes the shape of the product compared with the desktop app. Three constraints drive everything below; the plan does not try to work around them with tricks that get apps rejected:

1. No system-wide insertion. A third-party app cannot type into another app. The desktop's hotkey-hold-paste loop does not exist. Text reaches other apps through the clipboard, the Share sheet, or our own keyboard extension.
2. Keyboard extensions cannot use the microphone and live under a small memory cap (tens of MB). The model can never run inside the keyboard. The keyboard is only a one-key "insert my last dictation" surface.
3. Background execution does not keep a large model resident for free. A suspended app that holds hundreds of MB is high on jetsam's list, and at 1 GB (the original Qwen plan) it was first. So "runs in the background" means the app loads the model on demand, fast, and drops it when the system asks, without losing the user's text.

The resulting interaction: the user triggers dictation from the Action Button, Back Tap, a Control Center control, a Lock Screen widget, or the app icon. A minimal capture sheet appears with the Dynamic Island showing the recording state. The user speaks, taps stop (or the sheet stops on silence if that setting is on). The text appears, is copied to the clipboard, and can be inserted with one key from the OpenFlow keyboard in whatever app they switch to. That is one trigger, one speak, one paste. It is the closest iOS allows to the desktop loop, and it is faster than Apple's dictation for anyone who dictates long passages, because there is no per-app permission dance and no cloud round trip.

## 1. Layout

```
apps/ios/
  project.yml                               XcodeGen spec; `xcodegen generate` makes OpenFlow.xcodeproj (not committed)
  OpenFlow/                                 app target (SwiftUI, iOS 18+)
    OpenFlowApp.swift, CaptureSheet.swift, HistoryView.swift, SettingsView.swift, ModelDownloadView.swift
    Intents/StartDictationIntent.swift      App Intent: Action Button, Shortcuts, Back Tap
    Info.plist, PrivacyInfo.xcprivacy, OpenFlow.entitlements (App Group only)
  OpenFlowKeyboard/                         keyboard extension: one row, "Insert last dictation", reads the App Group store
  OpenFlowWidgets/                          Live Activity (Dynamic Island pill) + ControlWidget (Control Center / Lock Screen)
  Packages/
    OpenFlowMobileCore/                     Swift package, builds and tests on macOS with the command line tools alone
      Sources/OpenFlowMobileCore/
        SpeechEngine.swift                  protocol: load(), unload(), transcribe(samples16k:) async throws -> Transcript
        ModelManager.swift                  the smart load/unload state machine (section 2)
        ModelStore.swift                    where weights live, checksum verification, isExcludedFromBackup
        ModelDownloader.swift               the only type allowed to touch URLSession; pinned URL + SHA-256
        AudioCapture.swift                  AVAudioEngine tap -> 16 kHz mono Float32, same downsample rule as desktop
        SilenceGate.swift                   port of audio.rs speech_level / is_silent with the same constants and vectors
        DictionaryPostPass.swift            deterministic spelling replace, since the engine ignores prompts
        TranscriptStore.swift               App Group container, last transcript + history, retention window
        ClipboardWriter.swift               UIPasteboard, localOnly = true, expiration 60 s (setting)
        Settings.swift                      keys + defaults (section 4)
      Tests/OpenFlowMobileCoreTests/        FakeEngine, ModelManager transitions, silence gate vectors, post-pass, store
    OpenFlowMoonshineEngine/                SpeechEngine on the official Moonshine Swift package; in the CLT gate through the macOS slice
```

Nothing from the desktop Rust is linked into the phone app. What is shared is the specification: the silence gate constants and test vectors, the dictionary semantics, and the load/unload policy, which the desktop local runner (docs/native-port/PLAN.md section 7) adopts as well.

## 2. Smart load and unload (ModelManager)

States: `unloaded -> loading -> ready -> unloading -> unloaded`, plus `failed(reason)`. One actor, all transitions logged with timestamps for the diagnostics screen.

Load triggers, in priority order:
- Prewarm on capture start. Loading takes 2 to 3 s and the user speaks for longer than that, so the load overlaps with the recording and inference starts the moment they stop. This is the single largest latency win on the phone and it costs nothing.
- Prewarm on the Action Button intent, before the sheet is even on screen.
- Never prewarm on app launch, and never while Low Power Mode is on or the thermal state is `.serious` or worse; in those cases load only when there is audio to transcribe.

Unload triggers:
- Idle timer after the last transcription (default 5 min, setting 1 to 30 min or "keep loaded while app is open").
- `didReceiveMemoryWarning`, immediately.
- Scene moves to background: unload after 20 s unless a transcription is in flight, in which case finish it, deliver the text to the store and clipboard, then unload.
- Thermal state reaches `.serious`.

Weights are memory-mapped safetensors, so a reload after an unload is served from the page cache when the system has not evicted it, which is what makes the aggressive unload cheap in practice. The manager exposes `residentBytes` for the Settings screen so the cost is visible.

## 3. Engine choice

Decided 2026-09-10: Moonshine, base-en by default and tiny-en for older phones, through the official Swift package (`moonshine-swift`, pinned by revision). The full spec, the measured numbers and the pins are in `M2-MOONSHINE.md`. In short: 62M and 27M parameters, 141 MB and 44 MB of weights, 0.44 s and 0.30 s warm on an M4 CPU, and on the reference clip both spelled "entro.ly" the way Qwen 1.7B does and Qwen 0.6B did not.

Qwen3-ASR-0.6B on MLX Swift is no longer the target; it returns later as an "accurate" option behind the same `SpeechEngine` seam if a phone measurement earns it. WhisperKit is dropped. Both stub packages are deleted in M2.

What Moonshine does not do: languages other than English under a commercial licence, and GPU or Neural Engine execution. Neither is part of the offer in section 0.

## 4. Settings (all local, UserDefaults in the App Group)

`engine` (moonshineBase | moonshineTiny, default moonshineBase), `stopOnSilence` (bool, default false), `silenceHoldMs` (default 1200), `dictionary` (800 chars), `clipboardExpirySeconds` (default 60, 0 = never), `saveHistory` (default true), `historyRetentionDays` (default 30), `unloadAfterMinutes` (default 5), `prewarmOnCapture` (default true), `hapticOnStop` (default true), `onboardingComplete`.

No analytics, no crash reporter, no remote config, no third-party SDK. The privacy manifest declares no collected data types. App Transport Security stays default and the only host the app contacts is the model host, listed in the manifest and in the download screen.

## 5. Lightweight budget

- Binary under 15 MB of our own code plus the 34 MB Moonshine static library for the device slice. Weights are downloaded once (141 MB base, 44 MB tiny, three files each), never bundled, stored under Application Support with `isExcludedFromBackup = true`, every file integrity checked with SHA-256 against a pinned value in the app.
- Zero idle work: no timers while the sheet is closed, no background modes except `audio` during a capture. The Live Activity is updated only on state changes.
- Capture pipeline allocates once per take; 16 kHz Float32 in a preallocated ring of 10 minutes maximum (the watchdog from the desktop). No allocation on the audio thread after the first block (section 8).
- Memory: model resident only per section 2. Expect a few hundred MB while loaded, measured as a footprint delta and shown in Settings, not typed into the copy. UI is plain SwiftUI, no image assets beyond the icon and SF Symbols.
- Battery: prewarm rules above. Recognition runs on the CPU, where it was measured; a Core ML execution provider is a later opt-in measurement, never a silent change.

## 6. Milestones and gates

**M1 (this unit):** `OpenFlowMobileCore` complete with tests, the app target, keyboard and widget targets scaffolded with real Swift files and a `project.yml` that generates a building Xcode project, `FakeEngine` wired so the whole app can be exercised in the Simulator before any model exists, ModelDownloader with a pinned URL and checksum placeholder, README under `apps/ios/`. Gate on this machine: `swift build` and `swift test` inside `Packages/OpenFlowMobileCore` (Swift 6 language mode, strict concurrency). Xcode gates run on Titan's machine: `xcodegen generate && xcodebuild -scheme OpenFlow -destination 'generic/platform=iOS Simulator' build`.

**M2:** the Moonshine engine, per `M2-MOONSHINE.md`: `OpenFlowMoonshineEngine` on the pinned Swift package, multi-file pinned downloads for base-en and tiny-en, the engine tested against the reference clip on this Mac, then measured on an iPhone (load time, footprint delta, the four-hour reload).

**M3:** keyboard insert, Live Activity, Control Center control, Action Button intent end to end; onboarding with the download screen; TestFlight.

**M4:** App Store: paid app, privacy manifest review, screenshots, and the same offline guarantee stated on the store page in one sentence.

Every commit: CHANGELOG.md entry under Unreleased (By:, Impact:), attribution trailers, no secrets, no network code outside `ModelDownloader`.

## 7. What the implementer must not do

Do not add a server, an account, analytics, or any SDK. Do not put the model in the keyboard extension. Do not change where inference runs without a measurement in this file. Do not bundle weights. Do not claim background residency the OS does not grant; the state machine in section 2 is the contract.

## 8. Lightweight audit, 2026-09-10

The desktop discipline this is measured against: do only what the caller is about to use, do it in O(n) where a sort would do, allocate outside the audio callback and never inside it, tell the user when a limit changed what they got, and keep a cost the user is quoted in one place next to the thing that sets it.

Already met before this branch:

- Capture allocates its ring once per take and never grows it, per section 5.
- The keyboard extension reads `last.json` alone, so a month of history is never parsed to insert one line.
- The Live Activity is updated on state changes only, with no timer of its own.
- The model unloads on idle, on background, on memory pressure and on thermal pressure, per section 2.
- `OpenFlowMobileCore` has no dependencies, so the whole brain builds and tests under the Command Line Tools.

Fixed on this branch:

- `SilenceGate.speechLevel` selects the 95th percentile in O(n) instead of sorting the block, matching `select_nth_unstable_by` in the desktop's `audio.rs`.
- The per-block level is computed only when stop-on-silence is on, since nothing else reads it, and the whole-take gate at stop is unchanged.
- The ring takes a block in at most two bulk copies rather than one indexed write with a modulo per sample.
- The microphone tap mixes to mono, converts and measures through buffers the capture owns, so the steady state of a take allocates nothing on the audio thread.
- A take that hits the ten-minute ceiling now says so on the capture sheet, and says that the beginning is the half that was dropped, which is the opposite end from the desktop because this is a ring.
- History is read from disk when the History tab appears and not on every take, scene change, memory warning and thermal notification.
- `history.json` has a documented ceiling of 500 records, so the file the append path decodes cannot grow without limit inside the retention window.
- The download size and the memory figure both come from `EngineProfile`, derived from the selected engine's pin, instead of being typed into two screens.
- Release builds compile at `-Osize` and every configuration strips dead code.
