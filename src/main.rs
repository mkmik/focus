//! POC: sticky-note windows whose titlebar is the same color as the body.
mod herdr;

use std::path::Path;
use std::ptr::{NonNull, null_mut};
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSComboBox,
    NSComboBoxWillDismissNotification, NSControl, NSControlStateValueOff, NSControlStateValueOn,
    NSControlTextDidChangeNotification, NSControlTextDidEndEditingNotification, NSEvent,
    NSEventModifierFlags, NSEventType, NSFloatingWindowLevel, NSFont, NSFontAttributeName,
    NSFontManager, NSFontTraitMask, NSForegroundColorAttributeName, NSLineBreakMode, NSMenu,
    NSMenuItem, NSMenuItemValidation, NSMutableParagraphStyle, NSNormalWindowLevel,
    NSParagraphStyleAttributeName, NSResponder, NSScrollView, NSStrikethroughStyleAttributeName,
    NSText, NSTextAlignment, NSTextDidChangeNotification, NSTextField, NSTextView,
    NSUnderlineStyle, NSView, NSViewController, NSWindow, NSWindowButton, NSWindowStyleMask,
    NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSMutableAttributedString,
    NSNotification, NSNotificationCenter, NSNotificationName, NSNumber, NSObject, NSObjectProtocol,
    NSPoint, NSRange, NSRect, NSSize, NSString, NSTimer, NSUserDefaults,
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

    let (notes, ticks): (Vec<_>, Vec<_>) = (0..COLORS.len()).map(|i| note(mtm, i, &db)).unzip();
    // NSApp holds its delegate weakly; this binding keeps it (and the notes) alive until exit.
    let delegate = Delegate::new(mtm, notes);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    // Every 10s, each note checks whether its workspace is in use, and updates its age.
    let tick = move || ticks.iter().for_each(|tick| tick());
    tick();
    let tick = RcBlock::new(move |_: NonNull<NSTimer>| tick());
    // SAFETY: needn't be Send, as a timer scheduled here fires on this (main) thread's run loop.
    unsafe { NSTimer::scheduledTimerWithTimeInterval_repeats_block(10.0, true, &tick) };

    // `activate()` is cooperative since macOS 14 and doesn't bring a shell-launched,
    // unbundled binary to the front; the deprecated call still does.
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    app.run();
}

/// Returns the note, and what updates its age.
fn note(
    mtm: MainThreadMarker,
    i: usize,
    db: &Rc<Connection>,
) -> (Retained<NSWindow>, Rc<dyn Fn()>) {
    let (r, g, b) = COLORS[i];
    let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
    let origin = NSPoint::new(100.0 + i as f64 * (SIZE + 30.0), 400.0);

    // Called on the subclass, it makes a scrollable Checklist.
    let scroll: Retained<NSScrollView> =
        unsafe { msg_send![Checklist::class(), scrollableTextView] };
    scroll.setHasVerticalScroller(false); // legacy (mouse) scrollers paint a gray track
    let text = scroll
        .documentView()
        .unwrap()
        .downcast::<Checklist>()
        .unwrap();
    text.setDrawsBackground(false);
    // 8px padding on every side; the inset alone would stack on the default 5px line padding.
    text.setTextContainerInset(NSSize::new(8.0, 8.0));
    unsafe { text.textContainer() }
        .unwrap()
        .setLineFragmentPadding(0.0);

    text.set_items(&load_list(db, i));
    // Save on every edit (typing, paste, cut, a click on a box), so quitting via Ctrl+C or a crash
    // loses nothing. `set_items` doesn't post this notification, so loading above doesn't re-save.
    let (db2, view) = (Rc::clone(db), text.clone());
    observe(unsafe { NSTextDidChangeNotification }, &text, move || {
        let json =
            serde_json::to_string(&view.items()).expect("strings and bools always serialize");
        save(&db2, "lists", i, &json)
    });

    let half = SIZE / 2.0;
    let body = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::ZERO, NSSize::new(SIZE, SIZE)),
    );
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

    // Top right, 8px in like the text: how long ago the workspace was last in use (see
    // `herdr::in_use` and `ago`), blank if never seen. In the workspace's bold gray, a little bigger.
    let age = NSTextField::labelWithString(&NSString::new(), mtm);
    age.setFont(Some(&NSFont::boldSystemFontOfSize(15.0)));
    age.setTextColor(Some(&NSColor::tertiaryLabelColor()));
    age.setAlignment(NSTextAlignment::Right);
    let h = age.intrinsicContentSize().height;
    age.setFrame(NSRect::new(
        NSPoint::new(half, SIZE - 8.0 - h),
        NSSize::new(half - 8.0, h),
    ));
    // Like the text, it stays at the top when the titlebar takes its share of the height.
    age.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
    body.addSubview(&age);

    // The text fills the rest: full width, between the workspace below and the age above
    // (non-flipped: y grows upwards). Only its height gives when the titlebar takes its share.
    let bottom = picker.frame().max().y.max(8.0 + height); // clear of the picker too
    scroll.setFrame(NSRect::new(
        NSPoint::new(0.0, bottom),
        NSSize::new(SIZE, SIZE - 8.0 - h - bottom),
    ));
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    body.addSubview(&scroll);
    let (db2, label) = (Rc::clone(db), workspace.clone());
    let tick: Rc<dyn Fn()> = Rc::new(move || {
        let name = label.stringValue().to_string();
        // Unreachable (not running?), the age just goes on counting.
        if herdr::in_use(&herdr::socket(), &name).unwrap_or(false) {
            set_worked(&db2, &name);
        }
        let secs = worked_ago(&db2, &name);
        age.setStringValue(&NSString::from_str(&secs.map(ago).unwrap_or_default()));
    });

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
    let (db2, label, combo, win, text2, tick2) = (
        Rc::clone(db),
        workspace.clone(),
        picker.clone(),
        window.clone(),
        text.clone(),
        Rc::clone(&tick),
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
                tick2();
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
    (window, tick)
}

/// `secs` ago, cut down to its biggest unit, to read at a glance: "now", "10m", "2h" or "3d".
fn ago(secs: i64) -> String {
    match secs {
        ..60 => "now".to_owned(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
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

/// Where an item's text starts: its box goes in the gap, and its lines all start there.
const INDENT: f64 = 18.0;
const BOX: f64 = 11.0;

define_class!(
    // SAFETY: NSTextView has no subclassing requirements, and Checklist doesn't implement Drop.
    #[unsafe(super(NSTextView, NSText, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Checklist;

    // Like Google Keep: each paragraph is an item with a box before it, checked ones struck
    // through. Shift+Return breaks the line within an item. An item is checked iff its first
    // character is struck through, and every edit spreads that to the whole item.
    impl Checklist {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            unsafe { msg_send![super(self), drawRect: dirty] }
            let storage = unsafe { self.textStorage() }.unwrap();
            NSColor::secondaryLabelColor().setStroke();
            for (start, _, line) in self.lines() {
                let y = line.origin.y + (line.size.height - BOX) / 2.0;
                let b = NSRect::new(NSPoint::new(line.origin.x, y), NSSize::new(BOX, BOX));
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(b, 2.0, 2.0).stroke();
                if checked(&storage, start) {
                    let check = NSBezierPath::new();
                    check.moveToPoint(NSPoint::new(b.origin.x + 2.5, y + BOX / 2.0));
                    check.lineToPoint(NSPoint::new(b.origin.x + BOX * 0.42, y + BOX - 2.5));
                    check.lineToPoint(NSPoint::new(b.origin.x + BOX - 2.0, y + 2.0));
                    check.setLineWidth(1.5);
                    check.stroke();
                }
            }
        }

        // A click on an item's box (or beside it, on its first line) toggles it.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let at = self.convertPoint_fromView(event.locationInWindow(), None);
            if at.x < self.textContainerOrigin().x + INDENT {
                let storage = unsafe { self.textStorage() }.unwrap();
                for (start, end, line) in self.lines() {
                    if (line.min().y..line.max().y).contains(&at.y) {
                        // An empty last item has no character to strike: it can't be checked.
                        if end > start {
                            set_checked(&storage, start, end, !checked(&storage, start));
                            self.didChangeText();
                        }
                        return;
                    }
                }
            }
            unsafe { msg_send![super(self), mouseDown: event] }
        }

        // Typing takes on the checkedness of the item it goes in, even at its start (where it'd
        // otherwise take after the item above's newline).
        #[unsafe(method(shouldChangeTextInRange:replacementString:))]
        fn should_change(&self, range: NSRange, s: Option<&NSString>) -> bool {
            let storage = unsafe { self.textStorage() }.unwrap();
            let (start, ..) = paragraph(&storage.string(), range.location);
            unsafe { self.setTypingAttributes(&attrs(checked(&storage, start))) };
            unsafe { msg_send![super(self), shouldChangeTextInRange: range, replacementString: s] }
        }

        // An edit can leave an item half checked (backspacing two into one, pasting styled
        // text): its first character decides. Then on to the notification, for the save.
        #[unsafe(method(didChangeText))]
        fn did_change_text(&self) {
            let storage = unsafe { self.textStorage() }.unwrap();
            storage.beginEditing();
            for (start, _, end) in paragraphs(&storage.string()) {
                set_checked(&storage, start, end, checked(&storage, start));
            }
            storage.endEditing();
            self.setNeedsDisplay(true); // the boxes of the items it moved
            unsafe { msg_send![super(self), didChangeText] }
        }

        #[unsafe(method(insertNewline:))]
        fn insert_newline(&self, sender: Option<&AnyObject>) {
            let app = NSApplication::sharedApplication(self.mtm());
            let shift = app
                .currentEvent()
                .is_some_and(|e| e.modifierFlags().contains(NSEventModifierFlags::Shift));
            if shift {
                return unsafe { msg_send![self, insertLineBreak: sender] };
            }
            unsafe { msg_send![super(self), insertNewline: sender] }
            // A new empty item starts unchecked, even after a checked one (whose newline it got).
            let storage = unsafe { self.textStorage() }.unwrap();
            let (start, contents, end) = paragraph(&storage.string(), self.selectedRange().location);
            if start == contents && checked(&storage, start) {
                set_checked(&storage, start, end, false);
                unsafe { self.setTypingAttributes(&attrs(false)) };
                self.didChangeText();
            }
        }
    }
);

impl Checklist {
    /// Each item's (start, end) in the text, and its first line's rect in the view.
    fn lines(&self) -> Vec<(usize, usize, NSRect)> {
        let (lm, tc) = unsafe { (self.layoutManager().unwrap(), self.textContainer().unwrap()) };
        lm.ensureLayoutForTextContainer(&tc);
        let origin = self.textContainerOrigin();
        let s = self.string();
        let paragraphs = paragraphs(&s).into_iter().map(|(start, _, end)| {
            let line = if start < s.length() {
                let glyph = lm.glyphIndexForCharacterAtIndex(start);
                unsafe { lm.lineFragmentRectForGlyphAtIndex_effectiveRange(glyph, null_mut()) }
            } else {
                lm.extraLineFragmentRect() // empty last item
            };
            let at = NSPoint::new(line.origin.x + origin.x, line.origin.y + origin.y);
            (start, end, NSRect::new(at, line.size))
        });
        paragraphs.collect()
    }

    /// Each item: whether it's checked, and its text (lines broken by '\n').
    fn items(&self) -> Vec<(bool, String)> {
        let storage = unsafe { self.textStorage() }.unwrap();
        let s = storage.string();
        let items = paragraphs(&s).into_iter().map(|(start, contents, _)| {
            let text = s.substringWithRange(NSRange::new(start, contents - start));
            (
                checked(&storage, start),
                text.to_string().replace('\u{2028}', "\n"),
            )
        });
        items.collect()
    }

    fn set_items(&self, items: &[(bool, String)]) {
        // Within an item, line breaks are line separators: a newline would start another item.
        let lines: Vec<_> = items
            .iter()
            .map(|(_, s)| s.replace('\n', "\u{2028}"))
            .collect();
        self.setString(&NSString::from_str(&lines.join("\n")));
        let storage = unsafe { self.textStorage() }.unwrap();
        for ((start, _, end), (checked, _)) in paragraphs(&storage.string()).into_iter().zip(items)
        {
            set_checked(&storage, start, end, *checked);
        }
        unsafe { self.setTypingAttributes(&attrs(false)) };
    }
}

/// How an item's text looks: indented past its box, and when checked, grayed and struck through.
fn attrs(checked: bool) -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
    let indent = NSMutableParagraphStyle::new();
    indent.setFirstLineHeadIndent(INDENT);
    indent.setHeadIndent(INDENT);
    let font = NSFont::systemFontOfSize(0.0);
    let color = if checked {
        NSColor::secondaryLabelColor()
    } else {
        NSColor::labelColor()
    };
    let strike = NSNumber::new_isize(NSUnderlineStyle::Single.0);
    let (mut keys, mut values) = unsafe {
        (
            vec![
                NSFontAttributeName,
                NSParagraphStyleAttributeName,
                NSForegroundColorAttributeName,
            ],
            vec![&*font as &AnyObject, &*indent, &*color],
        )
    };
    if checked {
        keys.push(unsafe { NSStrikethroughStyleAttributeName });
        values.push(&*strike);
    }
    NSDictionary::from_slices(&keys, &values)
}

/// Checks, or unchecks, the item from `start` to `end`.
fn set_checked(text: &NSMutableAttributedString, start: usize, end: usize, checked: bool) {
    let range = NSRange::new(start, end - start);
    unsafe { text.setAttributes_range(Some(&attrs(checked)), range) };
}

/// Whether the item starting `at` is checked.
fn checked(text: &NSAttributedString, at: usize) -> bool {
    let strike = unsafe { NSStrikethroughStyleAttributeName };
    at < text.length()
        && unsafe { text.attribute_atIndex_effectiveRange(strike, at, null_mut()) }.is_some()
}

/// The paragraph `at` is in: its start, its contents' end, and where the next one starts.
fn paragraph(s: &NSString, at: usize) -> (usize, usize, usize) {
    let (mut start, mut contents, mut end) = (0, 0, 0);
    let range = NSRange::new(at, 0);
    unsafe {
        s.getParagraphStart_end_contentsEnd_forRange(&mut start, &mut end, &mut contents, range)
    };
    (start, contents, end)
}

/// All of `s`'s paragraphs, as `paragraph` has them: there's always one, if only empty.
fn paragraphs(s: &NSString) -> Vec<(usize, usize, usize)> {
    let mut all = vec![paragraph(s, 0)];
    loop {
        let &(_, contents, end) = all.last().unwrap();
        if end == s.length() {
            // Ending on a newline, there's an empty one after it.
            if contents < end {
                all.push((end, end, end));
            }
            return all;
        }
        all.push(paragraph(s, end));
    }
}

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
/// no migration; so did checklists (`lists`), superseding the plain text `notes`, which stay as a
/// backup (see `load_list`). Except `worked`: one row per herdr workspace (by name) a note has seen in use.
fn open_db(path: &Path) -> Connection {
    let db = Connection::open(path).expect("can't open the notes db");
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS titles (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS lists (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS workspaces (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS worked (name TEXT PRIMARY KEY, at INTEGER NOT NULL);",
    )
    .expect("can't create the notes tables");
    db
}

/// The note's checklist items, as JSON `[[checked, text], ...]`; from before checklists, one
/// unchecked item per line of its text.
fn load_list(db: &Connection, id: usize) -> Vec<(bool, String)> {
    match load(db, "lists", id) {
        // Fatal, like `load`: starting blank would overwrite it on the first keystroke.
        Some(json) => serde_json::from_str(&json).expect("can't parse a saved list"),
        None => load(db, "notes", id)
            .unwrap_or_default()
            .lines()
            .map(|line| (false, line.to_owned()))
            .collect(),
    }
}

/// `table` goes into the SQL as is, hence 'static: "notes", "lists", "titles" or "workspaces".
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

/// Writes down that the workspace named `name` is in use, or just was.
fn set_worked(db: &Connection, name: &str) {
    // Log, don't panic: this runs inside an AppKit callback, and the next poll retries.
    let sql = "INSERT OR REPLACE INTO worked (name, at) VALUES (?1, unixepoch())";
    if let Err(e) = db.execute(sql, [name]) {
        eprintln!("can't save when {name} worked: {e}");
    }
}

/// How many seconds ago the workspace named `name` was last seen in use.
fn worked_ago(db: &Connection, name: &str) -> Option<i64> {
    // Errors leave the age blank too: it's only for show, and the next poll retries.
    let sql = "SELECT unixepoch() - at FROM worked WHERE name = ?1";
    db.query_row(sql, [name], |row| row.get(0)).ok()
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
    assert_eq!(worked_ago(&db, "focus"), None);
    set_worked(&db, "focus");
    assert!(worked_ago(&db, "focus").is_some_and(|secs| (0..=1).contains(&secs)));
}

#[test]
fn lists_migrate_from_notes() {
    let db = open_db(Path::new(":memory:"));
    assert_eq!(load_list(&db, 0), []);
    save(&db, "notes", 0, "milk\neggs\n");
    let items = vec![(false, "milk".to_owned()), (false, "eggs".to_owned())];
    assert_eq!(load_list(&db, 0), items);
    save(&db, "lists", 0, r#"[[true,"two\nlines"],[false,""]]"#);
    let items = vec![(true, "two\nlines".to_owned()), (false, String::new())];
    assert_eq!(load_list(&db, 0), items);
}

#[test]
fn paragraphs_are_items() {
    let p = |s: &str| paragraphs(&NSString::from_str(s));
    assert_eq!(p(""), [(0, 0, 0)]);
    assert_eq!(p("a"), [(0, 1, 1)]);
    assert_eq!(p("a\nbc\n"), [(0, 1, 2), (2, 4, 5), (5, 5, 5)]);
    assert_eq!(p("a\u{2028}b\n\nc"), [(0, 3, 4), (4, 4, 5), (5, 6, 6)]);
}

#[test]
fn ages_read_at_a_glance() {
    let ages = [-5, 0, 59, 60, 3599, 3600, 86399, 86400, 30 * 86400].map(ago);
    assert_eq!(
        ages,
        ["now", "now", "now", "1m", "59m", "1h", "23h", "1d", "30d"]
    );
}
