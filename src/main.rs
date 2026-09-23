//! POC: sticky-note windows whose titlebar is the same color as the body.
mod herdr;

use std::path::Path;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSAutoresizingMaskOptions, NSColor, NSComboBox,
    NSComboBoxWillDismissNotification, NSControl, NSControlStateValueOff, NSControlStateValueOn,
    NSControlTextDidChangeNotification, NSControlTextDidEndEditingNotification, NSEvent,
    NSEventType, NSFloatingWindowLevel, NSFont, NSFontAttributeName, NSFontManager,
    NSFontTraitMask, NSLineBreakMode, NSMenu, NSMenuItem, NSMenuItemValidation,
    NSMutableParagraphStyle, NSNormalWindowLevel, NSParagraphStyleAttributeName, NSResponder,
    NSTextAlignment, NSTextDidChangeNotification, NSTextField, NSTextView, NSView,
    NSViewController, NSWindow, NSWindowButton, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSNotification, NSNotificationCenter, NSNotificationName,
    NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSUserDefaults,
};
use rusqlite::{Connection, OptionalExtension};

const SIZE: f64 = 220.0;
const COLORS: [(f64, f64, f64); 4] = [
    (1.00, 0.95, 0.55), // yellow
    (1.00, 0.77, 0.85), // pink
    (0.70, 0.88, 1.00), // blue
    (0.78, 0.95, 0.66), // green
];
/// User defaults key behind Window > Keep All Windows on Top.
const KEEP_ON_TOP: &str = "KeepOnTop";

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // Paper stays light in Dark Mode too: black text, light traffic lights.
    app.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua }).as_deref());
    app.setMainMenu(Some(&main_menu(mtm)));

    // Where macOS apps keep per-user data: ~/Library/Application Support/focus/notes.sqlite
    let dir = std::env::home_dir()
        .expect("no home dir")
        .join("Library/Application Support/focus");
    std::fs::create_dir_all(&dir).expect("can't create the app data dir");
    let db = Rc::new(open_db(&dir.join("notes.sqlite")));

    let notes = (0..COLORS.len()).map(|i| note(mtm, i, &db)).collect();
    // NSApp holds its delegate weakly; this binding keeps it (and the notes) alive until exit.
    let delegate = Delegate::new(mtm, notes);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    // `activate()` is cooperative since macOS 14 and doesn't bring a shell-launched,
    // unbundled binary to the front; the deprecated call still does.
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    app.run();
}

fn note(mtm: MainThreadMarker, i: usize, db: &Rc<Connection>) -> Retained<NSWindow> {
    let (r, g, b) = COLORS[i];
    let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
    let origin = NSPoint::new(100.0 + i as f64 * (SIZE + 30.0), 400.0);

    let scroll = NSTextView::scrollableTextView(mtm);
    scroll.setHasVerticalScroller(false); // legacy (mouse) scrollers paint a gray track
    let text = scroll
        .documentView()
        .unwrap()
        .downcast::<NSTextView>()
        .unwrap();
    text.setDrawsBackground(false);
    // 8px padding on every side; the inset alone would stack on the default 5px line padding.
    text.setTextContainerInset(NSSize::new(8.0, 8.0));
    unsafe { text.textContainer() }
        .unwrap()
        .setLineFragmentPadding(0.0);

    if let Some(saved) = load(db, "notes", i) {
        text.setString(&NSString::from_str(&saved));
    }
    // Save on every edit (typing, paste, cut), so quitting via Ctrl+C or a crash loses nothing.
    // `setString` doesn't post this notification, so loading above doesn't re-save.
    let (db2, view) = (Rc::clone(db), text.clone());
    observe(unsafe { NSTextDidChangeNotification }, &text, move || {
        save(&db2, "notes", i, &view.string().to_string())
    });

    // Only the top-left quarter of the note is text (non-flipped: y grows upwards).
    // Size and right/bottom margins are all flexible, so resizes split evenly and keep it a quarter.
    let half = SIZE / 2.0;
    let body = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(SIZE, SIZE)),
    );
    scroll.setFrame(NSRect::new(
        NSPoint::new(0.0, half),
        NSSize::new(half, half),
    ));
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable
            | NSAutoresizingMaskOptions::ViewMaxXMargin
            | NSAutoresizingMaskOptions::ViewHeightSizable
            | NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    body.addSubview(&scroll);
    let vc = NSViewController::new(mtm);
    vc.setView(&body);

    // Titled, closable, miniaturizable, resizable; not released on close (the Retained owns it).
    let window = NSWindow::windowWithContentViewController(&vc);
    // Close button only: minimize and zoom gray out, and the edges don't resize.
    window.setStyleMask(NSWindowStyleMask::Titled | NSWindowStyleMask::Closable);
    // The trick: a transparent titlebar draws nothing, so the window's background
    // color shows through it and titlebar + body become one uniform color.
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden); // we draw an editable one below
    window.setBackgroundColor(Some(&color));
    window.setFrame_display(NSRect::new(origin, NSSize::new(SIZE, SIZE)), false);
    // After setFrame, not before: restores the position saved in the user defaults
    // (`defaults read focus`) and re-saves it on every move. Being non-resizable, it keeps SIZE.
    window.setFrameAutosaveName(&NSString::from_str(&format!("note{i}")));

    // The title: a label pixel-identical to AppKit's own title once that's too long to center
    // (titlebar font, from 6pt past the traffic lights to 6pt before the edge, 1pt above them).
    let saved = NSString::from_str(&load(db, "titles", i).unwrap_or_default());
    let title: Retained<Label> = unsafe { msg_send![Label::class(), labelWithString: &*saved] };
    title.setFont(Some(&NSFont::titleBarFontOfSize(0.0)));
    title.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let zoom = window
        .standardWindowButton(NSWindowButton::ZoomButton)
        .unwrap();
    let (x, y) = (zoom.frame().max().x + 6.0, zoom.frame().min().y + 1.0);
    let size = NSSize::new(SIZE - x - 6.0, zoom.frame().size.height);
    title.setFrame(NSRect::new(NSPoint::new(x, y), size));
    unsafe { zoom.superview() }.unwrap().addSubview(&title);
    // The real title stays hidden, but VoiceOver and the Dock's window list still read it.
    window.setTitle(&saved);
    // A double-click edits it, all selected, like renaming in Finder.
    let (field, win) = (title.clone(), window.clone());
    observe(&NSString::from_str(DOUBLE_CLICK), &title, move || {
        field.setEditable(true);
        win.makeFirstResponder(Some(&field));
    });
    let (db2, field, win) = (Rc::clone(db), title.clone(), window.clone());
    observe(
        unsafe { NSControlTextDidChangeNotification },
        &title,
        move || {
            let s = field.stringValue();
            win.setTitle(&s);
            save(&db2, "titles", i, &s.to_string());
        },
    );
    // Done (Return, or a click in the note): back to a label, and on to the note's text.
    let (field, win, text2) = (title.clone(), window.clone(), text.clone());
    observe(
        unsafe { NSControlTextDidEndEditingNotification },
        &title,
        move || {
            field.setEditable(false);
            win.makeFirstResponder(Some(&text2));
        },
    );

    // Bottom right, 8px in like the text: the herdr workspace the note is about. Bold gray like
    // the title of a note in the background (tertiary is what AppKit dims that to), until a
    // double-click swaps it for a combobox of herdr's workspace names.
    let saved = NSString::from_str(&load(db, "workspaces", i).unwrap_or_default());
    let workspace: Retained<Label> = unsafe { msg_send![Label::class(), labelWithString: &*saved] };
    workspace.setFont(Some(&NSFont::titleBarFontOfSize(0.0)));
    workspace.setTextColor(Some(&NSColor::tertiaryLabelColor()));
    workspace.setAlignment(NSTextAlignment::Right);
    workspace.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    // Unset, it shows where to double-click: a placeholder, in regular italic, not bold. An
    // attributed placeholder ignores the label's alignment, hence one of its own.
    let italic = NSFontManager::sharedFontManager(mtm).convertFont_toHaveTrait(
        &NSFont::systemFontOfSize(0.0),
        NSFontTraitMask::ItalicFontMask,
    );
    let right = NSMutableParagraphStyle::new();
    right.setAlignment(NSTextAlignment::Right);
    let attributes = NSDictionary::from_slices(
        &unsafe { [NSFontAttributeName, NSParagraphStyleAttributeName] },
        &[&*italic as &AnyObject, &*right],
    );
    let placeholder = NSString::from_str("herdr...");
    let placeholder = unsafe { NSAttributedString::new_with_attributes(&placeholder, &attributes) };
    workspace.setPlaceholderAttributedString(Some(&placeholder));
    let height = workspace.intrinsicContentSize().height;
    workspace.setFrame(NSRect::new(
        NSPoint::new(8.0, 8.0),
        NSSize::new(SIZE - 16.0, height),
    ));
    body.addSubview(&workspace);
    // Same width, centered on the label's line.
    let picker = NSComboBox::new(mtm);
    let h = picker.intrinsicContentSize().height;
    picker.setFrame(NSRect::new(
        NSPoint::new(8.0, 8.0 + (height - h) / 2.0),
        NSSize::new(SIZE - 16.0, h),
    ));
    picker.setCompletes(true); // typing a name's start fills in the rest
    picker.setHidden(true);
    body.addSubview(&picker);

    let (label, combo, win) = (workspace.clone(), picker.clone(), window.clone());
    observe(&NSString::from_str(DOUBLE_CLICK), &workspace, move || {
        combo.removeAllItems();
        unsafe { combo.addItemWithObjectValue(&NSString::from_str(NO_WORKSPACE)) };
        match herdr::workspaces(&herdr::socket()) {
            Ok(names) => {
                for name in names {
                    unsafe { combo.addItemWithObjectValue(&NSString::from_str(&name)) };
                }
            }
            // Leaves just (none) to pick: a name can still be typed in.
            Err(e) => eprintln!("can't list herdr's workspaces: {e}"),
        }
        // Empty, the current name as its placeholder: a list opens scrolled to the item that
        // matches the field, which would hide (none) above it.
        combo.setStringValue(&NSString::new());
        combo.setPlaceholderString(Some(&label.stringValue()));
        label.setHidden(true);
        combo.setHidden(false);
        win.makeFirstResponder(Some(&combo));
    });
    // Done (Return, or a click elsewhere): back to the label, and on to the note's text.
    let (db2, label, combo, win, text2) = (
        Rc::clone(db),
        workspace.clone(),
        picker.clone(),
        window.clone(),
        text.clone(),
    );
    observe(
        unsafe { NSControlTextDidEndEditingNotification },
        &picker,
        move || {
            // Left empty, it keeps the current name; (none) unsets it, back to the placeholder.
            let s = combo.stringValue().to_string();
            if !s.is_empty() {
                let s = if s == NO_WORKSPACE { "" } else { s.as_str() };
                label.setStringValue(&NSString::from_str(s));
                save(&db2, "workspaces", i, s);
            }
            combo.setHidden(true);
            label.setHidden(false);
            win.makeFirstResponder(Some(&text2));
        },
    );
    // A click in the list is done too: of all the ways the list closes, only that one is on a
    // mouse-up (a click elsewhere closes it on the mouse-down). The pick only reaches the field
    // after the list is gone, too late for the save above, so put it there now.
    let (combo, win) = (picker.clone(), window.clone());
    observe(
        unsafe { NSComboBoxWillDismissNotification },
        &picker,
        move || {
            let app = NSApplication::sharedApplication(mtm);
            let click = app
                .currentEvent()
                .is_some_and(|e| e.r#type() == NSEventType::LeftMouseUp);
            if click && let Some(pick) = combo.objectValueOfSelectedItem() {
                unsafe { combo.setObjectValue(Some(&pick)) };
                win.makeFirstResponder(Some(&text));
            }
        },
    );

    window.makeKeyAndOrderFront(None);
    window
}

/// Posted by a [`Label`] when double-clicked.
const DOUBLE_CLICK: &str = "LabelDoubleClick";
/// Tops the list of workspaces; picking it unsets the note's.
const NO_WORKSPACE: &str = "(none)";

define_class!(
    // SAFETY: NSTextField has no subclassing requirements, and Label doesn't implement Drop.
    #[unsafe(super(NSTextField, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Label;

    impl Label {
        // A double-click posts DOUBLE_CLICK, for `observe` to act on. Other clicks get the label
        // default, and a label counts as titlebar: dragging the title moves the window.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if event.clickCount() == 2 {
                let name = NSString::from_str(DOUBLE_CLICK);
                let center = NSNotificationCenter::defaultCenter();
                unsafe { center.postNotificationName_object(&name, Some(self)) };
            } else {
                unsafe { msg_send![super(self), mouseDown: event] }
            }
        }
    }
);

/// Runs `f` on every `name` notification `object` posts. No queue: it runs synchronously on the
/// posting (main) thread, so it needn't be Send.
fn observe(name: &NSNotificationName, object: &AnyObject, f: impl Fn() + 'static) {
    let block = RcBlock::new(move |_: NonNull<NSNotification>| f());
    unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(name),
            Some(object),
            None,
            &block,
        )
    };
}

/// One row per note in each table, keyed by the note index (like the `note{i}` frame autosave
/// names). Titles and workspaces got their own tables, not columns, so notes dbs from before need
/// no migration.
fn open_db(path: &Path) -> Connection {
    let db = Connection::open(path).expect("can't open the notes db");
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS titles (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS workspaces (id INTEGER PRIMARY KEY, text TEXT NOT NULL);",
    )
    .expect("can't create the notes tables");
    db
}

/// `table` goes into the SQL as is, hence 'static: "notes", "titles" or "workspaces".
fn load(db: &Connection, table: &'static str, id: usize) -> Option<String> {
    // Fatal: starting blank would overwrite the saved note on the first keystroke.
    let sql = format!("SELECT text FROM {table} WHERE id = ?1");
    db.query_row(&sql, [id as i64], |row| row.get(0))
        .optional()
        .expect("can't read the notes db")
}

fn save(db: &Connection, table: &'static str, id: usize, text: &str) {
    // Log, don't panic: this runs inside an AppKit callback, and the next edit retries.
    let sql = format!("INSERT OR REPLACE INTO {table} (id, text) VALUES (?1, ?2)");
    if let Err(e) = db.execute(&sql, (id as i64, text)) {
        eprintln!("can't save note{id} ({table}): {e}");
    }
}

/// Without a menu bar there's no Cmd+Q and no copy/paste in the text views.
fn main_menu(mtm: MainThreadMarker) -> Retained<NSMenu> {
    let bar = NSMenu::new(mtm);
    bar.addItem(&submenu(mtm, "App", &[("Quit", sel!(terminate:), "q")]));
    bar.addItem(&submenu(
        mtm,
        "Edit",
        &[
            ("Cut", sel!(cut:), "x"),
            ("Copy", sel!(copy:), "c"),
            ("Paste", sel!(paste:), "v"),
            ("Select All", sel!(selectAll:), "a"),
        ],
    ));
    bar.addItem(&submenu(
        mtm,
        "Window",
        &[("Keep All Windows on Top", sel!(toggleKeepOnTop:), "")],
    ));
    bar
}

fn submenu(
    mtm: MainThreadMarker,
    title: &str,
    items: &[(&str, Sel, &str)],
) -> Retained<NSMenuItem> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
    for &(title, action, key) in items {
        let (title, key) = (NSString::from_str(title), NSString::from_str(key));
        // nil target: the action goes up the responder chain (text view, NSApp, then its delegate).
        unsafe { menu.addItemWithTitle_action_keyEquivalent(&title, Some(action), &key) };
    }
    let top = NSMenuItem::new(mtm);
    top.setSubmenu(Some(&menu));
    top
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Delegate doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Vec<Retained<NSWindow>>]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}
    unsafe impl NSApplicationDelegate for Delegate {}

    unsafe impl NSMenuItemValidation for Delegate {
        // Keep All Windows on Top is the only item whose action reaches us: checkmark it when on.
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            item.setState(if keep_on_top() {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            true
        }
    }

    impl Delegate {
        #[unsafe(method(toggleKeepOnTop:))]
        fn toggle_keep_on_top(&self, _sender: Option<&AnyObject>) {
            NSUserDefaults::standardUserDefaults()
                .setBool_forKey(!keep_on_top(), &NSString::from_str(KEEP_ON_TOP));
            self.float();
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker, notes: Vec<Retained<NSWindow>>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(notes);
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        this.float();
        this
    }

    /// Floating windows stay above other apps' windows, even when we're in the background.
    fn float(&self) {
        let level = if keep_on_top() {
            NSFloatingWindowLevel
        } else {
            NSNormalWindowLevel
        };
        for note in self.ivars() {
            note.setLevel(level);
        }
    }
}

/// Saved in the user defaults next to the note frames (`defaults read focus`), so it survives relaunches.
fn keep_on_top() -> bool {
    NSUserDefaults::standardUserDefaults().boolForKey(&NSString::from_str(KEEP_ON_TOP))
}

#[test]
fn notes_round_trip() {
    let db = open_db(Path::new(":memory:"));
    assert_eq!(load(&db, "notes", 0), None);
    save(&db, "notes", 0, "first");
    save(&db, "notes", 0, "second");
    save(&db, "notes", 1, "");
    save(&db, "titles", 0, "title");
    save(&db, "workspaces", 1, "focus");
    assert_eq!(load(&db, "notes", 0).as_deref(), Some("second"));
    assert_eq!(load(&db, "notes", 1).as_deref(), Some(""));
    assert_eq!(load(&db, "titles", 0).as_deref(), Some("title"));
    assert_eq!(load(&db, "titles", 1), None);
    assert_eq!(load(&db, "workspaces", 1).as_deref(), Some("focus"));
}
