//! The one window: a source-list sidebar on the left, one page at a time on the
//! right.
//!
//! This reverses the shape Milestone B landed with. Settings, History and
//! Plugins were three independent `NSWindow`s reached from the menu bar; the
//! Tauri build has been a single window with five screens since the beginning,
//! and the native host was the odd one out. What was actually missing was the
//! screen the menu bar could never stand in for: the main one, with the hold
//! button on it. Once that exists it has to live somewhere, and a fourth
//! independent window would have been the wrong somewhere.
//!
//! Two AppKit choices are load-bearing:
//!
//! - **`NSSplitViewController` with `sidebarWithViewController`,** not a hand
//!   built split view. The sidebar item is what supplies the vibrancy, the
//!   full-height layout that runs the sidebar up behind the title bar, and the
//!   collapse behaviour. Painting an `NSVisualEffectView` by hand gets the
//!   translucency and none of the rest, and gets it slightly wrong besides.
//! - **No `NSTabView` anywhere.** Its strip rides on the edge of a rectangle,
//!   which is the framed look this window exists to stop drawing. Pages are
//!   swapped in and out of a plain container instead, and their contents sit on
//!   the rounded cards in [`crate::ui::card`].
//!
//! One thing that looks like a bug and is not: on macOS 26 the sidebar pane is
//! drawn as a rounded panel inset a few points from the window edge, so the
//! window background shows as a faint outline around it. That is the system's
//! own sidebar rendering, not this window's. It was checked rather than
//! assumed -- replacing the pane with an `NSVisualEffectView` filling it edge
//! to edge leaves the outline exactly where it was, and System Settings has
//! the same edge. Nothing here should try to paint it out.
//!
//! Layout stays on autoresizing masks, as the rest of this crate does. The
//! split view controller uses auto layout internally to place the two panes,
//! but that stops at the pane: inside it, a page's frame is set by the
//! container and its own subviews spring off that. Nothing here mixes the two
//! models in one view.
//!
//! All four pages are built together, when the window is. That keeps the
//! swapping trivial, and it puts one rule on the pages: anything that costs the
//! user something -- a keychain read, a device enumeration, a network call --
//! belongs in the page's show path, not its constructor, because a page that is
//! built is not a page that was asked for. Settings is where that bites; see
//! the note in its `new`.
//!
//! Pages are built against the content pane's measured size rather than an
//! assumed one. The window is laid out once, before any page exists, and each
//! page is then handed the rect it actually got -- the same lesson the settings
//! tabs taught when a form built at a guessed width lost its right-hand column.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSControlTextEditingDelegate, NSFocusRingType,
    NSFont, NSImage, NSImageSymbolConfiguration, NSImageView, NSResponder, NSScrollView,
    NSSplitViewController, NSSplitViewItem, NSTableCellView, NSTableColumn,
    NSTableColumnResizingOptions, NSTableView, NSTableViewColumnAutoresizingStyle,
    NSTableViewDataSource, NSTableViewDelegate, NSTableViewStyle, NSTextField,
    NSTitlebarSeparatorStyle, NSView, NSViewController, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_foundation::{
    NSIndexSet, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};

use openflow_core::engine::RecordingState;

use crate::ui::dictate::DictatePage;
use crate::ui::history::HistoryPage;
use crate::ui::note;
use crate::ui::plugins::PluginsPage;
use crate::ui::settings::SettingsPage;

/// The window's content size on first launch. Wide enough that the sidebar
/// leaves the History table its five columns.
const WINDOW_WIDTH: f64 = 880.0;
const WINDOW_HEIGHT: f64 = 580.0;
/// The sidebar's resting width, and the range the user may drag it to.
const SIDEBAR_WIDTH: f64 = 196.0;
const SIDEBAR_MIN: f64 = 168.0;
const SIDEBAR_MAX: f64 = 260.0;
/// Never smaller than the narrowest page can stand. The pages spring, but the
/// History table's columns do not, and below this they start eating each other.
const MIN_WIDTH: f64 = 720.0;
const MIN_HEIGHT: f64 = 440.0;

const SIDEBAR_COLUMN: &str = "page";

/// The pages, in sidebar order: title, window title, and the SF Symbol the
/// sidebar row carries.
const PAGES: &[(&str, &str, &str)] = &[
    ("Dictate", "OpenFlow", "mic.fill"),
    ("History", "OpenFlow History", "clock.arrow.circlepath"),
    ("Plugins", "OpenFlow Plugins", "puzzlepiece.extension.fill"),
    ("Settings", "OpenFlow Settings", "gearshape.fill"),
];
/// Settings is the only page a caller can name a place *inside*, so its index
/// is needed by name rather than by position.
const SETTINGS: usize = 3;

/// Row metrics. The icon column is the width of the largest symbol plus the
/// gap after it, so the titles line up whatever glyph sits beside them -- a
/// sidebar whose text starts at a different x on each row is the thing that
/// reads as hand-made.
const ROW_HEIGHT: f64 = 30.0;
const ICON_SIZE: f64 = 16.0;
const ICON_LEFT: f64 = 6.0;
const TITLE_LEFT: f64 = 30.0;
/// The footer under the rows: a caption and the endpoint under it.
const FOOTER_HEIGHT: f64 = 48.0;
const FOOTER_INSET: f64 = 14.0;

pub struct MainIvars {
    /// Held for the footer, which re-reads the endpoint on every present.
    engine: std::sync::Arc<openflow_core::engine::Engine>,
    window: Retained<NSWindow>,
    sidebar: Retained<NSTableView>,
    /// The line at the foot of the sidebar naming what will transcribe.
    endpoint: Retained<NSTextField>,
    /// The pane a page's view is installed into. Exactly one subview at a time.
    container: Retained<NSView>,
    dictate: Retained<DictatePage>,
    history: Retained<HistoryPage>,
    plugins: Retained<PluginsPage>,
    settings: Retained<SettingsPage>,
    /// Kept alive for as long as the window is: the window holds the controller
    /// as its `contentViewController`, but the items are ours.
    _split: Retained<NSSplitViewController>,
    current: Cell<usize>,
}

define_class!(
    // SAFETY: NSObject imposes no subclassing requirements; this class holds
    // only ivars and implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowMainWindow"]
    #[ivars = MainIvars]
    pub struct MainWindow;

    unsafe impl NSObjectProtocol for MainWindow {}

    unsafe impl NSWindowDelegate for MainWindow {
        /// Hidden, not closed, exactly as the three separate windows were: the
        /// pages keep their state and their scroll position, and the Dock icon
        /// goes away through the same pair.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            crate::ui::dismiss_window(&self.ivars().window, "main");
            false
        }

        /// Give the keyboard back to the record button, but only if nothing
        /// else has it.
        ///
        /// A system alert over the window -- the microphone prompt is the one
        /// that happens here -- leaves the window with no first responder at
        /// all when it goes away, and Space then does nothing until the user
        /// tabs somewhere. Restoring it unconditionally would be worse than
        /// the problem: it would yank focus off whatever the user had
        /// deliberately tabbed to every time they switched back to the app. A
        /// window reports *itself* as first responder when nothing else is,
        /// which is exactly the case worth repairing and no other.
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            let window = &self.ivars().window;
            let vacant = window
                .firstResponder()
                .is_none_or(|responder| std::ptr::eq(&*responder, &**window as &NSResponder));
            if vacant {
                self.focus_current();
            }
        }
    }

    unsafe impl NSTableViewDataSource for MainWindow {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> isize {
            PAGES.len() as isize
        }
    }

    // `NSTableViewDelegate` inherits from this one; the table never edits a
    // cell, so there is nothing to implement.
    unsafe impl NSControlTextEditingDelegate for MainWindow {}

    unsafe impl NSTableViewDelegate for MainWindow {
        /// One row: a symbol and its title, in an `NSTableCellView` so the
        /// source-list style has the two things it dresses -- it tints the
        /// image and inverts the text when the row is selected, which a text
        /// cell drawing its own string cannot be asked to do.
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for_row(
            &self,
            _table: &NSTableView,
            _column: Option<&NSTableColumn>,
            row: isize,
        ) -> Option<Retained<NSView>> {
            let mtm = MainThreadMarker::from(self);
            // One expression, no `?` and no early `return`: `define_class!`
            // rewrites the body into a method that does not return this Option
            // itself, and neither reaches past the rewrite.
            usize::try_from(row)
                .ok()
                .and_then(|row| PAGES.get(row))
                .map(|(title, _, symbol)| {
                    let cell = NSTableCellView::initWithFrame(
                        NSTableCellView::alloc(mtm),
                        NSRect::new(
                            NSPoint::new(0.0, 0.0),
                            NSSize::new(SIDEBAR_WIDTH, ROW_HEIGHT),
                        ),
                    );

                    let icon = NSImageView::initWithFrame(
                        NSImageView::alloc(mtm),
                        NSRect::new(
                            NSPoint::new(ICON_LEFT, (ROW_HEIGHT - ICON_SIZE) / 2.0),
                            NSSize::new(ICON_SIZE, ICON_SIZE),
                        ),
                    );
                    if let Some(image) =
                        NSImage::imageWithSystemSymbolName_accessibilityDescription(
                            &NSString::from_str(symbol),
                            Some(&NSString::from_str(title)),
                        )
                    {
                        icon.setImage(Some(&image));
                        // `NSFontWeight` is a bare `CGFloat`; 0.0 is regular.
                        icon.setSymbolConfiguration(Some(
                            &NSImageSymbolConfiguration::configurationWithPointSize_weight(
                                ICON_SIZE - 2.0,
                                0.0,
                            ),
                        ));
                    }
                    cell.addSubview(&icon);
                    // SAFETY: both outlets are plain weak references the cell
                    // uses to find what to tint and what to invert on
                    // selection; the views are already its subviews.
                    unsafe { cell.setImageView(Some(&icon)) };

                    let text = NSTextField::labelWithString(&NSString::from_str(title), mtm);
                    text.setFont(Some(&NSFont::systemFontOfSize(NSFont::systemFontSize())));
                    // A label draws its line at the top of its frame, not down
                    // the middle of it, so a field given the whole row height
                    // puts the title above the icon beside it. Give it the
                    // height of its own line and centre that instead.
                    let line = text.sizeThatFits(NSSize::new(f64::MAX, f64::MAX)).height;
                    text.setFrame(NSRect::new(
                        NSPoint::new(TITLE_LEFT, ((ROW_HEIGHT - line) / 2.0).floor()),
                        NSSize::new(SIDEBAR_WIDTH - TITLE_LEFT - ICON_LEFT, line),
                    ));
                    // The title follows the row when the divider is dragged;
                    // the icon stays where it is.
                    text.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
                    cell.addSubview(&text);
                    unsafe { cell.setTextField(Some(&text)) };

                    Retained::into_super(cell)
                })
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_did_change(&self, _notification: &NSNotification) {
            let row = self.ivars().sidebar.selectedRow();
            if let Ok(index) = usize::try_from(row) {
                self.show_page(index);
            }
        }
    }
);

impl MainWindow {
    pub fn new(app: &std::rc::Rc<crate::app::App>, mtm: MainThreadMarker) -> Retained<Self> {
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(
                    NSPoint::new(0.0, 0.0),
                    NSSize::new(WINDOW_WIDTH, WINDOW_HEIGHT),
                ),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable
                    | NSWindowStyleMask::Resizable
                    // The sidebar runs to the top of the window rather than
                    // stopping under a drawn title bar. Without this the
                    // sidebar item's full-height layout has nothing to fill.
                    | NSWindowStyleMask::FullSizeContentView,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str(PAGES[0].1));
        window.setTitlebarAppearsTransparent(true);
        unsafe { window.setReleasedWhenClosed(false) };
        window.setMinSize(NSSize::new(MIN_WIDTH, MIN_HEIGHT));
        // Reopening puts the window back where the user left it, which three
        // separate windows never did for each other.
        window.setFrameAutosaveName(&NSString::from_str("OpenFlowMain"));
        window.center();

        // ── The two panes ──
        let (sidebar, sidebar_view, endpoint) = build_sidebar(mtm);
        let sidebar_controller = NSViewController::new(mtm);
        sidebar_controller.setView(&sidebar_view);

        let container = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(WINDOW_WIDTH - SIDEBAR_WIDTH, WINDOW_HEIGHT),
            ),
        );
        let content_controller = NSViewController::new(mtm);
        content_controller.setView(&container);

        let sidebar_item = NSSplitViewItem::sidebarWithViewController(&sidebar_controller);
        sidebar_item.setMinimumThickness(SIDEBAR_MIN);
        sidebar_item.setMaximumThickness(SIDEBAR_MAX);
        sidebar_item.setCanCollapse(true);
        // The sidebar fills the window's full height, running up behind the
        // title bar. This is the half of the Ventura look that `FullSizeContentView`
        // on the window exists to allow.
        sidebar_item.setAllowsFullHeightLayout(true);
        // No hairline between the title bar and the pane beneath it. The
        // separator is what makes a window read as a box with a lid.
        sidebar_item.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);

        let content_item = NSSplitViewItem::contentListWithViewController(&content_controller);
        content_item.setTitlebarSeparatorStyle(NSTitlebarSeparatorStyle::None);

        let split = NSSplitViewController::new(mtm);
        split.addSplitViewItem(&sidebar_item);
        split.addSplitViewItem(&content_item);
        window.setContentViewController(Some(&split));

        // Lay the panes out before measuring. The pages need the content
        // pane's real rect, and until this runs the container still has the
        // placeholder frame it was created with.
        if let Some(content) = window.contentView() {
            content.layoutSubtreeIfNeeded();
        }
        let page_size = page_frame(&container).size;

        let dictate = DictatePage::new(app, mtm, page_size);
        let history = HistoryPage::new(app, mtm, page_size);
        let plugins = PluginsPage::new(app, mtm, page_size);
        let settings = SettingsPage::new(app, mtm, page_size);

        let this = Self::alloc(mtm).set_ivars(MainIvars {
            window,
            engine: std::sync::Arc::clone(app.engine()),
            sidebar,
            endpoint,
            container,
            dictate,
            history,
            plugins,
            settings,
            _split: split,
            current: Cell::new(usize::MAX),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };

        this.ivars()
            .window
            .setDelegate(Some(ProtocolObject::from_ref(&*this)));
        let sidebar = &this.ivars().sidebar;
        // Weak properties, both of them: the table does not retain us, and
        // `App` owns the only strong reference to this window.
        unsafe {
            sidebar.setDataSource(Some(ProtocolObject::from_ref(&*this)));
            sidebar.setDelegate(Some(ProtocolObject::from_ref(&*this)));
        }
        sidebar.reloadData();
        this.show_page(0);
        this
    }

    /// Install page `index` in the content pane and title the window after it.
    ///
    /// Swapping the subview rather than hiding all three keeps exactly one page
    /// in the view hierarchy, so a table that is not on screen is not being
    /// asked to draw.
    pub fn show_page(&self, index: usize) {
        let ivars = self.ivars();
        if ivars.current.get() == index {
            return;
        }
        let Some((_, title, _)) = PAGES.get(index) else {
            return;
        };
        // Leaving Settings is what closing its window used to be: it commits
        // a key or a URL the user typed and never tabbed out of, and it takes
        // down a hotkey recorder that would otherwise go on swallowing every
        // keystroke in the app.
        if ivars.current.get() == SETTINGS {
            ivars.settings.on_hidden();
        }
        let view = match index {
            0 => ivars.dictate.view(),
            1 => ivars.history.view(),
            2 => ivars.plugins.view(),
            _ => ivars.settings.view(),
        };
        // Whatever is showing comes out first: a view can only have one
        // superview, and adding it a second time is a silent no-op that leaves
        // the old page underneath.
        for existing in ivars.container.subviews().iter() {
            existing.removeFromSuperview();
        }
        view.setFrame(page_frame(&ivars.container));
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        ivars.container.addSubview(&view);
        ivars.current.set(index);
        ivars.window.setTitle(&NSString::from_str(title));

        // The sidebar may not be the thing that asked for this: a tray click
        // names a page directly. Keep the selection in step, and do it without
        // re-entering `show_page` -- setting the selection sends the delegate
        // notification, which lands back here and returns immediately because
        // `current` is already this index.
        let indexes = NSIndexSet::indexSetWithIndex(index);
        ivars
            .sidebar
            .selectRowIndexes_byExtendingSelection(&indexes, false);

        // Pages that read the world when they come forward do it here rather
        // than on a timer.
        match index {
            0 => {
                ivars.dictate.load();
                // After the view is in the hierarchy: a view with no window
                // has no first responder to become.
                ivars.dictate.focus_record();
            }
            1 => ivars.history.load(),
            2 => ivars.plugins.load(),
            // Every time, not just on first build: another surface can change
            // a value while this page is hidden -- the pill's own drag writes
            // the overlay position, and the wizard rewrites the lot.
            _ => ivars.settings.reload(),
        }
    }

    /// Select a page by the name the tray and `Navigate` use.
    ///
    /// A name that is not a page may still be a group inside Settings: the
    /// tray, the wizard and `Navigate` have always been able to ask for
    /// "voice" or "privacy", and those used to be tabs. They open Settings on
    /// that group now.
    pub fn show_named(&self, name: &str) {
        if let Some(index) = page_index(name) {
            self.show_page(index);
            return;
        }
        if crate::ui::settings::section_index(name).is_some() {
            self.show_page(SETTINGS);
            self.ivars().settings.select_section(name);
        }
    }

    /// On screen, as the Dock-icon rule reads it.
    pub fn is_visible(&self) -> bool {
        self.ivars().window.isVisible()
    }

    /// The window itself, for the one thing that needs it: hanging the setup
    /// wizard off it as a sheet.
    pub fn window(&self) -> Retained<NSWindow> {
        self.ivars().window.clone()
    }

    pub fn present(&self) {
        self.refresh_endpoint();
        crate::ui::present_window(&self.ivars().window, "main");
        // After presenting, not before. A window assigns its first responder
        // when it first becomes key, from its own key view loop, and that
        // assignment lands on top of anything set while the window was still
        // off screen -- which is where the focus set during `show_page` went
        // on the very first open.
        self.focus_current();
    }

    /// Re-read which endpoint will be asked to transcribe. A settings read, so
    /// it happens here rather than once at build time: Settings can change it
    /// while the window is open, and this is the line that would go on lying.
    fn refresh_endpoint(&self) {
        let name = self.ivars().engine.settings().provider_name();
        let shown = crate::ui::history::provider_label(&name);
        self.ivars()
            .endpoint
            .setStringValue(&NSString::from_str(&shown));
    }

    /// Give the keyboard to whatever the current page wants it on. Only
    /// Dictate wants it: the other two are lists, and a table that steals
    /// first responder from itself scrolls to the top.
    fn focus_current(&self) {
        if self.ivars().current.get() == 0 {
            self.ivars().dictate.focus_record();
        }
    }

    /// Run `body` against the History page, whether or not it is showing: the
    /// engine's `HistoryChanged` arrives regardless of which page is forward.
    pub fn history(&self) -> Retained<HistoryPage> {
        self.ivars().history.clone()
    }

    pub fn dictate(&self) -> Retained<DictatePage> {
        self.ivars().dictate.clone()
    }

    pub fn settings(&self) -> Retained<SettingsPage> {
        self.ivars().settings.clone()
    }

    /// Re-read everything a page shows. Called when the window is presented,
    /// because Settings may have changed a binding while it was hidden.
    pub fn reload(&self) {
        let ivars = self.ivars();
        ivars.dictate.load();
        match ivars.current.get() {
            1 => ivars.history.load(),
            2 => ivars.plugins.load(),
            SETTINGS => ivars.settings.reload(),
            _ => {}
        }
    }

    /// The recording state, for the page that draws it.
    pub fn set_state(&self, state: RecordingState) {
        self.ivars().dictate.set_state(state);
    }
}

/// The rect a page gets inside the content pane.
///
/// `FullSizeContentView` runs the window's content all the way to the top of
/// the frame so the sidebar can sit under the title bar. The content pane is
/// not the sidebar, though: a page laid out in the full rect puts its first
/// card behind the title and the traffic lights. AppKit already knows how far
/// down the title bar reaches and reports it as the safe area, so the inset is
/// asked for rather than written down -- a constant here would be a guess that
/// goes stale the first time the window grows a toolbar.
fn page_frame(container: &NSView) -> NSRect {
    let bounds = container.bounds();
    let insets = container.safeAreaInsets();
    NSRect::new(
        NSPoint::new(bounds.origin.x, bounds.origin.y),
        NSSize::new(
            bounds.size.width,
            (bounds.size.height - insets.top).max(0.0),
        ),
    )
}

/// The page `name` refers to, or `None` for a name this window does not have.
///
/// A free function so the mapping the tray depends on can be tested without an
/// `NSWindow`: every menu click arrives as one of these strings, and a name
/// that silently matched nothing would be a menu item that does nothing.
fn page_index(name: &str) -> Option<usize> {
    PAGES
        .iter()
        .position(|(title, _, _)| title.eq_ignore_ascii_case(name))
}

/// The source list. View-based, unlike the two tables that came before it: a
/// row here is a symbol beside a title rather than one string, and an
/// `NSTableCellView` is what the source-list style knows how to dress -- it
/// tints the `imageView` and inverts the `textField` on selection, neither of
/// which a text cell drawing its own string can be asked to do.
///
/// Returns the container the sidebar item is given, not the scroll view itself.
/// A scroll view handed straight to `NSViewController::setView` is the root of
/// the sidebar pane, and AppKit inset and rounded it against the vibrancy
/// behind it -- a visible box round the sidebar, in the window built to stop
/// drawing boxes. A plain container with the scroll view pinned inside it is
/// what a sidebar is normally made of, and it lies flat.
fn build_sidebar(
    mtm: MainThreadMarker,
) -> (
    Retained<NSTableView>,
    Retained<NSView>,
    Retained<NSTextField>,
) {
    let frame = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(SIDEBAR_WIDTH, WINDOW_HEIGHT),
    );
    let container = NSView::initWithFrame(NSView::alloc(mtm), frame);
    // The list stops above the footer rather than running under it.
    let list_frame = NSRect::new(
        NSPoint::new(0.0, FOOTER_HEIGHT),
        NSSize::new(
            frame.size.width,
            (frame.size.height - FOOTER_HEIGHT).max(0.0),
        ),
    );
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), list_frame);
    let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), list_frame);

    let column = NSTableColumn::initWithIdentifier(
        NSTableColumn::alloc(mtm),
        &NSString::from_str(SIDEBAR_COLUMN),
    );
    // Not `SIDEBAR_WIDTH`. The source-list style reserves horizontal padding
    // outside the column, so a column as wide as the pane makes a table wider
    // than the pane -- measured at 230pt inside a 196pt clip view -- and the
    // right-hand end of the selection pill is simply scrolled out of sight.
    // That is what "the highlight runs off the edge and never closes" is: not
    // a pill drawn square, a rounded one whose end is off screen. Starting
    // narrow and letting the autoresizing mask grow the column to whatever is
    // left inside the padding gets both ends back, at every divider position.
    column.setMinWidth(40.0);
    column.setWidth(40.0);
    column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
    table.setColumnAutoresizingStyle(
        NSTableViewColumnAutoresizingStyle::UniformColumnAutoresizingStyle,
    );
    table.addTableColumn(&column);
    table.setRowHeight(ROW_HEIGHT);
    // No header, no stripes, and the source-list style: that style is what
    // supplies the inset rows and the system's own selection highlight, which
    // is the one thing a hand-drawn sidebar can never quite match.
    table.setHeaderView(None);
    table.setStyle(NSTableViewStyle::SourceList);
    table.setUsesAlternatingRowBackgroundColors(false);
    table.setAllowsMultipleSelection(false);
    table.setAllowsEmptySelection(false);

    // No focus ring. A source list is always the thing being pointed at, and
    // AppKit's default draws a rounded blue rectangle round the whole sidebar
    // the moment it takes first responder -- a box, in the one window built to
    // stop drawing boxes.
    table.setFocusRingType(NSFocusRingType::None);

    scroll.setDocumentView(Some(&table));
    // The document view of a scroll view keeps the frame it was created with
    // unless it is told to follow; `sizeToFit` then hands the column whatever
    // the style's padding leaves of that width.
    table.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    table.sizeToFit();
    scroll.setHasVerticalScroller(false);
    scroll.setDrawsBackground(false);
    scroll.setBorderType(objc2_app_kit::NSBorderType::NoBorder);
    scroll.setFocusRingType(NSFocusRingType::None);
    // And the clip view between them, which draws its own.
    scroll.contentView().setFocusRingType(NSFocusRingType::None);
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    container.addSubview(&scroll);

    // ── The footer ──
    //
    // Four rows leave most of a sidebar empty, and the one piece of state the
    // main window never showed anywhere is the one that decides whether
    // dictation works at all: which endpoint is going to be asked. It is a
    // settings read, so it happens on the way in rather than in a constructor
    // -- the rule the pages follow, and the same reason Settings stopped
    // reading the keychain at launch.
    let caption = note(
        mtm,
        "Transcribing with",
        NSRect::new(
            NSPoint::new(FOOTER_INSET, FOOTER_HEIGHT - 15.0),
            NSSize::new((frame.size.width - FOOTER_INSET * 2.0).max(0.0), 13.0),
        ),
    );
    caption.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
    );
    let endpoint = note(
        mtm,
        "",
        NSRect::new(
            NSPoint::new(FOOTER_INSET, FOOTER_HEIGHT - 30.0),
            NSSize::new((frame.size.width - FOOTER_INSET * 2.0).max(0.0), 14.0),
        ),
    );
    endpoint.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    endpoint.setTextColor(Some(&objc2_app_kit::NSColor::secondaryLabelColor()));
    endpoint.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
    endpoint.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
    );
    container.addSubview(&caption);
    container.addSubview(&endpoint);

    (table, container, endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openflow_core::engine::Remedy;

    /// Every name the tray and `Navigate` send has a page behind it. These are
    /// the literals in `tray.rs` and in `App::handle_event`, so a page renamed
    /// on one side and not the other fails here rather than in the menu.
    #[test]
    fn the_names_the_tray_sends_all_resolve() {
        assert_eq!(page_index("history"), Some(1));
        assert_eq!(page_index("plugins"), Some(2));
    }

    /// The sidebar's own titles resolve too, and to their own rows.
    #[test]
    fn every_sidebar_title_resolves_to_its_own_row() {
        for (index, (title, _, _)) in PAGES.iter().enumerate() {
            assert_eq!(page_index(title), Some(index), "{}", title);
        }
    }

    /// Dictate is first: it is what the window opens on when no page is named,
    /// and it is the screen this window was built to hold.
    #[test]
    fn dictate_is_the_first_page() {
        assert_eq!(PAGES[0].0, "Dictate");
        assert_eq!(page_index("dictate"), Some(0));
    }

    /// A name nothing answers to leaves the window where it was rather than
    /// falling through to page zero.
    #[test]
    fn an_unknown_name_selects_nothing() {
        assert_eq!(page_index("nowhere"), None);
        assert_eq!(page_index(""), None);
    }

    /// `SETTINGS` has to keep pointing at the Settings row, because leaving
    /// that page is what commits a half-typed key.
    #[test]
    fn the_settings_index_names_the_settings_page() {
        assert_eq!(PAGES[SETTINGS].0, "Settings");
        assert_eq!(page_index("settings"), Some(SETTINGS));
    }

    /// Every remedy the engine can attach to a failure has to name something
    /// this window opens, in the same two vocabularies `show_named` tries.
    ///
    /// The tray offers "Fix this in ..." only when a remedy exists, so a target
    /// nothing answers to would be an item that opens the window and leaves it
    /// wherever the user last was -- an offer of help that lands nowhere. This
    /// is the seam between a core enum and a native screen, and nothing else
    /// checks it.
    #[test]
    fn every_remedy_the_engine_can_attach_opens_something() {
        for remedy in [
            Remedy::Microphone,
            Remedy::Providers,
            Remedy::Plugins,
            Remedy::History,
        ] {
            let target = remedy.target();
            assert!(
                page_index(target).is_some()
                    || crate::ui::settings::section_index(target).is_some(),
                "{:?} names {:?}, which this window cannot open",
                remedy,
                target
            );
        }
    }

    /// The group names inside Settings must not collide with page names, or
    /// `show_named` would answer a group with a page.
    #[test]
    fn a_settings_group_is_not_also_a_page() {
        for group in ["general", "providers", "voice", "privacy"] {
            assert!(
                crate::ui::settings::section_index(group).is_some(),
                "{group}"
            );
            assert_eq!(page_index(group), None, "{group}");
        }
    }
}
