//! The overlay pill: a borderless `NSPanel` holding one custom-drawn view.
//!
//! Geometry, colours and animation are ported from `overlay.html` rather than
//! reinvented, so the native build and the Tauri build look identical: 28 px
//! tall, 28 / 82 / 72 px wide for idle / recording / transcribing, the same
//! `rgba(26,19,50,0.94)` body, the same per-position corner radii, the same ten
//! waveform bars and three pulsing dots.
//!
//! The only thing that ticks is one `NSTimer` at 30 Hz, and it exists only
//! while the pill is animating. On idle it is invalidated, which is what keeps
//! the app at the measured idle floor.

use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSEvent, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSPanel, NSScreen, NSStringDrawing, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{
    NSDictionary, NSPoint, NSRect, NSRunLoop, NSRunLoopCommonModes, NSSize, NSString, NSTimer,
};

use openflow_core::audio::{meter_decay, meter_fraction};
use openflow_core::engine::{Engine, RecordingState};

// ── Geometry, ported from overlay.html ────────────────────

/// The pill is always this tall; only its width changes.
pub const OVERLAY_HEIGHT: f64 = 28.0;
pub const WIDTH_IDLE: f64 = 28.0;
pub const WIDTH_RECORDING: f64 = 82.0;
pub const WIDTH_TRANSCRIBING: f64 = 72.0;
/// Wide enough for a line of transcript while the recording is still running.
pub const WIDTH_PREVIEW: f64 = 320.0;
/// The corner radius `overlay.html` uses on whichever corners face the screen.
pub const CORNER_RADIUS: f64 = 10.0;

/// The eight anchors, as fractions of the work area in the HTML's coordinate
/// space: x from left, y from *top*.
pub const POSITIONS: &[(&str, f64, f64)] = &[
    ("top-left", 0.0, 0.0),
    ("top-center", 0.5, 0.0),
    ("top-right", 1.0, 0.0),
    ("left-center", 0.0, 0.5),
    ("right-center", 1.0, 0.5),
    ("bottom-left", 0.0, 1.0),
    ("bottom-center", 0.5, 1.0),
    ("bottom-right", 1.0, 1.0),
];

pub fn is_known_position(name: &str) -> bool {
    POSITIONS.iter().any(|(known, _, _)| *known == name)
}

/// Where the screen the pill was left on is recorded.
///
/// Its own setting rather than a second field inside `overlay_position`,
/// because that value is what the Settings popup matches its menu items
/// against: anything but one of the eight anchor names selects nothing there,
/// and `is_known_position` would reject every stored value.
pub const OVERLAY_SCREEN: &str = "overlay_screen";

/// A name for one screen, stable for as long as the arrangement is.
///
/// The full frame, not the visible one: the visible frame moves when the Dock
/// is hidden or the menu bar changes height, and a name that changed for those
/// would send the pill home for no reason. Rounded, because a screen's frame
/// is whole points in practice and an exact float comparison would be a name
/// that sometimes does not match itself.
///
/// Not the display ID. `NSScreenNumber` is assigned by the window server and
/// is not promised to be the same after a display is unplugged and replugged,
/// which is precisely when this has to still work.
///
/// Through `i64` rather than formatting the rounded float, because `f64` has a
/// negative zero and `-0.0` prints as `-0`. A screen sitting on the origin with
/// a hair of negative jitter would then be named `-0` once and `0` the next
/// time, and a name that does not match itself reads as "that display is gone".
/// The test below is what found that.
pub fn screen_name(frame: Rect) -> String {
    format!(
        "{},{},{},{}",
        frame.x.round() as i64,
        frame.y.round() as i64,
        frame.width.round() as i64,
        frame.height.round() as i64
    )
}

/// Which of this machine's screens the pill should open on.
///
/// `None` means "no opinion", and every caller reads that as the screen macOS
/// would have chosen anyway. There are two ways to get it and they are both
/// deliberate:
///
/// - nothing stored -- an install from before the pill remembered anything,
///   which must behave exactly as it did before;
/// - stored, but no screen on this machine answers to that name -- the display
///   was unplugged, or the arrangement changed under it. Putting the pill back
///   where it was is not possible then, and the one outcome that must never
///   happen is placing it on a screen that is not there, where it would be a
///   recording indicator the user cannot see.
pub fn screen_for_name(stored: Option<&str>, screens: &[Rect]) -> Option<usize> {
    let stored = stored?;
    screens
        .iter()
        .position(|frame| screen_name(*frame) == stored)
}

/// A rectangle in AppKit screen coordinates: origin bottom-left, y upwards.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// What the pill shows once a dictation is over, for as long as it holds.
///
/// Not a [`RecordingState`]: the engine has no such state, and inventing one
/// would put a display concern into a state machine two hosts share. It is
/// driven by the result and error events instead, exactly as `overlay.html`
/// drives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Error,
}

impl Outcome {
    /// Seconds the badge stays up. Failure holds longer because it is the one
    /// worth catching, and the text is on the clipboard either way.
    pub fn hold(self) -> f64 {
        match self {
            Self::Done => 1.2,
            Self::Error => 2.2,
        }
    }
}

/// The pill's width, given everything that can widen it.
///
/// An outcome badge keeps the resting footprint, so nothing moves on the way
/// out; a reading of the recording in progress needs room for a line of text.
pub fn pill_width(state: RecordingState, outcome: Option<Outcome>, previewing: bool) -> f64 {
    if outcome.is_some() {
        return WIDTH_IDLE;
    }
    if previewing && matches!(state, RecordingState::Recording) {
        return WIDTH_PREVIEW;
    }
    width_for(state)
}

/// Whether an outcome badge may take the pill over in this state.
///
/// A `TranscriptionResult` or `TranscriptionError` for the *previous* take can
/// arrive while the next capture is already recording -- hold the key down
/// again while a take is still in flight and it always does. Painting the badge
/// then stops the waveform and snaps the pill to its resting width for the
/// whole hold, and the badge's own timer restores the width without restarting
/// the animation, so the recording pill sits there frozen. The notification
/// still fires; the pill just keeps showing the capture that is live.
pub fn badge_may_show(state: RecordingState) -> bool {
    !matches!(state, RecordingState::Recording)
}

/// Whether the pill has something moving in it, and so needs its 30 Hz timer.
///
/// Read by both the state change and the badge's settle, so a badge that came
/// and went during a capture cannot leave the waveform stopped.
pub fn animates_in(state: RecordingState) -> bool {
    matches!(
        state,
        RecordingState::Recording | RecordingState::Transcribing
    )
}

pub fn width_for(state: RecordingState) -> f64 {
    match state {
        RecordingState::Recording => WIDTH_RECORDING,
        RecordingState::Transcribing => WIDTH_TRANSCRIBING,
        // `Formatting` is never emitted; if a future host does emit it, resting
        // width is the honest answer rather than a fourth shape nobody drew.
        _ => WIDTH_IDLE,
    }
}

/// Where the pill's bottom-left corner goes, for a window `width` wide inside
/// `visible` (the screen minus menu bar and Dock).
///
/// `overlay.html` measures y downwards from the top of the work area; AppKit
/// measures upwards from the bottom, so the y fraction is inverted here and
/// nowhere else.
pub fn anchor_for(position: &str, width: f64, visible: Rect) -> (f64, f64) {
    let (_, fx, fy) = POSITIONS
        .iter()
        .find(|(name, _, _)| *name == position)
        .copied()
        .unwrap_or(("left-center", 0.0, 0.5));
    let span_x = (visible.width - width).max(0.0);
    let span_y = (visible.height - OVERLAY_HEIGHT).max(0.0);
    let x = visible.x + fx * span_x;
    let y = visible.y + (1.0 - fy) * span_y;
    (x, y)
}

/// The anchor a pill dropped at `origin` is closest to, by the same normalized
/// distance `overlay.html` uses when a drag ends.
pub fn nearest_position(origin: (f64, f64), width: f64, visible: Rect) -> &'static str {
    let span_x = (visible.width - width).max(1.0);
    let span_y = (visible.height - OVERLAY_HEIGHT).max(1.0);
    let px = (origin.0 - visible.x) / span_x;
    // Back to the HTML's top-down y before comparing with the table.
    let py = 1.0 - (origin.1 - visible.y) / span_y;

    let mut nearest = "left-center";
    let mut best = f64::INFINITY;
    for (name, fx, fy) in POSITIONS {
        let distance = ((px - fx).powi(2) + (py - fy).powi(2)).sqrt();
        if distance < best {
            best = distance;
            nearest = name;
        }
    }
    nearest
}

/// Corner radii in visual order: top-left, top-right, bottom-right, bottom-left,
/// exactly as the `border-radius` shorthand in `overlay.html` writes them.
pub fn corner_radii(position: &str) -> [f64; 4] {
    let r = CORNER_RADIUS;
    match position {
        "top-left" => [0.0, 0.0, r, 0.0],
        "top-center" => [0.0, 0.0, r, r],
        "top-right" => [0.0, 0.0, 0.0, r],
        "right-center" => [r, 0.0, 0.0, r],
        "bottom-left" => [0.0, r, 0.0, 0.0],
        "bottom-center" => [r, r, 0.0, 0.0],
        "bottom-right" => [r, 0.0, 0.0, 0.0],
        // left-center is the default, and the default's radii are its own.
        _ => [0.0, r, r, 0.0],
    }
}

// ── Animation ─────────────────────────────────────────────
//
// The dots are still the CSS keyframes, ported. The waveform is not: it is the
// microphone. See `Waveform`.

/// One bar per reading on screen at once.
const WAVE_BARS: usize = 10;
const WAVE_BAR_WIDTH: f64 = 2.5;
const WAVE_BAR_GAP: f64 = 2.0;
/// The waveform's own width: one bar per reading, with a gap between each pair.
/// Derived rather than written down twice, so the text that sits after the bars
/// cannot drift away from where they actually end.
const WAVEFORM_WIDTH: f64 =
    WAVE_BAR_WIDTH * WAVE_BARS as f64 + WAVE_BAR_GAP * (WAVE_BARS as f64 - 1.0);
/// Ticks between shifts. At 30 Hz three ticks is a new bar every 100 ms, so the
/// ten on screen are the last second of speech: quick enough to feel live, slow
/// enough that a syllable is a bar rather than a blur.
const WAVE_STEP_TICKS: usize = 3;
/// The gap either side of a run of content inside the pill.
const PILL_PADDING: f64 = 6.0;
/// How much of a clipped line is faded out at its leading edge.
const FADE_WIDTH: f64 = 16.0;
const WAVE_MIN_HEIGHT: f64 = 4.0;
const WAVE_MAX_HEIGHT: f64 = 16.0;
const DOT_DELAYS: [f64; 3] = [0.0, 0.15, 0.3];
const DOT_PERIOD: f64 = 0.8;

/// `ease-in-out` on a 0..1 progress, which is what the CSS timing function
/// approximates closely enough at 30 Hz.
fn ease_in_out(t: f64) -> f64 {
    0.5 - 0.5 * (PI * t).cos()
}

/// What the bars have heard, and what they are currently drawing.
///
/// The bars used to be a clock. `overlay.html` animates them with ten CSS
/// keyframes on ten `animation-delay`s, and the port reproduced that faithfully
/// -- so the waveform danced identically whether the microphone was live, muted
/// or never granted, which is exactly the moment the pill is the only thing the
/// user can see. It is the microphone now.
///
/// Pure and deterministic: readings in, heights out, no AppKit. The shape of
/// the wave is testable without a microphone or a screen.
#[derive(Default)]
pub struct Waveform {
    /// Newest at the end. The wave travels leftwards, the way every other
    /// waveform display does.
    heard: [f64; WAVE_BARS],
    /// What is drawn, which lags `heard`: see `sample`.
    shown: [f64; WAVE_BARS],
    /// The loudest reading since the last shift. A bar covers three ticks, so
    /// without this two readings in three would be dropped -- and the one kept
    /// would be whichever the clock happened to land on rather than the loudest
    /// thing said in that window.
    pending: f64,
    ticks: usize,
}

impl Waveform {
    /// One 30 Hz tick at `level`, 0.0 to 1.0.
    pub fn sample(&mut self, level: f64) {
        self.pending = self.pending.max(level.clamp(0.0, 1.0));
        self.ticks += 1;
        if self.ticks >= WAVE_STEP_TICKS {
            self.heard.rotate_left(1);
            self.heard[WAVE_BARS - 1] = self.pending;
            self.pending = 0.0;
            self.ticks = 0;
        }
        // Each bar rises to its reading at once and eases down towards it, so a
        // peak keeps its shape as it travels instead of flickering between the
        // ten-a-second shifts. The same curve the core meter falls on.
        for (shown, heard) in self.shown.iter_mut().zip(self.heard) {
            *shown = meter_decay(*shown as f32, heard as f32) as f64;
        }
    }

    /// Back to a flat line, for the start of the next recording.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Bar heights in points, in drawing order.
    pub fn heights(&self) -> [f64; WAVE_BARS] {
        self.shown
            .map(|level| WAVE_MIN_HEIGHT + (WAVE_MAX_HEIGHT - WAVE_MIN_HEIGHT) * level)
    }
}

/// Opacity of transcribing dot `index` at `elapsed` seconds: 0.25 at the ends
/// of the cycle, 1.0 in the middle.
pub fn dot_opacity(index: usize, elapsed: f64) -> f64 {
    let delay = DOT_DELAYS[index % DOT_DELAYS.len()];
    let phase = (elapsed + delay).rem_euclid(DOT_PERIOD) / DOT_PERIOD;
    0.25 + 0.75
        * ease_in_out(if phase <= 0.5 {
            phase * 2.0
        } else {
            (1.0 - phase) * 2.0
        })
}

// ── The view ──────────────────────────────────────────────

pub struct PillIvars {
    state: Cell<RecordingState>,
    position: RefCell<String>,
    /// Seconds since the current animation started.
    elapsed: Cell<f64>,
    /// Where the pointer sat inside the window when a drag began.
    drag_offset: Cell<Option<NSPoint>>,
    /// Set for as long as a badge is up; overrides the state's drawing.
    outcome: Cell<Option<Outcome>>,
    /// The latest reading of the recording in progress, replaced whole.
    partial: RefCell<String>,
    /// True once the readings have stopped but the line is still worth showing.
    partial_held: Cell<bool>,
    /// What the bars are drawing, fed from the microphone on every tick.
    waveform: RefCell<Waveform>,
    engine: RefCell<Option<Arc<Engine>>>,
}

define_class!(
    // SAFETY: NSView permits subclassing and this class overrides only drawing
    // and mouse handling. It has no Drop impl.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowPillView"]
    #[ivars = PillIvars]
    struct PillView;

    impl PillView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = { event.locationInWindow() };
            self.ivars().drag_offset.set(Some(point));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let Some(offset) = self.ivars().drag_offset.get() else {
                return;
            };
            let Some(window) = self.window() else { return };
            let in_window = { event.locationInWindow() };
            let frame = window.frame();
            let origin = NSPoint::new(
                frame.origin.x + in_window.x - offset.x,
                frame.origin.y + in_window.y - offset.y,
            );
            window.setFrameOrigin(origin);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            if self.ivars().drag_offset.take().is_none() {
                return;
            }
            self.settle();
        }
    }
);

impl PillView {
    fn new(mtm: MainThreadMarker, position: String) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PillIvars {
            state: Cell::new(RecordingState::Idle),
            position: RefCell::new(position),
            elapsed: Cell::new(0.0),
            drag_offset: Cell::new(None),
            outcome: Cell::new(None),
            partial: RefCell::new(String::new()),
            partial_held: Cell::new(false),
            waveform: RefCell::new(Waveform::default()),
            engine: RefCell::new(None),
        });
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(WIDTH_IDLE, OVERLAY_HEIGHT),
        );
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.setWantsLayer(true);
        this
    }

    /// Snap to the nearest anchor and persist it, the way `finishDrag` does.
    fn settle(&self) {
        let Some(window) = self.window() else { return };
        let Some(visible) = visible_frame(&window) else {
            return;
        };
        let frame = window.frame();
        let nearest = nearest_position((frame.origin.x, frame.origin.y), frame.size.width, visible);
        *self.ivars().position.borrow_mut() = nearest.to_string();
        let (x, y) = anchor_for(nearest, frame.size.width, visible);
        window.setFrameOrigin(NSPoint::new(x, y));
        self.setNeedsDisplay(true);

        if let Some(engine) = self.ivars().engine.borrow().as_ref() {
            let _ = engine.settings().set("overlay_position", nearest);
            // The anchor alone says where on a screen, never which screen. A
            // drag onto a second display used to be forgotten the moment the
            // app was quit, because this is the only place the pill's position
            // is written down and it wrote eight names and nothing else.
            if let Some(screen) = window.screen() {
                let frame = screen.frame();
                let _ = engine.settings().set(
                    OVERLAY_SCREEN,
                    &screen_name(Rect {
                        x: frame.origin.x,
                        y: frame.origin.y,
                        width: frame.size.width,
                        height: frame.size.height,
                    }),
                );
            }
        }
    }

    fn draw(&self) {
        let bounds = self.bounds();
        let state = self.ivars().state.get();
        let outcome = self.ivars().outcome.get();
        let position = self.ivars().position.borrow().clone();
        let elapsed = self.ivars().elapsed.get();

        // Body: rgba(26,19,50,0.94) with a 1 px border whose colour is the
        // state's accent, inset by half the stroke so the stroke stays inside.
        let inset = NSRect::new(
            NSPoint::new(bounds.origin.x + 0.5, bounds.origin.y + 0.5),
            NSSize::new(
                (bounds.size.width - 1.0).max(0.0),
                (bounds.size.height - 1.0).max(0.0),
            ),
        );
        let path = rounded_path(inset, corner_radii(&position));
        {
            NSColor::colorWithSRGBRed_green_blue_alpha(
                26.0 / 255.0,
                19.0 / 255.0,
                50.0 / 255.0,
                0.94,
            )
            .set();
        }
        path.fill();
        let (br, bg, bb, ba) = match (outcome, state) {
            (Some(Outcome::Done), _) => (34.0, 197.0, 94.0, 0.55),
            (Some(Outcome::Error), _) => (239.0, 68.0, 68.0, 0.6),
            (None, RecordingState::Recording) => (239.0, 68.0, 68.0, 0.5),
            (None, RecordingState::Transcribing) => (123.0, 163.0, 201.0, 0.4),
            _ => (255.0, 255.0, 255.0, 0.06),
        };
        {
            NSColor::colorWithSRGBRed_green_blue_alpha(br / 255.0, bg / 255.0, bb / 255.0, ba)
                .set();
        }
        path.setLineWidth(1.0);
        path.stroke();

        // A badge stands in for the logo and the animation both: the pill is
        // back at its resting width, so there is room for one glyph and no more.
        if let Some(outcome) = outcome {
            let size = 14.0;
            let frame = NSRect::new(
                NSPoint::new(
                    (bounds.size.width - size) / 2.0,
                    (bounds.size.height - size) / 2.0,
                ),
                NSSize::new(size, size),
            );
            let (r, g, b) = match outcome {
                Outcome::Done => (34.0, 197.0, 94.0),
                Outcome::Error => (239.0, 68.0, 68.0),
            };
            {
                NSColor::colorWithSRGBRed_green_blue_alpha(r / 255.0, g / 255.0, b / 255.0, 1.0)
                    .set();
            }
            draw_badge(outcome, frame);
            return;
        }

        // Logo: 14x14, centred when idle, 6 px from the left edge otherwise.
        let logo_size = 14.0;
        let logo_x = if matches!(
            state,
            RecordingState::Recording | RecordingState::Transcribing
        ) {
            6.0
        } else {
            (bounds.size.width - logo_size) / 2.0
        };
        let logo_y = (bounds.size.height - logo_size) / 2.0;
        let (lr, lg, lb, la) = match state {
            RecordingState::Recording => (239.0, 68.0, 68.0, 1.0),
            RecordingState::Transcribing => (123.0, 163.0, 201.0, 1.0),
            _ => (255.0, 255.0, 255.0, 0.7),
        };
        {
            NSColor::colorWithSRGBRed_green_blue_alpha(lr / 255.0, lg / 255.0, lb / 255.0, la)
                .set();
        }
        draw_microphone(NSRect::new(
            NSPoint::new(logo_x, logo_y),
            NSSize::new(logo_size, logo_size),
        ));

        match state {
            RecordingState::Recording => self.draw_waveform(bounds, logo_x + logo_size),
            RecordingState::Transcribing => self.draw_dots(bounds, logo_x + logo_size, elapsed),
            _ => {}
        }

        // The transcript, in whatever room the waveform left.
        let partial = self.ivars().partial.borrow();
        if !partial.is_empty() && matches!(state, RecordingState::Recording) {
            let text_x = logo_x + logo_size + PILL_PADDING + WAVEFORM_WIDTH + PILL_PADDING;
            draw_partial(
                &partial,
                NSRect::new(
                    NSPoint::new(text_x, bounds.origin.y),
                    NSSize::new(
                        (bounds.size.width - text_x - PILL_PADDING).max(0.0),
                        bounds.size.height,
                    ),
                ),
                self.ivars().partial_held.get(),
            );
        }
    }

    /// Ten 2.5 px bars, 2 px apart, between 4 px and 16 px tall -- the same
    /// geometry `overlay.html` draws, with the heights coming from what was
    /// heard rather than from a clock.
    fn draw_waveform(&self, bounds: NSRect, after_logo: f64) {
        {
            NSColor::colorWithSRGBRed_green_blue_alpha(
                239.0 / 255.0,
                68.0 / 255.0,
                68.0 / 255.0,
                1.0,
            )
            .set();
        }
        let bar_width = WAVE_BAR_WIDTH;
        let gap = WAVE_BAR_GAP;
        let mut x = after_logo + PILL_PADDING;
        for height in self.ivars().waveform.borrow().heights() {
            let y = (bounds.size.height - height) / 2.0;
            let bar = NSRect::new(NSPoint::new(x, y), NSSize::new(bar_width, height));
            let path = { NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bar, 1.25, 1.25) };
            path.fill();
            x += bar_width + gap;
        }
    }

    /// Three 4 px dots, 3 px apart, pulsing between 0.25 and 1.0 opacity.
    fn draw_dots(&self, bounds: NSRect, after_logo: f64, elapsed: f64) {
        let diameter = 4.0;
        let gap = 3.0;
        let mut x = after_logo + 6.0;
        let y = (bounds.size.height - diameter) / 2.0;
        for index in 0..DOT_DELAYS.len() {
            {
                NSColor::colorWithSRGBRed_green_blue_alpha(
                    123.0 / 255.0,
                    163.0 / 255.0,
                    201.0 / 255.0,
                    dot_opacity(index, elapsed),
                )
                .set();
            }
            let dot = NSRect::new(NSPoint::new(x, y), NSSize::new(diameter, diameter));
            { NSBezierPath::bezierPathWithOvalInRect(dot) }.fill();
            x += diameter + gap;
        }
    }
}

/// The mic glyph from `overlay.html`, drawn as primitives rather than parsed
/// from the SVG path: a capsule for the body, an arc for the pickup ring, and a
/// stem. At 14 px the two are indistinguishable, and this needs no path parser.
/// One line of transcript, anchored to the right of `rect` so the words being
/// spoken now stay on screen while the beginning scrolls out of it.
///
/// `overlay.html` clips with `overflow: hidden` and softens the cut with a CSS
/// mask. There is no mask here: the longest suffix that fits is measured and
/// drawn, and a short gradient over its left edge says the same thing -- there
/// is more text than there is pill.
fn draw_partial(text: &str, rect: NSRect, held: bool) {
    if text.is_empty() || rect.size.width <= 1.0 {
        return;
    }
    let font = NSFont::systemFontOfSize(11.0);
    // Held text is dimmed: a line that stopped updating and a line whose
    // transcriber died are the same picture otherwise.
    let alpha = if held { 0.45 } else { 0.92 };
    let color = NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, alpha);
    let attributes = NSDictionary::from_slices(
        &[unsafe { NSFontAttributeName }, unsafe {
            NSForegroundColorAttributeName
        }],
        &[&*font as &AnyObject, &*color as &AnyObject],
    );
    let width_of = |candidate: &str| -> f64 {
        let string = NSString::from_str(candidate);
        unsafe { string.sizeWithAttributes(Some(&attributes)) }.width
    };

    // Longest suffix that fits, by binary search over character boundaries. The
    // starts are searched rather than the ends: it is the tail that must
    // survive.
    let starts: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let (mut low, mut high) = (0usize, starts.len());
    while low < high {
        let middle = (low + high) / 2;
        if width_of(&text[starts[middle]..]) <= rect.size.width {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    let shown = &text[starts.get(low).copied().unwrap_or(text.len())..];
    if shown.is_empty() {
        return;
    }

    let line_height = font.ascender() - font.descender();
    let origin = NSPoint::new(
        rect.origin.x,
        rect.origin.y + (rect.size.height - line_height) / 2.0 - font.descender(),
    );
    let string = NSString::from_str(shown);
    unsafe { string.drawAtPoint_withAttributes(origin, Some(&attributes)) };

    // Something was dropped, so fade the edge it was dropped from. Painted as
    // columns of the body colour rather than an `NSGradient`, which would need
    // its own object per draw for a band this narrow.
    if low > 0 {
        let band = FADE_WIDTH.min(rect.size.width);
        let columns = band.ceil() as usize;
        for column in 0..columns {
            let x = column as f64;
            let opacity = 1.0 - x / band;
            {
                NSColor::colorWithSRGBRed_green_blue_alpha(
                    26.0 / 255.0,
                    19.0 / 255.0,
                    50.0 / 255.0,
                    opacity,
                )
                .set();
            }
            let strip = NSRect::new(
                NSPoint::new(rect.origin.x + x, rect.origin.y),
                NSSize::new(1.0, rect.size.height),
            );
            NSBezierPath::fillRect(strip);
        }
    }
}

/// The check and the cross from `overlay.html`, on the same 24-unit grid the
/// microphone uses: `M4 12.5l5.5 5.5L20 7` and `M6 6l12 12M18 6L6 18`, stroked
/// round rather than filled.
fn draw_badge(outcome: Outcome, frame: NSRect) {
    let unit = frame.size.width / 24.0;
    let x = |v: f64| frame.origin.x + v * unit;
    let y = |v: f64| frame.origin.y + (24.0 - v) * unit;

    let path = NSBezierPath::new();
    path.setLineWidth(2.6 * unit);
    path.setLineCapStyle(objc2_app_kit::NSLineCapStyle::Round);
    path.setLineJoinStyle(objc2_app_kit::NSLineJoinStyle::Round);
    match outcome {
        Outcome::Done => {
            path.moveToPoint(NSPoint::new(x(4.0), y(12.5)));
            path.lineToPoint(NSPoint::new(x(9.5), y(18.0)));
            path.lineToPoint(NSPoint::new(x(20.0), y(7.0)));
        }
        Outcome::Error => {
            path.moveToPoint(NSPoint::new(x(6.0), y(6.0)));
            path.lineToPoint(NSPoint::new(x(18.0), y(18.0)));
            path.moveToPoint(NSPoint::new(x(18.0), y(6.0)));
            path.lineToPoint(NSPoint::new(x(6.0), y(18.0)));
        }
    }
    path.stroke();
}

fn draw_microphone(frame: NSRect) {
    let unit = frame.size.width / 24.0;
    let x = |v: f64| frame.origin.x + v * unit;
    let y = |v: f64| frame.origin.y + (24.0 - v) * unit;

    // Body: rounded rect from (9,1) to (15,13), radius 3.
    let body = NSRect::new(
        NSPoint::new(x(9.0), y(13.0)),
        NSSize::new(6.0 * unit, 12.0 * unit),
    );
    let capsule =
        { NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(body, 3.0 * unit, 3.0 * unit) };
    capsule.fill();

    // Pickup ring: the open U from (5,10) round to (19,10), 2 px thick.
    let ring = NSBezierPath::new();
    {
        ring.setLineWidth(2.0 * unit);
        ring.moveToPoint(NSPoint::new(x(4.0), y(10.0)));
        // Increasing angles, so the four-argument append's counter-clockwise
        // default is the short way round here; this one is right as it stands.
        ring.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle(
            NSPoint::new(x(12.0), y(12.0)),
            8.0 * unit,
            180.0,
            360.0,
        );
    }
    ring.stroke();

    // Stem and base.
    let stem = NSRect::new(
        NSPoint::new(x(11.0), y(23.0)),
        NSSize::new(2.0 * unit, 3.0 * unit),
    );
    { NSBezierPath::bezierPathWithRect(stem) }.fill();
}

/// A rounded rectangle with per-corner radii, in visual order (top-left,
/// top-right, bottom-right, bottom-left). `NSBezierPath`'s own rounded-rect
/// helper only takes one radius, and every position in `overlay.html` rounds a
/// different pair of corners.
///
/// The walk is clockwise -- along the top left to right, down the right side,
/// back along the bottom, up the left -- which is what puts the four radii in
/// the visual order `corner_radii` writes them in, and matches the CSS
/// shorthand this is ported from. So every arc has to say `clockwise: true`.
/// The four-argument `appendBezierPathWithArcWithCenter:...:endAngle:` defaults
/// to counter-clockwise, and in an unflipped view (this one) that turned the
/// top-right arc from 90 degrees to 0 into the 270-degree way round: each
/// corner came out as a concave scoop instead of a round-off. Reversing the
/// walk instead would fix the arcs and leave the radii array indexed
/// backwards, so the direction is stated here rather than in the caller.
fn rounded_path(rect: NSRect, radii: [f64; 4]) -> Retained<NSBezierPath> {
    let [tl, tr, br, bl] = radii;
    let left = rect.origin.x;
    let bottom = rect.origin.y;
    let right = left + rect.size.width;
    let top = bottom + rect.size.height;
    let path = NSBezierPath::new();
    {
        path.moveToPoint(NSPoint::new(left + tl, top));
        path.lineToPoint(NSPoint::new(right - tr, top));
        if tr > 0.0 {
            path.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
                NSPoint::new(right - tr, top - tr),
                tr,
                90.0,
                0.0,
                true,
            );
        }
        path.lineToPoint(NSPoint::new(right, bottom + br));
        if br > 0.0 {
            path.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
                NSPoint::new(right - br, bottom + br),
                br,
                0.0,
                -90.0,
                true,
            );
        }
        path.lineToPoint(NSPoint::new(left + bl, bottom));
        if bl > 0.0 {
            path.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
                NSPoint::new(left + bl, bottom + bl),
                bl,
                270.0,
                180.0,
                true,
            );
        }
        path.lineToPoint(NSPoint::new(left, top - tl));
        if tl > 0.0 {
            path.appendBezierPathWithArcWithCenter_radius_startAngle_endAngle_clockwise(
                NSPoint::new(left + tl, top - tl),
                tl,
                180.0,
                90.0,
                true,
            );
        }
        path.closePath();
    }
    path
}

/// Every screen attached right now, in AppKit's order.
///
/// The only enumeration of screens in this crate. `visible_frame` below asks
/// one window which screen it is on; this asks what the choices are, which is
/// the question that has to be answered before the panel is placed at all.
fn screen_frames(mtm: MainThreadMarker) -> Vec<Rect> {
    NSScreen::screens(mtm)
        .iter()
        .map(|screen| {
            let frame = screen.frame();
            Rect {
                x: frame.origin.x,
                y: frame.origin.y,
                width: frame.size.width,
                height: frame.size.height,
            }
        })
        .collect()
}

/// The visible area of the screen at `index`, for placing the panel onto it.
fn visible_frame_of(mtm: MainThreadMarker, index: usize) -> Option<Rect> {
    let screens = NSScreen::screens(mtm);
    let screen = screens.iter().nth(index)?;
    let frame = screen.visibleFrame();
    Some(Rect {
        x: frame.origin.x,
        y: frame.origin.y,
        width: frame.size.width,
        height: frame.size.height,
    })
}

fn visible_frame(window: &NSWindow) -> Option<Rect> {
    let screen = window
        .screen()
        .or_else(|| MainThreadMarker::new().and_then(NSScreen::mainScreen))?;
    let frame = screen.visibleFrame();
    Some(Rect {
        x: frame.origin.x,
        y: frame.origin.y,
        width: frame.size.width,
        height: frame.size.height,
    })
}

// ── The panel ─────────────────────────────────────────────

pub struct Overlay {
    panel: Retained<NSPanel>,
    view: Retained<PillView>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    /// A one-shot that takes the outcome badge back down.
    badge_timer: RefCell<Option<Retained<NSTimer>>>,
    state: Cell<RecordingState>,
    engine: Arc<Engine>,
}

impl Overlay {
    pub fn new(engine: &Arc<Engine>, mtm: MainThreadMarker) -> Self {
        let mut position = engine.settings().overlay_position();
        if !is_known_position(&position) {
            position = "left-center".to_string();
        }
        // A database that cannot be read is not a screen: `Err` and "no such
        // row" both mean the pill opens where it always did.
        let remembered_screen = engine.settings().get(OVERLAY_SCREEN).ok().flatten();

        let content = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(WIDTH_IDLE, OVERLAY_HEIGHT),
        );
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel = {
            NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                content,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // Above every ordinary window, on every Space, and never a full-screen
        // app's problem. `NSStatusWindowLevel` is 25.
        panel.setLevel(25);
        panel.setOpaque(false);
        panel.setHasShadow(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        panel.setIgnoresMouseEvents(false);
        panel.setMovableByWindowBackground(false);
        panel.setFloatingPanel(true);
        panel.setHidesOnDeactivate(false);

        let view = PillView::new(mtm, position.clone());
        *view.ivars().engine.borrow_mut() = Some(Arc::clone(engine));
        panel.setContentView(Some(&view));

        let overlay = Self {
            panel,
            view,
            timer: RefCell::new(None),
            badge_timer: RefCell::new(None),
            state: Cell::new(RecordingState::Idle),
            engine: Arc::clone(engine),
        };
        // Onto the remembered screen *before* the first snap, because `snap`
        // anchors within whichever screen the panel is already on -- and a
        // panel built at the content origin is on the main one. Nothing to
        // restore, or a screen that is no longer here, leaves it exactly where
        // it would have been.
        if let Some(index) = screen_for_name(remembered_screen.as_deref(), &screen_frames(mtm)) {
            if let Some(visible) = visible_frame_of(mtm, index) {
                overlay
                    .panel
                    .setFrameOrigin(NSPoint::new(visible.x, visible.y));
            }
        }
        overlay.snap(WIDTH_IDLE, false);
        overlay.panel.orderFrontRegardless();
        overlay
    }

    /// Read the hide-when-idle setting fresh, like `overlay.html` does, so the
    /// toggle takes effect the moment it is saved.
    pub fn apply_visibility_setting(&self) {
        let hide_when_idle = self.engine.settings().overlay_only_while_recording();
        let idle = matches!(self.state.get(), RecordingState::Idle);
        if hide_when_idle && idle {
            self.panel.orderOut(None);
        } else {
            self.panel.orderFrontRegardless();
        }
    }

    /// Move the pill to whichever anchor the settings window just chose.
    pub fn set_position(&self, position: &str) {
        if !is_known_position(position) {
            return;
        }
        *self.view.ivars().position.borrow_mut() = position.to_string();
        self.snap(self.panel.frame().size.width, false);
        self.view.setNeedsDisplay(true);
    }

    pub fn set_state(&self, state: RecordingState) {
        let previous = self.state.get();
        self.state.set(state);
        self.view.ivars().state.set(state);

        // The result event arrives just before the `idle` that follows it. Let
        // the badge's own timer close that out; taking the pill down here would
        // erase the outcome in the frame it appeared.
        if matches!(state, RecordingState::Idle) && self.view.ivars().outcome.get().is_some() {
            return;
        }
        self.clear_badge();
        if !matches!(state, RecordingState::Recording) {
            self.clear_partial();
        }
        if previous != state {
            self.view.ivars().elapsed.set(0.0);
        }

        let previewing = !self.view.ivars().partial.borrow().is_empty();
        self.snap(pill_width(state, None, previewing), previous != state);
        self.view.setNeedsDisplay(true);

        if animates_in(state) {
            self.start_animating();
        } else {
            self.stop_animating();
        }
        self.apply_visibility_setting();
    }

    /// Show how the dictation ended, and hold it long enough to be read.
    ///
    /// Always visible, even under hide-when-idle: the badge exists precisely to
    /// be seen without switching to the app the text was meant for.
    ///
    /// Skipped entirely while a capture is running; see [`badge_may_show`]. The
    /// caller has already notified either way, so nothing is lost but the dot.
    pub fn show_outcome(&self, outcome: Outcome) {
        if !badge_may_show(self.state.get()) {
            return;
        }
        self.clear_partial();
        self.view.ivars().outcome.set(Some(outcome));
        self.stop_animating();
        // Same footprint as idle, so the pill does not jump on its way out.
        self.snap(WIDTH_IDLE, true);
        self.view.setNeedsDisplay(true);
        self.panel.orderFrontRegardless();

        let block = block2::RcBlock::new(move |_timer: core::ptr::NonNull<NSTimer>| {
            crate::app::with_app(|app| app.overlay().settle_outcome());
        });
        let timer =
            unsafe { NSTimer::timerWithTimeInterval_repeats_block(outcome.hold(), false, &block) };
        unsafe {
            NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        }
        *self.badge_timer.borrow_mut() = Some(timer);
    }

    /// Take the badge down and let the pill rest, or carry on if a new capture
    /// started while it was up.
    pub fn settle_outcome(&self) {
        self.clear_badge();
        let state = self.state.get();
        let previewing = !self.view.ivars().partial.borrow().is_empty();
        self.snap(pill_width(state, None, previewing), true);
        self.view.setNeedsDisplay(true);
        // The width comes back on its own; the animation does not. A capture
        // that started while the badge was up would otherwise get its width
        // restored and its waveform left stopped.
        if animates_in(state) {
            self.start_animating();
        }
        self.apply_visibility_setting();
    }

    /// Show the latest reading of the recording in progress.
    ///
    /// `held` means the readings have stopped but the line still stands; it is
    /// drawn dimmed, because a preview that stopped tracking and one whose
    /// transcriber died look alike otherwise.
    pub fn set_partial(&self, text: &str, held: bool) {
        // A reading that arrives after the key came up has nothing to say about
        // a pill that is already transcribing or resting.
        if !matches!(self.state.get(), RecordingState::Recording) {
            return;
        }
        let widen = self.view.ivars().partial.borrow().is_empty();
        self.view
            .ivars()
            .partial
            .borrow_mut()
            .replace_range(.., text);
        self.view.ivars().partial_held.set(held);
        if widen {
            self.snap(WIDTH_PREVIEW, true);
        }
        self.view.setNeedsDisplay(true);
    }

    fn clear_partial(&self) {
        self.view.ivars().partial.borrow_mut().clear();
        self.view.ivars().partial_held.set(false);
    }

    fn clear_badge(&self) {
        self.view.ivars().outcome.set(None);
        if let Some(timer) = self.badge_timer.borrow_mut().take() {
            timer.invalidate();
        }
    }

    /// Resize and re-anchor. `animated` runs it inside an `NSAnimationContext`
    /// so the width change eases the way the CSS transition does, without
    /// blocking the main thread the way `setFrame:display:animate:` would.
    fn snap(&self, width: f64, animated: bool) {
        let Some(visible) = visible_frame(&self.panel) else {
            return;
        };
        let position = self.view.ivars().position.borrow().clone();
        let (x, y) = anchor_for(&position, width, visible);
        let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(width, OVERLAY_HEIGHT));
        if animated {
            crate::ui::animate(0.2, || {
                let animator: Retained<NSPanel> = unsafe { msg_send![&*self.panel, animator] };
                animator.setFrame_display(frame, true);
            });
        } else {
            self.panel.setFrame_display(frame, true);
        }
    }

    /// One 30 Hz timer, and only while something is moving.
    fn start_animating(&self) {
        if self.timer.borrow().is_some() {
            return;
        }
        let view = self.view.clone();
        let block = block2::RcBlock::new(move |_timer: core::ptr::NonNull<NSTimer>| {
            let ivars = view.ivars();
            ivars.elapsed.set(ivars.elapsed.get() + 1.0 / 30.0);
            // Only while there are bars to feed. Transcribing animates too, and
            // the pill draws dots then; asking the recorder how loud it is when
            // it has already stopped would sample a level that is pinned at
            // zero and scroll a flat line across a waveform nobody is drawing.
            if ivars.state.get() == RecordingState::Recording {
                let level = ivars
                    .engine
                    .borrow()
                    .as_ref()
                    .map(|engine| meter_fraction(engine.input_level()))
                    .unwrap_or(0.0);
                ivars.waveform.borrow_mut().sample(level as f64);
            }
            view.setNeedsDisplay(true);
        });
        // Built unscheduled and added in the common modes, not scheduled in the
        // default mode: a timer in the default mode alone stops firing while a
        // menu is tracking, so opening the menu bar item mid-recording froze
        // the waveform until the menu closed.
        let timer =
            unsafe { NSTimer::timerWithTimeInterval_repeats_block(1.0 / 30.0, true, &block) };
        unsafe {
            NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        }
        *self.timer.borrow_mut() = Some(timer);
    }

    fn stop_animating(&self) {
        if let Some(timer) = self.timer.borrow_mut().take() {
            timer.invalidate();
        }
        self.view.ivars().elapsed.set(0.0);
        // Flat again, or the next recording opens on the tail of the last one.
        self.view.ivars().waveform.borrow_mut().reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect {
        x: 0.0,
        y: 25.0,
        width: 1440.0,
        height: 875.0,
    };

    /// This Mac's built-in display and an external one beside it, in the
    /// arrangement `NSScreen::screens` reports: the main screen is always at
    /// the origin, and the others sit around it.
    const BUILT_IN: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 1728.0,
        height: 1117.0,
    };
    const EXTERNAL: Rect = Rect {
        x: 1728.0,
        y: 0.0,
        width: 2048.0,
        height: 858.0,
    };

    /// A screen answers to its own name.
    ///
    /// Trivial on its own, and here because everything below is a comparison
    /// of two names: if naming were not reflexive, the pill would go home
    /// every launch and every one of these tests would still pass.
    #[test]
    fn a_screen_answers_to_its_own_name() {
        let screens = [BUILT_IN, EXTERNAL];
        for (index, screen) in screens.iter().enumerate() {
            assert_eq!(
                screen_for_name(Some(&screen_name(*screen)), &screens),
                Some(index)
            );
        }
    }

    /// The pill opens on the screen it was left on.
    ///
    /// The regression: the only thing ever written down about the pill's
    /// position was one of eight anchor names, which says where on a screen
    /// and never which screen. A user who dragged it to their second display
    /// found it back on the built-in one after every restart.
    #[test]
    fn the_pill_opens_on_the_screen_it_was_left_on() {
        assert_eq!(
            screen_for_name(Some(&screen_name(EXTERNAL)), &[BUILT_IN, EXTERNAL]),
            Some(1),
            "left on the external display, opened on the built-in one"
        );
    }

    /// A screen that is not there any more sends the pill home.
    ///
    /// The one outcome that must never happen: the pill is the only indication
    /// that a recording is running, and placed on a display that was unplugged
    /// it is an indicator the user cannot see. "Somewhere visible" beats
    /// "where it was".
    #[test]
    fn a_screen_that_is_gone_sends_the_pill_home() {
        assert_eq!(
            screen_for_name(Some(&screen_name(EXTERNAL)), &[BUILT_IN]),
            None,
            "the external display was unplugged and the pill went with it"
        );
        assert_eq!(
            screen_for_name(Some("nonsense"), &[BUILT_IN, EXTERNAL]),
            None,
            "a value this cannot read is not a screen"
        );
    }

    /// An install that never recorded a screen behaves exactly as it did.
    #[test]
    fn nothing_remembered_is_no_opinion() {
        assert_eq!(screen_for_name(None, &[BUILT_IN, EXTERNAL]), None);
        assert_eq!(screen_for_name(Some(""), &[BUILT_IN, EXTERNAL]), None);
    }

    /// Two identical displays are told apart.
    ///
    /// A name built from the size alone would match the wrong one, and the
    /// pair of matching monitors is the arrangement where a person is most
    /// likely to care which of them it is on.
    #[test]
    fn two_identical_displays_are_told_apart() {
        let left = Rect {
            x: 0.0,
            y: 0.0,
            width: 2560.0,
            height: 1440.0,
        };
        let right = Rect { x: 2560.0, ..left };
        assert_ne!(screen_name(left), screen_name(right));
        assert_eq!(
            screen_for_name(Some(&screen_name(right)), &[left, right]),
            Some(1)
        );
    }

    /// The name survives the sub-point jitter a float frame can carry.
    ///
    /// An exact float comparison would be a name that does not always match
    /// itself, which reads as "the display is gone" and puts the pill back on
    /// the built-in screen for no reason the user can see.
    #[test]
    fn the_name_does_not_move_with_sub_point_jitter() {
        let jittered = Rect {
            x: EXTERNAL.x + 0.0001,
            y: EXTERNAL.y - 0.0001,
            ..EXTERNAL
        };
        assert_eq!(screen_name(jittered), screen_name(EXTERNAL));
    }

    /// The screen is recorded under its own key, not inside the anchor.
    ///
    /// `overlay_position` is what the Settings popup matches its menu items
    /// against. A value carrying a screen in it would select nothing there and
    /// fail `is_known_position`, so the pill would also lose its anchor -- a
    /// second bug, in a window the user is looking at.
    #[test]
    fn the_screen_is_not_stored_in_the_anchor() {
        assert_ne!(OVERLAY_SCREEN, "overlay_position");
        for screen in [BUILT_IN, EXTERNAL] {
            assert!(
                !is_known_position(&screen_name(screen)),
                "a screen name is not an anchor, so the two cannot share a key"
            );
        }
    }

    /// Both ends of the wire, checked in this file.
    ///
    /// Everything above is about a *decision*. None of it is about the
    /// decision being made: every test here still passes with the write
    /// removed from `settle`, or the lookup removed from `Overlay::new`, and
    /// either of those is the whole bug back. Neither end can be reached from
    /// a test -- one needs a dragged window, the other an `NSApplication` --
    /// so what is checked is that the calls are there, the way the status-row
    /// test reads its own call sites.
    #[test]
    fn both_ends_of_the_wire_are_connected() {
        let source = include_str!("overlay.rs");
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("the code above the tests");

        // Declared, read when the pill opens, written when it is dropped.
        let mentions = code.matches("OVERLAY_SCREEN").count();
        assert!(
            mentions >= 3,
            "OVERLAY_SCREEN appears {mentions} times outside the tests: it has \
             to be declared, read when the panel is placed, and written when a \
             drag settles. Fewer means one end of the wire is loose and the \
             pill forgets its screen again"
        );
        // Counted, not `contains`. Every one of these names its own
        // definition, so `contains` is satisfied by the function existing and
        // says nothing about it being called -- which is exactly the forgery
        // this test has to catch. The first version used `contains` and stayed
        // green with the call removed from `Overlay::new`.
        let asked = code.matches("screen_for_name(").count();
        assert!(
            asked >= 2,
            "screen_for_name appears {asked} times outside the tests -- its \
             definition and nothing else. Nothing asks which screen to open \
             on, so the panel is placed wherever a window at the content \
             origin lands"
        );
        let named = code.matches("screen_name(").count();
        assert!(
            named >= 3,
            "screen_name appears {named} times outside the tests: it has to be \
             defined, used to match a stored name, and used to write one down \
             when a drag settles"
        );
        let enumerated = code.matches("screen_frames(").count();
        assert!(
            enumerated >= 2,
            "screen_frames appears {enumerated} times outside the tests, so \
             nothing ever asks what the choices are"
        );
    }

    /// The badge keeps the resting footprint on purpose: an outcome that
    /// resized the pill would move it at the moment the user is reading it.
    #[test]
    fn an_outcome_badge_never_widens_the_pill() {
        for outcome in [Outcome::Done, Outcome::Error] {
            for state in [
                RecordingState::Idle,
                RecordingState::Recording,
                RecordingState::Transcribing,
            ] {
                assert_eq!(pill_width(state, Some(outcome), false), WIDTH_IDLE);
                // Even with a reading on screen: the badge replaced it.
                assert_eq!(pill_width(state, Some(outcome), true), WIDTH_IDLE);
            }
        }
    }

    /// And a reading only widens the shape that has room to show one.
    #[test]
    fn only_a_running_recording_widens_for_a_reading() {
        assert_eq!(
            pill_width(RecordingState::Recording, None, true),
            WIDTH_PREVIEW
        );
        assert_eq!(
            pill_width(RecordingState::Recording, None, false),
            WIDTH_RECORDING
        );
        // Transcribing and idle keep their own widths whatever is left over:
        // the line is cleared when the capture ends, and a wide empty pill
        // would sit there through the whole transcription.
        assert_eq!(
            pill_width(RecordingState::Transcribing, None, true),
            WIDTH_TRANSCRIBING
        );
        assert_eq!(pill_width(RecordingState::Idle, None, true), WIDTH_IDLE);
    }

    /// A result for the previous take routinely lands while the next capture is
    /// already running -- hold the key again before a take comes back and it
    /// always does. The badge must not take that pill over: it would stop the
    /// waveform, and the settle that follows only restores the width.
    #[test]
    fn a_badge_never_interrupts_a_live_recording() {
        assert!(!badge_may_show(RecordingState::Recording));
        assert!(badge_may_show(RecordingState::Idle));
        assert!(badge_may_show(RecordingState::Transcribing));

        // And whatever the badge did to the timer, settling reads the same rule
        // the state change does, so the pill it hands back is still animating.
        assert!(animates_in(RecordingState::Recording));
        assert!(animates_in(RecordingState::Transcribing));
        assert!(!animates_in(RecordingState::Idle));
        assert!(!animates_in(RecordingState::Formatting));
    }

    /// The pill's rounded corners must round *away* material, not scoop it out.
    ///
    /// The walk is clockwise but the four-argument arc append is
    /// counter-clockwise by default, so 90 degrees to 0 was drawn the
    /// 270-degree way and every rounded corner came out concave. Nothing in a
    /// width or an anchor catches that; only the geometry does.
    #[test]
    fn rounded_corners_cut_material_away_rather_than_scooping_it_out() {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(100.0, 28.0));
        // left-center's radii: square down the screen edge, round on the two
        // corners that face into the desktop.
        let path = rounded_path(rect, [0.0, CORNER_RADIUS, CORNER_RADIUS, 0.0]);

        let (left, bottom) = (rect.origin.x, rect.origin.y);
        let (right, top) = (left + rect.size.width, bottom + rect.size.height);

        // A pixel inside each rounded corner is outside the shape.
        for point in [
            NSPoint::new(right - 1.0, top - 1.0),
            NSPoint::new(right - 1.0, bottom + 1.0),
        ] {
            assert!(
                !path.containsPoint(point),
                "({}, {}) is in a rounded corner and must be outside the pill",
                point.x,
                point.y
            );
        }
        // The same pixel in each square corner is inside it.
        for point in [
            NSPoint::new(left + 1.0, top - 1.0),
            NSPoint::new(left + 1.0, bottom + 1.0),
        ] {
            assert!(
                path.containsPoint(point),
                "({}, {}) is in a square corner and must be inside the pill",
                point.x,
                point.y
            );
        }

        // The pill's whole right-hand end must still be solid. This is what an
        // arc drawn the long way round actually costs: the 270-degree sweep
        // loops back through the body, and the winding it leaves behind hollows
        // out the 20 px between the two corners it was meant to round. The
        // corner pixels above read as outside either way, which is why they are
        // not enough on their own.
        // Inboard of where each arc begins, so these sit under the straight
        // top and bottom edges and are body by construction, not corner.
        let inboard = right - (CORNER_RADIUS + 2.0);
        for point in [
            NSPoint::new(inboard, top - 2.0),
            NSPoint::new(inboard, bottom + 2.0),
        ] {
            assert!(
                path.containsPoint(point),
                "({}, {}) is body, not corner, and must be inside the pill",
                point.x,
                point.y
            );
        }

        // And the shape stays inside the frame it was asked for: an arc drawn
        // the wrong way round can also bulge past the edge it was rounding.
        let bounds = path.bounds();
        assert!(
            (bounds.origin.x - left).abs() < 0.01
                && (bounds.origin.y - bottom).abs() < 0.01
                && (bounds.size.width - rect.size.width).abs() < 0.01
                && (bounds.size.height - rect.size.height).abs() < 0.01,
            "path bounds {:?} are not the rect it was built from",
            bounds
        );
    }

    /// Failure holds longer than success, and both hold long enough to read.
    #[test]
    fn failure_stays_up_longer_than_success() {
        assert!(Outcome::Error.hold() > Outcome::Done.hold());
        assert!(Outcome::Done.hold() >= 1.0);
    }

    /// The eight anchors have to land on the edges of the *work area*, not the
    /// screen: a pill under the menu bar or behind the Dock is unreachable.
    #[test]
    fn anchors_sit_on_the_work_area_edges() {
        // left-center: hard left, vertically centred.
        let (x, y) = anchor_for("left-center", WIDTH_IDLE, SCREEN);
        assert_eq!(x, 0.0);
        assert_eq!(y, 25.0 + (875.0 - OVERLAY_HEIGHT) / 2.0);

        // top-left is the *top* of the work area in AppKit's upward y.
        let (x, y) = anchor_for("top-left", WIDTH_IDLE, SCREEN);
        assert_eq!(x, 0.0);
        assert_eq!(y, 25.0 + 875.0 - OVERLAY_HEIGHT);

        // bottom-right is the origin corner of the work area, minus the width.
        let (x, y) = anchor_for("bottom-right", WIDTH_RECORDING, SCREEN);
        assert_eq!(x, 1440.0 - WIDTH_RECORDING);
        assert_eq!(y, 25.0);

        // top-center splits the horizontal span.
        let (x, y) = anchor_for("top-center", WIDTH_TRANSCRIBING, SCREEN);
        assert_eq!(x, (1440.0 - WIDTH_TRANSCRIBING) / 2.0);
        assert_eq!(y, 25.0 + 875.0 - OVERLAY_HEIGHT);

        // An anchor nobody defined falls back to the shipped default rather
        // than to (0,0), which would hide the pill behind the Dock.
        assert_eq!(
            anchor_for("nowhere", WIDTH_IDLE, SCREEN),
            anchor_for("left-center", WIDTH_IDLE, SCREEN)
        );
    }

    /// Every anchor must round-trip: drop the pill exactly on an anchor and the
    /// drag handler has to name that same anchor back.
    #[test]
    fn every_anchor_is_its_own_nearest() {
        for (name, _, _) in POSITIONS {
            let origin = anchor_for(name, WIDTH_IDLE, SCREEN);
            assert_eq!(
                nearest_position(origin, WIDTH_IDLE, SCREEN),
                *name,
                "{name} should snap back to itself"
            );
        }
    }

    /// And a drop that is not on an anchor has to pick the near one, not just
    /// any one: without this the round-trip above would pass for a function
    /// that always returned the input.
    #[test]
    fn a_drop_near_an_edge_snaps_to_that_edge() {
        // 20 px in from the left, a third of the way down: nearest is left-center.
        let origin = (SCREEN.x + 20.0, SCREEN.y + SCREEN.height * 0.45);
        assert_eq!(nearest_position(origin, WIDTH_IDLE, SCREEN), "left-center");

        // Top-right corner of the work area.
        let origin = (
            SCREEN.x + SCREEN.width - WIDTH_IDLE,
            SCREEN.y + SCREEN.height,
        );
        assert_eq!(nearest_position(origin, WIDTH_IDLE, SCREEN), "top-right");

        // Middle of the bottom edge.
        let origin = (SCREEN.x + (SCREEN.width - WIDTH_IDLE) / 2.0, SCREEN.y);
        assert_eq!(
            nearest_position(origin, WIDTH_IDLE, SCREEN),
            "bottom-center"
        );
    }

    #[test]
    fn widths_match_the_overlay_stylesheet() {
        assert_eq!(width_for(RecordingState::Idle), 28.0);
        assert_eq!(width_for(RecordingState::Recording), 82.0);
        assert_eq!(width_for(RecordingState::Transcribing), 72.0);
        // Never emitted, and it must not invent a fourth width if it ever is.
        assert_eq!(width_for(RecordingState::Formatting), 28.0);
    }

    /// The radii are the `border-radius` shorthand from `overlay.html`, in the
    /// same order, so the pill's rounded corners always face the screen.
    #[test]
    fn corner_radii_face_away_from_the_screen_edge() {
        assert_eq!(corner_radii("left-center"), [0.0, 10.0, 10.0, 0.0]);
        assert_eq!(corner_radii("right-center"), [10.0, 0.0, 0.0, 10.0]);
        assert_eq!(corner_radii("top-left"), [0.0, 0.0, 10.0, 0.0]);
        assert_eq!(corner_radii("top-center"), [0.0, 0.0, 10.0, 10.0]);
        assert_eq!(corner_radii("top-right"), [0.0, 0.0, 0.0, 10.0]);
        assert_eq!(corner_radii("bottom-left"), [0.0, 10.0, 0.0, 0.0]);
        assert_eq!(corner_radii("bottom-center"), [10.0, 10.0, 0.0, 0.0]);
        assert_eq!(corner_radii("bottom-right"), [10.0, 0.0, 0.0, 0.0]);
        assert_eq!(corner_radii("nowhere"), corner_radii("left-center"));
    }

    #[test]
    fn the_dots_stay_inside_the_css_keyframes() {
        for step in 0..90 {
            let elapsed = step as f64 / 30.0;
            for index in 0..3 {
                let opacity = dot_opacity(index, elapsed);
                assert!(
                    (0.25..=1.0).contains(&opacity),
                    "dot {index} at {elapsed}s was {opacity}"
                );
            }
        }
        // They have to actually move, or the assertion above would hold for a
        // constant.
        assert!((dot_opacity(0, 0.0) - dot_opacity(0, 0.4)).abs() > 0.5);
    }

    /// Whatever it is fed, it draws inside the geometry `overlay.html` uses.
    /// A bar taller than the pill would be clipped by the panel, and a
    /// negative one would draw upside down.
    #[test]
    fn the_bars_stay_inside_the_pill_whatever_the_microphone_says() {
        let mut wave = Waveform::default();
        for (step, level) in [0.0, 1.0, 0.5, -3.0, 7.0, f64::MAX, 0.0]
            .into_iter()
            .cycle()
            .take(120)
            .enumerate()
        {
            wave.sample(level);
            for height in wave.heights() {
                assert!(
                    (WAVE_MIN_HEIGHT..=WAVE_MAX_HEIGHT).contains(&height),
                    "step {step} at level {level} drew {height}"
                );
            }
        }
    }

    /// Silence is a flat line. This is the whole point of the change: the bars
    /// used to dance to a clock, so a muted or ungranted microphone looked
    /// exactly like a working one at the only moment the pill is on screen.
    #[test]
    fn silence_is_flat_and_speech_is_not() {
        let mut wave = Waveform::default();
        for _ in 0..60 {
            wave.sample(0.0);
        }
        assert!(wave.heights().iter().all(|h| *h == WAVE_MIN_HEIGHT));

        for _ in 0..60 {
            wave.sample(0.9);
        }
        assert!(wave.heights().iter().any(|h| *h > WAVE_MIN_HEIGHT + 5.0));
    }

    /// One loud moment becomes one tall bar, and that bar walks to the left as
    /// the readings behind it arrive. A waveform that grew everywhere at once
    /// would be a level meter wearing ten bars.
    #[test]
    fn a_peak_travels_leftwards() {
        let mut wave = Waveform::default();
        // Fill the row with silence so the peak is the only thing in it.
        for _ in 0..WAVE_BARS * WAVE_STEP_TICKS {
            wave.sample(0.0);
        }
        for _ in 0..WAVE_STEP_TICKS {
            wave.sample(1.0);
        }
        let loudest = |wave: &Waveform| {
            let heights = wave.heights();
            (0..WAVE_BARS)
                .max_by(|a, b| heights[*a].partial_cmp(&heights[*b]).unwrap())
                .unwrap()
        };
        // It arrives at the right-hand end, where the newest reading goes.
        let first = loudest(&wave);
        assert_eq!(first, WAVE_BARS - 1);

        for _ in 0..WAVE_STEP_TICKS * 3 {
            wave.sample(0.0);
        }
        let later = loudest(&wave);
        assert!(later < first, "peak sat at {first} and then {later}");
    }

    /// A bar is three ticks, and it is the loudest of the three. Keeping
    /// whichever the clock landed on would drop two readings in three and miss
    /// exactly the transients a waveform exists to show.
    #[test]
    fn a_bar_keeps_the_loudest_of_the_ticks_it_covers() {
        let mut loud = Waveform::default();
        let mut quiet = Waveform::default();
        // Same window, same total, but one of them has a spike in it.
        for level in [0.0, 1.0, 0.0] {
            loud.sample(level);
        }
        for _ in 0..WAVE_STEP_TICKS {
            quiet.sample(0.0);
        }
        assert!(loud.heights()[WAVE_BARS - 1] > quiet.heights()[WAVE_BARS - 1]);
    }

    /// The next recording opens on a flat line, not on the tail of the last.
    #[test]
    fn reset_flattens_it() {
        let mut wave = Waveform::default();
        for _ in 0..60 {
            wave.sample(1.0);
        }
        wave.reset();
        assert!(wave.heights().iter().all(|h| *h == WAVE_MIN_HEIGHT));
    }
}
