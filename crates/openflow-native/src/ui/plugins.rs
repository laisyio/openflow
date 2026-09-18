//! The Plugins page: the web build's plugins screen as an `NSTableView`.
//!
//! A page, not a window: the main window owns the frame and hands this one a
//! content rect to build itself into. See [`crate::ui::history`] for why the
//! rect is passed in rather than assumed.
//!
//! Same cell-based table as History, for the same reason: the rows are strings.
//! Enable and Disable go straight to `PluginManager`, and Install reads a
//! `manifest.json` out of a folder the user picked and hands the text to
//! `install_plugin`, which is exactly what the Tauri `install_plugin` command
//! does with the string the web screen would have sent it. The manifest is
//! validated inside core, so nothing here has to know what a valid one is.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSButton, NSControl, NSModalResponseOK, NSOpenPanel, NSScrollView,
    NSTableColumn, NSTableView, NSTableViewDataSource, NSTableViewStyle, NSTextField, NSView,
    NSWorkspace,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};

use openflow_core::engine::Engine;
use openflow_core::plugins::PluginInfo;

use crate::ui::card::{Card, GAP, MARGIN};
use crate::ui::{button, empty_state, note};

/// Gap between the card's edge and the table inside it, matching History's.
const TABLE_INSET: f64 = 10.0;

const COLUMN_NAME: &str = "name";
const COLUMN_VERSION: &str = "version";
const COLUMN_HOOKS: &str = "hooks";
const COLUMN_STATUS: &str = "status";
const COLUMN_DESCRIPTION: &str = "description";

// ── Row formatting ────────────────────────────────────────

/// The version column. A manifest may or may not spell the leading `v`; the
/// column always does, so the numbers line up.
pub fn version_label(version: &str) -> String {
    let version = version.trim();
    if version.is_empty() {
        "unversioned".to_string()
    } else if version.starts_with('v') || version.starts_with('V') {
        version.to_string()
    } else {
        format!("v{}", version)
    }
}

/// The hooks column. A plugin with no hooks is inert, and saying so is more
/// use than an empty cell.
pub fn hooks_label(hooks: &[String]) -> String {
    let hooks: Vec<&str> = hooks
        .iter()
        .map(|hook| hook.trim())
        .filter(|hook| !hook.is_empty())
        .collect();
    if hooks.is_empty() {
        "no hooks".to_string()
    } else {
        hooks.join(", ")
    }
}

/// The status column, which is also what the toggle button will do next.
pub fn status_label(enabled: bool) -> &'static str {
    if enabled {
        "Enabled"
    } else {
        "Disabled"
    }
}

/// What the Enable/Disable button says for the selected row.
pub fn toggle_title(selected: Option<bool>) -> &'static str {
    match selected {
        Some(true) => "Disable",
        _ => "Enable",
    }
}

/// What one row shows, in column order.
pub fn row_columns(plugin: &PluginInfo) -> [String; 5] {
    [
        plugin.manifest.name.clone(),
        version_label(&plugin.manifest.version),
        hooks_label(&plugin.manifest.hooks),
        status_label(plugin.enabled).to_string(),
        plugin.manifest.description.clone(),
    ]
}

/// The line under the table.
pub fn status_line(count: usize) -> String {
    match count {
        0 => "No plugins installed. Install one from a folder holding a manifest.json.".to_string(),
        1 => "1 plugin installed.".to_string(),
        count => format!("{} plugins installed.", count),
    }
}

/// The manifest a chosen folder must hold. The user picks the plugin's folder,
/// not the file, because that is how the folder is laid out on disk.
pub fn manifest_path(folder: &Path) -> PathBuf {
    folder.join("manifest.json")
}

/// Read the manifest text out of a chosen folder, naming the folder when it is
/// not there: "no manifest" with no path is unactionable.
pub fn read_manifest(folder: &Path) -> Result<String, String> {
    let path = manifest_path(folder);
    std::fs::read_to_string(&path)
        .map_err(|error| format!("Could not read {}: {}", path.display(), error))
}

/// Where one entry of the picked folder may be copied, or `None` when it may
/// not be copied at all.
///
/// `install_plugin` writes the manifest and nothing else
/// (crates/openflow-core/src/plugins.rs:121-141), so the entrypoint the manifest
/// names has to be carried over separately or the plugin fails the first time a
/// hook runs it. This is the one gate that copy goes through, and it answers
/// two questions at once because splitting them across two functions is how
/// such rules drift apart.
///
/// Safety: the name came from outside, so it is checked rather than trusted. It
/// has to be a single plain component landing as a direct child of the plugin
/// directory. (The directory itself is already safe: `validate_manifest`
/// rejects an id that could traverse.)
///
/// Policy, and the reason this is not a bare path check:
///
/// - Any dot-file is refused, `.enabled` above all. That name is core's enable
///   marker (crates/openflow-core/src/plugins.rs:90 reads it, :112 writes it),
///   so copying it out of another `~/.openflow/plugins/<id>` would install a
///   plugin already switched on: the status line would say to enable it, the
///   `PluginInfo` core just returned would say `enabled: false`, and an
///   executable nobody enabled would run on the next dictation.
/// - `manifest.json` is refused because core owns it. It was written moments
///   ago from the text this install validated, and copying the folder's copy
///   over it would replace a checked manifest with whatever is on disk now.
pub fn copy_destination(plugin_dir: &Path, file_name: &OsStr) -> Option<PathBuf> {
    let text = file_name.to_str()?;
    if text.is_empty() || text == "." || text == ".." || text.contains('/') || text.contains('\\') {
        return None;
    }
    if text.starts_with('.') || text == "manifest.json" {
        return None;
    }
    let name = Path::new(text);
    if name.components().count() != 1 || name.is_absolute() {
        return None;
    }
    let destination = plugin_dir.join(name);
    // Whatever the name was, the result has to be a direct child of the plugin
    // directory. This is the assertion the rules above are trying to make true.
    (destination.parent() == Some(plugin_dir)).then_some(destination)
}

/// Whether two paths name the same directory on disk.
fn same_directory(one: &Path, other: &Path) -> bool {
    match (one.canonicalize(), other.canonicalize()) {
        (Ok(one), Ok(other)) => one == other,
        // An uncanonicalisable path cannot be proven different, so fall back to
        // comparing what we were given rather than deciding they differ.
        _ => one == other,
    }
}

/// Copy the picked folder's own files into the installed plugin's directory,
/// answering how many landed.
///
/// Not recursive, on purpose: a plugin is a manifest and an entrypoint beside
/// it, and a recursive copy of a folder the user picked by hand is a much
/// larger promise. Symlinks are skipped rather than followed, so a link cannot
/// pull a file in from outside the folder that was chosen. `fs::copy` carries
/// the permission bits across on Unix, which is what keeps an executable
/// entrypoint executable. Which names are carried at all is
/// [`copy_destination`]'s decision, including the dot-files and the manifest it
/// refuses.
pub fn copy_plugin_files(source: &Path, plugin_dir: &Path) -> Result<usize, String> {
    if same_directory(source, plugin_dir) {
        return Err(format!(
            "{} is already the installed plugin, so there is nothing to copy.",
            source.display()
        ));
    }
    let entries = std::fs::read_dir(source)
        .map_err(|error| format!("Could not read {}: {}", source.display(), error))?;
    let mut copied = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        // `symlink_metadata`, never `metadata`: this must not follow a link.
        let Ok(metadata) = path.symlink_metadata() else {
            continue;
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let Some(destination) = copy_destination(plugin_dir, &entry.file_name()) else {
            continue;
        };
        std::fs::copy(&path, &destination).map_err(|error| {
            format!(
                "Could not copy {}: {}",
                entry.file_name().to_string_lossy(),
                error
            )
        })?;
        copied += 1;
    }
    Ok(copied)
}

/// What the status line says after a successful install.
///
/// The count excludes the manifest, which core wrote itself, so zero is a real
/// answer and worth saying plainly: a plugin whose manifest names an entrypoint
/// that was not in the folder will fail at hook time, and this is the only
/// moment the user can still see why.
pub fn installed_line(name: &str, files: usize) -> String {
    match files {
        0 => format!(
            "Installed {}. Nothing was beside its manifest, so it will fail if it names an entrypoint.",
            name
        ),
        1 => format!(
            "Installed {} and 1 file beside it. Enable it to let it run.",
            name
        ),
        files => format!(
            "Installed {} and {} files beside it. Enable it to let it run.",
            name, files
        ),
    }
}

// ── The window ────────────────────────────────────────────

struct Controls {
    table: Retained<NSTableView>,
    /// Shown in the card in place of the list when there is nothing to list.
    empty: crate::ui::EmptyState,
    status: Retained<NSTextField>,
    toggle: Retained<NSButton>,
    install: Retained<NSButton>,
    reveal: Retained<NSButton>,
}

pub struct PluginsIvars {
    engine: Arc<Engine>,
    view: Retained<NSView>,
    controls: Controls,
    rows: RefCell<Vec<PluginInfo>>,
}

define_class!(
    // SAFETY: NSObject imposes no subclassing requirements; this class holds
    // only ivars and implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OpenFlowPluginsPage"]
    #[ivars = PluginsIvars]
    pub struct PluginsPage;

    unsafe impl NSObjectProtocol for PluginsPage {}

    unsafe impl NSTableViewDataSource for PluginsPage {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> isize {
            self.ivars().rows.borrow().len() as isize
        }

        #[unsafe(method_id(tableView:objectValueForTableColumn:row:))]
        fn object_value(
            &self,
            _table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: isize,
        ) -> Option<Retained<AnyObject>> {
            let identifier = column.map(|column| column.identifier().to_string());
            self.cell_value(row, identifier.as_deref()).map(|value| {
                let string: Retained<NSString> = NSString::from_str(&value);
                // SAFETY: the table takes an `id`, and a text cell wants a
                // string.
                unsafe { Retained::cast_unchecked(string) }
            })
        }
    }

    impl PluginsPage {
        #[unsafe(method(toggleSelected:))]
        fn toggle_selected(&self, _sender: &NSControl) {
            let Some(plugin) = self.selected() else {
                self.say("Select a plugin first.");
                return;
            };
            let plugins = self.ivars().engine.plugins();
            let id = &plugin.manifest.id;
            let result = if plugin.enabled {
                plugins.disable_plugin(id)
            } else {
                plugins.enable_plugin(id)
            };
            match result {
                Ok(()) => {
                    self.load();
                    self.say(&format!(
                        "{} is now {}.",
                        plugin.manifest.name,
                        status_label(!plugin.enabled).to_lowercase()
                    ));
                }
                Err(error) => self.say(&error),
            }
        }

        #[unsafe(method(installFromFolder:))]
        fn install_from_folder(&self, _sender: &NSControl) {
            let Some(folder) = self.choose_folder() else {
                return;
            };
            let manifest = match read_manifest(&folder) {
                Ok(manifest) => manifest,
                Err(error) => {
                    self.say(&error);
                    return;
                }
            };
            // The same call the Tauri `install_plugin` command makes, with the
            // same string: core validates the manifest and writes it into the
            // plugins directory, disabled. It writes only the manifest, so the
            // rest of the folder is carried over here; without it the plugin is
            // listed, enabled, and then fails at hook time with no entrypoint
            // on disk.
            let plugin = match self.ivars().engine.plugins().install_plugin(&manifest) {
                Ok(plugin) => plugin,
                Err(error) => {
                    self.say(&error);
                    return;
                }
            };
            let outcome = copy_plugin_files(&folder, Path::new(&plugin.path));
            self.load();
            match outcome {
                Ok(copied) => self.say(&installed_line(&plugin.manifest.name, copied)),
                Err(error) => self.say(&error),
            }
        }

        #[unsafe(method(revealFolder:))]
        fn reveal_folder(&self, _sender: &NSControl) {
            let directory = self.ivars().engine.plugins().plugins_dir().to_path_buf();
            let path = NSString::from_str(&directory.to_string_lossy());
            let url = NSURL::fileURLWithPath(&path);
            let opened = NSWorkspace::sharedWorkspace().openURL(&url);
            if !opened {
                self.say(&format!("Could not open {}.", directory.display()));
            }
        }

        #[unsafe(method(selectionChanged:))]
        fn selection_changed(&self, _sender: &NSControl) {
            self.update_toggle();
        }
    }
);

impl PluginsPage {
    /// Build the page into a view of `size`, the content pane the main window
    /// has to give it.
    pub fn new(
        app: &std::rc::Rc<crate::app::App>,
        mtm: MainThreadMarker,
        size: NSSize,
    ) -> Retained<Self> {
        let engine = Arc::clone(app.engine());

        let (view, controls) = build_content(mtm, size);

        let this = Self::alloc(mtm).set_ivars(PluginsIvars {
            engine,
            view,
            controls,
            rows: RefCell::new(Vec::new()),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };

        let table = &this.ivars().controls.table;
        // Weak property: the table does not retain us.
        unsafe { table.setDataSource(Some(ProtocolObject::from_ref(&*this))) };
        this.wire_actions();
        this.load();
        this
    }

    fn wire_actions(&self) {
        let controls = &self.ivars().controls;
        let target: &AnyObject = self.as_ref();
        crate::ui::wire(&controls.toggle, target, sel!(toggleSelected:));
        crate::ui::wire(&controls.install, target, sel!(installFromFolder:));
        crate::ui::wire(&controls.reveal, target, sel!(revealFolder:));
        unsafe {
            controls.table.setTarget(Some(target));
            // A single click retitles the button to match the row; a double
            // click flips the plugin.
            controls.table.setAction(Some(sel!(selectionChanged:)));
            controls.table.setDoubleAction(Some(sel!(toggleSelected:)));
        }
    }

    /// The view the main window installs in its content pane.
    pub fn view(&self) -> Retained<NSView> {
        self.ivars().view.clone()
    }

    /// Re-read the plugin directory. Called when the page is shown and after
    /// every change this page makes; nothing else writes it.
    pub fn load(&self) {
        let ivars = self.ivars();
        let plugins = ivars.engine.plugins().list_plugins();
        let count = plugins.len();
        *ivars.rows.borrow_mut() = plugins;
        ivars.controls.table.reloadData();
        self.update_toggle();
        // The empty state carries the sentence when there is nothing to list,
        // so the line under the card would only repeat it.
        ivars.controls.empty.set_hidden(count > 0);
        ivars
            .controls
            .table
            .enclosingScrollView()
            .inspect(|scroll| {
                scroll.setHidden(count == 0);
            });
        let line = if count == 0 {
            String::new()
        } else {
            status_line(count)
        };
        self.say(&line);
    }

    fn update_toggle(&self) {
        let title = toggle_title(self.selected().map(|plugin| plugin.enabled));
        self.ivars()
            .controls
            .toggle
            .setTitle(&NSString::from_str(title));
    }

    fn cell_value(&self, row: isize, column: Option<&str>) -> Option<String> {
        let rows = self.ivars().rows.borrow();
        let plugin = rows.get(usize::try_from(row).ok()?)?;
        let [name, version, hooks, status, description] = row_columns(plugin);
        Some(match column {
            Some(COLUMN_VERSION) => version,
            Some(COLUMN_HOOKS) => hooks,
            Some(COLUMN_STATUS) => status,
            Some(COLUMN_DESCRIPTION) => description,
            _ => name,
        })
    }

    fn selected(&self) -> Option<PluginInfo> {
        let ivars = self.ivars();
        let index = usize::try_from(ivars.controls.table.selectedRow()).ok()?;
        ivars.rows.borrow().get(index).cloned()
    }

    /// Ask for the plugin's folder. Directories only, one at a time, no new
    /// folders: the user is pointing at something that already exists.
    fn choose_folder(&self) -> Option<PathBuf> {
        let mtm = MainThreadMarker::new()?;
        let panel = NSOpenPanel::openPanel(mtm);
        panel.setCanChooseFiles(false);
        panel.setCanChooseDirectories(true);
        panel.setAllowsMultipleSelection(false);
        panel.setCanCreateDirectories(false);
        panel.setPrompt(Some(&NSString::from_str("Install")));
        panel.setMessage(Some(&NSString::from_str(
            "Choose the plugin folder, the one holding manifest.json.",
        )));
        if panel.runModal() != NSModalResponseOK {
            return None;
        }
        let url = panel.URL()?;
        let path = url.path()?;
        Some(PathBuf::from(path.to_string()))
    }

    fn say(&self, message: &str) {
        self.ivars()
            .controls
            .status
            .setStringValue(&NSString::from_str(message));
    }
}

// ── Layout ────────────────────────────────────────────────

/// Lay the page out into a view of `size`. Same shape as History's: a card
/// holding the list, a row of verbs under it, and a margin all the way round.
fn build_content(mtm: MainThreadMarker, size: NSSize) -> (Retained<NSView>, Controls) {
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), size),
    );
    let inner = size.width - MARGIN * 2.0;
    crate::ui::page_heading(
        mtm,
        &view,
        "Plugins",
        "Only enable plugins you trust. Local-only protection does not restrict plugin code.",
    );

    // ── Bottom row: the verbs, and the status line above them ──
    let mut x = MARGIN;
    let mut action = |title: &str, width: f64| {
        let control = button(
            mtm,
            NSRect::new(NSPoint::new(x, MARGIN), NSSize::new(width, 26.0)),
            title,
            0,
        );
        control.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMaxYMargin);
        x += width + 8.0;
        control
    };
    let toggle = action("Enable", 90.0);
    let install = action("Install from folder", 150.0);
    let reveal = action("Reveal in Finder", 140.0);

    let status = note(
        mtm,
        "",
        NSRect::new(
            NSPoint::new(MARGIN, MARGIN + 30.0),
            NSSize::new(inner, 16.0),
        ),
    );
    status.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin,
    );

    // ── The list, on a card above them ──
    let card_bottom = MARGIN + 30.0 + 16.0 + GAP;
    let card_height = (size.height - MARGIN - crate::ui::PAGE_HEADER_HEIGHT - card_bottom).max(0.0);
    let card = Card::new(
        mtm,
        NSRect::new(
            NSPoint::new(MARGIN, card_bottom),
            NSSize::new(inner, card_height),
        ),
    );
    card.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );

    let table_frame = NSRect::new(
        NSPoint::new(TABLE_INSET, TABLE_INSET),
        NSSize::new(
            (inner - TABLE_INSET * 2.0).max(0.0),
            (card_height - TABLE_INSET * 2.0).max(0.0),
        ),
    );
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), table_frame);
    let table = NSTableView::initWithFrame(
        NSTableView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), table_frame.size),
    );
    for (identifier, title, width) in [
        (COLUMN_NAME, "Plugin", 130.0),
        (COLUMN_VERSION, "Version", 70.0),
        (COLUMN_HOOKS, "Hooks", 130.0),
        (COLUMN_STATUS, "Status", 70.0),
        (COLUMN_DESCRIPTION, "What it does", 190.0),
    ] {
        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str(identifier),
        );
        column.setWidth(width);
        column.setTitle(&NSString::from_str(title));
        table.addTableColumn(&column);
    }
    table.setStyle(NSTableViewStyle::Inset);
    table.setUsesAlternatingRowBackgroundColors(false);
    table.setAllowsMultipleSelection(false);
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setBorderType(objc2_app_kit::NSBorderType::NoBorder);
    scroll.setDrawsBackground(false);
    scroll.setDocumentView(Some(&table));
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    card.addSubview(&scroll);

    // Sits over the table, in the card, and takes its place when the list is
    // empty. The status line below then says nothing rather than saying the
    // same sentence twice.
    let empty = empty_state(
        mtm,
        table_frame,
        "puzzlepiece.extension",
        "No plugins yet",
        "Install one from a folder holding a manifest.json.",
    );
    card.addSubview(&empty.view);

    view.addSubview(&card);
    view.addSubview(&toggle);
    view.addSubview(&install);
    view.addSubview(&reveal);
    view.addSubview(&status);

    (
        view,
        Controls {
            table,
            empty,
            status,
            toggle,
            install,
            reveal,
        },
    )
}

pub(super) fn preview_views(mtm: MainThreadMarker) -> Vec<(String, Retained<NSView>)> {
    [
        ("plugins", NSSize::new(704.0, 620.0)),
        ("plugins-narrow", NSSize::new(520.0, 440.0)),
    ]
    .into_iter()
    .map(|(name, size)| {
        let (view, controls) = build_content(mtm, size);
        controls
            .table
            .enclosingScrollView()
            .inspect(|scroll| scroll.setHidden(true));
        (name.to_string(), view)
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openflow_core::plugins::PluginManifest;

    fn plugin(enabled: bool) -> PluginInfo {
        PluginInfo {
            manifest: PluginManifest {
                id: "wordcount".to_string(),
                name: "Word count".to_string(),
                version: "1.2.0".to_string(),
                description: "Counts the words in a dictation.".to_string(),
                author: None,
                hooks: vec!["on_transcribe".to_string(), "on_format".to_string()],
                entrypoint: Some("run.sh".to_string()),
            },
            enabled,
            path: "/tmp/wordcount".to_string(),
        }
    }

    /// The version column marks versions consistently, whether or not the
    /// manifest wrote the `v`, and says so when there is no version at all.
    #[test]
    fn the_version_column_is_marked_once() {
        assert_eq!(version_label("1.2.0"), "v1.2.0");
        assert_eq!(version_label("v1.2.0"), "v1.2.0");
        assert_eq!(version_label(" 0.1 "), "v0.1");
        assert_eq!(version_label(""), "unversioned");
    }

    /// A plugin with no hooks never runs, and the row has to say so rather than
    /// leaving a blank that reads as "not loaded yet".
    #[test]
    fn the_hooks_column_names_an_inert_plugin() {
        assert_eq!(
            hooks_label(&["on_transcribe".to_string(), "on_format".to_string()]),
            "on_transcribe, on_format"
        );
        assert_eq!(hooks_label(&[]), "no hooks");
        assert_eq!(hooks_label(&["  ".to_string()]), "no hooks");
    }

    /// The button says what it will do, not what the row is, and defaults to
    /// Enable when nothing is selected.
    #[test]
    fn the_toggle_says_what_it_will_do() {
        assert_eq!(toggle_title(Some(true)), "Disable");
        assert_eq!(toggle_title(Some(false)), "Enable");
        assert_eq!(toggle_title(None), "Enable");
        assert_eq!(status_label(true), "Enabled");
        assert_eq!(status_label(false), "Disabled");
    }

    /// Five columns, in the order the table declares them.
    #[test]
    fn a_row_carries_every_column() {
        let columns = row_columns(&plugin(true));
        assert_eq!(columns[0], "Word count");
        assert_eq!(columns[1], "v1.2.0");
        assert_eq!(columns[2], "on_transcribe, on_format");
        assert_eq!(columns[3], "Enabled");
        assert_eq!(columns[4], "Counts the words in a dictation.");
        assert_eq!(row_columns(&plugin(false))[3], "Disabled");
    }

    /// The only file name in this app that comes from outside: it has to land
    /// as a direct child of the plugin directory or not be copied at all.
    #[test]
    fn only_a_plain_file_name_is_copied_into_the_plugin_directory() {
        let plugin_dir = Path::new("/tmp/plugins/wordcount");
        assert_eq!(
            copy_destination(plugin_dir, OsStr::new("run.sh")),
            Some(plugin_dir.join("run.sh"))
        );
        for refused in [
            "..",
            ".",
            "",
            "../evil",
            "sub/run.sh",
            "/etc/passwd",
            // Core's enable marker: carrying it installs a plugin already
            // switched on.
            ".enabled",
            ".hidden",
            // Core wrote this one, from the manifest this install validated.
            "manifest.json",
        ] {
            assert_eq!(
                copy_destination(plugin_dir, OsStr::new(refused)),
                None,
                "{refused} must be refused"
            );
        }
        // Every accepted name is a direct child, which is the property the
        // rules exist to guarantee.
        for accepted in ["run.sh", "hook.py", "a.b.c"] {
            let destination = copy_destination(plugin_dir, OsStr::new(accepted)).unwrap();
            assert_eq!(destination.parent(), Some(plugin_dir));
        }
    }

    /// The entrypoint has to arrive beside the manifest, and picking the
    /// installed plugin's own folder has to be refused rather than copying a
    /// file over itself.
    #[test]
    fn installing_carries_the_folder_beside_the_manifest() {
        let root =
            std::env::temp_dir().join(format!("openflow-install-test-{}", std::process::id()));
        let source = root.join("picked");
        let plugin_dir = root.join("installed");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(source.join("nested/ignored.txt"), "no").unwrap();
        // What a folder copied out of another installed plugin carries: the
        // enable marker, and a manifest that is not the one core validated.
        std::fs::write(source.join(".enabled"), "").unwrap();
        std::fs::write(source.join("manifest.json"), "{\"tampered\":true}").unwrap();
        std::fs::write(plugin_dir.join("manifest.json"), "{\"checked\":true}").unwrap();

        let copied = copy_plugin_files(&source, &plugin_dir).unwrap();
        assert_eq!(
            copied, 1,
            "run.sh only: not the directory, not the marker, not the manifest"
        );
        assert!(plugin_dir.join("run.sh").exists());
        assert!(!plugin_dir.join("nested").exists());

        // `list_plugins` decides `enabled` by whether this file is there
        // (crates/openflow-core/src/plugins.rs:90), so a plugin installed from
        // a folder has to report enabled == false.
        assert!(
            !plugin_dir.join(".enabled").exists(),
            "an installed plugin must not arrive switched on"
        );
        // And the manifest core wrote is still the one on disk.
        assert_eq!(
            std::fs::read_to_string(plugin_dir.join("manifest.json")).unwrap(),
            "{\"checked\":true}"
        );

        let error = copy_plugin_files(&plugin_dir, &plugin_dir).unwrap_err();
        assert!(
            error.contains("already the installed plugin"),
            "a self-copy must be refused: {error}"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_installed_line_counts_the_files_it_carried() {
        assert_eq!(
            installed_line("Word count", 1),
            "Installed Word count and 1 file beside it. Enable it to let it run."
        );
        assert_eq!(
            installed_line("Word count", 2),
            "Installed Word count and 2 files beside it. Enable it to let it run."
        );
        // The manifest is not counted, so zero is reachable and has to say
        // something a user can act on.
        assert_eq!(
            installed_line("Word count", 0),
            "Installed Word count. Nothing was beside its manifest, so it will fail if it names an entrypoint."
        );
    }

    #[test]
    fn the_status_line_counts_plugins() {
        assert!(status_line(0).contains("manifest.json"));
        assert_eq!(status_line(1), "1 plugin installed.");
        assert_eq!(status_line(4), "4 plugins installed.");
    }

    /// Install points at a folder; the manifest inside it is what core is
    /// handed. A folder without one has to name the path it looked at.
    #[test]
    fn a_folder_without_a_manifest_names_the_path_it_looked_at() {
        let folder =
            std::env::temp_dir().join(format!("openflow-plugin-test-{}", std::process::id()));
        assert_eq!(manifest_path(&folder), folder.join("manifest.json"));

        let error = read_manifest(&folder).unwrap_err();
        assert!(
            error.contains("manifest.json"),
            "the error must name the file: {error}"
        );

        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(manifest_path(&folder), "{\"id\":\"x\"}").unwrap();
        assert_eq!(read_manifest(&folder).unwrap(), "{\"id\":\"x\"}");
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
