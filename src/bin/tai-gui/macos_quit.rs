//! macOS: the app menu's Quit (Cmd+Q) closes the window instead of
//! terminating the app at once, so it asks first when a terminal is busy,
//! as the window's close button does (main.rs, `confirm_quit`).
//!
//! winit 0.30.13's default menu gives Quit the action `terminate:`
//! (src/platform_impl/macos/menu.rs), which ends the app without a close
//! request egui could cancel. Pointed at the window's `performClose:`,
//! it asks the window whether it should close, which winit turns into
//! that close request. Quitting from the Dock or at logout still
//! terminates without asking.

use objc2::{MainThreadMarker, runtime::AnyObject, sel};
use objc2_app_kit::NSApplication;

/// Point the menu item that sends `terminate:` at the window's
/// `performClose:`. False until the menu and the window exist.
pub fn quit_closes_the_window() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    let (Some(menu), Some(window)) = (app.mainMenu(), app.windows().firstObject()) else {
        return false;
    };
    for i in 0..menu.numberOfItems() {
        let Some(submenu) = menu.itemAtIndex(i).and_then(|item| item.submenu()) else {
            continue;
        };
        for j in 0..submenu.numberOfItems() {
            let Some(item) = submenu.itemAtIndex(j) else {
                continue;
            };
            if item.action() == Some(sel!(terminate:)) {
                let target: &AnyObject = &window;
                // SAFETY: an NSWindow answers performClose:, and it lives
                // as long as the app, so the menu's unretained target
                // stays valid.
                unsafe {
                    item.setTarget(Some(target));
                    item.setAction(Some(sel!(performClose:)));
                }
                return true;
            }
        }
    }
    false
}
