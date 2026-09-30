# Computer use with Jev — native Mac preview

OpenFlow can turn a spoken or typed command into one action in another Mac app.
Jev chooses from that window’s actual Accessibility controls; OpenFlow displays
the proposal, and **nothing executes until you click Confirm action**.

## Setup

1. Build the native macOS app (`cargo build -p openflow-native --release`, or use
   the existing native app bundling script). This feature is not in the Tauri or
   iOS builds.
2. Get a key from [TypeSafe](https://typesafe.ai) or use your
   [OpenRouter API key](https://openrouter.ai/settings/keys).
3. Switch to the app you want to control. In OpenFlow’s menu bar choose
   **Computer use…**. The target app is captured before OpenFlow activates.
4. Choose **Jev via TypeSafe** or **Jev via OpenRouter**, paste that provider’s
   key, save it, then enable session sharing. A blank Save key removes only that
   provider’s Jev key. You can paste an existing OpenRouter key; OpenFlow does not
   automatically copy your speech/cleanup credentials. Switching providers
   cancels pending work, clears the unsaved key field and requires fresh consent.
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
Closing also ends sharing consent and clears the command field and target app.
Reopening requires a currently active external app; OpenFlow checks its retained
process identity, bundle identifier and launch time before accepting commands.
Cancellation
cannot retract a request already sent to a provider or undo an executed action.

## Privacy and safeguards

- Each provider’s Jev key is stored separately in macOS Keychain, not SQLite.
  Requests use that provider’s fixed HTTPS endpoint, no redirects, cross-provider
  fallback or automatic retry. The HTTP connection
  pool is reused; keys are attached per request, not stored in client defaults.
- TypeSafe receives the command, application name and bounded control labels, roles
  and supported actions. **Labels can contain sensitive information.** No screen
  images or `AXValue` contents are read. Accessibility labels can nevertheless
  include current selections or other content exposed by the app. Secure-text controls are excluded.
  The window title and text selection range are checked locally and not sent.
  With OpenRouter selected, OpenRouter also receives the request and routes it
  to TypeSafe. The sharing switch explicitly names both recipients.
- Voice goes to the configured speech provider (or the local runner). Command
  transcription bypasses cleanup, plugins, clipboard insertion and history.
  Normal dictation’s live preview is not started. The existing local runner may
  use a private temporary audio file during decoding, as for ordinary dictation.
- Local-only protection blocks Jev, even when speech recognition runs locally.
  OpenFlow never changes that setting to make computer use work.
- Proposals expire 30 seconds after the snapshot. Before acting, OpenFlow checks
  the original window identity/title, control list and retained target identity;
  typing also requires the same selection range. Changes require another review.
  The confirmation button is also disabled when the proposal expires.
- Duplicate role/label targets and unlabeled press targets are rejected; rephrase
  the command or use the app directly. Untrusted review strings are escaped.
- Every action requires confirmation, including seemingly harmless clicks.
  Clicking a control can delete data, send a message or make a purchase: read the
  review carefully. Model confidence is not a security guarantee.
- Mutation calls have a separate two-second timeout. A missing acknowledgement
  does not prove an action failed: OpenFlow brings the app forward and warns you
  to check its result before trying again. No action is retried automatically.

## Scope and limits

Supported actions are Accessibility press, focus, literal selected-text insertion
and page up/down scrolling **when the app exposes those capabilities**. Canvas
UIs, inaccessible controls, secure fields, launching apps, arbitrary hotkeys,
shell commands, generated replies and multi-step autonomous tasks are not
supported. Many apps do not expose page-scroll actions or writable selected text.
There is no coordinate-click or clipboard fallback that could hit another app.

Enumeration is bounded to 80 controls, 512 visited nodes and depth 12, with ranged
child reads. Each Accessibility read receives a messaging timeout capped at 50 ms
and the remaining 800 ms traversal budget; exhausted snapshots are rejected.
This remains main-thread work with cooperative macOS timeouts, not a hard
wall-clock guarantee. Rapidly changing labels elsewhere in a window can invalidate
the conservative whole-window comparison and require another review.
Large windows may expose only an initial subset. Jev gets at most 2,000 bytes of
command text, has a 10-second request timeout and a 128 KiB response limit.
Low-confidence, malformed, unsupported and unknown-target answers fail closed.

Direct TypeSafe uses `https://api.typesafe.ai/v1/systemone` with `jev-1.13.0`,
following the [TypeSafe HTTP contract](https://docs.typesafe.ai/api).
OpenRouter uses `https://openrouter.ai/api/alpha/decisions` with
`typesafe/jev-1.13`, following its [documented Decisions API example](https://openrouter.ai/blog/insights/what-is-jev/).
This is not the chat-completions API or the separate Jev Router model. OpenRouter’s
Decisions endpoint is currently an alpha API; both providers’ responses pass the
same strict validation before a proposal can be confirmed. Unlike the
[voice-browser reference](https://github.com/moritzkremb/jev-voice-browser), this
preview does not speculate on partial transcripts or automatically execute them.
No end-to-end latency claim is made.
When response model metadata is supplied, it must name the requested model or
its versioned release; unexpected or malformed model identifiers are rejected.

## Verification

Automated coverage uses synthetic commands, loopback HTTP fixtures and retained
Core Foundation identity fixtures. It tests request/response validation, network
errors, redirects, response limits, target changes, selection changes, expiration,
recording ownership and cancellation cleanup. The native snapshot harness renders
the actual setup/review controls in light and dark appearances.

These checks do not substitute for live Jev accuracy or real-app Accessibility
compatibility testing. Those require the selected provider’s key and explicit macOS permissions;
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

OpenRouter follow-up verification on 2026-09-30: the workspace suite passed
338 tests with eight opt-in tests ignored; workspace Clippy passed with warnings
denied, formatting checks passed and the native release build succeeded. The
snapshot harness rendered 56 light/dark fixtures; the TypeSafe setup and OpenRouter
action-review layouts were visually inspected. New checks cover provider-specific endpoints, models and credential
slots, the OpenRouter Decisions response envelope, ambiguous targets, escaped
review text and exhausted Accessibility read budgets. Requests in tests use only
synthetic credentials and loopback servers. Live inference through either
provider and real-app action execution remain unverified.

A read-only review by Claude Opus 5.5 (`claude-opus-5-5`) found no critical or
high-severity issues. Its findings led to escaped/validated review roles,
ambiguity rejection, tighter Accessibility read budgets, fresh application
identity checks, expiring confirmation UI, clearer command-recording ownership
and corrected privacy copy. Conservative whole-window invalidation is retained
intentionally. A recorder still being stopped is not forcibly reclaimed; wait for
its owner to finish before starting another recording.

The follow-up Opus 5.5 review found a medium-severity ambiguity in action timeout
reporting and a low-severity response-model validation gap. Both were addressed:
mutations no longer inherit short snapshot read timeouts, uncertain outcomes are
not reported as refusals, and supplied response model identifiers are checked.
Regression tests cover both cases. Both reviews were source-only, not live API or
Accessibility execution tests. Live response probability completeness and real-app
typing capabilities remain compatibility checks for a credentialed manual trial.
The same unchanged runner-retention test also failed once in the September 30
parallel suite run, then passed in isolation and in the full 338-test rerun.
