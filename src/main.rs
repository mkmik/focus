//! POC: sticky-note windows whose titlebar is the same color as the body.
use objc2::rc::Retained;
use objc2::runtime::Sel;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSApplication, NSApplicationActivationPolicy,
    NSAutoresizingMaskOptions, NSColor, NSMenu, NSMenuItem, NSTextView, NSView, NSViewController,
    NSWindow, NSWindowTitleVisibility,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

const SIZE: f64 = 220.0;
const COLORS: [(f64, f64, f64); 4] = [
    (1.00, 0.95, 0.55), // yellow
    (1.00, 0.77, 0.85), // pink
    (0.70, 0.88, 1.00), // blue
    (0.78, 0.95, 0.66), // green
];

fn main() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // Paper stays light in Dark Mode too: black text, light traffic lights.
    app.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua }).as_deref());
    app.setMainMenu(Some(&main_menu(mtm)));

    let _notes: Vec<_> = (0..COLORS.len()).map(|i| note(mtm, i)).collect();

    // `activate()` is cooperative since macOS 14 and doesn't bring a shell-launched,
    // unbundled binary to the front; the deprecated call still does.
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    app.run();
}

fn note(mtm: MainThreadMarker, i: usize) -> Retained<NSWindow> {
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
        // nil target: the action goes up the responder chain (text view, then NSApp).
        unsafe { menu.addItemWithTitle_action_keyEquivalent(&title, Some(action), &key) };
    }
    let top = NSMenuItem::new(mtm);
    top.setSubmenu(Some(&menu));
    top
}
