//! The menu-bar item and its dropdown, directly on AppKit.
//!
//! It replaces the vendored fork of fyne.io/systray that the Go build used,
//! and keeps that package's model, because main.rs is written against it:
//!
//! - Every call can come from any thread. The work runs on the main thread, and
//!   the call waits for it (systray's `waitUntilDone: YES`).
//! - A row is a small handle. Rows are added once, then shown, hidden and
//!   relabelled, never removed.
//! - Each row has a click channel with no buffer. A click goes through only
//!   when a receiver is waiting, and is dropped otherwise, as systray's
//!   non-blocking send to `ClickedCh` does.
//!
//! The menu is attached to the status item, so AppKit opens it natively,
//! directly below the menu bar. The fork patched `show_menu` to get the same
//! result.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use crossbeam_channel::{Receiver, Sender};
use dispatch2::run_on_main;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationDelegate, NSCellImagePosition, NSControlStateValueOff,
    NSControlStateValueOn, NSImage, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSNotification, NSObjectProtocol, NSSize, NSString};

/// The status item, its menu and the object that receives their events. They
/// live on the main thread only.
struct State {
    status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    target: Retained<Target>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static ON_READY: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

/// Click channels by row id. Ids start at 1, because 0 is the default tag of
/// every menu item, separators included.
static CLICKS: Mutex<Option<HashMap<isize, Sender<()>>>> = Mutex::new(None);
static NEXT_ID: AtomicIsize = AtomicIsize::new(1);
static ON_OPEN: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Target does not
    // implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GeminiUsageTrayTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(menuHandler:))]
        fn menu_handler(&self, sender: &NSMenuItem) {
            let tx = CLICKS
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|m| m.get(&sender.tag()).cloned());
            if let Some(tx) = tx {
                let _ = tx.try_send(()); // nobody waiting: drop it, as systray does
            }
        }
    }

    unsafe impl NSObjectProtocol for Target {}

    unsafe impl NSApplicationDelegate for Target {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _notification: &NSNotification) {
            let mtm = self.mtm();
            create_status_item(mtm, self.retain());
            if let Some(on_ready) = ON_READY.with(|r| r.borrow_mut().take()) {
                on_ready();
            }
        }
    }

    unsafe impl NSMenuDelegate for Target {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            if let Some(f) = ON_OPEN.get() {
                f();
            }
        }
    }
);

impl Target {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSObject's init takes no arguments and returns an instance.
        unsafe { msg_send![super(this), init] }
    }
}

/// Starts the app, calls `on_ready` once AppKit has finished launching, and
/// runs the event loop until the app quits. Call it on the main thread.
pub fn run(on_ready: impl FnOnce() + 'static) {
    let mtm = MainThreadMarker::new().expect("tray::run must be called on the main thread");
    ON_READY.with(|r| *r.borrow_mut() = Some(Box::new(on_ready)));

    let app = NSApplication::sharedApplication(mtm);
    let target = Target::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    app.run();
    drop(target);
}

fn create_status_item(mtm: MainThreadMarker, target: Retained<Target>) {
    let status_item =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);

    let menu = NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    menu.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    // After the user removes the item, nothing brings it back - not even a
    // restart - unless the app asks, so make sure it is visible at startup.
    status_item.setVisible(true);
    status_item.setMenu(Some(&menu));

    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            status_item,
            menu,
            target,
        })
    });
}

/// Runs `f` with the state on the main thread, and waits for it.
fn with_state<R: Send>(f: impl FnOnce(MainThreadMarker, &State) -> R + Send) -> Option<R> {
    run_on_main(|mtm| STATE.with(|s| s.borrow().as_ref().map(|st| f(mtm, st))))
}

/// Calls `f` each time the menu opens. Only the first call has an effect.
pub fn on_menu_will_open(f: impl Fn() + Send + Sync + 'static) {
    let _ = ON_OPEN.set(Box::new(f));
}

/// Sets the text in the menu bar.
pub fn set_title(title: &str) {
    let title = title.to_string();
    with_state(move |mtm, st| {
        if let Some(button) = st.status_item.button(mtm) {
            button.setTitle(&NSString::from_str(&title));
        }
        update_title_button_style(mtm, st);
    });
}

/// Sets the tooltip of the menu-bar item.
pub fn set_tooltip(tooltip: &str) {
    let tooltip = tooltip.to_string();
    with_state(move |mtm, st| {
        if let Some(button) = st.status_item.button(mtm) {
            button.setToolTip(Some(&NSString::from_str(&tooltip)));
        }
    });
}

/// Sets the image in front of the title, or removes it with `None`. The image
/// is scaled to 16×16 pt and is not a template, so it keeps its colour.
pub fn set_icon(png: Option<&'static [u8]>) {
    with_state(move |mtm, st| {
        let image = png.and_then(|bytes| {
            let data = NSData::with_bytes(bytes);
            let image = NSImage::initWithData(NSImage::alloc(), &data)?;
            image.setSize(NSSize::new(16.0, 16.0));
            image.setTemplate(false);
            Some(image)
        });
        if let Some(button) = st.status_item.button(mtm) {
            button.setImage(image.as_deref());
        }
        update_title_button_style(mtm, st);
    });
}

/// Puts the image left of the title, or alone, or nowhere, as systray does.
fn update_title_button_style(mtm: MainThreadMarker, st: &State) {
    let Some(button) = st.status_item.button(mtm) else {
        return;
    };
    let position = if button.image().is_none() {
        NSCellImagePosition::NoImage
    } else if button.title().length() == 0 {
        NSCellImagePosition::ImageOnly
    } else {
        NSCellImagePosition::ImageLeft
    };
    button.setImagePosition(position);
}

/// Adds a separator line at the end of the menu.
pub fn add_separator() {
    with_state(|mtm, st| st.menu.addItem(&NSMenuItem::separatorItem(mtm)));
}

/// Adds a row at the end of the menu.
pub fn add_item(title: &str, tooltip: &str) -> MenuItem {
    add(title, tooltip, false)
}

/// Adds a row with a checkmark at the end of the menu.
pub fn add_checkbox(title: &str, tooltip: &str, checked: bool) -> MenuItem {
    add(title, tooltip, checked)
}

fn add(title: &str, tooltip: &str, checked: bool) -> MenuItem {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = crossbeam_channel::bounded(0);
    CLICKS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(id, tx);

    let (title, tooltip) = (title.to_string(), tooltip.to_string());
    with_state(move |mtm, st| {
        // SAFETY: the action is a method that Target implements, and Target is
        // the item's target.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&title),
                Some(sel!(menuHandler:)),
                &NSString::from_str(""),
            )
        };
        item.setTag(id);
        // SAFETY: the target is retained in STATE for as long as the menu.
        unsafe { item.setTarget(Some(&st.target)) };
        item.setToolTip(Some(&NSString::from_str(&tooltip)));
        item.setEnabled(true);
        item.setState(state_value(checked));
        st.menu.addItem(&item);
    });
    MenuItem { id, clicked: rx }
}

fn state_value(checked: bool) -> isize {
    if checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    }
}

/// One row of the menu. It is cheap to clone and can be used from any thread.
#[derive(Clone)]
pub struct MenuItem {
    id: isize,
    clicked: Receiver<()>,
}

impl MenuItem {
    /// Receives one message for each click on the row.
    pub fn clicked(&self) -> &Receiver<()> {
        &self.clicked
    }

    fn update(&self, f: impl FnOnce(&NSMenuItem) + Send) {
        let id = self.id;
        with_state(move |_, st| {
            if let Some(item) = st.menu.itemWithTag(id) {
                f(&item);
            }
        });
    }

    pub fn set_title(&self, title: &str) {
        let title = title.to_string();
        self.update(move |i| i.setTitle(&NSString::from_str(&title)));
    }

    pub fn set_tooltip(&self, tooltip: &str) {
        let tooltip = tooltip.to_string();
        self.update(move |i| i.setToolTip(Some(&NSString::from_str(&tooltip))));
    }

    pub fn show(&self) {
        self.update(|i| i.setHidden(false));
    }

    pub fn hide(&self) {
        self.update(|i| i.setHidden(true));
    }

    pub fn disable(&self) {
        self.update(|i| i.setEnabled(false));
    }

    pub fn check(&self) {
        self.update(|i| i.setState(state_value(true)));
    }

    pub fn uncheck(&self) {
        self.update(|i| i.setState(state_value(false)));
    }

    /// Reports whether the row shows a checkmark.
    pub fn checked(&self) -> bool {
        let id = self.id;
        with_state(move |_, st| {
            st.menu
                .itemWithTag(id)
                .is_some_and(|i| i.state() == state_value(true))
        })
        .unwrap_or(false)
    }
}

/// Quits the app.
pub fn quit() {
    run_on_main(|mtm| NSApplication::sharedApplication(mtm).terminate(None));
}
