//! The native Dictate workspace: one recording action, an explicit processing
//! route, and the latest words within reach.
//!
//! The big button drives the same `hotkey_pressed` / `hotkey_released` pair the
//! global shortcut does, so holding it is holding the shortcut, down to the
//! silence gate and the live preview on the pill.
//!
//! The button is a subclass rather than a plain `NSButton` because a button
//! cannot express "hold". `NSButton`'s action fires once, on mouse-up, and its
//! `mouseDown:` runs a tracking loop that never returns until the mouse is
//! released -- so the press half of a press-and-hold is unreachable through the
//! ordinary target/action path. [`HoldButton`] overrides both halves and does
//! not call super, which is what makes the down edge and the up edge separate
//! events.
//!
//! It answers the keyboard for the same reason the web screen does. A control
//! whose only gesture is "hold the mouse down on it" cannot be operated from
//! the keyboard at all. Space and Return press and release it, `isARepeat`
//! drops the auto-repeat of a held key so the press is not delivered twice, and
//! the Dictate page takes first responder when it comes forward so the key
//! reaches the button without a Tab first.
//!
//! The keyboard is not enough for VoiceOver, which walks the screen with its
//! own cursor and never moves the first responder, so the Space key above is
//! not a way in for it. What VoiceOver sends -- and what every scripted click
//! sends -- is `AXPress`, and `NSButton` answers that by asking its cell to
//! `performClick:`, which sends the button's *action*. This button has no
//! action; its overrides send two selectors of their own to the target. So the
//! press was accepted, reported as handled, and did nothing at all.
//!
//! [`HoldButton::accessibility_perform_press`] closes that. It does not try to
//! make `AXPress` hold, which one instant cannot express: it hands the two
//! edges out one press at a time, so the first press starts the capture and the
//! next one ends it, and `accessibilityHelp` says so out loud because the
//! visible title still reads "Hold to record".

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAccessibility, NSAttributedStringNSStringDrawing, NSAutoresizingMaskOptions, NSBezelStyle,
    NSBezierPath, NSBox, NSBoxType, NSButton, NSColor, NSControl, NSEvent, NSFocusRingType,
    NSFontAttributeName, NSForegroundColorAttributeName, NSScrollView, NSTextAlignment,
    NSTextField, NSUnderlineStyleAttributeName, NSView,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSNumber, NSObject, NSPoint, NSRect, NSSize, NSString,
};

use openflow_core::engine::{Engine, EngineEvent, RecordingState};
use openflow_core::insert::InsertMethod;
use openflow_core::transcribe::{is_loopback_url, Provider};

use crate::hotkeys;
use crate::ui::card::{Flipped, GAP, MARGIN, PADDING};
use crate::ui::{allow_wrapping, body_font, display_font, note};

/// Fixed reading rhythm inside a scrollable document; short windows never
/// compress the recording or cancellation targets out of reach.
const RECORDER_HEIGHT: f64 = 272.0;
const HEADER_HEIGHT: f64 = 198.0;

const HOME_LINKS: &[(&str, &str)] = &[
    ("Set up OpenFlow", "onboarding"),
    ("Your history", "history"),
    ("Local or cloud", "providers"),
    ("Privacy & history", "privacy"),
];

fn home_link_destination(tag: isize) -> Option<&'static str> {
    HOME_LINKS
        .get(usize::try_from(tag).ok()?)
        .map(|entry| entry.1)
}

/// Space for a wrapped transcript preview. The copy action always keeps the
/// entire result even when the bounded visible preview is truncated.
const RESULT_LINES: f64 = 76.0;
/// Height of the section that shows the last result: a caption, the gap under it,
/// the three lines, and the padding round all of it.
const RESULT_HEIGHT: f64 = PADDING + 20.0 + 8.0 + RESULT_LINES + PADDING;
/// Where the last result is cut. The full text is one click away, on the
/// clipboard, so the preview only has to be recognisable.
const RESULT_CHARS: usize = 220;

// ── The hold button ───────────────────────────────────────

/// Whether the button is mid-hold. Only the `AXPress` path reads it: the mouse
/// and the keyboard each bring their own down edge and up edge, but a press is
/// a single instant with no "still held", so that path has to remember which
/// half it owes. `Cell<bool>` so the class still implements no Drop.
#[derive(Default)]
pub struct HoldButtonIvars {
    holding: Cell<bool>,
}

define_class!(
    // SAFETY: `NSButton` is designed for subclassing, this class holds only a
    // `Cell<bool>` and implements no Drop, and every method here is one AppKit
    // already defines with this signature.
    #[unsafe(super(NSButton))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowHoldButton"]
    #[ivars = HoldButtonIvars]
    pub struct HoldButton;

    impl HoldButton {
        /// Native push bezels cap their visible height even with a 48pt frame.
        /// Paint the actual target; retain NSButton's accessibility and focus
        /// machinery and the existing separate down/up event paths.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let fill = if !self.isEnabled() {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.32, 0.39, 0.39, 1.0)
            } else if self.isHighlighted() {
                NSColor::colorWithSRGBRed_green_blue_alpha(0.06, 0.35, 0.31, 1.0)
            } else {
                NSColor::colorWithSRGBRed_green_blue_alpha(22.0 / 255.0, 116.0 / 255.0, 107.0 / 255.0, 1.0)
            };
            fill.setFill();
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, 10.0, 10.0).fill();
            let font = display_font(17.0);
            let ink = NSColor::whiteColor();
            let attributes = NSDictionary::from_slices(
                &[unsafe { NSFontAttributeName }, unsafe { NSForegroundColorAttributeName }],
                &[&*font as &AnyObject, &*ink as &AnyObject],
            );
            let text = unsafe { NSAttributedString::new_with_attributes(&self.title(), &attributes) };
            let text_size = text.size();
            let x = (bounds.size.width - text_size.width - 28.0) / 2.0;
            text.drawAtPoint(NSPoint::new(x + 28.0, (bounds.size.height - text_size.height) / 2.0));
            // A compact, crisp microphone silhouette, not a decorative emoji.
            ink.setFill();
            ink.setStroke();
            let cy = bounds.size.height / 2.0;
            let y = |offset: f64| cy + if self.isFlipped() { -offset } else { offset };
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                NSRect::new(NSPoint::new(x + 5.0, y(-2.0).min(y(11.0))), NSSize::new(8.0, 13.0)), 4.0, 4.0,
            ).fill();
            let mic = NSBezierPath::bezierPath();
            mic.moveToPoint(NSPoint::new(x + 1.0, y(2.0)));
            mic.curveToPoint_controlPoint1_controlPoint2(
                NSPoint::new(x + 17.0, y(2.0)),
                NSPoint::new(x + 1.0, y(-9.0)), NSPoint::new(x + 17.0, y(-9.0)),
            );
            mic.moveToPoint(NSPoint::new(x + 9.0, y(-6.0)));
            mic.lineToPoint(NSPoint::new(x + 9.0, y(-11.0)));
            mic.moveToPoint(NSPoint::new(x + 4.0, y(-11.0)));
            mic.lineToPoint(NSPoint::new(x + 14.0, y(-11.0)));
            mic.setLineWidth(1.7);
            mic.stroke();
        }

        #[unsafe(method(focusRingMaskBounds))]
        fn focus_ring_mask_bounds(&self) -> NSRect { self.bounds() }

        #[unsafe(method(drawFocusRingMask))]
        fn draw_focus_ring_mask(&self) {
            NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(self.bounds(), 10.0, 10.0).fill();
        }

        /// Deliberately does not call super. `NSButton`'s `mouseDown:` runs its
        /// own tracking loop and only returns once the button has been
        /// released, which would collapse the press and the release into one
        /// event and lose the hold.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            if !self.isEnabled() {
                return;
            }
            self.begin_hold();
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            self.end_hold();
        }

        /// Focusable, so Space and Return can reach it. A disabled button is
        /// skipped, which is what keeps the focus ring off it while a
        /// transcription is running.
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            self.isEnabled()
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !is_activation_key(event) {
                // Everything else is somebody else's: Tab out, arrow keys to
                // the sidebar, Cmd-anything to the menu.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
                return;
            }
            // The auto-repeat of a held key. The press already happened, and
            // delivering it again would start a second capture.
            if event.isARepeat() || !self.isEnabled() {
                return;
            }
            self.begin_hold();
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            if !is_activation_key(event) {
                let _: () = unsafe { msg_send![super(self), keyUp: event] };
                return;
            }
            self.end_hold();
        }

        /// The only way in that does not need a mouse or the first responder.
        /// VoiceOver's VO-Space and every scripted click arrive here, and
        /// `NSButton`'s own answer -- `performClick:`, which sends the action --
        /// is a no-op on a button whose action is nil, which this one's is.
        ///
        /// A press is one instant and a hold is two edges, so one press cannot
        /// be both: the press alternates instead, starting the capture and then
        /// ending it. Returning false while disabled is how the transcribing
        /// state refuses a second capture, the same as `mouseDown:` does.
        ///
        /// Written without an early return: `define_class!` rewrites the body
        /// to hand AppKit an ObjC `BOOL`, and only the tail expression is
        /// converted for it.
        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool {
            if !self.isEnabled() {
                false
            } else {
                if self.ivars().holding.get() {
                    self.end_hold();
                } else {
                    self.begin_hold();
                }
                true
            }
        }

        /// Said out loud because the visible title cannot be: it reads "Hold to
        /// record", and holding is exactly what this path does not do.
        #[unsafe(method_id(accessibilityHelp))]
        fn accessibility_help(&self) -> Retained<NSString> {
            NSString::from_str(PRESS_HELP)
        }
    }
);

/// What VoiceOver reads after the button's title. Spelled as two presses
/// because that is what `accessibilityPerformPress` above actually does.
const PRESS_HELP: &str =
    "Press to start recording, then press again to stop. Holding the button works too.";

/// Space or Return, the two keys the web screen's `record-button` listens for.
/// Read from the key code rather than the characters so a non-Latin keyboard
/// layout answers the same.
fn is_activation_key(event: &NSEvent) -> bool {
    matches!(event.keyCode(), KEY_SPACE | KEY_RETURN | KEY_ENTER)
}

const KEY_RETURN: u16 = 36;
const KEY_SPACE: u16 = 49;
/// The numeric keypad's Enter, which macOS reports separately.
const KEY_ENTER: u16 = 76;

impl HoldButton {
    /// The down edge, whichever of the three gestures brought it. Recording the
    /// hold here rather than in each of them is what keeps the `AXPress` toggle
    /// in step with a mouse or a key that got there first.
    fn begin_hold(&self) {
        self.ivars().holding.set(true);
        self.setHighlighted(true);
        self.send(sel!(holdBegan:));
    }

    /// The up edge. Unconditional, like `mouseUp:` has always been: a release
    /// with no press behind it is the engine's to ignore, not this button's.
    fn end_hold(&self) {
        self.ivars().holding.set(false);
        self.setHighlighted(false);
        self.send(sel!(holdEnded:));
    }

    /// Send `selector` to whatever this button's target is. The target is
    /// `NSControl`'s ordinary weak property, so the page owns the button and
    /// the button does not own the page.
    fn send(&self, selector: objc2::runtime::Sel) {
        let Some(target) = self.target() else {
            return;
        };
        // SAFETY: both selectors take one `id` and return void, and the only
        // object ever wired up as this button's target implements them.
        let _: () = unsafe { msg_send![&*target, performSelector: selector, withObject: self] };
    }
}

// ── The page ──────────────────────────────────────────────

struct Controls {
    links: Vec<Retained<NSButton>>,
    route: Retained<NSTextField>,
    eyebrow: Retained<NSTextField>,
    title: Retained<NSTextField>,
    body: Retained<NSTextField>,
    record: Retained<HoldButton>,
    cancel: Retained<NSButton>,
    hint: Retained<NSTextField>,
    result: Retained<NSButton>,
    result_caption: Retained<NSTextField>,
}

pub struct DictateIvars {
    engine: Arc<Engine>,
    view: Retained<NSView>,
    controls: Controls,
    /// The full text behind the truncated card, so clicking it copies all of
    /// what was said rather than what fits.
    last: RefCell<Option<String>>,
    /// What the page is currently showing. Kept because the idle copy depends
    /// on settings as well as on state, so `load` has to redraw the state it is
    /// already in rather than assume it is idle.
    state: Cell<RecordingState>,
    /// Set while the card is reporting a failure instead of a transcript, to
    /// where in the app that failure is answered. The card is the page's
    /// "what just happened", and what just happened was the failure.
    problem: RefCell<Option<String>>,
    refresh: Arc<crate::ui::refresh::RefreshGate>,
}

define_class!(
    // SAFETY: NSObject imposes no subclassing requirements; this class holds
    // only ivars and implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowDictatePage"]
    #[ivars = DictateIvars]
    pub struct DictatePage;

    impl DictatePage {
        #[unsafe(method(openHomeLink:))]
        fn open_home_link(&self, sender: &NSControl) {
            if self.ivars().state.get() != RecordingState::Idle { return; }
            let Some(destination) = home_link_destination(sender.tag()) else { return; };
            crate::app::with_app(|app| {
                app.handle_event(EngineEvent::Navigate(destination.to_string()));
            });
        }

        /// The same entry point the global shortcut uses, so the button and the
        /// hotkey cannot drift apart: silence gate, live preview and insert
        /// method are all decided downstream of here.
        #[unsafe(method(holdBegan:))]
        fn hold_began(&self, _sender: &AnyObject) {
            crate::hotkeys::capture_edge(&self.ivars().engine, true);
        }

        #[unsafe(method(holdEnded:))]
        fn hold_ended(&self, _sender: &AnyObject) {
            crate::hotkeys::capture_edge(&self.ivars().engine, false);
        }

        #[unsafe(method(cancelTranscription:))]
        fn cancel_transcription(&self, _sender: &NSControl) {
            let _ = self.ivars().engine.cancel_transcription();
        }

        /// Clipboard only, never a keystroke: the web screen's result button is
        /// `copyToClipboard`, and the user is looking at this window rather
        /// than at the app they would want the text typed into.
        #[unsafe(method(copyLast:))]
        fn copy_last(&self, _sender: &NSControl) {
            // While the card is reporting a failure it is the way to the screen
            // that answers it, not a copy of a transcript that never arrived.
            let problem = self.ivars().problem.borrow().clone();
            if let Some(target) = problem {
                crate::app::with_app(|app| {
                    app.handle_event(EngineEvent::Navigate(target));
                });
                return;
            }
            let text = self.ivars().last.borrow().clone();
            let Some(text) = text else {
                return;
            };
            let caption = match self.ivars().engine.copy_text(&text) {
                Ok(()) => "Copied to clipboard".to_string(),
                Err(error) => error,
            };
            self.ivars()
                .controls
                .result_caption
                .setStringValue(&NSString::from_str(&caption));
        }
    }
);

impl DictatePage {
    /// Build the page into a view of `size`, the content pane the main window
    /// has to give it.
    pub fn new(
        app: &std::rc::Rc<crate::app::App>,
        mtm: MainThreadMarker,
        size: NSSize,
    ) -> Retained<Self> {
        let engine = Arc::clone(app.engine());
        let (view, controls) = build_content(mtm, size);

        let this = Self::alloc(mtm).set_ivars(DictateIvars {
            engine,
            view,
            controls,
            last: RefCell::new(None),
            state: Cell::new(RecordingState::Idle),
            problem: RefCell::new(None),
            refresh: Arc::new(crate::ui::refresh::RefreshGate::default()),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };

        let controls = &this.ivars().controls;
        let target: &AnyObject = this.as_ref();
        // The hold button carries no action of its own: its overrides send
        // `holdBegan:` and `holdEnded:` to whatever the target is.
        unsafe { controls.record.setTarget(Some(target)) };
        crate::ui::wire(&controls.cancel, target, sel!(cancelTranscription:));
        crate::ui::wire(&controls.result, target, sel!(copyLast:));
        for link in &controls.links {
            crate::ui::wire(link, target, sel!(openHomeLink:));
        }

        this.set_state(RecordingState::Idle);
        this
    }

    /// The view the main window installs in its content pane.
    pub fn view(&self) -> Retained<NSView> {
        self.ivars().view.clone()
    }

    /// Put the keyboard on the record button. Called when the page comes
    /// forward, so Space works without tabbing to it first -- the web screen's
    /// button is focusable for the same reason.
    pub fn focus_record(&self) {
        let ivars = self.ivars();
        if let Some(window) = ivars.view.window() {
            window.makeFirstResponder(Some(&ivars.controls.record));
        }
    }

    /// Re-read what the page shows when it is not being driven by an event: the
    /// bindings, which Settings can change, and the newest transcription.
    pub fn load(&self) {
        let ivars = self.ivars();
        if !ivars.refresh.visible() {
            return;
        }
        let settings = ivars.engine.settings();
        let record = binding_text(settings, "record");
        let recopy = binding_text(settings, "recopy");
        ivars
            .controls
            .hint
            .setStringValue(&NSString::from_str(&shortcut_hint(
                &record,
                &recopy,
                settings.insert_method(),
            )));

        // The card is the page's "what just happened", and while a take has
        // failed that is the failure -- not the take before it. Reloading is
        // what every navigation into this page does, so without this the
        // menu-bar item that offers to fix the failure would clear the copy of
        // it on the way to the screen that answers it.
        if ivars.problem.borrow().is_some() {
            return;
        }

        let generation = ivars.refresh.invalidate();
        let engine = Arc::clone(&ivars.engine);
        let gate = Arc::clone(&ivars.refresh);
        crate::app::spawn(async move {
            let newest = tokio::task::spawn_blocking(move || {
                if !gate.accepts(generation) {
                    return Ok(Vec::new());
                }
                engine.history(1)
            })
            .await;
            crate::events::on_main(move || {
                crate::app::with_app(|app| {
                    app.with_main(|window| {
                        let page = window.dictate();
                        if page.ivars().refresh.accepts(generation) {
                            if let Ok(Ok(rows)) = newest {
                                page.loaded(rows.into_iter().next());
                            }
                        }
                    });
                });
            });
        });

        self.set_state(ivars.state.get());
    }

    pub fn on_shown(&self) {
        self.ivars().refresh.show();
        self.load();
        self.focus_record();
    }

    pub fn on_hidden(&self) {
        self.ivars().refresh.hide();
    }

    pub fn invalidate(&self) {
        self.ivars().refresh.invalidate();
        self.load();
    }

    fn loaded(&self, newest: Option<openflow_core::db::Transcription>) {
        let ivars = self.ivars();
        match newest {
            Some(row) => {
                let text = row.formatted_text.unwrap_or(row.raw_text);
                self.set_last(&text, "Last transcription · Click the text to copy");
            }
            None => {
                *ivars.last.borrow_mut() = None;
                ivars.controls.result.setTitle(&NSString::from_str(
                    "Your next transcription appears here. You can copy it without switching apps.",
                ));
                ivars.controls.result.setEnabled(false);
                ivars
                    .controls
                    .result_caption
                    .setStringValue(&NSString::from_str("Your words, within reach"));
            }
        }

        // The panel copy names the insert method too, so redraw whatever state
        // the page is in rather than leaving yesterday's sentence up.
        self.set_state(ivars.state.get());
    }

    /// Report a failure on the result card, and offer the screen that answers
    /// it. `target` is a [`openflow_core::engine::EngineEvent::Navigate`] name,
    /// or `None` when there is nowhere useful to go.
    pub fn set_problem(&self, message: &str, target: Option<&str>) {
        let ivars = self.ivars();
        ivars.refresh.invalidate();
        *ivars.problem.borrow_mut() = target.map(str::to_string);
        ivars
            .controls
            .result
            .setTitle(&NSString::from_str(&preview_of(message)));
        // Readable either way; clickable only when the click leads somewhere.
        ivars.controls.result.setEnabled(target.is_some());
        ivars
            .controls
            .result_caption
            .setStringValue(&NSString::from_str(match target {
                Some(_) => "That take did not finish \u{2014} click to fix it",
                None => "That take did not finish",
            }));
    }

    /// Put the card back to the last transcript once the failure is answered.
    pub fn clear_problem(&self) {
        if self.ivars().problem.borrow().is_none() {
            return;
        }
        *self.ivars().problem.borrow_mut() = None;
        self.load();
    }

    /// Show `text` on the result card, with `caption` above it.
    pub fn set_last(&self, text: &str, caption: &str) {
        self.ivars().refresh.invalidate();
        let ivars = self.ivars();
        *ivars.problem.borrow_mut() = None;
        *ivars.last.borrow_mut() = Some(text.to_string());
        ivars
            .controls
            .result
            .setTitle(&NSString::from_str(&preview_of(text)));
        ivars.controls.result.setEnabled(true);
        ivars
            .controls
            .result_caption
            .setStringValue(&NSString::from_str(caption));
    }

    /// The three states the web screen draws, with its copy.
    ///
    /// `Formatting` is folded into `Transcribing` for the same reason the pill
    /// folds it: the pipeline never emits it, and inventing a fourth panel here
    /// would be inventing a state the engine does not have.
    pub fn set_state(&self, state: RecordingState) {
        self.ivars().state.set(state);
        for link in &self.ivars().controls.links {
            link.setEnabled(state == RecordingState::Idle);
        }
        let settings = self.ivars().engine.settings();
        let cleanup = settings.format_enabled();
        let idle_body = idle_body(cleanup, settings.insert_method());
        let transcribing_body = transcribing_body(cleanup, settings.is_local_backend());
        let controls = &self.ivars().controls;
        controls.route.setStringValue(&NSString::from_str(
            processing_route(
                settings.is_local_backend(),
                &settings.provider(),
                effective_cleanup_provider(settings).as_ref(),
                settings.local_only(),
            )
            .as_str(),
        ));
        let (eyebrow, title, body, action, enabled, cancel) = match state {
            RecordingState::Recording => (
                "Listening now",
                "Keep talking\u{2026}",
                "Release to finish. With VoiceOver, press the recording button again to stop.",
                "Release to finish",
                true,
                false,
            ),
            RecordingState::Transcribing | RecordingState::Formatting => (
                "Turning speech into text",
                "One moment\u{2026}",
                transcribing_body,
                "Transcribing\u{2026}",
                false,
                true,
            ),
            RecordingState::Idle => (
                "A little less typing",
                "Speak a thought.",
                idle_body.as_str(),
                "Hold to record",
                true,
                false,
            ),
        };
        controls
            .eyebrow
            .setStringValue(&NSString::from_str(eyebrow));
        controls.title.setStringValue(&NSString::from_str(title));
        controls.body.setStringValue(&NSString::from_str(body));
        controls.record.setTitle(&NSString::from_str(action));
        controls.record.setEnabled(enabled);
        controls.cancel.setHidden(!cancel);
    }
}

/// One line of the result card, cut so the card keeps its shape.
fn preview_of(text: &str) -> String {
    // Retain at most the visible scalar values plus an ellipsis, not the
    // entire transcript. Scan only until the next normalized scalar proves
    // truncation. Leading/trailing whitespace still needs scanning to preserve
    // split_whitespace().join(" ") semantics exactly, including no trailing
    // separator or ellipsis for an otherwise exactly-at-limit result.
    let mut preview = String::with_capacity(text.len().min(RESULT_CHARS * 4) + 3);
    let mut count = 0;
    let mut separator = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            separator = count != 0;
            continue;
        }
        if separator {
            if count == RESULT_CHARS {
                preview.push('\u{2026}');
                return preview;
            }
            preview.push(' ');
            count += 1;
            separator = false;
        }
        if count == RESULT_CHARS {
            preview.push('\u{2026}');
            return preview;
        }
        preview.push(ch);
        count += 1;
    }
    preview
}

/// The binding for `action` as the recorder spells it. Same helper Settings
/// uses, kept separate rather than shared because the two screens are allowed
/// to disagree about what to say when nothing is bound.
/// "pastes" or "types", because they are not the same promise. Paste sends Cmd+V
/// and takes the clipboard with it; Type sends the characters and never touches
/// it. The Settings page spells the difference out one screen away, so the main
/// screen should not tell every user it pastes.
fn insertion_verb(method: InsertMethod) -> &'static str {
    match method {
        InsertMethod::Paste => "pastes",
        InsertMethod::Type => "types",
    }
}

/// What the idle panel promises will happen when the key comes up.
///
/// Both halves of it are settings: cleanup can be off, and the text can be
/// typed rather than pasted. The line was fixed copy that claimed both, so on a
/// machine with Smart cleanup off and Insert text by set to Type -- a
/// combination the Settings page offers on purpose -- the first sentence a user
/// reads on the main screen described someone else's setup.
fn idle_body(cleanup: bool, method: InsertMethod) -> String {
    let verb = insertion_verb(method);
    if cleanup {
        format!("Release when you\u{2019}re done. OpenFlow cleans it up and {verb} it for you.")
    } else {
        format!("Release when you\u{2019}re done. OpenFlow {verb} it for you.")
    }
}

/// What the waiting panel says is happening, while it happens.
///
/// The same two settings as the idle line, read one panel later. It claimed
/// "Your provider is transcribing and formatting the result" on every machine,
/// which is two claims and both can be false: cleanup can be off, and on the
/// local backend there is no provider in it at all -- the sidecar runs on this
/// Mac, which is the whole promise of that setting. Cleanup is the exception
/// worth spelling out, because it does go to the provider even on the local
/// backend: the sidecar transcribes and nothing else.
fn transcribing_body(cleanup: bool, local: bool) -> &'static str {
    match (local, cleanup) {
        (false, true) => "Your provider is transcribing and formatting the result.",
        (false, false) => "Your provider is transcribing the recording.",
        (true, true) => "OpenFlow is transcribing on this Mac, then your provider cleans it up.",
        (true, false) => "OpenFlow is transcribing on this Mac.",
    }
}

fn binding_text(settings: &openflow_core::settings::Settings, action: &str) -> String {
    settings
        .shortcut(action)
        .map(|shortcut| hotkeys::describe(&shortcut))
        .unwrap_or_else(|_| "Not set".to_string())
}

/// A configured route is not a readiness check. In particular Local only can
/// coexist with a cloud backend selection; the engine correctly blocks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessingDestination {
    OnDevice,
    Loopback,
    CustomServer,
    Hosted,
}

/// Match Engine's effective endpoint, not the separately stored preference:
/// enabling “same provider” leaves that dormant preference in the database.
fn effective_cleanup_provider(settings: &openflow_core::settings::Settings) -> Option<Provider> {
    settings.format_enabled().then(|| {
        if settings.same_provider() {
            settings.provider()
        } else {
            settings.formatting_provider()
        }
    })
}

impl ProcessingDestination {
    fn provider(provider: &Provider) -> Self {
        match provider {
            Provider::Custom { base_url } if is_loopback_url(base_url) => Self::Loopback,
            // Custom endpoints can be LAN, VPN, or hosted. Do not infer privacy
            // from an arbitrary hostname or resolve DNS just to draw the UI.
            Provider::Custom { .. } => Self::CustomServer,
            _ => Self::Hosted,
        }
    }
    fn leaves_machine(self) -> bool {
        matches!(self, Self::CustomServer | Self::Hosted)
    }
    fn label(self) -> &'static str {
        match self {
            Self::OnDevice => "on-device",
            Self::Loopback => "server on this Mac",
            Self::CustomServer => "custom server",
            Self::Hosted => "cloud provider",
        }
    }
}

fn processing_route(
    local: bool,
    audio: &Provider,
    cleanup: Option<&Provider>,
    local_only: bool,
) -> String {
    let audio = if local {
        ProcessingDestination::OnDevice
    } else {
        ProcessingDestination::provider(audio)
    };
    let cleanup = cleanup.map(ProcessingDestination::provider);
    if local_only && audio.leaves_machine() {
        return format!(
            "Setup needed: Local only blocks audio sent to your {}.",
            audio.label()
        );
    }
    if local_only && cleanup.is_some_and(ProcessingDestination::leaves_machine) {
        return "Setup needed: Local only blocks cleanup outside this Mac.".to_string();
    }
    let cleanup = cleanup.map_or("off", ProcessingDestination::label);
    format!("Audio: {} · Text cleanup: {cleanup}", audio.label())
}

fn shortcut_hint(record: &str, recopy: &str, method: InsertMethod) -> String {
    let first = if record == "Not set" {
        "Add a global recording shortcut in Set up OpenFlow.".to_string()
    } else {
        format!("Hold {record} in any app to record.")
    };
    if recopy == "Not set" {
        first
    } else {
        format!("{first}\n{recopy} {} again.", insertion_verb(method))
    }
}

// ── Layout ────────────────────────────────────────────────

/// A bounded document scrolls in short windows instead of squeezing the record
/// action into the result. Only width springs: the reading order stays stable.
fn document_height() -> f64 {
    MARGIN * 2.0 + HEADER_HEIGHT + GAP * 2.0 + RECORDER_HEIGHT + RESULT_HEIGHT
}

const WIDTH: NSAutoresizingMaskOptions = NSAutoresizingMaskOptions::ViewWidthSizable;

/// Underlined native buttons keep navigation visible and keyboard accessible.
fn home_link(mtm: MainThreadMarker, title: &str, tag: isize, frame: NSRect) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setTitle(&NSString::from_str(title));
    button.setTag(tag);
    button.setBordered(false);
    button.setAlignment(NSTextAlignment::Left);
    button.setFocusRingType(NSFocusRingType::Exterior);
    button.setAccessibilityLabel(Some(&NSString::from_str(title)));
    let font = display_font(15.0);
    let ink = NSColor::linkColor();
    let underline = NSNumber::new_i32(1);
    let attributes = NSDictionary::from_slices(
        &[
            unsafe { NSFontAttributeName },
            unsafe { NSForegroundColorAttributeName },
            unsafe { NSUnderlineStyleAttributeName },
        ],
        &[
            &*font as &AnyObject,
            &*ink as &AnyObject,
            &*underline as &AnyObject,
        ],
    );
    // Each key's value has the AppKit-documented font/color/number type.
    let attributed =
        unsafe { NSAttributedString::new_with_attributes(&NSString::from_str(title), &attributes) };
    button.setAttributedTitle(&attributed);
    button
}

fn rule(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSBox> {
    let line = NSBox::initWithFrame(NSBox::alloc(mtm), frame);
    line.setBoxType(NSBoxType::Separator);
    line.setAutoresizingMask(WIDTH);
    line
}

fn build_content(mtm: MainThreadMarker, size: NSSize) -> (Retained<NSView>, Controls) {
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), size),
    );
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);
    scroll.setAutoresizingMask(WIDTH | NSAutoresizingMaskOptions::ViewHeightSizable);
    let document = Flipped::new(
        mtm,
        NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(size.width, document_height()),
        ),
    );
    document.setAutoresizingMask(WIDTH);
    let inner = size.width - MARGIN * 2.0;

    let brand = note(
        mtm,
        "OPENFLOW",
        NSRect::new(NSPoint::new(MARGIN, MARGIN), NSSize::new(inner, 18.0)),
    );
    brand.setFont(Some(&display_font(12.0)));
    brand.setAutoresizingMask(WIDTH);
    let page_title = NSTextField::labelWithString(
        &NSString::from_str("Less typing.\nMore getting on with it."),
        mtm,
    );
    page_title.setFont(Some(&crate::ui::editorial_font(32.0)));
    page_title.setFrame(NSRect::new(
        NSPoint::new(MARGIN, MARGIN + 22.0),
        NSSize::new(inner, 80.0),
    ));
    allow_wrapping(&page_title, inner);
    page_title.setMaximumNumberOfLines(2);
    page_title.setAutoresizingMask(WIDTH);
    let intro = note(
        mtm,
        "Dictate where you work. Keep control of where your words go.",
        NSRect::new(
            NSPoint::new(MARGIN, MARGIN + 106.0),
            NSSize::new(inner, 38.0),
        ),
    );
    intro.setFont(Some(&body_font(14.0)));
    allow_wrapping(&intro, inner);
    intro.setAutoresizingMask(WIDTH);
    document.addSubview(&brand);
    document.addSubview(&page_title);
    document.addSubview(&intro);

    // Two short rows keep the useful links above the recording workspace even
    // in a narrow window. Flexible columns share width changes.
    let mut links = Vec::new();
    let column_width = (inner - GAP) / 2.0;
    for (index, (title, _)) in HOME_LINKS.iter().enumerate() {
        let column = index % 2;
        let link = home_link(
            mtm,
            title,
            index as isize,
            NSRect::new(
                NSPoint::new(
                    MARGIN + column as f64 * (column_width + GAP),
                    MARGIN + 144.0 + (index / 2) as f64 * 28.0,
                ),
                NSSize::new(column_width, 28.0),
            ),
        );
        // Flexible width and the opposite margin share the resize delta.
        link.setAutoresizingMask(
            WIDTH
                | if column == 0 {
                    NSAutoresizingMaskOptions::ViewMaxXMargin
                } else {
                    NSAutoresizingMaskOptions::ViewMinXMargin
                },
        );
        document.addSubview(&link);
        links.push(link);
    }

    let recorder_top = MARGIN + HEADER_HEIGHT + GAP;
    document.addSubview(&rule(
        mtm,
        NSRect::new(
            NSPoint::new(MARGIN, recorder_top - 8.0),
            NSSize::new(inner, 1.0),
        ),
    ));
    let recorder = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(
            NSPoint::new(MARGIN, recorder_top),
            NSSize::new(inner, RECORDER_HEIGHT),
        ),
    );
    recorder.setAutoresizingMask(WIDTH);
    let rect = |top: f64, height: f64| {
        NSRect::new(
            NSPoint::new(0.0, RECORDER_HEIGHT - top - height),
            NSSize::new(inner, height),
        )
    };
    let eyebrow = note(mtm, "", rect(0.0, 18.0));
    // The state title says the same thing more clearly; keep its existing state
    // binding without duplicating the visible status.
    eyebrow.setHidden(true);
    let title = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    title.setFont(Some(&display_font(22.0)));
    title.setFrame(rect(0.0, 32.0));
    title.setAutoresizingMask(WIDTH);
    let body = note(mtm, "", rect(36.0, 44.0));
    body.setFont(Some(&body_font(14.0)));
    allow_wrapping(&body, inner);
    body.setAutoresizingMask(WIDTH);
    let record = HoldButton::new(
        mtm,
        NSRect::new(
            NSPoint::new(0.0, RECORDER_HEIGHT - 88.0 - 48.0),
            NSSize::new(244.0_f64.min(inner), 48.0),
        ),
    );
    record.setBezelStyle(NSBezelStyle::Push);
    record.setFocusRingType(NSFocusRingType::Exterior);
    record.setControlSize(objc2_app_kit::NSControlSize::Large);
    record.setFont(Some(&display_font(17.0)));
    let cancel = crate::ui::button(
        mtm,
        NSRect::new(
            NSPoint::new(0.0, RECORDER_HEIGHT - 142.0 - 28.0),
            NSSize::new(182.0, 28.0),
        ),
        "Cancel transcription",
        0,
    );
    cancel.setHidden(true);
    let hint = note(mtm, "", rect(176.0, 42.0));
    hint.setFont(Some(&body_font(13.0)));
    allow_wrapping(&hint, inner);
    hint.setAutoresizingMask(WIDTH);
    let route = note(mtm, "", rect(228.0, 40.0));
    route.setFont(Some(&body_font(12.0)));
    allow_wrapping(&route, inner);
    route.setAutoresizingMask(WIDTH);
    for child in [
        &*eyebrow as &NSView,
        &title,
        &body,
        &record,
        &cancel,
        &hint,
        &route,
    ] {
        recorder.addSubview(child);
    }
    document.addSubview(&recorder);

    let result_top = recorder_top + RECORDER_HEIGHT + GAP;
    document.addSubview(&rule(
        mtm,
        NSRect::new(
            NSPoint::new(MARGIN, result_top - 8.0),
            NSSize::new(inner, 1.0),
        ),
    ));
    let result_area = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(
            NSPoint::new(MARGIN, result_top),
            NSSize::new(inner, RESULT_HEIGHT),
        ),
    );
    result_area.setAutoresizingMask(WIDTH);
    let result_caption = note(
        mtm,
        "",
        NSRect::new(
            NSPoint::new(0.0, RESULT_HEIGHT - 28.0),
            NSSize::new(inner, 22.0),
        ),
    );
    result_caption.setFont(Some(&display_font(13.0)));
    result_caption.setAutoresizingMask(WIDTH);
    let result = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(
            NSPoint::new(0.0, RESULT_HEIGHT - 40.0 - RESULT_LINES),
            NSSize::new(inner, RESULT_LINES),
        ),
    );
    result.setBordered(false);
    result.setAlignment(NSTextAlignment::Left);
    result.setFocusRingType(NSFocusRingType::Exterior);
    result.setFont(Some(&body_font(15.0)));
    result.setAutoresizingMask(WIDTH);
    if let Some(cell) = result.cell() {
        cell.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByWordWrapping);
        cell.setWraps(true);
    }
    result_area.addSubview(&result_caption);
    result_area.addSubview(&result);
    document.addSubview(&result_area);
    scroll.setDocumentView(Some(&document));
    (
        Retained::into_super(scroll),
        Controls {
            links,
            route,
            eyebrow,
            title,
            body,
            record,
            cancel,
            hint,
            result,
            result_caption,
        },
    )
}

/// Render actual native controls without constructing an engine or accessing
/// settings, credentials, history, network or microphone. Used by UI snapshots.
pub(super) fn preview_views(mtm: MainThreadMarker) -> Vec<(String, Retained<NSView>)> {
    [
        (704.0, 620.0, 704.0, "dictate"),
        (440.0, 410.0, 440.0, "dictate-narrow"),
        (704.0, 740.0, 704.0, "dictate-full"),
        (704.0, 410.0, 440.0, "dictate-resized"),
        (704.0, 410.0, 440.0, "dictate-transcribing"),
    ]
    .into_iter()
    .map(|(width, height, final_width, name)| {
        let (view, controls) = build_content(mtm, NSSize::new(width, height));
        view.setFrameSize(NSSize::new(final_width, height));
        view.layoutSubtreeIfNeeded();
        // Exercise actual AppKit springs, not just separately constructed
        // narrow fixtures. Every link must remain an unobstructed target.
        for row in controls.links.chunks(2) {
            let left = row[0].frame();
            let right = row[1].frame();
            assert!(left.size.width >= 160.0);
            assert!(right.size.width >= 160.0);
            assert!(left.origin.x + left.size.width <= right.origin.x);
            assert!(right.origin.x + right.size.width <= final_width);
        }
        controls
            .eyebrow
            .setStringValue(&NSString::from_str("A little less typing"));
        controls
            .title
            .setStringValue(&NSString::from_str("Speak a thought."));
        controls
            .body
            .setStringValue(&NSString::from_str(&idle_body(false, InsertMethod::Paste)));
        controls
            .record
            .setTitle(&NSString::from_str("Hold to record"));
        controls.hint.setStringValue(&NSString::from_str(
            "Hold Right Option in any app to record.\nCommand + Shift + V pastes again.",
        ));
        controls
            .route
            .setStringValue(&NSString::from_str(&processing_route(
                true,
                &Provider::Groq,
                None,
                true,
            )));
        controls.result_caption.setStringValue(&NSString::from_str(
            "Last transcription · Click the text to copy",
        ));
        controls.result.setTitle(&NSString::from_str(
            "Let's leave a little room to think before we start the next project.",
        ));
        if name == "dictate-transcribing" {
            controls
                .title
                .setStringValue(&NSString::from_str("Turning speech into text."));
            controls.body.setStringValue(&NSString::from_str(
                "You can cancel if this is taking too long.",
            ));
            controls
                .record
                .setTitle(&NSString::from_str("Transcribing…"));
            controls.record.setEnabled(false);
            controls.cancel.setHidden(false);
            for link in &controls.links {
                link.setEnabled(false);
            }
        }
        (name.to_string(), view)
    })
    .collect()
}

impl HoldButton {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HoldButtonIvars::default());
        // SAFETY: `initWithFrame:` is `NSView`'s designated initialiser, which
        // `NSButton` inherits.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

#[cfg(test)]
#[path = "dictate_link_tests.rs"]
mod link_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::ClassType;

    /// Previous production implementation, retained only as an equivalence
    /// oracle and opt-in comparison baseline. No personal transcripts used.
    fn preview_reference(text: &str) -> String {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let preview: String = flat.chars().take(RESULT_CHARS).collect();
        if flat.chars().count() > RESULT_CHARS {
            format!("{preview}\u{2026}")
        } else {
            preview
        }
    }

    #[test]
    fn bounded_preview_matches_reference_at_unicode_and_whitespace_boundaries() {
        let mut cases = vec![
            String::new(),
            " \t\n\r\u{a0}\u{2003}".to_string(),
            "  Hello\tthere\nfriend  ".to_string(),
            "你好\u{3000}世界\u{a0}👩🏽‍💻 e\u{301}".to_string(),
            "a".repeat(1024 * 1024),
            format!("{}word", " \n\t".repeat(2000)),
        ];
        for scalar in ["a", "你", "🦀", "e\u{301}"] {
            for length in [RESULT_CHARS - 1, RESULT_CHARS, RESULT_CHARS + 1] {
                let word = scalar.repeat(length);
                cases.extend([
                    word.clone(),
                    format!("  {word}  \n"),
                    format!("{word}\u{2003}next"),
                    format!("{word}{}", "\u{a0}".repeat(2000)),
                ]);
            }
        }
        for input in cases {
            let actual = preview_of(&input);
            assert_eq!(actual, preview_reference(&input));
            assert!(actual.chars().count() <= RESULT_CHARS + 1);
            assert!(actual.capacity() <= RESULT_CHARS * 4 + 3);
        }
        // Preserve the old boundary behavior: a normalized separator that is
        // the 220th scalar is visible immediately before the ellipsis.
        assert_eq!(
            preview_of(&format!("{}  b", "a".repeat(219))),
            format!("{} …", "a".repeat(219))
        );
        assert_eq!(
            preview_of(&format!("{}  ", "a".repeat(220))),
            "a".repeat(220)
        );
    }

    #[test]
    fn bounded_preview_matches_reference_for_generated_mixed_text() {
        // Deterministic property-style coverage without a new dependency.
        let alphabet = [
            'a', 'z', '你', '界', '🦀', '\u{301}', '\u{200d}', ' ', '\t', '\n', '\r', '\u{a0}',
            '\u{2003}', '\u{3000}',
        ];
        let mut seed = 0x9e37_79b9_u64;
        for case in 0..1000 {
            let mut input = String::new();
            for _ in 0..(case * 17 % 1300) {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                input.push(alphabet[(seed >> 32) as usize % alphabet.len()]);
            }
            assert_eq!(
                preview_of(&input),
                preview_reference(&input),
                "generated case {case}"
            );
        }
    }

    #[test]
    #[ignore = "opt-in synthetic performance comparison; run this exact filter only"]
    fn benchmark_dictate_preview_compare() {
        use std::{hint::black_box, time::Instant};
        let phrase = "A synthetic reference transcript with ordinary words and whitespace. ";
        let long = phrase.repeat((1024_usize * 1024).div_ceil(phrase.len()));
        let long = &long[..1024 * 1024];
        let short = "Let's leave a little room to think before starting the next project.";
        let unicode = "  你好\u{3000}世界\nA small idea 👩🏽‍💻 and café.  ";
        let measure = |f: fn(&str) -> String, input: &str, iterations: usize| {
            let start = Instant::now();
            for _ in 0..iterations {
                black_box(f(black_box(input)));
            }
            start.elapsed().as_nanos() as f64 / iterations as f64
        };
        for (name, input, iterations) in [
            ("ascii-short", short, 10_000),
            ("unicode-short", unicode, 10_000),
            ("transcript-1MiB", long, 10),
        ] {
            assert_eq!(
                black_box(preview_of(input)),
                black_box(preview_reference(input))
            );
            let mut old = Vec::new();
            let mut new = Vec::new();
            // Three unrecorded warmups and 31 alternating-order samples.
            for sample in 0..34 {
                let (before, after) = if sample % 2 == 0 {
                    (
                        measure(preview_reference, input, iterations),
                        measure(preview_of, input, iterations),
                    )
                } else {
                    let after = measure(preview_of, input, iterations);
                    (measure(preview_reference, input, iterations), after)
                };
                if sample >= 3 {
                    old.push(before);
                    new.push(after);
                }
            }
            old.sort_by(f64::total_cmp);
            new.sort_by(f64::total_cmp);
            println!("preview benchmark {name}: bytes={} samples=31 warmups=3 iterations={iterations} old_median_ns={:.1} old_p95_ns={:.1} new_median_ns={:.1} new_p95_ns={:.1} median_speedup={:.2}x", input.len(), old[15], old[29], new[15], new[29], old[15] / new[15]);
        }
    }

    /// Whether `OpenFlowHoldButton` implements `selector` itself rather than
    /// inheriting it. `class_copyMethodList`, which is what this reads, lists
    /// only a class's own methods, so it answers exactly that question without
    /// needing an instance -- and an instance would need the main thread and a
    /// window, which a test does not have.
    fn overrides(selector: objc2::runtime::Sel) -> bool {
        HoldButton::class()
            .instance_methods()
            .iter()
            .any(|method| method.name() == selector)
    }

    /// The button has to answer `AXPress` itself. Inheriting it is not
    /// harmless: `NSButton` answers `AXPress` by asking its cell to
    /// `performClick:`, which sends the button's action, and this button has no
    /// action -- so the press is accepted, reported as handled, and does
    /// nothing. VoiceOver sends nothing else, because its cursor never moves
    /// the first responder and so never reaches the Space key path below, which
    /// leaves the whole Dictate page unusable with VoiceOver on.
    #[test]
    fn the_hold_button_answers_press_itself() {
        assert!(
            overrides(sel!(accessibilityPerformPress)),
            "HoldButton inherits NSButton's accessibilityPerformPress, which \
             clicks a nil action and does nothing"
        );
    }

    /// A press cannot be a hold, so the press has to be spelled out. The
    /// visible title says "Hold to record" and cannot say anything else.
    #[test]
    fn the_press_help_names_both_presses() {
        assert!(
            overrides(sel!(accessibilityHelp)),
            "HoldButton has no accessibilityHelp, so VoiceOver reads only the \
             title, which describes a gesture the press path does not use"
        );
        let help = PRESS_HELP.to_lowercase();
        assert!(help.contains("press to start"), "{PRESS_HELP}");
        assert!(help.contains("press again"), "{PRESS_HELP}");
    }

    /// The press is an addition, not a replacement: the mouse and the keyboard
    /// are still the two gestures that carry a real hold, and a control that
    /// stopped taking first responder would lose the keyboard entirely.
    #[test]
    fn the_hold_button_keeps_the_mouse_and_the_keyboard() {
        for selector in [
            sel!(mouseDown:),
            sel!(mouseUp:),
            sel!(keyDown:),
            sel!(keyUp:),
            sel!(acceptsFirstResponder),
        ] {
            assert!(overrides(selector), "{selector:?} is no longer overridden");
        }
    }

    /// The card shows one line, so newlines and runs of spaces collapse.
    #[test]
    fn the_result_preview_is_one_flat_line() {
        assert_eq!(preview_of("hello  there\nfriend"), "hello there friend");
        assert_eq!(preview_of(""), "");
    }

    /// A long transcription is cut, and the cut is marked.
    #[test]
    fn a_long_result_is_marked_where_it_was_cut() {
        let long = "a".repeat(RESULT_CHARS + 5);
        let preview = preview_of(&long);
        assert_eq!(preview.chars().count(), RESULT_CHARS + 1);
        assert!(preview.ends_with('\u{2026}'));
    }

    /// Neither half of the idle promise is fixed, so neither is claimed when
    /// it is off. The combination that matters is the last one: Smart cleanup
    /// off and Insert text by set to Type is a setup the Settings page offers
    /// on purpose, and the sentence used to describe someone else's.
    #[test]
    fn the_idle_panel_promises_only_what_the_settings_do() {
        assert_eq!(
            idle_body(true, InsertMethod::Paste),
            "Release when you\u{2019}re done. OpenFlow cleans it up and pastes it for you."
        );
        assert_eq!(
            idle_body(false, InsertMethod::Paste),
            "Release when you\u{2019}re done. OpenFlow pastes it for you."
        );
        assert_eq!(
            idle_body(false, InsertMethod::Type),
            "Release when you\u{2019}re done. OpenFlow types it for you."
        );
    }

    /// Paste and Type are different promises about the user's clipboard, which
    /// is why the Settings page spells the difference out. The main screen must
    /// not tell every user it pastes.
    #[test]
    fn the_insertion_verb_follows_the_insert_method() {
        assert_eq!(insertion_verb(InsertMethod::Paste), "pastes");
        assert_eq!(insertion_verb(InsertMethod::Type), "types");
    }

    /// The waiting panel makes the same two claims one panel later, and both
    /// can be false. "Your provider" is the one to watch: on the local backend
    /// the transcription happens on this Mac, and saying otherwise contradicts
    /// the setting the user turned on to stop it leaving.
    #[test]
    fn the_waiting_panel_does_not_name_a_provider_that_is_not_involved() {
        assert!(transcribing_body(true, false).contains("provider"));
        assert!(transcribing_body(false, false).contains("provider"));

        assert!(
            !transcribing_body(false, true).contains("provider"),
            "on-device with cleanup off, no provider sees the take at all"
        );
        // Cleanup is the exception: it goes to the provider even on the local
        // backend, so this one names both.
        let both = transcribing_body(true, true);
        assert!(both.contains("this Mac"), "{both}");
        assert!(both.contains("provider"), "{both}");
    }

    /// Formatting is claimed only when it will happen.
    #[test]
    fn the_waiting_panel_claims_formatting_only_when_cleanup_is_on() {
        for local in [false, true] {
            assert!(
                !transcribing_body(false, local).contains("formatting")
                    && !transcribing_body(false, local).contains("cleans it up"),
                "cleanup is off: {}",
                transcribing_body(false, local)
            );
        }
    }

    #[test]
    fn the_document_preserves_both_cards_in_short_windows() {
        let result_bottom = MARGIN + HEADER_HEIGHT + GAP * 2.0 + RECORDER_HEIGHT + RESULT_HEIGHT;
        assert_eq!(document_height() - result_bottom, MARGIN);
        assert!(
            document_height() > 410.0,
            "short windows must scroll, not compress controls"
        );
        for width in [440.0, 704.0] {
            let available = width - MARGIN * 2.0 - PADDING * 2.0;
            assert!(
                available >= 244.0,
                "record target fits at the narrow snapshot size"
            );
        }
    }

    #[test]
    fn unset_shortcuts_are_not_described_as_working() {
        let hint = shortcut_hint("Not set", "Not set", InsertMethod::Paste);
        assert!(hint.contains("Add a global recording shortcut"));
        assert!(!hint.contains("Not set"));
        assert!(!hint.contains("again"));
        assert!(
            shortcut_hint("Right Option", "Command V", InsertMethod::Type).contains("types again")
        );
    }

    #[test]
    fn processing_route_discloses_audio_text_and_conflicting_privacy_settings() {
        let hosted = Provider::OpenRouter;
        let loopback = Provider::from_str("custom:http://localhost:8080/v1");
        let lan = Provider::from_str("custom:http://192.168.1.5:8080/v1");
        for (provider, destination, blocked) in [
            (&hosted, ProcessingDestination::Hosted, true),
            (&loopback, ProcessingDestination::Loopback, false),
            (&lan, ProcessingDestination::CustomServer, true),
        ] {
            assert_eq!(ProcessingDestination::provider(provider), destination);
            let guarded_audio = processing_route(false, provider, None, true);
            assert_eq!(guarded_audio.contains("Setup needed"), blocked);
            let guarded_cleanup = processing_route(true, &hosted, Some(provider), true);
            assert_eq!(guarded_cleanup.contains("Setup needed"), blocked);
            let unguarded = processing_route(false, provider, Some(provider), false);
            assert!(unguarded.contains(destination.label()));
            assert!(!unguarded.contains("blocks"));
            assert!(!unguarded.contains("Ready"));
        }
        assert_eq!(
            processing_route(true, &hosted, Some(&loopback), true),
            "Audio: on-device · Text cleanup: server on this Mac"
        );
        assert_eq!(
            processing_route(false, &loopback, None, true),
            "Audio: server on this Mac · Text cleanup: off"
        );
        assert!(processing_route(false, &lan, None, false).contains("custom server"));
        assert!(!processing_route(false, &lan, None, false).contains("cloud"));
    }

    #[test]
    fn effective_cleanup_follows_same_provider_over_dormant_stored_endpoint() {
        use openflow_core::{db::Database, secrets::SecretStore, settings::Settings};
        let dir =
            std::env::temp_dir().join(format!("openflow-dictate-route-{}", uuid::Uuid::new_v4()));
        let settings = Settings::new(
            Database::new(dir.clone()).unwrap(),
            SecretStore::new(dir.clone()),
        );
        settings.set("provider", "groq").unwrap();
        settings
            .set("formatting_provider", "custom:http://localhost:8080/v1")
            .unwrap();
        settings.set("format_enabled", "true").unwrap();
        for (same, destination, blocked) in [
            ("true", ProcessingDestination::Hosted, true),
            ("false", ProcessingDestination::Loopback, false),
        ] {
            settings.set("same_provider", same).unwrap();
            let cleanup = effective_cleanup_provider(&settings).expect("cleanup enabled");
            assert_eq!(ProcessingDestination::provider(&cleanup), destination);
            let route = processing_route(true, &settings.provider(), Some(&cleanup), true);
            assert_eq!(
                route.contains("Setup needed"),
                blocked,
                "same_provider={same}: {route}"
            );
        }
        // The reverse conflict is equally important: a dormant hosted endpoint
        // cannot imply text leaves this Mac while the shared endpoint is local.
        settings
            .set("provider", "custom:http://127.0.0.1:8080/v1")
            .unwrap();
        settings.set("formatting_provider", "openrouter").unwrap();
        settings.set("same_provider", "true").unwrap();
        assert_eq!(
            ProcessingDestination::provider(&effective_cleanup_provider(&settings).unwrap()),
            ProcessingDestination::Loopback
        );
        settings.set("same_provider", "false").unwrap();
        assert_eq!(
            ProcessingDestination::provider(&effective_cleanup_provider(&settings).unwrap()),
            ProcessingDestination::Hosted
        );
        settings.set("format_enabled", "false").unwrap();
        assert!(effective_cleanup_provider(&settings).is_none());
        drop(settings);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn custom_record_action_retains_native_focus_ring_and_accessibility() {
        for selector in [
            sel!(drawRect:),
            sel!(drawFocusRingMask),
            sel!(focusRingMaskBounds),
            sel!(accessibilityPerformPress),
            sel!(acceptsFirstResponder),
        ] {
            assert!(overrides(selector), "{selector:?} must remain implemented");
        }
    }

    /// Exactly at the limit nothing is cut, so no ellipsis is added.
    #[test]
    fn a_result_at_the_limit_is_not_marked() {
        let exact = "b".repeat(RESULT_CHARS);
        assert_eq!(preview_of(&exact), exact);
    }
}
