# OpenFlow desktop design

## Product and intent
Native macOS dictation for people who want to speak instead of type. First impression:
**I know how to start, and I know where my words go.** Keep AppKit, keyboard access,
system permissions, and the existing engine. This is an application, not a landing page.

## Visual system
- Direction: a plainspoken, editorial workspace with obvious links. Native surfaces,
  deep ink, restrained teal, and thin rules instead of enclosing the home in cards.
- Home headline and sidebar wordmark: Georgia Bold, 32 pt and 22 pt respectively,
  with native fallback. Other display text: Avenir Next Demi Bold, 22–30 pt.
  Section titles: 16–19 pt.
- Body: Avenir Next Regular, 13–14 pt where layout permits. Native SF fallback and
  native control fonts preserve platform legibility and accessibility.
- Surfaces and text: semantic AppKit window/control backgrounds and label colors
  follow the user's appearance and contrast settings. The light/dark palette is
  intentionally native rather than hard-coded canvas RGB values.
- Primary recording action: deep teal #16746B with white text/microphone in both
  appearances; darker while pressed, muted when disabled. Native focus-ring mask.
- Use semantic AppKit labels/controls where required for accessibility. Resolve
  custom colors at drawing time so appearance switching does not retain stale fills.
- Rhythm: 4 pt base, 24 pt page inset, 16 pt group gap, 20 pt card padding.
- Radii: 12 pt major groups, native small control radii. Do not turn every row into a card.
- Decoration: one primary action, small meaningful SF Symbols; no gradients, stock
  photos, decorative statistics, perpetual animation, or network-loaded fonts.
- Motion: functional only; no animation necessary to understand state.

## Information architecture
- Dictate: an editorial introduction, four underlined native links (setup, history,
  local/cloud processing, privacy), readiness, one recording action, shortcut,
  recent result. Links navigate without changing preferences and are disabled
  during recording/processing. Recording and cancellation fit a 410 pt viewport;
  secondary details scroll. Keep native accessibility and focus affordances.
- History: readable saved work, clear copy/search/delete affordances.
- Settings: advanced control remains available without burdening first run.
- Extensions/plugins: describe executable trust; never imply Local only confines plugins.
- First run: welcome → processing/privacy choice → actual connection/setup →
  microphone/shortcut preferences → truthful next action. No credentials for local mode.
- Cloud audio goes to the chosen provider; optional cleanup sends transcript text.
  On-device install/download needs a one-time network connection. History is local,
  unencrypted, and optional. Local-only restrictions must be enforced, not just copy.
- Do not say ready when runtime/model/permissions are still missing. Permission denial,
  download failure, cancellation and return navigation must have recoverable paths.

## Validation
Three serial implementation-complete → independent-review → correction design rounds,
followed by three measured performance rounds. Record evidence and limits in
docs/desktop-overhaul-validation.md. Do not count repeated tests as independent review.

## Decisions
2026-09-15: User delegated visual decisions. Preserve native architecture and all
existing workflows; prioritize first-run clarity over adding new providers/features.
2026-09-15: User confirmed the Basecamp-inspired direction belongs inside the native
desktop app, not a new website. Borrow the direct writing and visible-link approach;
retain original OpenFlow branding and existing onboarding/privacy behavior. Follow-up
evidence lives in docs/link-first-desktop-validation.md.
