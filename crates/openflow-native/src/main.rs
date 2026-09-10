//! OpenFlow as a native macOS app: an `NSApplication` in accessory mode with a
//! status item, a borderless overlay panel and one settings window, driven by
//! the same [`openflow_core::engine::Engine`] the Tauri shell drives.
//!
//! There is no webview and no polling anywhere. Everything that moves is either
//! an AppKit event, a `global-hotkey` callback, a `muda` menu callback, or the
//! engine finishing a job on its own runtime; each of those hops to the main
//! thread through `dispatch2` before it touches a window.
//!
//! The crate is macOS only. On any other target it compiles to a `main` that
//! says so, which keeps `cargo clippy --workspace` honest on Linux CI without
//! pulling AppKit, tray-icon or rodio into the build.

#[cfg(target_os = "macos")]
mod events;
#[cfg(target_os = "macos")]
mod hotkeys;
#[cfg(target_os = "macos")]
mod instance;
#[cfg(target_os = "macos")]
mod menu;
#[cfg(target_os = "macos")]
mod overlay;
#[cfg(target_os = "macos")]
mod trace;
#[cfg(target_os = "macos")]
mod tray;
#[cfg(target_os = "macos")]
mod tts_player;
#[cfg(target_os = "macos")]
mod ui;

#[cfg(target_os = "macos")]
mod app;

// Not gated: pure string handling, and the stub `main` below answers
// `--version` too, so the one line three surfaces agree on is compiled and
// tested on every platform in the matrix rather than only on the one that can
// run the app.
mod version;

// Not gated either, though for a narrower reason: the ServiceManagement calls
// inside are macOS only, but the state-to-screen mapping beside them is pure,
// and a macOS-gated module would take its tests with it. Ungated, `cargo test
// --workspace` covers that mapping wherever it is run, including the Linux
// checkout that never compiles a line of AppKit.
mod login_item;

#[cfg(target_os = "macos")]
fn main() {
    app::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    if std::env::args().any(|argument| argument == "--version") {
        println!("{}", version::long());
        return;
    }
    eprintln!(
        "openflow-native is the macOS AppKit build of OpenFlow. \
         On this platform, build the Tauri app in src-tauri instead."
    );
}
