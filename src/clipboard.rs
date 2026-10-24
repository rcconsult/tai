//! System clipboard access.
//!
//! On Linux this is miniquad's own clipboard (X11 and Wayland). On macOS,
//! miniquad 0.4.11's clipboard is a stub (`MacosClipboard` in
//! `native/macos.rs`: get returns None, set does nothing), so tai uses
//! arboard there instead.

#[cfg(target_os = "macos")]
pub fn get() -> Option<String> {
    match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
        Ok(text) => Some(text),
        Err(e) => {
            tracing::warn!("clipboard read failed: {e}");
            None
        }
    }
}

#[cfg(target_os = "macos")]
pub fn set(text: &str) {
    if let Err(e) = arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
        tracing::warn!("clipboard write failed: {e}");
    }
}

#[cfg(not(target_os = "macos"))]
pub fn get() -> Option<String> {
    macroquad::miniquad::window::clipboard_get()
}

#[cfg(not(target_os = "macos"))]
pub fn set(text: &str) {
    macroquad::miniquad::window::clipboard_set(text);
}
