//! The rounded card that content is grouped into, and the margins around it.
//!
//! Ventura's System Settings is the reference the main window is aiming at, and
//! the difference from what this app drew before is one rule: nothing touches
//! the window frame. The window's own background shows through a margin on
//! every side, and each group of controls sits on a rounded, slightly raised
//! panel floating on top of it. The older idiom -- a bordered `NSBox`, a
//! bezelled scroll view flush against the frame, a tab strip riding on a
//! rectangle's edge -- draws a line exactly where this draws a gap. That is the
//! whole visual difference, and it is why `NSTabView` is not used here.
//!
//! The fill is drawn rather than set on a layer. A `CGColor` handed to
//! `layer.backgroundColor` is resolved once, at the moment it is set, so a card
//! built in light mode keeps its light fill after the user switches to dark.
//! Drawing in `drawRect:` reads the semantic colour every time AppKit asks for
//! a redraw, and AppKit asks on an appearance change, so both modes come out
//! right with nothing to switch on.

use objc2::rc::Retained;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBezierPath, NSColor, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize};

/// Gap between the content pane's edge and the cards inside it. The "left and
/// right both leave room" rule, in one number.
pub const MARGIN: f64 = 24.0;
/// Vertical gap between two stacked cards.
pub const GAP: f64 = 16.0;
/// Gap between a card's own edge and the controls inside it.
pub const PADDING: f64 = 20.0;
/// Corner radius for the larger content groups; native controls retain their
/// own smaller radii.
pub const RADIUS: f64 = 12.0;

define_class!(
    // SAFETY: `NSView` is designed for subclassing, this class adds no ivars
    // and implements no Drop, and every method below is one AppKit already
    // defines with this signature.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowCard"]
    pub struct Card;

    impl Card {
        /// Fill, then hairline. The border is what keeps the card visible when
        /// its fill and the window's background are nearly the same value,
        /// which is the case in dark mode.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            // Inset by half the line width so the stroke lands inside the
            // view. Stroking on the boundary puts half of it outside and
            // clips it to a quarter-point smear on a Retina display.
            let rect = NSRect::new(
                NSPoint::new(bounds.origin.x + 0.5, bounds.origin.y + 0.5),
                NSSize::new(
                    (bounds.size.width - 1.0).max(0.0),
                    (bounds.size.height - 1.0).max(0.0),
                ),
            );
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                rect, RADIUS, RADIUS,
            );
            NSColor::controlBackgroundColor().setFill();
            path.fill();
            NSColor::separatorColor().setStroke();
            path.setLineWidth(1.0);
            path.stroke();
        }
    }
);

define_class!(
    // SAFETY: `NSView` is designed for subclassing, this class adds no ivars
    // and implements no Drop, and `isFlipped` is a property AppKit already
    // defines with this signature.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowFlippedView"]
    pub struct Flipped;

    impl Flipped {
        /// Origin at the top left. A scroll view shows the origin of its
        /// document first, so an unflipped document opens on its bottom --
        /// the end of a settings page rather than the start of it.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl Flipped {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `initWithFrame:` is `NSView`'s designated initialiser.
        unsafe { msg_send![this, initWithFrame: frame] }
    }
}

impl Card {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: `initWithFrame:` is `NSView`'s designated initialiser.
        let this: Retained<Self> = unsafe { msg_send![this, initWithFrame: frame] };
        this
    }
}
