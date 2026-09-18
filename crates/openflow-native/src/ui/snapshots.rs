//! Deterministic native screenshots, without constructing an Engine or App.
//!
//! `--ui-snapshots <new-directory>` renders synthetic preview states through
//! AppKit itself. It does not launch the ordinary app, register shortcuts,
//! inspect personal storage, request permissions, or start an audio device.
//! These are layout fixtures, not evidence that live recording works.

use std::cell::RefCell;
use std::io::Write;
use std::path::Path;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath,
    NSBitmapImageFileType, NSColor, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect};

define_class!(
    // SAFETY: NSView supports subclassing; these override its drawing methods
    // without storing additional state or owning a delegate.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowSnapshotBackdrop"]
    struct Backdrop;

    impl Backdrop {
        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool { true }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            NSColor::windowBackgroundColor().setFill();
            NSBezierPath::fillRect(self.bounds());
        }
    }
);

pub fn run(output: &Path) -> Result<usize, String> {
    // Refuse existing directories rather than overwriting review evidence.
    std::fs::create_dir(output)
        .map_err(|error| format!("Could not create snapshot directory: {error}"))?;
    let mtm = MainThreadMarker::new().ok_or("Native snapshots require the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    app.finishLaunching();

    let mut count = 0;
    for (name, appearance_name) in [
        ("light", unsafe { NSAppearanceNameAqua }),
        ("dark", unsafe { NSAppearanceNameDarkAqua }),
    ] {
        let appearance = NSAppearance::appearanceNamed(appearance_name)
            .ok_or("The requested native appearance is unavailable")?;
        app.setAppearance(Some(&appearance));
        let mut views = super::onboarding::preview_views(mtm);
        views.extend(super::dictate::preview_views(mtm));
        views.extend(super::settings::preview_views(mtm));
        views.extend(super::history::preview_views(mtm));
        views.extend(super::plugins::preview_views(mtm));
        for (state, view) in views {
            if !safe_name(&state) {
                return Err("A snapshot fixture has an unsafe name".to_string());
            }
            let path = output.join(format!("{state}-{name}.png"));
            render(&view, &appearance, &path, mtm)?;
            count += 1;
        }
    }
    Ok(count)
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn render(
    view: &NSView,
    appearance: &NSAppearance,
    path: &Path,
    mtm: MainThreadMarker,
) -> Result<(), String> {
    let bounds = view.bounds();
    if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
        return Err("A snapshot fixture has an empty frame".to_string());
    }
    // Supply backing scale, effective appearance and view lifecycle without
    // ordering a window onto the screen or making it key.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::new(-10000.0, -10000.0), bounds.size),
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setAppearance(Some(appearance));
    let backdrop: Retained<Backdrop> =
        unsafe { msg_send![Backdrop::alloc(mtm), initWithFrame: bounds] };
    backdrop.addSubview(view);
    window.setContentView(Some(&backdrop));
    view.setAppearance(Some(appearance));
    backdrop.layoutSubtreeIfNeeded();

    let outcome = RefCell::new(None);
    let draw = RcBlock::new(|| {
        let result = (|| {
            let bitmap = backdrop
                .bitmapImageRepForCachingDisplayInRect(bounds)
                .ok_or("AppKit could not allocate the snapshot bitmap")?;
            backdrop.cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
            // Empty properties contain no incorrectly typed format-specific
            // entries. PNG encoding needs no additional properties.
            let data = unsafe {
                bitmap.representationUsingType_properties(
                    NSBitmapImageFileType::PNG,
                    &NSDictionary::new(),
                )
            }
            .ok_or("AppKit could not encode the snapshot PNG")?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|error| format!("Could not create snapshot: {error}"))?;
            file.write_all(&data.to_vec())
                .map_err(|error| format!("Could not write snapshot: {error}"))?;
            Ok(())
        })();
        *outcome.borrow_mut() = Some(result);
    });
    appearance.performAsCurrentDrawingAppearance(&draw);
    drop(draw);
    window.setContentView(None);
    outcome
        .into_inner()
        .unwrap_or_else(|| Err("AppKit did not render the snapshot".to_string()))
}

#[cfg(test)]
mod tests {
    use super::safe_name;

    #[test]
    fn snapshot_names_stay_in_the_requested_directory() {
        assert!(safe_name("onboarding-local-ready_2"));
        for name in ["", "../outside", "/absolute", "a/b", "a\\b", "a.png"] {
            assert!(!safe_name(name));
        }
    }
}
