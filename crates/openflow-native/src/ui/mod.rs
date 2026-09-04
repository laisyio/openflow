//! Stock AppKit controls, laid out by hand.
//!
//! No auto layout anywhere in this crate: frames are shorter to read than
//! constraints and there is nothing to solve at run time. Windows that are a
//! two-column form are a fixed size and set their frames outright; the main
//! window's pages are handed the size of the pane they were given and spring
//! off it with autoresizing masks. `NSSplitViewController` does use auto layout
//! to place its own two panes, but that stops at the pane -- no view here mixes
//! the two models.

pub mod card;
pub mod dictate;
pub mod history;
pub mod main_window;
pub mod onboarding;
pub mod plugins;
pub mod recorder;
pub mod settings;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAnimationContext, NSApplication, NSAutoresizingMaskOptions, NSBezelStyle, NSButton, NSColor,
    NSComboBox, NSControl, NSFont, NSImage, NSImageSymbolConfiguration, NSImageView, NSPopUpButton,
    NSScrollView, NSSecureTextField, NSSwitch, NSTextAlignment, NSTextField, NSTextView, NSView,
    NSWindow, NSWindowCollectionBehavior,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// Height of one form row.
pub const ROW: f64 = 24.0;
/// Height of one line of [`note`] text, measured rather than guessed: a note
/// laid out at any width reports a multiple of this, and the test beside
/// [`Form::status_row`] fails if the font ever stops agreeing.
pub const NOTE_LINE: f64 = 13.0;

/// Vertical gap between rows.
pub const GAP: f64 = 10.0;
/// Width of the label column.
pub const LABEL_WIDTH: f64 = 132.0;
/// Where the control column starts.
pub const CONTROL_X: f64 = LABEL_WIDTH + 10.0;

/// How many lines `text` takes when wrapped into a column `width` wide, at the
/// font [`note`] uses.
///
/// AppKit measures text without a main thread and without an `NSApplication`,
/// so a layout question that used to need the running app -- does this sentence
/// fit the box reserved for it -- is arithmetic a test can do. Measured through
/// `NSAttributedString`, which agrees to the point with what `wrap` gets from a
/// real `NSTextField`.
#[cfg(test)]
pub(crate) fn wrapped_lines(text: &str, width: f64) -> usize {
    use objc2_app_kit::{NSAttributedStringNSExtendedStringDrawing, NSFontAttributeName};
    use objc2_foundation::{NSAttributedString, NSDictionary};

    let font = NSFont::systemFontOfSize(10.0);
    let font: &AnyObject = &font;
    let attributes = NSDictionary::from_slices(&[unsafe { NSFontAttributeName }], &[font]);
    let string = NSString::from_str(text);
    let attributed = unsafe { NSAttributedString::new_with_attributes(&string, &attributes) };
    let height = attributed
        .boundingRectWithSize_options_context(
            NSSize::new(width, f64::MAX),
            objc2_app_kit::NSStringDrawingOptions::UsesLineFragmentOrigin,
            None,
        )
        .size
        .height;
    (height / NOTE_LINE).round() as usize
}

/// Bring `window` forward from a menu bar click, the way the Tauri host does.
///
/// The Tauri equivalent is `WebviewWindow::show()` followed by `set_focus()`,
/// and `set_focus` on macOS ends in `activateIgnoringOtherApps:YES` plus
/// `makeKeyAndOrderFront:`. The first Milestone A build called only
/// `makeKeyAndOrderFront:` and the cooperative `NSApplication::activate()`, and
/// for an `LSUIElement` app invoked from a status item that is not enough: the
/// app is not the active app, so the cooperative activate is declined, and the
/// window opens unseen. Three calls fix it, and each is doing separate work:
///
/// - `MoveToActiveSpace` so the window follows the user to whichever Space is
///   in front. Without it, a window ordered front while another Space is active
///   opens on the Space it was last on, which looks exactly like nothing
///   happening.
/// - `FullScreenAuxiliary` so it can also open over a full-screen app. That is
///   the reachable case here rather than a wrong one: the status item is
///   clickable while another app is full screen, so the window the click asks
///   for has to be able to join that Space. Without it macOS either swaps
///   Spaces under the user or leaves the window behind.
/// - `orderFrontRegardless` for the ordering an inactive app is otherwise
///   refused, and `activateIgnoringOtherApps:` for the activation. Deprecated
///   since macOS 14 in favour of the cooperative `activate`, still functional,
///   and still what a menu bar app needs, which is why it goes through
///   `msg_send!` rather than the deprecated binding.
pub fn present_window(window: &NSWindow, name: &str) {
    crate::trace!("show window={}", name);
    // The Dock icon first, then the window. An accessory app cannot take focus
    // the way a regular one can, so activating before the switch leaves the
    // window on screen but behind whatever the user was already in.
    crate::app::refresh_dock_presence(true);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::MoveToActiveSpace
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: a no-argument-beyond-BOOL void method on NSApplication, sent
        // on the main thread.
        let _: () = unsafe { msg_send![&*app, activateIgnoringOtherApps: true] };
    }
}

/// Order a window out and give the Dock icon back if it was the last one.
///
/// Windows here are hidden rather than closed, so this is what closing means.
/// It is the counterpart to [`present_window`], and both sides have to run
/// through the pair, or the app keeps a Dock icon for a window nobody can see.
pub fn dismiss_window(window: &NSWindow, name: &str) {
    crate::trace!("hide window={}", name);
    window.orderOut(None);
    crate::app::refresh_dock_presence(false);
}

/// Present `sheet` on `parent`, which has to be on screen already.
///
/// The sheet counterpart of [`present_window`], and deliberately shorter: a
/// sheet does not activate anything or join a Space on its own -- it goes
/// wherever its parent is -- and the Dock rule belongs to the parent, which is
/// already accounted for. Presenting a second sheet on a window that has one is
/// a no-op rather than a queue, because the only sheet here is the wizard and
/// asking for it twice means the user clicked twice.
pub fn present_sheet(parent: &NSWindow, sheet: &NSWindow, name: &str) {
    crate::trace!("show sheet={}", name);
    if parent.attachedSheet().is_some() {
        return;
    }
    parent.beginSheet_completionHandler(sheet, None);
}

/// End `sheet`, whoever its parent is. Falls back to ordering out so a sheet
/// that was somehow presented on nothing can still be got rid of.
pub fn dismiss_sheet(sheet: &NSWindow, name: &str) {
    crate::trace!("hide sheet={}", name);
    match sheet.sheetParent() {
        Some(parent) => parent.endSheet(sheet),
        None => sheet.orderOut(None),
    }
}

/// Run `body` inside an animation group of `duration` seconds. Unlike
/// `setFrame:display:animate:` this does not block the main thread, which
/// matters because the hotkey path runs on it.
pub fn animate(duration: f64, body: impl Fn()) {
    let block = block2::RcBlock::new(move |context: core::ptr::NonNull<NSAnimationContext>| {
        // SAFETY: AppKit hands us a live context for the duration of the block.
        unsafe { context.as_ref() }.setDuration(duration);
        body();
    });
    NSAnimationContext::runAnimationGroup(&block);
}

/// A top-down cursor over a form's content view.
pub struct Form {
    pub view: Retained<NSView>,
    y: f64,
    width: f64,
    height: f64,
}

impl Form {
    pub fn new(mtm: MainThreadMarker, width: f64, height: f64) -> Self {
        let view = {
            NSView::initWithFrame(
                NSView::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, height)),
            )
        };
        Self {
            view,
            y: height - 18.0,
            width,
            height,
        }
    }

    /// Shrink the view to the height the rows actually used, and hand it back.
    ///
    /// Rows are placed downwards from the top of a view built at a generous
    /// height, so the leftover is at the bottom -- but the coordinate system
    /// grows upwards, and simply shortening the frame would push the rows out
    /// through the top. Everything moves down by the slack first. Cards need
    /// this: a card is only the right shape if it is as tall as what is in it,
    /// and only the form knows how far down it got.
    pub fn fit(&mut self) -> Retained<NSView> {
        let used = (self.height - self.y).min(self.height);
        let slack = self.height - used;
        if slack > 0.0 {
            for subview in self.view.subviews().iter() {
                let mut frame = subview.frame();
                frame.origin.y -= slack;
                subview.setFrame(frame);
            }
            let origin = self.view.frame().origin;
            self.view
                .setFrame(NSRect::new(origin, NSSize::new(self.width, used)));
            self.height = used;
            self.y = 0.0;
        }
        self.view.clone()
    }

    /// A labelled row: the label frame and the control frame beside it.
    pub fn row(&mut self, height: f64) -> (NSRect, NSRect) {
        self.y -= height;
        let label = NSRect::new(
            NSPoint::new(0.0, self.y + (height - 17.0) / 2.0),
            NSSize::new(LABEL_WIDTH, 17.0),
        );
        let control = NSRect::new(
            NSPoint::new(CONTROL_X, self.y),
            NSSize::new(self.width - CONTROL_X, height),
        );
        self.y -= GAP;
        (label, control)
    }

    /// A row with no label column, spanning the whole width.
    pub fn full(&mut self, height: f64) -> NSRect {
        self.y -= height;
        let frame = NSRect::new(NSPoint::new(0.0, self.y), NSSize::new(self.width, height));
        self.y -= GAP;
        frame
    }

    /// A control column row with nothing in the label column, for a button or a
    /// status line that belongs to the row above it.
    pub fn control_only(&mut self, height: f64) -> NSRect {
        self.y -= height;
        let frame = NSRect::new(
            NSPoint::new(CONTROL_X, self.y),
            NSSize::new(self.width - CONTROL_X, height),
        );
        self.y -= GAP;
        frame
    }

    /// A status line in the control column, `lines` tall and capped there.
    ///
    /// Unlike [`Form::note_row`], which measures the sentence it is given, this
    /// row is built empty and filled later -- with a message chosen at run time,
    /// sometimes by macOS or by a server rather than by us. The card's height is
    /// already fixed by then, so the space has to be reserved up front, and the
    /// only honest thing to do with a message that outgrows it is to say so:
    /// `ByTruncatingTail` ends an over-long line in an ellipsis instead of
    /// letting it run past the bottom edge and vanish without a mark.
    pub fn status_row(&mut self, mtm: MainThreadMarker, lines: usize) -> Retained<NSTextField> {
        let frame = self.control_only(NOTE_LINE * lines as f64);
        self.status_field(mtm, frame, lines)
    }

    /// The same, spanning both columns.
    pub fn status_full(&mut self, mtm: MainThreadMarker, lines: usize) -> Retained<NSTextField> {
        let frame = self.full(NOTE_LINE * lines as f64);
        self.status_field(mtm, frame, lines)
    }

    fn status_field(
        &self,
        mtm: MainThreadMarker,
        frame: NSRect,
        lines: usize,
    ) -> Retained<NSTextField> {
        let field = note(mtm, "", frame);
        allow_wrapping(&field, frame.size.width);
        field.setMaximumNumberOfLines(lines as isize);
        field.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
        self.add(&field);
        field
    }

    /// A wrapped hint under the row above it, as tall as its text needs.
    ///
    /// The fixed-height variant truncated: these sentences are longer than the
    /// control column is wide, and a label that cannot wrap simply loses its
    /// tail -- off the right of a window that, until now, could not even be
    /// widened to read it.
    pub fn note_row(&mut self, mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
        let width = self.width - CONTROL_X;
        let field = note(
            mtm,
            text,
            NSRect::new(NSPoint::new(CONTROL_X, 0.0), NSSize::new(width, 14.0)),
        );
        wrap(&field, width);
        let height = field.frame().size.height;
        self.y -= height;
        field.setFrameOrigin(NSPoint::new(CONTROL_X, self.y));
        self.y -= GAP;
        self.add(&field);
        field
    }

    pub fn add(&self, view: &NSView) {
        self.view.addSubview(view);
    }
}

/// A four-way spring: the horizontal margins keep a block centred as its
/// container widens, and the vertical ones share the extra height out rather
/// than letting it drift to one edge.
pub const CENTRED: NSAutoresizingMaskOptions = NSAutoresizingMaskOptions(
    NSAutoresizingMaskOptions::ViewMinXMargin.0
        | NSAutoresizingMaskOptions::ViewMaxXMargin.0
        | NSAutoresizingMaskOptions::ViewMinYMargin.0
        | NSAutoresizingMaskOptions::ViewMaxYMargin.0,
);

/// The block an empty list shows in place of its rows: a symbol, a headline,
/// and the one line that says what would put something in it.
///
/// It goes *inside* the card, where the rows would have been. Both list pages
/// used to say this under the table instead, which leaves the eye a framed
/// rectangle of nothing with a caption beneath it -- the caption explains the
/// emptiness from outside it, and the emptiness stays the biggest thing on the
/// screen. The system's own empty lists put the sentence where the content
/// would be, and so does this.
///
/// The caller hides and shows it, and may rewrite the two lines: an empty list
/// and a search that found nothing are both empty, and saying so in the same
/// words would lose the difference between them.
pub struct EmptyState {
    pub view: Retained<NSView>,
    headline: Retained<NSTextField>,
    detail: Retained<NSTextField>,
}

impl EmptyState {
    pub fn say(&self, headline: &str, detail: &str) {
        self.headline.setStringValue(&NSString::from_str(headline));
        self.detail.setStringValue(&NSString::from_str(detail));
    }

    pub fn set_hidden(&self, hidden: bool) {
        self.view.setHidden(hidden);
    }
}

pub fn empty_state(
    mtm: MainThreadMarker,
    frame: NSRect,
    symbol: &str,
    headline: &str,
    detail: &str,
) -> EmptyState {
    let holder = NSView::initWithFrame(NSView::alloc(mtm), frame);
    holder.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    const GLYPH: f64 = 38.0;
    const HEADLINE: f64 = 22.0;
    const DETAIL: f64 = 16.0;
    const BLOCK: f64 = GLYPH + 12.0 + HEADLINE + 4.0 + DETAIL;

    let width = frame.size.width;
    // Laid out downwards from the top of a block centred in the frame.
    let mut y = (frame.size.height + BLOCK) / 2.0;

    let glyph = NSImageView::initWithFrame(
        NSImageView::alloc(mtm),
        NSRect::new(
            NSPoint::new((width - GLYPH) / 2.0, y - GLYPH),
            NSSize::new(GLYPH, GLYPH),
        ),
    );
    if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol),
        Some(&NSString::from_str(headline)),
    ) {
        glyph.setImage(Some(&image));
        // `NSFontWeight` is a bare `CGFloat`; 0.0 is the regular weight.
        glyph.setSymbolConfiguration(Some(
            &NSImageSymbolConfiguration::configurationWithPointSize_weight(GLYPH - 8.0, 0.0),
        ));
        // Tertiary, not the accent colour: nothing here is being pointed at.
        glyph.setContentTintColor(Some(&NSColor::tertiaryLabelColor()));
    }
    glyph.setAutoresizingMask(CENTRED);
    holder.addSubview(&glyph);
    y -= GLYPH + 12.0;

    let title = NSTextField::labelWithString(&NSString::from_str(headline), mtm);
    title.setFrame(NSRect::new(
        NSPoint::new(0.0, y - HEADLINE),
        NSSize::new(width, HEADLINE),
    ));
    title.setAlignment(NSTextAlignment::Center);
    title.setFont(Some(&NSFont::systemFontOfSize(15.0)));
    title.setTextColor(Some(&NSColor::secondaryLabelColor()));
    title.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable
            | NSAutoresizingMaskOptions::ViewMinYMargin
            | NSAutoresizingMaskOptions::ViewMaxYMargin,
    );
    holder.addSubview(&title);
    y -= HEADLINE + 4.0;

    let line = NSTextField::labelWithString(&NSString::from_str(detail), mtm);
    line.setFrame(NSRect::new(
        NSPoint::new(0.0, y - DETAIL),
        NSSize::new(width, DETAIL),
    ));
    line.setAlignment(NSTextAlignment::Center);
    line.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    line.setTextColor(Some(&NSColor::tertiaryLabelColor()));
    line.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable
            | NSAutoresizingMaskOptions::ViewMinYMargin
            | NSAutoresizingMaskOptions::ViewMaxYMargin,
    );
    holder.addSubview(&line);

    EmptyState {
        view: holder,
        headline: title,
        detail: line,
    }
}

pub fn label(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let field = { NSTextField::labelWithString(&NSString::from_str(text), mtm) };
    {
        field.setFrame(frame);
        field.setAlignment(NSTextAlignment::Right);
        field.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
    }
    field
}

/// A muted line of explanatory text under a control.
/// Let a label wrap inside `width`, and grow it to whatever height that needs.
///
/// `labelWithString:` gives a single line that truncates. Wrapping has to be
/// asked for on the cell, and the frame resized afterwards, or the extra lines
/// are laid out underneath the visible one and clipped.
pub fn wrap(field: &NSTextField, width: f64) {
    allow_wrapping(field, width);
    let fitted = field.sizeThatFits(NSSize::new(width, f64::MAX));
    let origin = field.frame().origin;
    field.setFrame(NSRect::new(
        origin,
        NSSize::new(width, fitted.height.ceil()),
    ));
}

/// Wrapping without the resize, for a label whose text arrives later and whose
/// frame was reserved for it. Growing that one to fit an empty string would
/// leave nothing to write into.
pub fn allow_wrapping(field: &NSTextField, width: f64) {
    field.setUsesSingleLineMode(false);
    field.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByWordWrapping);
    field.setPreferredMaxLayoutWidth(width);
}

pub fn note(mtm: MainThreadMarker, text: &str, frame: NSRect) -> Retained<NSTextField> {
    let field = { NSTextField::labelWithString(&NSString::from_str(text), mtm) };
    {
        field.setFrame(frame);
        field.setFont(Some(&NSFont::systemFontOfSize(10.0)));
        field.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
    }
    field
}

pub fn text_field(mtm: MainThreadMarker, frame: NSRect, tag: isize) -> Retained<NSTextField> {
    let field = { NSTextField::initWithFrame(NSTextField::alloc(mtm), frame) };
    {
        field.setTag(tag);
        field.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
    }
    field
}

pub fn secure_field(
    mtm: MainThreadMarker,
    frame: NSRect,
    tag: isize,
) -> Retained<NSSecureTextField> {
    let field = { NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm), frame) };
    {
        field.setTag(tag);
        field.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
    }
    field
}

pub fn popup(
    mtm: MainThreadMarker,
    frame: NSRect,
    tag: isize,
    titles: &[&str],
) -> Retained<NSPopUpButton> {
    let button =
        { NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), frame, false) };
    {
        button.setTag(tag);
        button.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        for title in titles {
            button.addItemWithTitle(&NSString::from_str(title));
        }
    }
    button
}

/// An editable popup: the fetched models are offered, but any model id can be
/// typed, which is what the web settings screen allows.
pub fn combo(mtm: MainThreadMarker, frame: NSRect, tag: isize) -> Retained<NSComboBox> {
    let box_ = { NSComboBox::initWithFrame(NSComboBox::alloc(mtm), frame) };
    {
        box_.setTag(tag);
        box_.setCompletes(true);
        box_.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
    }
    box_
}

pub fn switch_control(mtm: MainThreadMarker, frame: NSRect, tag: isize) -> Retained<NSSwitch> {
    let switch = { NSSwitch::initWithFrame(NSSwitch::alloc(mtm), frame) };
    switch.setTag(tag);
    switch
}

pub fn button(mtm: MainThreadMarker, frame: NSRect, title: &str, tag: isize) -> Retained<NSButton> {
    let button = { NSButton::initWithFrame(NSButton::alloc(mtm), frame) };
    {
        button.setTitle(&NSString::from_str(title));
        button.setBezelStyle(NSBezelStyle::Push);
        button.setTag(tag);
        button.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
    }
    button
}

/// One option of a radio group. AppKit groups radio buttons by superview and
/// action, so every option in a group has to be added to the same view and
/// wired to the same selector; the tag says which one was picked.
pub fn radio(mtm: MainThreadMarker, frame: NSRect, title: &str, tag: isize) -> Retained<NSButton> {
    let button = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    button.setButtonType(objc2_app_kit::NSButtonType::Radio);
    button.setTitle(&NSString::from_str(title));
    button.setTag(tag);
    button.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    button
}

/// A scrollable text view, for the dictionary.
pub fn text_view(
    mtm: MainThreadMarker,
    frame: NSRect,
) -> (Retained<NSScrollView>, Retained<NSTextView>) {
    let scroll = { NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame) };
    let content = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(frame.size.width, frame.size.height),
    );
    let view = { NSTextView::initWithFrame(NSTextView::alloc(mtm), content) };
    {
        scroll.setHasVerticalScroller(true);
        scroll.setBorderType(objc2_app_kit::NSBorderType::BezelBorder);
        view.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        view.setRichText(false);
        view.setAutomaticQuoteSubstitutionEnabled(false);
        view.setAutomaticSpellingCorrectionEnabled(false);
        scroll.setDocumentView(Some(&view));
    }
    (scroll, view)
}

/// Wire a control to `action` on `target`.
pub fn wire(control: &NSControl, target: &AnyObject, action: objc2::runtime::Sel) {
    unsafe {
        control.setTarget(Some(target));
        control.setAction(Some(action));
    }
}
