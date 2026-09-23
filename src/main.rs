//! POC: sticky-note windows whose titlebar is the same color as the body.
use std::path::Path;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSAutoresizingMaskOptions, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFloatingWindowLevel, NSMenu, NSMenuItem, NSMenuItemValidation,
    NSNormalWindowLevel, NSTextDidChangeNotification, NSTextView, NSView, NSViewController,
    NSWindow, NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString, NSUserDefaults,
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

    if let Some(saved) = load(db, i) {
        text.setString(&NSString::from_str(&saved));
    }
    // Save on every edit (typing, paste, cut), so quitting via Ctrl+C or a crash loses nothing.
    // `setString` doesn't post this notification, so loading above doesn't re-save.
    let (db, view) = (Rc::clone(db), text.clone());
    let on_change =
        RcBlock::new(move |_: NonNull<NSNotification>| save(&db, i, &view.string().to_string()));
    // No queue: the block runs synchronously on the posting (main) thread, so it needn't be Send.
    unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSTextDidChangeNotification),
            Some(&text),
            None,
            &on_change,
        )
    };

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
    // The trick: a transparent titlebar draws nothing, so the window's background
    // color shows through it and titlebar + body become one uniform color.
    window.setTitlebarAppearsTransparent(true);
    window.setTitleVisibility(NSWindowTitleVisibility::Hidden); // defaults to "Untitled"
    window.setBackgroundColor(Some(&color));
    window.setFrame_display(NSRect::new(origin, NSSize::new(SIZE, SIZE)), false);
    // After setFrame, not before: restores the frame saved in the user defaults
    // (`defaults read focus`) and re-saves it on every move/resize.
    window.setFrameAutosaveName(&NSString::from_str(&format!("note{i}")));
    window.makeKeyAndOrderFront(None);
    window
}

/// One row per note, keyed by the note index (like the `note{i}` frame autosave names).
fn open_db(path: &Path) -> Connection {
    let db = Connection::open(path).expect("can't open the notes db");
    db.execute(
        "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, text TEXT NOT NULL)",
        (),
    )
    .expect("can't create the notes table");
    db
}

fn load(db: &Connection, id: usize) -> Option<String> {
    // Fatal: starting blank would overwrite the saved note on the first keystroke.
    db.query_row("SELECT text FROM notes WHERE id = ?1", [id as i64], |row| {
        row.get(0)
    })
    .optional()
    .expect("can't read the notes db")
}

fn save(db: &Connection, id: usize, text: &str) {
    // Log, don't panic: this runs inside an AppKit callback, and the next edit retries.
    let sql = "INSERT OR REPLACE INTO notes (id, text) VALUES (?1, ?2)";
    if let Err(e) = db.execute(sql, (id as i64, text)) {
        eprintln!("can't save note{id}: {e}");
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
    assert_eq!(load(&db, 0), None);
    save(&db, 0, "first");
    save(&db, 0, "second");
    save(&db, 1, "");
    assert_eq!(load(&db, 0).as_deref(), Some("second"));
    assert_eq!(load(&db, 1).as_deref(), Some(""));
}
