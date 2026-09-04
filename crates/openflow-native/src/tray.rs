//! The status item: a `tray-icon` in the menu bar with a `muda` menu.
//!
//! The menu mirrors the Tauri build's, including the detail that made it
//! correct: recents are keyed by row id, never by list index, so a
//! transcription that lands between building the menu and clicking it cannot
//! make the click paste the wrong row.

use std::cell::RefCell;
use std::sync::Arc;

use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use objc2::AllocAnyThread;
use objc2::MainThreadMarker;
use objc2_app_kit::NSImage;
use objc2_foundation::{NSData, NSSize};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use openflow_core::engine::{Engine, EngineEvent, RecordingState};

/// How many recents the menu shows, matching `build_tray_menu` in the Tauri
/// host.
pub const RECENTS: usize = 20;
/// Where a recent's preview is cut, matching the same function.
pub const PREVIEW_CHARS: usize = 40;

const ID_MAIN: &str = "main";
const ID_SETTINGS: &str = "settings";
const ID_HISTORY: &str = "history";
const ID_PLUGINS: &str = "plugins";
const ID_QUIT: &str = "quit";
const RECENT_PREFIX: &str = "recent:";

/// The point size `tray-icon` draws a status item's image at, which it fixes
/// rather than derives (`platform_impl/macos/mod.rs`: `let icon_height: f64 =
/// 18.0`). Repeated here because the vector below has to be told a size and
/// this is the one that leaves the icon exactly the size it is today.
const ICON_POINTS: f64 = 18.0;

/// One line of a recent transcription, cut the way the Tauri tray cuts it.
pub fn preview_of(text: &str) -> String {
    let preview: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        format!("{}...", preview)
    } else {
        preview
    }
}

/// The status line at the top of the menu.
pub fn status_line(state: RecordingState) -> &'static str {
    match state {
        RecordingState::Recording => "OpenFlow: Recording",
        RecordingState::Transcribing => "OpenFlow: Transcribing",
        RecordingState::Formatting => "OpenFlow: Formatting",
        RecordingState::Idle => "OpenFlow: Ready",
    }
}

pub struct Tray {
    icon: TrayIcon,
    status: RefCell<RecordingState>,
    /// The disabled first line. Retained so a state change can retitle it
    /// instead of rebuilding the menu, which costs a history query and ~25
    /// items on the main thread three times per dictation.
    status_item: RefCell<MenuItem>,
}

impl Tray {
    pub fn new(engine: &Arc<Engine>) -> Result<Self, String> {
        let (menu, status_item) = build_menu(engine, RecordingState::Idle)?;
        let icon = TrayIconBuilder::new()
            .with_id("main_tray")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .with_tooltip("OpenFlow: Ready")
            .with_icon(embedded_icon()?)
            .with_icon_as_template(true)
            .build()
            .map_err(|error| format!("Could not create the menu bar item: {}", error))?;
        draw_the_icon_as_a_vector(&icon);
        Ok(Self {
            icon,
            status: RefCell::new(RecordingState::Idle),
            status_item: RefCell::new(status_item),
        })
    }

    pub fn set_status(&self, state: RecordingState) {
        if *self.status.borrow() == state {
            return;
        }
        *self.status.borrow_mut() = state;
        let _ = self.icon.set_tooltip(Some(status_line(state)));
        self.status_item.borrow().set_text(status_line(state));
    }

    pub fn set_tooltip(&self, text: &str) {
        let _ = self.icon.set_tooltip(Some(text));
    }

    /// Rebuild the whole menu. Only the recents can change shape, so this runs
    /// on `HistoryChanged` and nowhere else.
    pub fn rebuild(&self, engine: &Arc<Engine>) {
        let state = *self.status.borrow();
        if let Ok((menu, status_item)) = build_menu(engine, state) {
            self.icon.set_menu(Some(Box::new(menu)));
            *self.status_item.borrow_mut() = status_item;
        }
    }
}

/// Build the menu, handing back the status line so the caller can retitle it
/// without rebuilding.
fn build_menu(engine: &Arc<Engine>, state: RecordingState) -> Result<(Menu, MenuItem), String> {
    let menu = Menu::new();
    let append = |item: &dyn muda::IsMenuItem| -> Result<(), String> {
        menu.append(item)
            .map_err(|error| format!("Could not build the menu: {}", error))
    };

    let status_item = MenuItem::with_id(MenuId::new("_status"), status_line(state), false, None);
    append(&status_item)?;

    let recents = engine.history(RECENTS).unwrap_or_default();
    if !recents.is_empty() {
        append(&PredefinedMenuItem::separator())?;
        append(&MenuItem::with_id(
            MenuId::new("_label_recents"),
            "Recent Transcriptions",
            false,
            None,
        ))?;
        for item in &recents {
            let text = item.formatted_text.as_deref().unwrap_or(&item.raw_text);
            append(&MenuItem::with_id(
                MenuId::new(format!("{}{}", RECENT_PREFIX, item.id)),
                preview_of(text),
                true,
                None,
            ))?;
        }
    }

    append(&PredefinedMenuItem::separator())?;
    // The window first, then the pages inside it. History and Plugins are no
    // longer windows of their own: they open the main window on that page,
    // which is why they keep their names but lose their ellipses.
    append(&MenuItem::with_id(
        MenuId::new(ID_MAIN),
        "Open OpenFlow",
        true,
        None,
    ))?;
    append(&MenuItem::with_id(
        MenuId::new(ID_HISTORY),
        "History",
        true,
        None,
    ))?;
    append(&MenuItem::with_id(
        MenuId::new(ID_PLUGINS),
        "Plugins",
        true,
        None,
    ))?;
    append(&MenuItem::with_id(
        MenuId::new(ID_SETTINGS),
        "Settings",
        true,
        None,
    ))?;
    append(&PredefinedMenuItem::separator())?;
    append(&MenuItem::with_id(MenuId::new(ID_QUIT), "Quit", true, None))?;
    Ok((menu, status_item))
}

/// The same 22 px template icon the Tauri tray uses, decoded to the RGBA buffer
/// `tray-icon` wants.
fn embedded_icon() -> Result<Icon, String> {
    let bytes: &[u8] = include_bytes!("../../../src-tauri/icons/icon.png");
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("The tray icon could not be read: {}", error))?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| format!("The tray icon could not be decoded: {}", error))?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err("The tray icon must be 8-bit RGBA".to_string());
    }
    buffer.truncate(info.buffer_size());
    Icon::from_rgba(buffer, info.width, info.height)
        .map_err(|error| format!("The tray icon is not a valid image: {}", error))
}

/// The same mark again, as a vector, over the bitmap `tray-icon` just installed.
///
/// The menu bar draws a template image at 18 pt. `tray-icon` hands AppKit a
/// bitmap, and the one it is handed is 22 px square, so on a Retina display
/// those 22 pixels are stretched over the 36 the screen actually asks for --
/// not a whole-number step, so every edge in the mark lands between pixels. A
/// PDF has nothing to stretch: AppKit rasterises it at whatever the display
/// wants, including the 3x one this app has never been run on.
///
/// Best effort, and deliberately not a `Result`. The raster icon is already in
/// the menu bar by the time this runs, so every early return here leaves the
/// icon that was going to be replaced rather than an empty status item. The one
/// thing it must not do is run off the main thread, which is why it asks for
/// the marker rather than assuming it.
fn draw_the_icon_as_a_vector(icon: &TrayIcon) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(status_item) = icon.ns_status_item() else {
        return;
    };
    let Some(button) = status_item.button(mtm) else {
        return;
    };
    let data = NSData::with_bytes(VECTOR_ICON);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        return;
    };
    // The PDF's own page is square, so one number does both sides.
    image.setSize(NSSize::new(ICON_POINTS, ICON_POINTS));
    // Same as `with_icon_as_template` above: the menu bar tints the alpha and
    // ignores the colour, which is how the icon follows dark mode and the
    // highlight without shipping four artworks.
    image.setTemplate(true);
    button.setImage(Some(&image));
}

/// The mark as a vector. Derived from the 512 px representation inside
/// `icon.icns` rather than drawn again: the shapes are a rectangle, a circle,
/// a circle-shaped counter and two straight cuts, and every one of them was
/// fitted to that bitmap and checked back against it. Rasterised at 512 px the
/// PDF differs from the source in 0.15% of pixels, all but 32 of which are the
/// antialiased edge itself.
const VECTOR_ICON: &[u8] = include_bytes!("../../../src-tauri/icons/tray.pdf");

/// Menu clicks arrive on the thread `muda` runs its handler on. Hop to the main
/// thread before touching a window or the engine.
pub fn install_handler() {
    MenuEvent::set_event_handler(Some(|event: MenuEvent| {
        let id = event.id().as_ref().to_string();
        crate::trace!("tray click id={}", id);
        crate::events::on_main(move || {
            crate::app::with_app(|app| match id.as_str() {
                // No page named: the window reopens where the user left it.
                ID_MAIN => app.handle_event(EngineEvent::Navigate("main".to_string())),
                // No tab: the window reopens where the user left it.
                ID_SETTINGS => app.handle_event(EngineEvent::Navigate("settings".to_string())),
                ID_HISTORY => app.handle_event(EngineEvent::Navigate("history".to_string())),
                ID_PLUGINS => app.handle_event(EngineEvent::Navigate("plugins".to_string())),
                ID_QUIT => app.handle_event(EngineEvent::Navigate("quit".to_string())),
                other => {
                    if let Some(row) = other.strip_prefix(RECENT_PREFIX) {
                        app.engine().paste_transcription(row);
                    }
                }
            });
        });
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2::ClassType;
    use objc2_foundation::NSObjectProtocol;

    /// The preview has to cut at 40 characters and mark the cut, and it has to
    /// count characters rather than bytes: a 40-emoji transcript is 160 bytes
    /// and slicing it by byte would panic.
    #[test]
    fn recents_are_truncated_the_way_the_tauri_tray_truncates_them() {
        assert_eq!(preview_of("short"), "short");

        let exactly_forty = "a".repeat(40);
        assert_eq!(preview_of(&exactly_forty), exactly_forty);

        let forty_one = "a".repeat(41);
        assert_eq!(preview_of(&forty_one), format!("{}...", "a".repeat(40)));

        let wide = "é".repeat(50);
        assert_eq!(preview_of(&wide), format!("{}...", "é".repeat(40)));
        assert_eq!(preview_of(&wide).chars().count(), 43);
    }

    /// The asset has to be a vector, and it has to be one all the way down.
    ///
    /// The class of the representation is not enough on its own, which was
    /// worth finding out rather than assuming: a PNG run through
    /// `sips -s format pdf` still reads back as an `NSPDFImageRep`, because
    /// that class describes the container and not what the page draws. So the
    /// last assertion is about the page itself. A raster wrapped in a PDF
    /// carries an image XObject and would be stretched at 2x exactly as the
    /// bitmap this replaced was; a page of paths has nothing to stretch.
    #[test]
    fn the_icon_is_a_vector_and_appkit_reads_it_as_one() {
        assert!(
            VECTOR_ICON.starts_with(b"%PDF-"),
            "the tray asset stopped being a PDF"
        );

        let data = NSData::with_bytes(VECTOR_ICON);
        let image = NSImage::initWithData(NSImage::alloc(), &data)
            .expect("AppKit has to be able to read the bundled tray icon");

        let size = image.size();
        assert!(
            size.width > 0.0 && size.height > 0.0,
            "an image with no size draws nothing: {size:?}"
        );
        assert_eq!(
            size.width, size.height,
            "the page is square, which is why one number sets both sides"
        );

        let reps = image.representations();
        assert_eq!(reps.len(), 1, "one page, one representation");
        let rep = reps.firstObject().expect("the representation");
        assert!(
            rep.isKindOfClass(objc2_app_kit::NSPDFImageRep::class()),
            "AppKit read it as {:?} rather than as a PDF",
            rep.class()
        );

        assert!(
            !VECTOR_ICON.windows(6).any(|window| window == b"/Image"),
            "the page draws a picture rather than the mark"
        );
        assert!(
            VECTOR_ICON.windows(3).any(|window| window == b" c\n"),
            "the page has no curves in it, so the two circles went missing"
        );
    }

    #[test]
    fn the_status_line_names_every_state() {
        assert_eq!(status_line(RecordingState::Idle), "OpenFlow: Ready");
        assert_eq!(
            status_line(RecordingState::Recording),
            "OpenFlow: Recording"
        );
        assert_eq!(
            status_line(RecordingState::Transcribing),
            "OpenFlow: Transcribing"
        );
    }

    /// The embedded icon has to decode at build time, or the menu bar item is
    /// blank on a user's machine and nothing says why.
    #[test]
    fn the_embedded_tray_icon_decodes() {
        assert!(embedded_icon().is_ok(), "the bundled icon.png must decode");
    }
}
