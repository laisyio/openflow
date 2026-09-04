# OpenFlow

OpenFlow is an open-source desktop dictation app built with Tauri, React, and Rust. Hold a configurable global shortcut, speak, and OpenFlow sends the completed recording to your chosen speech-to-text provider. It can optionally clean up the transcript, copy it to the clipboard, and ask the operating system to paste it into the focused app.

The project is currently an early source build. There are no official pre-built or notarized releases yet.

## What works

- Guided onboarding for Groq (recommended), OpenAI, OpenRouter, Deepgram, and custom OpenAI-compatible endpoints
- A personal dictionary of names and terms, sent to Whisper as a spelling hint on Groq, OpenAI, and custom endpoints
- Configurable transcription and cleanup models with provider model discovery
- Global hold-to-record shortcut (`Option+V` by default) and re-copy shortcut (`Ctrl+Shift+V` by default)
- Searchable local transcription history
- Clipboard preservation: dictating does not cost you the text or image you had copied (on by default)
- Microphone selection, language hints, light/dark themes, a system tray menu, and a movable status overlay
- Optional LLM cleanup for punctuation, paragraphs, and spoken editing commands
- OpenRouter Gemini 3.1 Flash TTS Preview with selectable voices and cancellable response streaming
- Self-hosted speech-to-text and speech synthesis on your own network, with no API key required
- On-device transcription on Apple silicon (native macOS build), faster than the cloud round trip and with a Local only switch that refuses anything leaving the machine
- Local executable hooks after transcription and formatting

### Streaming scope

Speech-to-text is not live streaming: every request carries a complete WAV and waits for a transcript back, and the transcript you keep is always the one returned for the finished recording.

Live preview is the exception to "nothing is uploaded until you let go". With it on, OpenFlow also re-uploads the whole recording so far every 0.8 seconds while you are still speaking, so the pill can show words as they arrive. That previewing stops 20 seconds into a take, or at the first reading that takes longer than 0.8 seconds, whichever comes first: a 60-second dictation sends up to 24 preview requests carrying about 4 minutes of audio in total, and then the final upload of the 60-second WAV. It is off by default for the hosted providers and **on by default for custom (self-hosted) endpoints**, where the requests cost nothing but your own hardware. Settings > General > "Live preview while recording" turns it off or on for any provider; turn it off before pointing a custom endpoint at a paid, per-minute API. On-device transcription previews the same way, against the local model, and nothing leaves the machine.

The Gemini TTS preview progressively appends ordered MP3 chunks and starts playback while the response is still downloading when the system webview supports Media Source Extensions; otherwise it falls back to playback after download. Groq's Orpheus returns WAV only, which cannot be streamed through Media Source Extensions, so it always plays after the download completes.

## Providers

| Provider | Transcription | Text cleanup | TTS preview |
| --- | --- | --- | --- |
| Groq | Whisper models | gpt-oss and Qwen models | Orpheus, after a one-time terms acceptance in the Groq console |
| OpenRouter | Whisper models | Chat models | Gemini 3.1 Flash TTS Preview |
| OpenAI | Whisper-compatible endpoint | Chat models | OpenAI-compatible speech endpoint, when OpenAI is also the transcription provider |
| Deepgram | Nova models | Use a separate cleanup provider or disable cleanup | No |
| Custom | OpenAI-compatible audio/transcriptions endpoint | OpenAI-compatible chat/completions endpoint | Self-hosted OpenAI-compatible audio/speech endpoint |

Model availability and billing are controlled by the provider. OpenFlow does not proxy requests or include hosted inference.

## Privacy and permissions

- API credentials are stored using macOS Keychain, Windows DPAPI, or Linux Secret Service. Linux requires an unlocked keyring and the `secret-tool` command.
- Recordings are held in memory for transcription and sent to the provider selected in Settings. Cleanup sends transcript text to the selected cleanup provider. Voice previews send their text to the selected speech endpoint. Your transcription key is only ever reused by that same service; a different hosted provider or a self-hosted server never receives it.
- Transcript history is stored locally in an unencrypted SQLite database in the operating system's application-data directory. You control it from Settings: delete individual entries, clear everything, turn saving off entirely, or set an auto-delete window (1/7/30/90 days) that is applied at launch and after each transcription.
- Auto-paste requires operating-system automation/accessibility permission. If permission is denied or a paste helper is unavailable, the transcript should still be available in OpenFlow and on the clipboard.
- Enabled plugins are local executables and are not sandboxed. They receive transcript data over standard input. Only install and enable plugins you trust.

## Local transcription (private)

The native macOS build can transcribe on the Mac itself, with no provider, no
key and no network. Settings -> Providers -> **Transcription runs: On this Mac**,
or pick the "On this Mac (private)" card during setup.

It runs [Qwen3-ASR](https://huggingface.co/mlx-community) through
[`mlx-audio`](https://github.com/Blaizzy/mlx-audio) as a supervised sidecar on
`127.0.0.1`, and it is faster than the cloud round trip it replaces: 0.40 s for
an 8.7 s dictation against 1.7 s through Groq, with no network variance.

**It needs a Python 3.10 or newer that you install yourself.** OpenFlow does not
bundle one -- MLX plus its packages is about 600 MB, and tripling the download
for everyone to serve the people who want this is the wrong trade. Homebrew
(`brew install python@3.12`) or a python.org installer both work; the 3.9 that
ships with macOS does not. Settings says so plainly when it cannot find one.

Two one-time steps, both in that panel, both resumable:

| Step | What it does | Size |
|------|--------------|------|
| **Install** | Creates a virtualenv beside the database and installs `mlx-audio` into it | about 600 MB on disk |
| **Download** | Fetches the model into the standard Hugging Face cache | 1.0 GB (fast) or 1.7 GB (accurate) |

Two models, measured on an M4 Air with the same 8.7 s clip:

| Model | Wait for a 10 s dictation | Memory while loaded | Notes |
|-------|---------------------------|---------------------|-------|
| **Accurate** (Qwen3-ASR 1.7B) | about 1.0 s | about 2.5 GB | The default. Keeps product names. |
| **Fast** (Qwen3-ASR 0.6B) | about 0.4 s | about 1.0 GB | Weaker on proper nouns. |

That memory is real, so the model is unloaded after an idle window (10 minutes
by default, configurable from 1 minute to 4 hours). The reload costs about 3 s,
and OpenFlow starts it the moment you press the record shortcut, so it happens
while you are still speaking.

Neither Qwen size honours the Whisper `prompt`, so the **Dictionary** setting is
applied to the finished transcript instead: whole-word, longest entry first,
your capitalisation, one pass. Write `ENTRO.LY` to fix the spelling of a word
the model heard correctly, and `intro dot lie -> ENTRO.LY` to fix one it did
not. The same field still goes to Whisper as a prompt on the online providers,
so one dictionary works everywhere.

**Local only** (same panel) refuses any request that would leave this Mac,
checked against the URL before a connection is opened, and again on every
redirect. It turns off cleanup and voice unless those are also pointed at
something running here. The runner itself binds loopback and nothing else, so it
is not reachable from another machine.

Two things it does not cover, both by design. **Install and Download** contact
PyPI and Hugging Face -- they are one-time steps you press a button for.
And **enabled plugins are separate programs**: OpenFlow hands them your
transcript over standard input and has no say in what they do with it, so a
plugin can reach the network whatever this toggle says. Only enable plugins you
trust.

To measure it without opening a window:

```bash
./target/release/openflow-native --transcribe /path/to/clip.wav
```

## Self-hosting on your LAN

Both speech-to-text and speech synthesis can point at a machine on your own
network, so audio never leaves it and there is no per-request cost.

Pick **Custom** as the transcription provider, or **Self-hosted / LAN** as the
speech endpoint, and give the OpenAI-compatible base URL:

```
http://192.168.1.10:8880/v1
```

OpenFlow appends the standard paths under it (`/audio/transcriptions`,
`/chat/completions`, `/audio/speech`). Plain `http` is accepted, and an empty
API key is fine -- when there is no key, no `Authorization` header is sent at
all, which is what unauthenticated local servers expect.

Servers that work as-is:

| Role | Server | Notes |
|------|--------|-------|
| Speech synthesis | [`kokoro-fastapi`](https://github.com/remsky/Kokoro-FastAPI) | Serves `/v1/audio/speech`, supports streaming. Model `kokoro`, voices like `af_bella`. |
| Speech synthesis | [`openedai-speech`](https://github.com/matatonic/openedai-speech) | Wraps Piper or XTTS behind the OpenAI API. Lighter on CPU-only boxes. |
| Transcription | [`faster-whisper-server`](https://github.com/fedirz/faster-whisper-server) | Serves `/v1/audio/transcriptions`. |
| Transcription | `whisper.cpp` server | Built-in OpenAI-compatible mode. |

On macOS the first connection to a LAN address triggers the system's local
network permission prompt. Approve it, or the requests fail in a way that looks
exactly like the server being down. (Settings -> Privacy & Security -> Local
Network if it was denied once.)

## Audio pipeline

Capture runs at the device's native rate and format, then downsamples to the
16 kHz mono WAV the speech models expect. The resampler low-passes before
decimating: skipping that step folds everything above the new 8 kHz Nyquist
back into the speech band (a 15 kHz whine lands on 1 kHz, on top of the voice),
and interpolation alone does not prevent it. Gain is then set from the 95th
percentile of sample magnitude rather than the absolute peak, so a single cough
or desk bump does not cancel the boost for an otherwise quiet recording.

## Build from source

### Prerequisites

- [Node.js](https://nodejs.org/) `^20.19.0` or `>=22.12.0`
- npm `>=10.8`
- A current stable [Rust toolchain](https://rustup.rs/)
- Platform prerequisites from the [Tauri v2 setup guide](https://v2.tauri.app/start/prerequisites/)

Linux additionally needs WebKitGTK 4.1, ALSA development headers, an app-indicator library, and Secret Service tools. On Ubuntu 22.04:

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential libasound2-dev libayatana-appindicator3-dev \
  libsecret-tools librsvg2-dev libssl-dev libwebkit2gtk-4.1-dev
```

Clone and launch the development app:

```bash
git clone https://github.com/laisyio/openflow.git
cd openflow
npm ci
npm run tauri:dev
```

The first recording prompts for microphone access. Auto-paste may separately prompt for Accessibility or Automation access on macOS. Grant only the permissions you want OpenFlow to use.

## Development commands

```bash
# Strict TypeScript check and production frontend build
npm run check

# Rust formatting, Clippy, and tests
npm run check:rust

# Dependency advisory gate
npm run audit:dependencies

# Build the platform's desktop bundles
npm run tauri:build
```

CI runs these checks from a clean install and compiles the desktop app on macOS, Windows, and Ubuntu.

## Native build (experimental, macOS only)

`crates/openflow-native` is the same app with AppKit windows instead of a WKWebView: a status item, one main window whose sidebar holds Dictate, History, Plugins and Settings, a setup wizard presented as a sheet on it, and the overlay pill as an `NSPanel`. It drives the same `openflow-core` engine as the Tauri build and reads the same database and keychain items, so the two can be swapped without reconfiguring anything. The plan is `docs/native-port/PLAN.md`; the local transcription runner is what is left of Milestone B.
`crates/openflow-native` is the same app with AppKit windows instead of a WKWebView: a status item, a setup wizard, Settings, History and Plugins windows, and the overlay pill as an `NSPanel`. It drives the same `openflow-core` engine as the Tauri build and reads the same database and keychain items, so the two can be swapped without reconfiguring anything. It can also transcribe on-device (see [Local transcription](#local-transcription-private)). The plan is `docs/native-port/PLAN.md`; streaming TTS is what is left of Milestone B.

A launch with no provider saved opens the setup wizard instead of Settings: provider, key, a connection test, then microphone and shortcut. Settings has a "Run setup again" button that reopens it.

```bash
# Build the binary
cargo build -p openflow-native --release

# Prove the engine comes up without opening a window
./target/release/openflow-native --self-check

# Which build is this? Version from Cargo.toml, commit baked in at build time
./target/release/openflow-native --version      # OpenFlow 0.1.0 (a1b2c3d)

# Transcribe one file with the saved settings and print the text and timing
./target/release/openflow-native --transcribe clip.wav

# Assemble target/OpenFlow.app, ad hoc signed; add --dmg for a disk image
bash scripts/bundle-native.sh
```

The cargo target is `openflow-native` because `src-tauri` already builds a bin called `openflow`; inside the bundle the executable is `Contents/MacOS/openflow`, and the suffix goes away when `src-tauri` is retired.

The first launch raises a keychain prompt for each saved API key. That is expected: the keychain items were created by the Tauri build, under its code signature, and macOS asks before letting a differently signed binary read them. Choose Always Allow to be asked once rather than once per launch.

To diagnose a launch you cannot attach a terminal to, set `OPENFLOW_TRACE=1` before opening the app and it will log tray clicks and window activations to stderr, which `open -a` routes to the unified log:

```bash
launchctl setenv OPENFLOW_TRACE 1     # then open the app
launchctl unsetenv OPENFLOW_TRACE     # back to silent, the default
```

Launch it with `open -a target/OpenFlow.app`, not from a shell. macOS binds microphone and accessibility grants to the *code signature* of whatever asked, and a binary started from a terminal inherits the terminal's identity, so the grant lands on the terminal and the app appears to have been refused.

Grant those permissions once, not once per build:

```bash
bash scripts/local-signing-identity.sh     # once per machine
```

That installs a self-signed code signing certificate in a keychain of its own and `scripts/bundle-native.sh` picks it up from then on. It matters because macOS matches a TCC grant against the signature's *designated requirement*, and an ad hoc signature has nothing durable to name itself by, so its requirement is the code hash -- which changes with the code. Signed with a certificate the requirement names the certificate, and every later build still satisfies it. The bundle script prints which of the two it produced:

```
signed: identifier "io.laisy.openflow" and certificate root = H"..."   # survives rebuilds
signed: cdhash H"..."                                                  # does not
```

The certificate is self-signed and never leaves this machine; it proves nothing to anyone else and is not a step towards distribution, which still wants a Developer ID. Set `OPENFLOW_SIGN_IDENTITY` to sign with something else, and expect to answer the microphone and accessibility prompts one last time on the first build after switching identities. `scripts/local-signing-identity.sh --remove` puts things back.

### Which build am I running

Three places say it, and they say the same thing because there is one source for each half. The version is the `version` line in `crates/openflow-native/Cargo.toml`; the commit is baked into the binary by `crates/openflow-native/build.rs`, from `OPENFLOW_COMMIT` if the environment sets it, else `git rev-parse --short HEAD`, else the literal `unknown` outside a checkout.

| Where | Shows |
| --- | --- |
| `openflow-native --version` | `OpenFlow 0.1.0 (a1b2c3d)` |
| **About OpenFlow** in the app menu | the same line, in Apple's standard About panel |
| `OpenFlow.app/Contents/Info.plist` | `CFBundleShortVersionString` and `CFBundleVersion` = the version, `OpenFlowCommit` = the commit |

The bundle script does not compute the commit itself: it runs the binary it just assembled with `--version` and copies the answer into the plist, so an app bundle can never name a commit its own executable was not built from. To read it back out of an installed app:

```bash
plutil -p /Applications/OpenFlow.app/Contents/Info.plist | grep -E 'Version|OpenFlowCommit'
```

### Cutting a release

```bash
bash scripts/bundle-native.sh --print-artifacts   # version=0.1.0  arch=aarch64  dmg=OpenFlow_0.1.0_aarch64.dmg
bash scripts/bundle-native.sh --dmg               # builds it, then prints the path and its SHA-256
```

The disk image is `target/OpenFlow_<version>_<arch>.dmg`: volume name `OpenFlow`, the app and an `Applications` symlink, nothing else. Re-running overwrites it. It is deterministic in name, layout and contents, not in bytes -- `hdiutil` writes filesystem creation times into the image, so two runs a minute apart differ.

To publish one:

1. Put the release's entry under `## Unreleased` in `CHANGELOG.md` and bump `version` in `crates/openflow-native/Cargo.toml`. Those are the two inputs; everything else is derived.
2. Push a tag whose name is `v` plus that version: `git tag v0.2.0 && git push origin v0.2.0`. `.github/workflows/release.yml` refuses if the two disagree, so a mistyped tag fails before it builds anything.
3. The workflow runs on `macos-14`: `cargo test --workspace`, `bash scripts/bundle-native.sh --dmg`, `hdiutil verify`, then `gh release create` with the DMG, a `.sha256` beside it, and the Unreleased section as the release notes.

`workflow_dispatch` runs the same job without publishing and leaves the image as a build artifact, which is how to prove a change to the workflow without spending a tag.

**The published image is signed ad hoc**, because no signing secret lives in this repository. macOS will refuse to open it on a first double-click; right-click and Open, or `xattr -d com.apple.quarantine`, until the follow-up lands. That follow-up is a Developer ID certificate in repository secrets plus `xcrun notarytool submit --wait` and `xcrun stapler staple`, and it plugs into one marked point in `release.yml` -- the bundle script already prefers `OPENFLOW_SIGN_IDENTITY` over everything else, so nothing but the workflow changes.

## How dictation works

1. Hold the record shortcut and speak.
2. Release it to stop recording.
3. OpenFlow converts the captured audio to a mono 16 kHz WAV.
4. The selected provider transcribes the completed recording.
5. If enabled, the selected chat model cleans up the text.
6. Enabled `after_transcribe` and `after_format` hooks can transform the result.
7. OpenFlow saves the result to local history. It also copies the result to the clipboard, unless "Keep my clipboard" is on, in which case whatever you had copied is put back after the paste is delivered.
8. The platform paste helper attempts to paste into the app focused at completion time.

Because network and cleanup latency vary, keep the intended destination focused until processing finishes. Review generated text before using it in commands, code, or other sensitive contexts.

## Plugin hooks

Plugins live under `~/.openflow/plugins/<plugin-id>/`. An enabled plugin may declare `after_transcribe` and/or `after_format` plus a relative executable `entrypoint` in `manifest.json`. OpenFlow passes a JSON payload on standard input and expects the updated payload as JSON on standard output. Hooks run serially with a five-second timeout.

Plugin entrypoints run with the user's operating-system permissions. There is currently no plugin marketplace, signature verification, or sandbox.

## Current limitations

- No meeting mode
- No context capture from the active window or screen
- Auto-paste depends on platform permissions and helpers, and targets whichever app is focused when processing completes
- Linux auto-paste requires `xdotool` or `ydotool`; Wayland compositor support varies
- Provider APIs can change independently of OpenFlow
- Pre-built signed/notarized installers and automatic updates are not available yet

## License

[MIT](LICENSE)
