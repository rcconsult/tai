# Vendored miniquad

Source: the crates.io release of miniquad 0.4.11 (not-fl3/miniquad),
MIT OR Apache-2.0, the version vendored macroquad 0.4.16 pins. Only
`src/`, `build.rs`, `Cargo.toml`, the licences and the README are
vendored; the first commit that adds this directory is the release
unchanged, so `git diff` against it shows every patch.

tai's root `Cargo.toml` uses this copy through `[patch.crates-io]`.
ADR 0015 decision 5 is why it is vendored: input methods and Wayland
need changes miniquad doesn't have.

## Patches

1. `Cargo.toml`: the dev-dependencies are gone (the examples aren't
   vendored), and `[lints.rust] warnings = "allow"`, as for macroquad
   (its patch 2).
5. Input methods and dead keys (ADR 0015). Numbered after the Wayland
   patches (2-4, branch `tai-wayland-native`), which this doesn't need.
   - `src/event.rs`: `EventHandler::ime_preedit_event` (the text being
     composed, empty when it ends) and `ime_commit_event` (committed
     text, or text with no key).
   - `src/lib.rs`, `src/native.rs`: `window::set_ime_area` (the cursor's
     cell in client pixels), a `SetImeArea` request. Windows takes its
     top-left as `set_ime_position`; X11, Android and iOS ignore it.
   - `src/native/macos.rs`: the view is an `NSTextInputClient`.
     `keyDown` runs `interpretKeyEvents` first, as Ghostty's does
     (`SurfaceView_AppKit.swift` at 108cacb1): a key that ends a
     composition sends only `ime_commit_event`; keys while composing
     send nothing (as winit does); others send `key_down_event` and then
     the text the system made, or their own characters as before.
     `setMarkedText`/`unmarkText` report the preedit; `insertText`
     outside `keyDown` (emoji picker, dictation) commits, joining a
     surrogate pair split over two calls; `firstRectForCharacterRange`
     answers with the `set_ime_area` cell; `doCommandBySelector`
     swallows commands so AppKit doesn't beep. A Ctrl chord types no
     text (the system gives Ctrl+/ a "/"; tai-gui gets none with Ctrl
     either), and `performKeyEquivalent` sends Cmd+. on as a key, which
     AppKit otherwise takes as cancel before `keyDown`.
7. X11 input method (ADR 0015 part 4), `src/native/linux_x11/xim.rs` and
   `src/native/linux_x11.rs`, set up as winit 0.30 does: `LC_CTYPE` from
   the environment (restored if Xlib can't use it), `XOpenIM` with the
   user's input method or Xlib's own (`@im=none`, which still composes
   dead keys and the Compose key), and an input context with on-the-spot
   preedit callbacks when the method offers them, else `PreeditNothing`.
   Every event goes through `XFilterEvent` first, as Xlib requires; a
   key press with keycode 0 carries a commit (`ime_commit_event`); other
   presses type `Xutf8LookupString`'s text, or the keysym's character as
   before when that text is a control character; FocusIn/FocusOut set
   the context's focus; `set_ime_area` sets the spot (the cell's
   bottom-left), only when it moves. The variadic Xlib calls are looked
   up with `dlsym`.
8. Windows input method (ADR 0015 part 6), `src/native/windows.rs`, as
   winit 0.30 does it: `WM_IME_COMPOSITION`'s result string is an
   `ime_commit_event` (whole UTF-16, so surrogate pairs survive; it was
   sent as characters one code unit at a time) and its composition
   string an `ime_preedit_event`; the message isn't passed to
   DefWindowProc and `WM_IME_SETCONTEXT` clears
   `ISC_SHOWUICOMPOSITIONWINDOW`, so the IME draws no composition window
   of its own; `WM_IME_ENDCOMPOSITION` commits what a Korean IME leaves
   and ends the preedit; the composition and candidate windows go to the
   `set_ime_area` cell (candidates beside it, `CFS_EXCLUDE`) instead of
   the caret tai never sets.
