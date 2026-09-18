# Mobile regression gates

The September 2026 performance sweep adds executable checks for the capture and
download lifecycle, in addition to the existing DSP/model tests.

- The sample ceiling is independent of silence detection. Capture keeps the
  opening of the take, accepts no samples after capacity, and emits one stop
  request. The optional 100 ms silence poll is not the watchdog.
- A capture holds a model lease from microphone start through delivery or
  cancellation. Abandoned prewarms get idle cleanup; a previous take's idle
  timer cannot unload the model in the middle of another recording.
- Starting, stopping, transcribing and cancellation are separate busy states.
  A session ID must still match before a result can reach the clipboard. A
  cancelled synchronous Moonshine decode drains before another take is admitted.
- Cancellation and persistence share an atomic delivery authorization. If
  cancellation wins before the repository admits a commit, neither last.json
  nor history is changed. An already admitted commit may finish after closing
  the sheet; cancellation does not delete accepted history. Clipboard delivery
  still requires the live session. No lock is held across encoding or disk I/O.
- Changing Base/Tiny during a take applies after that take settles. The old
  engine is unloaded before the replacement is used.
- A download has an owned cancellable task and per-model admission across
  downloader instances. Attempts use unique staging directories. Resume reuses
  checksum-verified complete files and URLSession resume data when the server
  supplies it; otherwise an unfinished file starts again. Failed installation
  restores the previous recogniser. Reopening checks installed state without
  downloading or hashing the complete model again.
- FIR state compacts once per block. Streaming upsampling keeps interpolation
  state across blocks. Stop computes the percentile once for both gate and gain.
- History disk work runs on a serial actor. Its bounded cache applies the same
  date/count retention as disk; foreground checks do not re-read an unchanged
  history file. Launch, retention changes and delivery update the visible list.

Run the host gate:

```sh
swift test --package-path apps/ios/Packages/OpenFlowMobileCore
swift test --package-path apps/ios/Packages/OpenFlowMoonshineEngine
python3 apps/ios/scripts/test-controller-host.py
```

The controller runner needs macOS and Swift 6 Command Line Tools, not Xcode.
It builds a fresh temporary package from the current controller and app-test
sources, with the actual MobileCore dependency and only Activity/Intent bridge
stubs. `--keep-temp` preserves that generated package for inspection. This gate
tests session/cancellation and engine replacement logic; it cannot validate
UIKit, OS permission/background behavior, or real speech quality.

The engine suite explicitly skips its real-weight tests unless
`OPENFLOW_MOONSHINE_MODEL_DIR` is supplied. Passing without that variable proves
package compatibility, not speech quality or model performance.

With Xcode installed:

```sh
cd apps/ios
xcodegen generate
xcodebuild -project OpenFlow.xcodeproj -scheme OpenFlow -configuration Release \
  -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build
xcodebuild -project OpenFlow.xcodeproj -scheme OpenFlow -configuration Debug \
  -destination 'platform=iOS Simulator,name=iPhone 16' CODE_SIGNING_ALLOWED=NO test
```

The CI workflow chooses an available iPhone Simulator instead of assuming a
particular device name. It compiles Release with the real Moonshine dependency
and runs app-controller tests with injected microphones, engines and clipboards.
Debug normally uses FakeEngine; a Debug build alone is not the release gate.

## Device-only checks

The app requests a finite iOS background execution task before stopping the
microphone and decoding. If iOS expires that allowance, output is invalidated,
the decoder is drained, and the current process retains the stopped audio for
an explicit Retry. This is not durable recovery after iOS terminates the app.
No timer or actor promises unlimited background execution.

Before shipping, test lock/background during decode, calls/audio interruptions,
permission dismissal, thermal pressure and memory warnings on supported phones.
Measure physical footprint during load, long decode, cancellation and unload;
`residentBytes` is the engine's documented load-footprint estimate, not a sampled
inference peak or proof that the allocator returned all pages. Compare cold and
warm runs, battery/thermal state and the exact model revision. Simulator and Mac
unit tests do not substitute for these measurements.
