# Computer use with Jev — native Mac preview

OpenFlow can turn a spoken or typed command into one action in another Mac app.
Jev chooses from that window’s actual Accessibility controls; OpenFlow displays
the proposal, and **nothing executes until you click Confirm action**.

## Setup

1. Build the native macOS app (`cargo build -p openflow-native --release`, or use
   the existing native app bundling script). This feature is not in the Tauri or
   iOS builds.
2. Get a TypeSafe API key from [TypeSafe](https://typesafe.ai). This is separate
   from your transcription/OpenRouter key.
3. Switch to the app you want to control. In OpenFlow’s menu bar choose
   **Computer use…**. The target app is captured before OpenFlow activates.
4. Save the TypeSafe key and enable session sharing. A blank Save key removes it.
5. Enable OpenFlow under macOS **System Settings → Privacy & Security →
   Accessibility**. Recording additionally needs Microphone permission and a
   working speech provider/local model in OpenFlow’s settings.
6. Type a command or choose **Record command**, then **Stop & review**. Recording
   stops automatically after 15 seconds. Jev receives the final transcript, not
   partial speech. Check the proposed app, window, control, action and text, then
   confirm. OpenFlow sends one action and brings the target app forward.

Examples: `focus Search`, `click New note`, `type "hello world" into Search`.
Text insertion requires one pair of straight double quotes. Speech recognition
may omit those quotes; edit the command and review it again. Inserted text is
copied from the quoted command, not generated. It replaces the target field’s
current selection (or inserts at its caret); it never synthesizes Return.

To change apps, activate the other app and reopen Computer use from the menu bar.
Cancel or closing the window cancels outstanding work and discards the proposal.
Closing also ends sharing consent and clears the command field. Cancellation
cannot retract a request already sent to a provider or undo an executed action.

## Privacy and safeguards

- The Jev key is stored in macOS Keychain, not SQLite. Requests use a fixed HTTPS
  TypeSafe endpoint, no redirects and no automatic retry. The HTTP connection
  pool is reused; keys are attached per request, not stored in client defaults.
- Jev receives the command, application name and bounded control labels, roles
  and supported actions. **Labels can contain sensitive information.** No screen
  images or editable-field values are read. Secure-text controls are excluded.
  The window title and text selection range are checked locally and not sent.
- Voice goes to the configured speech provider (or the local runner). Command
  transcription bypasses cleanup, plugins, clipboard insertion and history.
  Normal dictation’s live preview is not started. The existing local runner may
  use a private temporary audio file during decoding, as for ordinary dictation.
- Local-only protection blocks Jev, even when speech recognition runs locally.
  OpenFlow never changes that setting to make computer use work.
- Proposals expire 30 seconds after the snapshot. Before acting, OpenFlow checks
  the original window identity/title, control list and retained target identity;
  typing also requires the same selection range. Changes require another review.
- Every action requires confirmation, including seemingly harmless clicks.
  Clicking a control can delete data, send a message or make a purchase: read the
  review carefully. Model confidence is not a security guarantee.

## Scope and limits

Supported actions are Accessibility press, focus, literal selected-text insertion
and page up/down scrolling **when the app exposes those capabilities**. Canvas
UIs, inaccessible controls, secure fields, launching apps, arbitrary hotkeys,
shell commands, generated replies and multi-step autonomous tasks are not
supported. Many apps do not expose page-scroll actions or writable selected text.
There is no coordinate-click or clipboard fallback that could hit another app.

Enumeration is bounded to 80 controls, 512 visited nodes and depth 12, with a
per-object messaging timeout and an approximately 800 ms traversal budget.
Large windows may expose only an initial subset. Jev gets at most 2,000 bytes of
command text, has a 10-second request timeout and a 128 KiB response limit.
Low-confidence, malformed, unsupported and unknown-target answers fail closed.

The integration follows the [TypeSafe HTTP contract](https://docs.typesafe.ai/api)
and pins `jev-1.13.0`. Unlike the
[voice-browser reference](https://github.com/moritzkremb/jev-voice-browser), this
preview does not speculate on partial transcripts or automatically execute them.
No end-to-end latency claim is made.

## Verification

Automated coverage uses synthetic commands, loopback HTTP fixtures and retained
Core Foundation identity fixtures. It tests request/response validation, network
errors, redirects, response limits, target changes, selection changes, expiration,
recording ownership and cancellation cleanup. The native snapshot harness renders
the actual setup/review controls in light and dark appearances.

These checks do not substitute for live Jev accuracy or real-app Accessibility
compatibility testing. Those require a TypeSafe key and explicit macOS permissions;
no personal credentials, live microphone or third-party app actions are used by
the automated checks.

Local verification on 2026-09-23 (Apple silicon): `cargo test --workspace` passed
330 tests, with eight existing opt-in tests ignored; workspace Clippy passed with
warnings denied, and the native release build succeeded. The snapshot harness
produced 56 light/dark fixtures, including setup and action review for this feature.
Live TypeSafe inference and third-party app execution remain unverified.

One parallel full-suite run hit the existing
`runtime_retention_never_follows_symlinks_or_prunes_unknown_metadata` assertion
in unchanged runner code. It passed immediately in isolation and in the subsequent
full-suite rerun. This intermittent runner-test failure is recorded, not treated
as a Jev regression or claimed fixed here.
