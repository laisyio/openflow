//! The status item: a `tray-icon` in the menu bar with a `muda` menu.
//!
//! The menu mirrors the Tauri build's, including the detail that made it
//! correct: recents are keyed by row id, never by list index, so a
//! transcription that lands between building the menu and clicking it cannot
//! make the click paste the wrong row.

use std::cell::RefCell;
use std::sync::Arc;

use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use openflow_core::engine::{Engine, EngineEvent, Failure, RecordingState};

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
/// The item under the status line that appears only while a failure is
/// standing, and only when that failure has somewhere to send the user.
const ID_REMEDY: &str = "remedy";
const RECENT_PREFIX: &str = "recent:";

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
    /// The failure the user has not been given an answer to yet.
    ///
    /// It lives here because this is the only surface in a `LSUIElement` app
    /// that survives a dictation. What it replaced -- one `set_tooltip` and a
    /// badge -- did not: the settling that follows every take overwrote the
    /// tooltip within the same run-loop turn, so the text of a failure never
    /// reached the screen at all.
    problem: RefCell<Option<Failure>>,
    /// The first line. Retained so a state change can retitle it instead of
    /// rebuilding the menu, which costs a history query and ~25 items on the
    /// main thread three times per dictation.
    status_item: RefCell<MenuItem>,
}

impl Tray {
    pub fn new(engine: &Arc<Engine>) -> Result<Self, String> {
        let (menu, status_item) = build_menu(engine, RecordingState::Idle, None)?;
        let icon = TrayIconBuilder::new()
            .with_id("main_tray")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(true)
            .with_tooltip("OpenFlow: Ready")
            .with_icon(embedded_icon()?)
            .with_icon_as_template(true)
            .build()
            .map_err(|error| format!("Could not create the menu bar item: {}", error))?;
        Ok(Self {
            icon,
            status: RefCell::new(RecordingState::Idle),
            problem: RefCell::new(None),
            status_item: RefCell::new(status_item),
        })
    }

    pub fn set_status(&self, state: RecordingState) {
        if *self.status.borrow() == state {
            return;
        }
        *self.status.borrow_mut() = state;
        self.render();
    }

    /// A passing message, for something that went right.
    ///
    /// Refused while a failure is standing. Everything that calls this is an
    /// acknowledgement the user already knows about -- their transcript, their
    /// re-copy -- and none of it is worth covering the one line that says the
    /// take before it lost their words.
    pub fn set_tooltip(&self, text: &str) {
        if self.problem.borrow().is_some() {
            return;
        }
        let _ = self.icon.set_tooltip(Some(text));
    }

    /// The failure now standing, or `None` once it has been answered.
    ///
    /// Whether the menu has to be rebuilt: the remedy item comes and goes, and
    /// an item cannot be added to a built menu. Rebuilding is not free, which
    /// is why `set_status` does not do it, but a failure is not a per-take
    /// event the way a state change is.
    pub fn set_problem(&self, problem: Option<Failure>) -> bool {
        let had_remedy = self.remedy().is_some();
        *self.problem.borrow_mut() = problem;
        self.render();
        had_remedy != self.remedy().is_some()
    }

    /// Where the standing failure says the user should go, if anywhere.
    pub fn remedy(&self) -> Option<&'static str> {
        self.problem
            .borrow()
            .as_ref()
            .and_then(|problem| problem.remedy)
            .map(|remedy| remedy.target())
    }

    /// Put the current line on both surfaces that carry it.
    ///
    /// A standing failure outranks the state, because the state it would be
    /// covering is "Ready" -- and a menu bar that says Ready is the reason the
    /// user would never look further.
    fn render(&self) {
        let line = line_for(*self.status.borrow(), self.problem.borrow().as_ref());
        let _ = self.icon.set_tooltip(Some(&line));
        self.status_item.borrow().set_text(&line);
    }

    /// Rebuild the whole menu. The recents and the remedy item are the two
    /// things that can change its shape.
    pub fn rebuild(&self, engine: &Arc<Engine>) {
        let state = *self.status.borrow();
        if let Ok((menu, status_item)) = build_menu(engine, state, self.remedy()) {
            self.icon.set_menu(Some(Box::new(menu)));
            *self.status_item.borrow_mut() = status_item;
            self.render();
        }
    }
}

/// The one line the menu bar carries, on both the tooltip and the first menu
/// item.
///
/// A standing failure outranks the state, and the state it is covering is
/// "Ready" -- a menu bar that says Ready after a take that lost the user's
/// words is the reason they would never look any further. It also means the
/// line no longer depends on the order the two arrive in: this used to be two
/// separate writes to one tooltip, and the settling that follows every take
/// won, within the same run-loop turn.
fn line_for(state: RecordingState, problem: Option<&Failure>) -> String {
    match problem {
        Some(problem) => format!("OpenFlow: {}", problem.message),
        None => status_line(state).to_string(),
    }
}

/// What the remedy item says, given where it goes.
///
/// Named after the screen the user lands on, so the item and what opens agree.
/// Two of the four are groups inside Settings and one is a page of its own,
/// which is why this is a table rather than a format string.
pub fn remedy_label(target: &str) -> &'static str {
    match target {
        "general" => "Fix this in General settings\u{2026}",
        "providers" => "Fix this in Providers settings\u{2026}",
        "privacy" => "Fix this in Privacy settings\u{2026}",
        "plugins" => "Open Plugins\u{2026}",
        _ => "Open Settings\u{2026}",
    }
}

/// Build the menu, handing back the status line so the caller can retitle it
/// without rebuilding.
fn build_menu(
    engine: &Arc<Engine>,
    state: RecordingState,
    remedy: Option<&str>,
) -> Result<(Menu, MenuItem), String> {
    let menu = Menu::new();
    let append = |item: &dyn muda::IsMenuItem| -> Result<(), String> {
        menu.append(item)
            .map_err(|error| format!("Could not build the menu: {}", error))
    };

    let status_item = MenuItem::with_id(MenuId::new("_status"), status_line(state), false, None);
    append(&status_item)?;
    // Directly under the line that says what went wrong, so the answer to it
    // is one item away rather than four pages in.
    if let Some(target) = remedy {
        append(&MenuItem::with_id(
            MenuId::new(ID_REMEDY),
            remedy_label(target),
            true,
            None,
        ))?;
    }

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
                // The item only exists while a remedy does, but the click and
                // the failure it answers arrive on different threads, so ask
                // again rather than trusting that it is still there.
                ID_REMEDY => {
                    if let Some(target) = app.tray().remedy() {
                        app.handle_event(EngineEvent::Navigate(target.to_string()));
                    }
                }
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
    use openflow_core::engine::Remedy;

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

    /// The failure a take ended in has to outlive the settling that follows
    /// it. Reporting used to be a bare `set_tooltip`, and the `Idle` that
    /// arrives immediately afterwards was another one: measured on the real
    /// app at 15 ms sampling, the text of a failed take never appeared on the
    /// menu bar at all -- it went `Recording` straight back to `Ready`.
    #[test]
    fn a_standing_failure_outranks_the_resting_state() {
        let problem = Failure::at("No sound reached OpenFlow.", Remedy::Microphone);

        assert_eq!(
            line_for(RecordingState::Idle, Some(&problem)),
            "OpenFlow: No sound reached OpenFlow.",
            "settling to Idle must not be what the user is left reading"
        );
        assert_eq!(
            line_for(RecordingState::Idle, None),
            "OpenFlow: Ready",
            "and with nothing standing, the state is the line"
        );
    }

    /// Every remedy the engine can attach has to name a screen, because the
    /// item is only offered when one exists. A target with no label would
    /// offer "Open Settings" and land the user on whichever group they last
    /// looked at.
    #[test]
    fn every_remedy_names_the_screen_it_opens() {
        for remedy in [
            Remedy::Microphone,
            Remedy::Providers,
            Remedy::Plugins,
            Remedy::History,
        ] {
            let label = remedy_label(remedy.target());
            assert_ne!(
                label, "Open Settings\u{2026}",
                "{:?} fell through to the catch-all label",
                remedy
            );
            assert!(
                label.ends_with('\u{2026}'),
                "{:?} opens something, so its item is elided",
                remedy
            );
        }
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
