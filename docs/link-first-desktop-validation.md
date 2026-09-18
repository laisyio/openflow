# Link-first native desktop follow-up

2026-09-15. User confirmed the Basecamp-inspired direction should be applied
inside the native macOS application, not to a new website. This is a scoped
follow-up to the completed three design and three performance rounds recorded
in `desktop-overhaul-validation.md`, not a claim of six additional rounds.

## Changes

- Original OpenFlow editorial headline and Georgia wordmark; semantic AppKit
  colors and locally available fonts. Thin rules replace the home screen cards.
- Four underlined, labeled native buttons lead directly to setup, history,
  processing providers and privacy. Navigation does not mutate preferences.
- Links disable while recording/processing, with an additional handler guard.
- Existing hold-to-record, keyboard/AX activation, cancellation, bounded result
  preview, privacy route reporting and onboarding behavior remain in place.
- Record and cancel targets fit the initial 410-point-high viewport. Secondary
  information remains scrollable.

## Review and verification

The design-review skill's visual checklist informed hierarchy, visible link
affordances, native focus rings and narrow-window checks. Its website-specific
browser/clean-checkout workflow was not run against this native, dirty checkout.
No previous work was stashed or committed.

An independent agent reviewed routes, accessibility labels, idle guards and
layout. It found cancellation partly below the shortest viewport. Tightening
the header/link rows moved the entire cancel target from y396–424 to y380–408.
A regression assertion and a resized processing-state fixture cover the fix.
The same independent reviewer rechecked the fix in source and the final native
light/dark processing and resized screenshots: cancellation is fully visible,
link rows remain separate, and no new blocking regression was found.

- `cargo test -p openflow-core -p openflow-native --offline`: 281 passed,
  5 intentionally ignored hardware/model or opt-in benchmark tests.
- `cargo clippy -p openflow-core -p openflow-native --all-targets --offline -- -D warnings`:
  no warnings.
- Native snapshot harness: 52 synthetic light/dark images generated at
  `target/link-first-verified`. Home fixtures include normal, narrow, full-height,
  actual 704→440-point resizing, and disabled-navigation/transcribing states.
  Resizing asserts nonoverlapping link targets at least 160 points wide.
- Inspected light/dark home, narrow/resized and processing screenshots. The
  processing fixtures exercise native control rendering, not live audio.
- Release build, strict bundle signature verification and isolated packaged
  `--self-check`: passed.

No live microphone, paid provider request, personal transcript inspection or
manual VoiceOver/key traversal was performed. Native focus/AX configuration and
existing input-handler tests were checked; end-to-end assistive technology use
is not claimed. No new performance improvement is claimed for this layout-only
follow-up; prior measured optimizations remain unchanged.

## Local artifacts

Updated bundle: `target/OpenFlow.app`. Previous bundle preserved as
`target/OpenFlow-before-link-first.app`. Both remain inside this checkout;
no installed application was replaced. The new bundle is ad-hoc signed because
no signing identity is configured, so macOS may require microphone/accessibility
permissions again. No permissions were granted automatically.
