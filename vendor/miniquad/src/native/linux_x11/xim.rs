//! tai patch 7: an X input method for the window (ADR 0015), set up as
//! winit 0.30 does (`platform_impl/linux/x11/ime`): the process's
//! `LC_CTYPE` from the environment, an input method from `XOpenIM` (the
//! user's, else Xlib's built-in one, which still composes dead keys and
//! the Compose key), and an input context with on-the-spot preedit
//! callbacks when the input method offers them. Every event goes through
//! `XFilterEvent`; keys are read with `Xutf8LookupString`.
//!
//! The variadic Xlib calls (`XCreateIC`, `XSetICValues`, `XGetIMValues`,
//! `XVaCreateNestedList`) are looked up here with `dlsym`, since the
//! crate's `declare_module!` takes fixed argument lists only.

use super::libx11::{Display, KeySym, Window, XEvent, XKeyEvent};
use std::ffi::{c_char, c_int, c_ulong, c_void, CStr};
use std::ptr::null_mut;

/// A C string literal (the crate is edition 2018: no c"..." literals).
macro_rules! cs {
    ($s:expr) => {
        concat!($s, "\0").as_ptr() as *const c_char
    };
}

type Xim = *mut c_void;
type Xic = *mut c_void;

const XIM_PREEDIT_CALLBACKS: c_ulong = 0x0002;
const XIM_PREEDIT_NOTHING: c_ulong = 0x0008;
const XIM_PREEDIT_NONE: c_ulong = 0x0010;
const XIM_STATUS_NOTHING: c_ulong = 0x0400;
const XIM_STATUS_NONE: c_ulong = 0x0800;

const X_BUFFER_OVERFLOW: c_int = -1;
const X_LOOKUP_CHARS: c_int = 2;
const X_LOOKUP_BOTH: c_int = 4;

#[repr(C)]
struct XimStyles {
    count_styles: u16,
    supported_styles: *mut c_ulong,
}

#[repr(C)]
struct XimCallback {
    client_data: *mut c_void,
    callback: *const c_void,
}

#[repr(C)]
struct XimText {
    length: u16,
    feedback: *mut c_ulong,
    encoding_is_wchar: c_int,
    string: *mut c_char,
}

#[repr(C)]
struct XimPreeditDrawCallbackStruct {
    caret: c_int,
    chg_first: c_int,
    chg_length: c_int,
    text: *mut XimText,
}

#[repr(C)]
struct XPoint {
    x: i16,
    y: i16,
}

/// What the input method reported while it filtered events.
pub enum ImeOut {
    Preedit(String),
}

struct Lib {
    open_im: unsafe extern "C" fn(*mut Display, *mut c_void, *mut c_char, *mut c_char) -> Xim,
    close_im: unsafe extern "C" fn(Xim) -> c_int,
    create_ic: unsafe extern "C" fn(Xim, ...) -> Xic,
    destroy_ic: unsafe extern "C" fn(Xic),
    set_ic_values: unsafe extern "C" fn(Xic, ...) -> *mut c_char,
    get_im_values: unsafe extern "C" fn(Xim, ...) -> *mut c_char,
    nested_list: unsafe extern "C" fn(c_int, ...) -> *mut c_void,
    set_ic_focus: unsafe extern "C" fn(Xic),
    unset_ic_focus: unsafe extern "C" fn(Xic),
    filter_event: unsafe extern "C" fn(*mut XEvent, Window) -> c_int,
    utf8_lookup: unsafe extern "C" fn(
        Xic,
        *mut XKeyEvent,
        *mut c_char,
        c_int,
        *mut KeySym,
        *mut c_int,
    ) -> c_int,
    set_locale_modifiers: unsafe extern "C" fn(*const c_char) -> *mut c_char,
    supports_locale: unsafe extern "C" fn() -> c_int,
    free: unsafe extern "C" fn(*mut c_void) -> c_int,
}

impl Lib {
    unsafe fn load() -> Option<Lib> {
        let mut handle = libc::dlopen(cs!("libX11.so.6"), libc::RTLD_LAZY);
        if handle.is_null() {
            handle = libc::dlopen(cs!("libX11.so"), libc::RTLD_LAZY);
        }
        if handle.is_null() {
            return None;
        }
        macro_rules! sym {
            ($name:literal) => {{
                let f = libc::dlsym(handle, cs!($name));
                if f.is_null() {
                    return None;
                }
                std::mem::transmute(f)
            }};
        }
        Some(Lib {
            open_im: sym!("XOpenIM"),
            close_im: sym!("XCloseIM"),
            create_ic: sym!("XCreateIC"),
            destroy_ic: sym!("XDestroyIC"),
            set_ic_values: sym!("XSetICValues"),
            get_im_values: sym!("XGetIMValues"),
            nested_list: sym!("XVaCreateNestedList"),
            set_ic_focus: sym!("XSetICFocus"),
            unset_ic_focus: sym!("XUnsetICFocus"),
            filter_event: sym!("XFilterEvent"),
            utf8_lookup: sym!("Xutf8LookupString"),
            set_locale_modifiers: sym!("XSetLocaleModifiers"),
            supports_locale: sym!("XSupportsLocale"),
            free: sym!("XFree"),
        })
    }
}

/// The window's input method and context. Boxed: the preedit callbacks
/// hold a pointer to it.
pub struct XInput {
    lib: Lib,
    im: Xim,
    ic: Xic,
    window: Window,
    /// The text being composed, as on-the-spot callbacks draw it.
    preedit: Vec<char>,
    out: Vec<ImeOut>,
    callbacks: [XimCallback; 4],
    area: Option<(i32, i32, i32, i32)>,
}

impl XInput {
    /// Open the input method for `window`, or None where there is none.
    pub unsafe fn open(display: *mut Display, window: Window) -> Option<Box<XInput>> {
        let lib = Lib::load()?;
        // Input methods open without the locale set, but can't commit
        // (winit's comment); keep the C locale where Xlib can't use the
        // environment's.
        let default = libc::setlocale(libc::LC_CTYPE, null_mut());
        let default = (!default.is_null()).then(|| CStr::from_ptr(default).to_owned());
        libc::setlocale(libc::LC_CTYPE, cs!(""));
        if (lib.supports_locale)() == 0 {
            if let Some(default) = &default {
                libc::setlocale(libc::LC_CTYPE, default.as_ptr());
            }
        }
        // The user's input method (XMODIFIERS), else Xlib's own.
        (lib.set_locale_modifiers)(cs!(""));
        let mut im = (lib.open_im)(display, null_mut(), null_mut(), null_mut());
        if im.is_null() {
            (lib.set_locale_modifiers)(cs!("@im=none"));
            im = (lib.open_im)(display, null_mut(), null_mut(), null_mut());
        }
        if im.is_null() {
            return None;
        }
        let mut input = Box::new(XInput {
            lib,
            im,
            ic: null_mut(),
            window,
            preedit: Vec::new(),
            out: Vec::new(),
            callbacks: [
                XimCallback { client_data: null_mut(), callback: preedit_start as *const c_void },
                XimCallback { client_data: null_mut(), callback: preedit_done as *const c_void },
                XimCallback { client_data: null_mut(), callback: preedit_draw as *const c_void },
                XimCallback { client_data: null_mut(), callback: preedit_caret as *const c_void },
            ],
            area: None,
        });
        let me = &mut *input as *mut XInput as *mut c_void;
        for cb in &mut input.callbacks {
            cb.client_data = me;
        }
        let styles = input.styles();
        let lib = &input.lib;
        let ic = if styles.contains(&(XIM_PREEDIT_CALLBACKS | XIM_STATUS_NOTHING)) {
            let attrs = (lib.nested_list)(
                0,
                cs!("preeditStartCallback"),
                &input.callbacks[0] as *const XimCallback,
                cs!("preeditDoneCallback"),
                &input.callbacks[1] as *const XimCallback,
                cs!("preeditDrawCallback"),
                &input.callbacks[2] as *const XimCallback,
                cs!("preeditCaretCallback"),
                &input.callbacks[3] as *const XimCallback,
                null_mut::<c_void>(),
            );
            let ic = (lib.create_ic)(
                input.im,
                cs!("inputStyle"),
                XIM_PREEDIT_CALLBACKS | XIM_STATUS_NOTHING,
                cs!("clientWindow"),
                window,
                cs!("focusWindow"),
                window,
                cs!("preeditAttributes"),
                attrs,
                null_mut::<c_void>(),
            );
            (lib.free)(attrs);
            ic
        } else {
            null_mut()
        };
        let ic = if !ic.is_null() {
            ic
        } else {
            // The input method draws the preedit itself, or there is none
            // (Xlib's own, for dead keys and Compose).
            let style = if styles.contains(&(XIM_PREEDIT_NOTHING | XIM_STATUS_NOTHING)) {
                XIM_PREEDIT_NOTHING | XIM_STATUS_NOTHING
            } else {
                XIM_PREEDIT_NONE | XIM_STATUS_NONE
            };
            (lib.create_ic)(
                input.im,
                cs!("inputStyle"),
                style,
                cs!("clientWindow"),
                window,
                cs!("focusWindow"),
                window,
                null_mut::<c_void>(),
            )
        };
        if ic.is_null() {
            (input.lib.close_im)(input.im);
            return None;
        }
        input.ic = ic;
        (input.lib.set_ic_focus)(ic);
        Some(input)
    }

    unsafe fn styles(&self) -> Vec<c_ulong> {
        let mut styles: *mut XimStyles = null_mut();
        let failed = (self.lib.get_im_values)(
            self.im,
            cs!("queryInputStyle"),
            &mut styles as *mut *mut XimStyles,
            null_mut::<c_void>(),
        );
        if !failed.is_null() || styles.is_null() {
            return Vec::new();
        }
        let s = &*styles;
        let out = std::slice::from_raw_parts(s.supported_styles, s.count_styles as usize).to_vec();
        (self.lib.free)(styles as *mut c_void);
        out
    }

    /// Give `event` to the input method first; true when it took it.
    pub unsafe fn filter(&mut self, event: &mut XEvent) -> bool {
        (self.lib.filter_event)(event, 0) != 0
    }

    /// What the input method reported meanwhile.
    pub fn take(&mut self) -> Vec<ImeOut> {
        std::mem::take(&mut self.out)
    }

    /// A key press's text, through the input context.
    pub unsafe fn lookup(&mut self, key: &mut XKeyEvent) -> String {
        let mut buf = vec![0u8; 64];
        loop {
            let mut keysym: KeySym = 0;
            let mut status: c_int = 0;
            let n = (self.lib.utf8_lookup)(
                self.ic,
                key,
                buf.as_mut_ptr() as *mut c_char,
                buf.len() as c_int,
                &mut keysym,
                &mut status,
            );
            if status == X_BUFFER_OVERFLOW {
                buf.resize(n.max(0) as usize + 1, 0);
                continue;
            }
            if status == X_LOOKUP_CHARS || status == X_LOOKUP_BOTH {
                return String::from_utf8_lossy(&buf[..n.max(0) as usize]).into_owned();
            }
            return String::new();
        }
    }

    pub unsafe fn focus(&mut self, focused: bool) {
        if focused {
            (self.lib.set_ic_focus)(self.ic);
        } else {
            (self.lib.unset_ic_focus)(self.ic);
        }
    }

    /// Where the composition and the candidate window go: the cursor's
    /// cell, in window pixels. The spot is the cell's bottom-left, where
    /// over-the-spot input methods draw. Sent only when it moves.
    pub unsafe fn set_area(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if self.area == Some((x, y, w, h)) {
            return;
        }
        self.area = Some((x, y, w, h));
        let spot = XPoint {
            x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            y: (y + h).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        };
        let attrs = (self.lib.nested_list)(
            0,
            cs!("spotLocation"),
            &spot as *const XPoint,
            null_mut::<c_void>(),
        );
        (self.lib.set_ic_values)(
            self.ic,
            cs!("preeditAttributes"),
            attrs,
            null_mut::<c_void>(),
        );
        (self.lib.free)(attrs);
    }
}

impl Drop for XInput {
    fn drop(&mut self) {
        unsafe {
            if !self.ic.is_null() {
                (self.lib.destroy_ic)(self.ic);
            }
            (self.lib.close_im)(self.im);
        }
    }
}

unsafe fn input(client: *mut c_void) -> &'static mut XInput {
    &mut *(client as *mut XInput)
}

unsafe extern "C" fn preedit_start(_ic: Xic, client: *mut c_void, _call: *mut c_void) -> c_int {
    input(client).preedit.clear();
    // No limit on the preedit's length.
    -1
}

unsafe extern "C" fn preedit_done(_ic: Xic, client: *mut c_void, _call: *mut c_void) {
    let me = input(client);
    me.preedit.clear();
    me.out.push(ImeOut::Preedit(String::new()));
}

unsafe extern "C" fn preedit_draw(
    _ic: Xic,
    client: *mut c_void,
    call: *mut XimPreeditDrawCallbackStruct,
) {
    let me = input(client);
    let call = &*call;
    let first = (call.chg_first.max(0) as usize).min(me.preedit.len());
    let end = (first + call.chg_length.max(0) as usize).min(me.preedit.len());
    let new: Vec<char> = if call.text.is_null() {
        Vec::new()
    } else {
        let text = &*call.text;
        if text.encoding_is_wchar != 0 || text.string.is_null() {
            let wide = text.string as *const i32;
            if wide.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(wide, text.length as usize)
                    .iter()
                    .filter_map(|&c| char::from_u32(c as u32))
                    .collect()
            }
        } else {
            CStr::from_ptr(text.string).to_string_lossy().chars().collect()
        }
    };
    me.preedit.splice(first..end, new);
    me.out.push(ImeOut::Preedit(me.preedit.iter().collect()));
}

unsafe extern "C" fn preedit_caret(_ic: Xic, _client: *mut c_void, _call: *mut c_void) {}

